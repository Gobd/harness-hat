use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
/// Container session management.
///
/// Each running container gets a `ContainerSession` that owns a PTY process
/// (`docker run -it …`) and a `vt100::Parser` screen buffer updated in
/// real-time by a background reader thread.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use std::time::Instant;
use tempfile::{NamedTempFile, TempDir};

use crate::config::{ContainerMount, MountMode};
use crate::container::helpers::{blend_toward_bg, luma_u8, xterm_256_index_to_rgb};
use tracing::instrument;

const INPUT_ECHO_GRACE: Duration = Duration::from_millis(350);

/// Live container session state and PTY plumbing for a launched container.
pub struct ContainerSession {
    pub container_name: String,
    pub agent_kind: crate::config::AgentKind,
    pub container_id: String,
    pub docker_name: String,
    pub project: String,
    pub session_token: String,
    pub parent_session_token: Option<String>,
    pub subagent_name: Option<String>,
    pub mount_target: String,
    pub launched_at: Instant,
    pub terminal_snapshot_hash: u64,
    pub terminal_changed_at: Instant,
    pub last_input_at: Arc<Mutex<Option<Instant>>>,
    pub term: Arc<FairMutex<Term<SessionEventProxy>>>,
    pub(crate) notifier: Notifier,
    pub(crate) window_size: Arc<Mutex<WindowSize>>,
    pub exited: Arc<AtomicBool>,
    pub has_bell: Arc<AtomicBool>,
    pub exit_reported: bool,
    pub(crate) _scoped_proxy: Option<crate::proxy::ScopedProxyListener>,
    pub(crate) _cred_tempfile: Option<NamedTempFile>,
    pub(crate) _env_tempfile: Option<NamedTempFile>,
    pub(crate) _hostdo_tempfile: Option<NamedTempFile>,
    pub(crate) _agent_config_tempdir: Option<TempDir>,
}

/// Event sink that keeps the Alacritty-backed PTY state synchronized with the
/// event loop and the UI.
#[derive(Clone)]
pub struct SessionEventProxy {
    pub(crate) sender: Arc<Mutex<Option<alacritty_terminal::event_loop::EventLoopSender>>>,
    pub(crate) window_size: Arc<Mutex<WindowSize>>,
    pub(crate) exited: Arc<AtomicBool>,
    pub(crate) has_bell: Arc<AtomicBool>,
    pub(crate) default_fg: alacritty_terminal::vte::ansi::Rgb,
    pub(crate) default_bg: alacritty_terminal::vte::ansi::Rgb,
    pub(crate) grayscale_palette: bool,
}

impl EventListener for SessionEventProxy {
    fn send_event(&self, event: Event) {
        let sender = self.sender.lock().ok().and_then(|s| s.clone());
        match event {
            Event::Bell => {
                self.has_bell.store(true, Ordering::Relaxed);
            }
            Event::Exit | Event::ChildExit(_) => {
                self.exited.store(true, Ordering::Relaxed);
            }
            Event::PtyWrite(s) => {
                if let Some(tx) = sender {
                    let _ = tx.send(Msg::Input(s.into_bytes().into()));
                }
            }
            Event::TextAreaSizeRequest(formatter) => {
                if let Some(tx) = sender {
                    let size = self.window_size.lock().map(|s| *s).unwrap_or(WindowSize {
                        num_lines: 24,
                        num_cols: 80,
                        cell_width: 0,
                        cell_height: 0,
                    });
                    let _ = tx.send(Msg::Input(formatter(size).into_bytes().into()));
                }
            }
            Event::ColorRequest(index, formatter) => {
                if let Some(tx) = sender {
                    let rgb = if index == 10 {
                        self.default_fg
                    } else if index == 11 {
                        self.default_bg
                    } else {
                        let (r, g, b) = xterm_256_index_to_rgb(index as u8);
                        let (r, g, b) = if self.grayscale_palette {
                            let y = luma_u8((r, g, b));
                            (y, y, y)
                        } else {
                            (r, g, b)
                        };
                        let blend_weight = if self.grayscale_palette { 0.45 } else { 0.35 };
                        let (r, g, b) = blend_toward_bg(
                            (r, g, b),
                            (self.default_bg.r, self.default_bg.g, self.default_bg.b),
                            blend_weight,
                        );
                        alacritty_terminal::vte::ansi::Rgb { r, g, b }
                    };
                    let _ = tx.send(Msg::Input(formatter(rgb).into_bytes().into()));
                }
            }
            Event::ClipboardLoad(_ty, formatter) => {
                if let Some(tx) = sender {
                    let _ = tx.send(Msg::Input(formatter("").into_bytes().into()));
                }
            }
            _ => {}
        }
    }
}

