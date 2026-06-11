mod app;
pub mod render;

use alacritty_terminal::grid::Dimensions;
use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture, Event, EventStream,
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::ResetColor,
    terminal::{
        EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use tokio::sync::mpsc;

use crate::activity::{Activity, ActivityEvent, ActivityId};
use crate::container::ContainerSession;
use crate::proxy::{NetworkDecision, PendingNetworkItem, ProxyState};
use crate::rules::NetworkPolicy;
use crate::server::SessionRegistry;
use crate::server::{ContainerStopDecision, ContainerStopItem, LaunchEvent, WorkspaceLaunchItem};
use crate::shared_config::SharedConfig;
use crate::state::{AuditEntry, StateManager};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiSessionGroup {
    workspace_name: String,
    workspace_idx: Option<usize>,
    template_name: String,
    template_idx: Option<usize>,
    terminal_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedContainerUsage {
    fetched_at: std::time::Instant,
    stats: Option<crate::container::ContainerUsageStats>,
    /// A background refresh for this container is currently running, so the UI
    /// thread should not spawn another or block on `docker stats`.
    in_flight: bool,
}

/// Result of a background `docker stats` fetch, delivered to the UI thread.
#[derive(Debug)]
pub(crate) struct ContainerUsageUpdate {
    pub docker_name: String,
    pub stats: Option<crate::container::ContainerUsageStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainerPickerState {
    /// First step: choose workspace for a new session.
    NewSessionWorkspace { cursor: usize },
    /// Second step: choose a template for a new session. Reached either via the
    /// workspace step (NewSession flow) or directly from a sidebar workspace
    /// entry. Esc/^B returns to the sidebar in both cases.
    NewSessionTemplate { workspace_idx: usize, cursor: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsAction {
    ReloadRules,
    RemoveWorkspace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SettingsActionRow {
    pub key: char,
    pub label: String,
    pub desc: &'static str,
    action: SettingsAction,
}

#[derive(Debug, Clone)]
/// A log line shown in the TUI log pane.
pub enum LogEntry {
    Audit(AuditEntry),
    Msg {
        text: String,
        is_error: bool,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(Debug, Clone, PartialEq)]
/// Selectable entries in the left sidebar.
pub enum SidebarItem {
    Session(usize),
    SessionTerminal(usize, usize),
    NetworkGroup(usize),
    Activity(ActivityId),
    Settings(usize),
    Launch(usize),
    Build(usize),
    NewSession,
    /// Non-selectable header rendered above the workspace launch entries.
    WorkspacesHeader,
    NewWorkspace,
}

#[derive(Debug, Clone, PartialEq)]
/// The currently focused UI region.
pub enum Focus {
    Sidebar,
    Terminal,
    Activity,
    Network,
    Settings,
    ContainerPicker,
    ImageBuild,
    NewWorkspace,
}

#[derive(Debug, Clone)]
/// Transient state for the new-workspace wizard.
pub struct NewWorkspaceState {
    pub cursor: usize,
    pub name: String,
    pub workspace_dir: String,
    pub project_type: crate::new_project::ProjectType,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoveWorkspaceConfirmState {
    pub workspace_name: String,
}

#[derive(Debug, Clone)]
pub struct BaseRulesChangedState {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedFileStamp {
    pub exists: bool,
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub content_hash: u64,
}

#[derive(Debug, Clone)]
pub struct PendingBaseRulesInternalWrite {
    pub expected_content: String,
    pub expires_at: std::time::Instant,
}

/// In-flight `/workspace/launch` request that is currently waiting on a
/// docker image build. The runtime forwards `BuildEvent::Output` lines to
/// `event_tx` while the build is running, and on `BuildEvent::Finished` it
/// completes the launch (or surfaces the build failure) and drops `event_tx`
/// to close the streaming response.
pub(crate) struct WorkspaceLaunchPending {
    pub(crate) event_tx: tokio::sync::mpsc::Sender<LaunchEvent>,
    pub(crate) workspace_name: String,
    pub(crate) template: String,
    pub(crate) workspace_idx: usize,
    pub(crate) template_idx: usize,
}

/// Top-level TUI application state and event loop ownership.
pub struct App {
    pub config: SharedConfig,
    pub loaded_config_path: PathBuf,
    pub token: String,
    pub session_registry: SessionRegistry,
    pub ca_cert_path: String,
    proxy_state: ProxyState,

    pub workspaces: Vec<WorkspaceStatus>,
    pub pending_stop: Vec<ContainerStopItem>,
    pub pending_net: Vec<PendingNetworkItem>,
    pub activities: Vec<Activity>,
    pub log: VecDeque<LogEntry>,
    pub log_scroll: usize,

    pub focus: Focus,
    pub sidebar_idx: usize,
    pub sidebar_offset: usize,
    pub(crate) session_groups: Vec<TuiSessionGroup>,
    pub active_session: Option<usize>,
    pub active_activity: Option<ActivityId>,
    pub active_network_session: Option<usize>,
    pub network_cursor: usize,
    pub preview_session: Option<usize>,
    pub active_settings_project: Option<usize>,
    pub settings_cursor: usize,

    pub(crate) container_picker: Option<ContainerPickerState>,
    pub build_container_idx: Option<usize>,
    pub build_project_idx: Option<usize>,
    pub build_session_group: Option<usize>,
    pub build_cursor: usize,
    pub build_output: VecDeque<(String, bool)>,
    pub build_scroll: usize,
    pub sessions: Vec<ContainerSession>,
    pub new_project: Option<NewWorkspaceState>,
    pub remove_workspace_confirm: Option<RemoveWorkspaceConfirmState>,
    pub base_rules_changed: Option<BaseRulesChangedState>,

    pub stop_pending_rx: mpsc::Receiver<ContainerStopItem>,
    pub launch_pending_rx: mpsc::Receiver<WorkspaceLaunchItem>,
    pub(crate) workspace_launch_pending: Option<WorkspaceLaunchPending>,
    pub net_pending_rx: mpsc::Receiver<PendingNetworkItem>,
    // Native OS approval dialog (macOS): results of finished `hh __dialog`
    // subprocesses come back here; `inflight` holds the activity id of the one
    // dialog currently on screen so we never pop two at once.
    native_dialog_rx: mpsc::UnboundedReceiver<app::native_dialog::NativeDialogResult>,
    native_dialog_tx: mpsc::UnboundedSender<app::native_dialog::NativeDialogResult>,
    native_dialog_inflight: Option<String>,
    // Bounded (H12): a malicious in-container client streaming events at full
    // speed otherwise grows the backing Vec without limit. On full the
    // producers (proxy/server) `try_send` and drop the event with a debug log
    // — the TUI's view is best-effort already.
    pub activity_rx: mpsc::Receiver<ActivityEvent>,
    pub audit_rx: mpsc::Receiver<AuditEntry>,
    build_event_rx: mpsc::UnboundedReceiver<BuildEvent>,
    build_event_tx: mpsc::UnboundedSender<BuildEvent>,
    build_task: Option<BuildTaskState>,
    /// Result of the most recent build, retained so the output pane (and its
    /// pass/fail banner) stays visible after the build task itself has ended.
    pub(crate) build_finished: Option<BuildFinished>,
    container_usage_rx: mpsc::UnboundedReceiver<ContainerUsageUpdate>,
    container_usage_tx: mpsc::UnboundedSender<ContainerUsageUpdate>,
    rules_scan_rx: mpsc::UnboundedReceiver<Vec<(PathBuf, WatchedFileStamp)>>,
    rules_scan_tx: mpsc::UnboundedSender<Vec<(PathBuf, WatchedFileStamp)>>,
    rules_scan_in_flight: bool,

    pub should_quit: bool,
    pub passthrough_mode: bool,
    pub passthrough_exit_code_slot: Option<Arc<AtomicI32>>,
    pub log_fullscreen: bool,
    pub terminal_fullscreen: bool,
    last_terminal_esc: Option<std::time::Instant>,
    pub scroll_mode: bool,
    pub terminal_scroll: usize,
    pub(crate) container_usage: HashMap<String, CachedContainerUsage>,
    last_base_rules_poll: std::time::Instant,
    watched_rules_stamps: HashMap<PathBuf, WatchedFileStamp>,
    pending_base_rules_internal_write: HashMap<PathBuf, PendingBaseRulesInternalWrite>,
}

/// Cached workspace metadata for the sidebar.
pub struct WorkspaceStatus {
    pub name: String,
    pub sidebar_hotkey: Option<char>,
}

#[derive(Debug)]
enum BuildEvent {
    Output {
        line: String,
        is_error: bool,
    },
    Finished {
        label: String,
        launch_project_idx: usize,
        launch_container_idx: usize,
        launch_session_group: Option<usize>,
        success: bool,
        cancelled: bool,
        exit_code: Option<i32>,
        error: Option<String>,
        diagnostic: Option<String>,
    },
}

#[derive(Debug)]
struct BuildTaskState {
    shell_command: String,
    /// Flipped to request cooperative cancellation of the spawned build task.
    cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Handle to the spawned build task so it can be aborted on quit.
    handle: tokio::task::JoinHandle<()>,
}

/// Outcome of a finished build, kept around so the user can read the captured
/// output and a clear status line instead of the view silently reverting to the
/// "build required" prompt.
#[derive(Debug, Clone)]
pub(crate) struct BuildFinished {
    pub command: String,
    pub cancelled: bool,
    pub exit_code: Option<i32>,
    pub diagnostic: Option<String>,
}

// ── Event loop ────────────────────────────────────────────────────────────────

pub async fn run(mut app: App) -> Result<()> {
    // Must run *before* `enable_raw_mode()`: the guard restores full termios on
    // drop, so capturing termios while already in raw mode would "restore" the
    // raw settings after shutdown and permanently corrupt the user's shell.
    let _termios_guard = disable_xon_xoff();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        crossterm::terminal::SetTitle("Harness Hat"),
        EnterAlternateScreen,
        cursor::Hide
    )?;
    let mut restore_guard = TerminalRestoreGuard::new();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    restore_terminal_output(terminal.backend_mut())?;
    terminal.show_cursor()?;
    restore_guard.disarm();

    result
}

fn restore_terminal_output<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    execute!(
        writer,
        LeaveAlternateScreen,
        cursor::Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        EnableLineWrap,
        ResetColor
    )
}

struct TerminalRestoreGuard {
    armed: bool,
}

impl TerminalRestoreGuard {
    fn new() -> Self {
        Self { armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = restore_terminal_output(&mut stdout);
    }
}

#[cfg(unix)]
fn disable_xon_xoff() -> Option<TermiosGuard> {
    disable_xon_xoff_on_fd(libc::STDIN_FILENO)
}

#[cfg(unix)]
fn disable_xon_xoff_on_fd(fd: i32) -> Option<TermiosGuard> {
    use std::mem::MaybeUninit;
    unsafe {
        let mut orig = MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(fd, orig.as_mut_ptr()) != 0 {
            return None;
        }
        let orig = orig.assume_init();
        let ixon_was_enabled = (orig.c_iflag & libc::IXON) != 0;
        let mut t = orig;
        t.c_iflag &= !libc::IXON;
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return None;
        }
        Some(TermiosGuard {
            fd,
            ixon_was_enabled,
        })
    }
}

#[cfg(not(unix))]
fn disable_xon_xoff() -> Option<()> {
    None
}

#[cfg(unix)]
struct TermiosGuard {
    fd: i32,
    ixon_was_enabled: bool,
}

#[cfg(unix)]
impl Drop for TermiosGuard {
    fn drop(&mut self) {
        unsafe {
            let mut cur = std::mem::MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(self.fd, cur.as_mut_ptr()) != 0 {
                return;
            }
            let mut cur = cur.assume_init();
            if self.ixon_was_enabled {
                cur.c_iflag |= libc::IXON;
            } else {
                cur.c_iflag &= !libc::IXON;
            }
            let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &cur);
        }
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut events = EventStream::new();
    let tick = tokio::time::Duration::from_millis(50);
    let mut mouse_capture_enabled = false;
    let mut approval_bell_rung = false;
    crate::notifications::init();

    loop {
        sync_mouse_capture(terminal.backend_mut(), app, &mut mouse_capture_enabled)?;
        app.drain_channels();
        app.tick_base_rules_file_watch();
        let modal_visible = app.has_pending_approval_modal();
        if modal_visible && !approval_bell_rung {
            ring_terminal_bell(terminal.backend_mut())?;
            if let Some(item) = app.pending_net.first() {
                crate::notifications::notify_pending_network_approval(
                    &item.host,
                    item.source_project.as_deref(),
                    app.pending_net.len(),
                );
            }
            approval_bell_rung = true;
        } else if !modal_visible {
            approval_bell_rung = false;
        }
        terminal.draw(|frame| render::render(frame, app))?;

        if app.should_quit {
            app.cancel_docker_build();
            app.terminate_all_sessions();
            break;
        }

        let timeout = tokio::time::sleep(tick);

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => app.handle_key(key),
                    Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
                    Some(Ok(Event::Paste(text))) => {
                        if app.focus == Focus::NewWorkspace {
                            app.append_new_project_text(&text);
                        } else if let Some(si) = app.active_session
                            && let Some(session) = app.sessions.get(si)
                        {
                            session.send_input(text.into_bytes());
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let (pty_cols, pty_rows) =
                            (cols.saturating_sub(38).max(20), rows.saturating_sub(10).max(6));
                        for session in &mut app.sessions {
                            let _ = session.resize(pty_rows, pty_cols);
                        }
                    }
                    None => break,
                    _ => {}
                }
                sync_mouse_capture(terminal.backend_mut(), app, &mut mouse_capture_enabled)?;
            }
            _ = timeout => {}
        }
    }

    Ok(())
}

fn ring_terminal_bell<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn should_enable_mouse_capture(app: &App) -> bool {
    let _ = app;
    false
}

fn sync_mouse_capture<W: std::io::Write>(
    writer: &mut W,
    app: &App,
    mouse_capture_enabled: &mut bool,
) -> std::io::Result<()> {
    let should_enable = should_enable_mouse_capture(app);
    if should_enable == *mouse_capture_enabled {
        return Ok(());
    }
    if should_enable {
        execute!(writer, EnableMouseCapture)?;
    } else {
        execute!(writer, DisableMouseCapture)?;
    }
    *mouse_capture_enabled = should_enable;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bell_tests {
    #[test]
    fn ring_terminal_bell_writes_bell_character() {
        let mut buf = Vec::new();
        super::ring_terminal_bell(&mut buf).expect("write bell");
        assert_eq!(buf, b"\x07");
    }
}