/// Helper struct to implement `alacritty_terminal::grid::Dimensions` for resizing the terminal view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TermSize {
    pub(crate) cols: usize,
    pub(crate) lines: usize,
}

impl Dimensions for TermSize {
    fn columns(&self) -> usize {
        self.cols
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn last_column(&self) -> alacritty_terminal::index::Column {
        alacritty_terminal::index::Column(self.cols.saturating_sub(1))
    }
    fn topmost_line(&self) -> alacritty_terminal::index::Line {
        alacritty_terminal::index::Line(0)
    }
    fn bottommost_line(&self) -> alacritty_terminal::index::Line {
        alacritty_terminal::index::Line(self.lines.saturating_sub(1) as i32)
    }
    fn history_size(&self) -> usize {
        0
    }
}

impl ContainerSession {
    /// Checks if the container session has exited.
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }
    /// Checks if the terminal has received a bell event.
    pub fn has_bell(&self) -> bool {
        self.has_bell.load(Ordering::Relaxed)
    }
    /// Resets the bell status for the terminal.
    pub fn clear_bell(&self) {
        self.has_bell.store(false, Ordering::Relaxed);
    }
    /// Sends input bytes to the container's PTY, mimicking user input.
    pub fn send_input(&self, bytes: Vec<u8>) {
        if let Ok(mut last_input_at) = self.last_input_at.lock() {
            *last_input_at = Some(Instant::now());
        }
        self.notifier.notify(bytes);
    }

    pub fn is_subagent(&self) -> bool {
        self.parent_session_token.is_some()
    }

    pub fn display_name(&self) -> &str {
        self.subagent_name
            .as_deref()
            .unwrap_or(self.container_name.as_str())
    }

    pub fn terminal_stable_for(&self) -> Duration {
        self.terminal_changed_at.elapsed()
    }

    pub fn refresh_terminal_snapshot(&mut self) {
        let hash = self.terminal_visible_hash();
        if hash != self.terminal_snapshot_hash {
            self.terminal_snapshot_hash = hash;
            if !self.recent_input_may_have_echoed() {
                self.terminal_changed_at = Instant::now();
            }
        }
    }

    pub fn terminal_bottom_lines(&self, rows: usize) -> Vec<String> {
        let term = self.term.lock();
        terminal_bottom_lines(&*term, rows)
    }

    pub fn terminal_visible_rows(&self) -> usize {
        self.term.lock().screen_lines()
    }

    pub fn terminal_total_rows(&self) -> usize {
        self.term.lock().total_lines()
    }

    fn terminal_visible_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let lines = {
            let term = self.term.lock();
            terminal_bottom_lines(&*term, term.screen_lines())
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        lines.hash(&mut hasher);
        hasher.finish()
    }

    fn recent_input_may_have_echoed(&self) -> bool {
        let last_input_at = self.last_input_at.lock().ok().and_then(|last| *last);
        recent_input_may_have_echoed(last_input_at, Instant::now())
    }

    /// Forcibly terminates the Docker container associated with this session.
    ///
    /// Uses `docker rm -f` to stop and remove the container. The command's output
    /// is discarded, and any errors during `docker rm` are suppressed.
    pub fn terminate(&self) {
        let target = if !self.container_id.is_empty() {
            &self.container_id
        } else {
            &self.docker_name
        };
        if target.is_empty() || target == "unknown" {
            return;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Resizes the terminal window within the container.
    ///
    /// This updates the internal window size and notifies the PTY of the change,
    /// which helps the running process inside the container adjust its output.
    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        if let Ok(size) = self.window_size.lock() {
            if size.num_lines == rows && size.num_cols == cols {
                return Ok(());
            }
        }
        let ws = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 0,
            cell_height: 0,
        };
        if let Ok(mut s) = self.window_size.lock() {
            *s = ws;
        }
        self.notifier.on_resize(ws);
        let mut term = self.term.lock();
        term.resize(TermSize {
            cols: cols as usize,
            lines: rows as usize,
        });
        Ok(())
    }

    /// Generates a human-readable label for the TUI tab, combining container and project names.
    pub fn tab_label(&self) -> String {
        format!("{} @ {}", self.display_name(), self.project)
    }
}

fn recent_input_may_have_echoed(last_input_at: Option<Instant>, now: Instant) -> bool {
    last_input_at.is_some_and(|last_input_at| now.duration_since(last_input_at) <= INPUT_ECHO_GRACE)
}

fn terminal_bottom_lines<T: EventListener>(term: &Term<T>, rows: usize) -> Vec<String> {
    let total_rows = term.total_lines();
    if total_rows == 0 || term.columns() == 0 {
        return Vec::new();
    }
    let rows = if rows == 0 {
        total_rows
    } else {
        rows.min(total_rows).max(1)
    };
    let cols = term.columns();
    let bottom = term.bottommost_line().0;
    let start = bottom - rows as i32 + 1;
    let grid = term.grid();

    (start..=bottom)
        .map(|line| {
            let mut out = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &grid[Point::new(Line(line), Column(col))];
                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let ch = if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::HIDDEN)
                {
                    ' '
                } else {
                    cell.c
                };
                out.push(ch);
            }
            out.trim_end().to_string()
        })
        .collect()
}

/// Rewrites loopback addresses (127.0.0.1, localhost, 0.0.0.0) to `host.docker.internal`
/// for reliable container-to-host communication.
#[instrument(level = "trace", skip(url))]
pub(crate) fn loopback_to_host_docker(url: &str) -> String {
    url.replace("127.0.0.1", "host.docker.internal")
        .replace("localhost", "host.docker.internal")
        .replace("0.0.0.0", "host.docker.internal")
}

/// Convert an arbitrary project or container name into a Docker-safe name.
#[instrument(level = "trace", skip(input))]
pub fn sanitize_docker_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "container".to_string()
    } else {
        out
    }
}

pub(crate) fn mount_mode_arg(mode: &MountMode) -> &'static str {
    match mode {
        MountMode::Ro => "ro",
        MountMode::Rw => "rw",
    }
}

pub(crate) fn find_codex_home_container_path(mounts: &[ContainerMount]) -> Option<&Path> {
    mounts.iter().find_map(|mount| {
        (mount.container == PathBuf::from("/home/ubuntu/.codex")
            || mount.container == PathBuf::from("/root/.codex"))
        .then_some(mount.container.as_path())
    })
}

pub(crate) fn mounts_include_codex_session_state(mounts: &[ContainerMount]) -> bool {
    mounts.iter().any(|mount| {
        let container = mount.container.to_string_lossy();
        container.contains(".codex")
            || container.contains(".config/codex")
            || container.contains("codex")
    })
}

pub(crate) fn append_codex_home_args(
    docker_args: &mut Vec<String>,
    host_path: &Path,
) -> Result<()> {
    let codex_home = host_path.join(".codex");
    std::fs::create_dir_all(&codex_home).with_context(|| {
        format!(
            "failed to create codex home directory at {}",
            codex_home.display()
        )
    })?;
    let container_path = "/home/ubuntu/.codex";
    docker_args.push("-e".to_string());
    docker_args.push(format!("CODEX_HOME={container_path}"));
    docker_args.push("-v".to_string());
    docker_args.push(format!("{}:{container_path}:rw", codex_home.display()));
    Ok(())
}

pub(crate) fn find_gemini_home_container_path(mounts: &[ContainerMount]) -> Option<&Path> {
    mounts.iter().find_map(|mount| {
        (mount.container == PathBuf::from("/home/ubuntu/.gemini")
            || mount.container == PathBuf::from("/root/.gemini"))
        .then_some(mount.container.as_path())
    })
}

pub(crate) fn find_gemini_home_mount(mounts: &[ContainerMount]) -> Option<(&Path, &MountMode)> {
    mounts.iter().find_map(|mount| {
        (mount.container == PathBuf::from("/home/ubuntu/.gemini")
            || mount.container == PathBuf::from("/root/.gemini"))
        .then_some((mount.host.as_path(), &mount.mode))
    })
}

pub(crate) fn mounts_include_gemini_session_state(mounts: &[ContainerMount]) -> bool {
    mounts.iter().any(|mount| {
        let container = mount.container.to_string_lossy();
        container.contains(".gemini") || container.contains(".config/gemini")
    })
}

pub(crate) fn append_gemini_home_args(
    docker_args: &mut Vec<String>,
    host_path: &Path,
) -> Result<()> {
    let gemini_home = host_path.join(".gemini");
    std::fs::create_dir_all(&gemini_home).with_context(|| {
        format!(
            "failed to create gemini home directory at {}",
            gemini_home.display()
        )
    })?;
    for container_path in ["/home/ubuntu/.gemini", "/root/.gemini"] {
        docker_args.push("-v".to_string());
        docker_args.push(format!("{}:{container_path}:rw", gemini_home.display()));
    }
    Ok(())
}

pub(crate) fn append_missing_gemini_home_mount_args(
    docker_args: &mut Vec<String>,
    mounts: &[ContainerMount],
    host_path: &Path,
    mode: &MountMode,
) {
    for container_path in ["/home/ubuntu/.gemini", "/root/.gemini"] {
        let target = PathBuf::from(container_path);
        if mounts.iter().any(|mount| mount.container == target) {
            continue;
        }
        docker_args.push("-v".to_string());
        docker_args.push(format!(
            "{}:{container_path}:{}",
            host_path.display(),
            mount_mode_arg(mode)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{append_gemini_home_args, append_missing_gemini_home_mount_args};
    use crate::config::{ContainerMount, MountMode};
    use alacritty_terminal::event::{Event, EventListener, WindowSize};
    use alacritty_terminal::vte::ansi::Rgb;
    use std::path::PathBuf;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn gemini_home_args_mounts_both_possible_homes() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-gemini-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp dir");
        let mut args = Vec::new();
        append_gemini_home_args(&mut args, &root).expect("append gemini args");

        let mounts: Vec<String> = args
            .chunks_exact(2)
            .filter_map(|chunk| {
                if chunk[0] == "-v" {
                    Some(chunk[1].clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            mounts
                .iter()
                .any(|m| m.ends_with(":/home/ubuntu/.gemini:rw"))
        );
        assert!(mounts.iter().any(|m| m.ends_with(":/root/.gemini:rw")));
    }

    #[test]
    fn missing_gemini_home_mount_args_mirror_existing_home_mount_to_root() {
        let host = PathBuf::from("/host/.gemini");
        let configured_mounts = vec![ContainerMount {
            host: host.clone(),
            container: PathBuf::from("/home/ubuntu/.gemini"),
            mode: MountMode::Rw,
        }];
        let mut args = Vec::new();

        append_missing_gemini_home_mount_args(&mut args, &configured_mounts, &host, &MountMode::Rw);

        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                "/host/.gemini:/root/.gemini:rw".to_string()
            ]
        );
    }

    #[test]
    fn missing_gemini_home_mount_args_do_not_duplicate_existing_targets() {
        let host = PathBuf::from("/host/.gemini");
        let configured_mounts = vec![
            ContainerMount {
                host: host.clone(),
                container: PathBuf::from("/home/ubuntu/.gemini"),
                mode: MountMode::Rw,
            },
            ContainerMount {
                host: host.clone(),
                container: PathBuf::from("/root/.gemini"),
                mode: MountMode::Rw,
            },
        ];
        let mut args = Vec::new();

        append_missing_gemini_home_mount_args(&mut args, &configured_mounts, &host, &MountMode::Rw);

        assert!(args.is_empty());
    }

    #[test]
    fn compose_no_proxy_handles_empty_and_duplicates() {
        use crate::container::helpers::compose_no_proxy;
        let bypass = vec![
            "google.com".to_string(),
            "  ".to_string(),
            "localhost".to_string(),
        ];
        let out = compose_no_proxy(&bypass);
        // Default: localhost,127.0.0.1,host.docker.internal
        assert!(out.contains("localhost"));
        assert!(out.contains("127.0.0.1"));
        assert!(out.contains("host.docker.internal"));
        assert!(out.contains("google.com"));
        // "localhost" was already there, "  " should be ignored
        assert_eq!(out.split(',').filter(|&s| s == "localhost").count(), 1);
        assert!(!out.contains("  "));
    }

    #[test]
    fn sanitize_docker_name_works() {
        use super::sanitize_docker_name;
        assert_eq!(sanitize_docker_name("my project"), "my-project");
        assert_eq!(sanitize_docker_name("my@proj!ect"), "my-proj-ect");
        assert_eq!(sanitize_docker_name(""), "container");
    }

    #[test]
    fn codex_home_args_mounts_correct_paths() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-codex-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp dir");
        let mut args = Vec::new();
        super::append_codex_home_args(&mut args, &root).expect("append codex args");

        assert!(args.contains(&"-e".to_string()));
        assert!(args.contains(&"CODEX_HOME=/home/ubuntu/.codex".to_string()));
        assert!(args.contains(&"-v".to_string()));
        assert!(args.contains(&format!("{}/.codex:/home/ubuntu/.codex:rw", root.display())));
    }

    #[test]
    fn session_event_proxy_sets_bell_only_on_terminal_bell_event() {
        let has_bell = Arc::new(AtomicBool::new(false));
        let proxy = super::SessionEventProxy {
            sender: Arc::new(Mutex::new(None)),
            window_size: Arc::new(Mutex::new(WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 0,
                cell_height: 0,
            })),
            exited: Arc::new(AtomicBool::new(false)),
            has_bell: has_bell.clone(),
            default_fg: Rgb { r: 0, g: 0, b: 0 },
            default_bg: Rgb { r: 0, g: 0, b: 0 },
            grayscale_palette: false,
        };

        assert!(!has_bell.load(Ordering::Relaxed));
        proxy.send_event(Event::Bell);
        assert!(has_bell.load(Ordering::Relaxed));
    }

    #[derive(Clone)]
    struct NoopListener;

    impl EventListener for NoopListener {
        fn send_event(&self, _event: Event) {}
    }

    fn test_term(
        cols: usize,
        lines: usize,
        history: usize,
    ) -> alacritty_terminal::term::Term<NoopListener> {
        let mut term_cfg = alacritty_terminal::term::Config::default();
        term_cfg.scrolling_history = history;
        alacritty_terminal::term::Term::new(
            term_cfg,
            &super::TermSize { cols, lines },
            NoopListener,
        )
    }

    fn write_to_term(term: &mut alacritty_terminal::term::Term<NoopListener>, bytes: &[u8]) {
        let mut parser = alacritty_terminal::vte::ansi::Processor::<
            alacritty_terminal::vte::ansi::StdSyncHandler,
        >::new();
        parser.advance(term, bytes);
    }

    #[test]
    fn terminal_bottom_lines_can_include_scrollback() {
        let mut term = test_term(12, 3, 10);
        write_to_term(&mut term, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        assert_eq!(
            super::terminal_bottom_lines(&term, 5),
            vec!["one", "two", "three", "four", "five"]
        );
    }

    #[test]
    fn terminal_bottom_lines_zero_requests_available_buffer() {
        let mut term = test_term(12, 3, 10);
        write_to_term(&mut term, b"one\r\ntwo\r\nthree\r\nfour");

        assert_eq!(
            super::terminal_bottom_lines(&term, 0),
            vec!["one", "two", "three", "four"]
        );
    }

    #[test]
    fn recent_input_echo_window_is_short_lived() {
        let now = std::time::Instant::now();

        assert!(super::recent_input_may_have_echoed(Some(now), now));
        assert!(super::recent_input_may_have_echoed(
            Some(now - super::INPUT_ECHO_GRACE),
            now,
        ));
        assert!(!super::recent_input_may_have_echoed(
            Some(now - super::INPUT_ECHO_GRACE - std::time::Duration::from_millis(1)),
            now,
        ));
        assert!(!super::recent_input_may_have_echoed(None, now));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeSessionSource {
    SetupTokenFile,
    #[cfg(target_os = "macos")]
    SetupTokenKeychain,
}

#[cfg(target_os = "macos")]
fn read_keychain_value(service: &str) -> Option<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let val = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

#[cfg(target_os = "macos")]
pub(crate) fn extract_claude_keychain_credential() -> Option<String> {
    read_keychain_value("Claude Code-credentials")
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn extract_claude_keychain_credential() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn read_claude_setup_token() -> Option<(String, ClaudeSessionSource)> {
    if let Some(token) = read_keychain_value("harness-hat-claude-setup-token") {
        return Some((token, ClaudeSessionSource::SetupTokenKeychain));
    }
    read_setup_token_file().map(|token| (token, ClaudeSessionSource::SetupTokenFile))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn read_claude_setup_token() -> Option<(String, ClaudeSessionSource)> {
    read_setup_token_file().map(|token| (token, ClaudeSessionSource::SetupTokenFile))
}

fn read_setup_token_file() -> Option<String> {
    let path = dirs::config_dir()?
        .join("harness-hat")
        .join("claude-setup-token");
    let contents = std::fs::read_to_string(path).ok()?;
    let token = contents.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}
