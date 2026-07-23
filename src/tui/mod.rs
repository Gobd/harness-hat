mod app;
mod remote;
pub mod render;

pub(crate) use remote::run_attached;

use alacritty_terminal::grid::Dimensions;
use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture, Event, EventStream,
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
    },
    execute,
    style::ResetColor,
    terminal::{
        EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use futures::StreamExt;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};
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
use crate::server::{
    ContainerStopDecision, ContainerStopItem, LaunchEvent, PendingItem, WorkspaceLaunchItem,
};
use crate::shared_config::SharedConfig;
use crate::state::{AuditEntry, StateManager};

const BUILD_EVENT_CHANNEL_CAPACITY: usize = 512;
const BUILD_OUTPUT_EVENTS_PER_TICK: usize = 48;

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

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ContainerPickerState {
    /// First step: choose workspace for a new session.
    NewSessionWorkspace { cursor: usize },
    /// Second step: choose a template for a new session. Reached either via the
    /// workspace step (NewSession flow) or directly from a sidebar workspace
    /// entry. Esc/^B returns to the sidebar in both cases.
    NewSessionTemplate {
        workspace_idx: usize,
        cursor: usize,
        templates: Vec<crate::config::ContainerDef>,
    },
}

pub(crate) fn move_wrapping_cursor(cursor: &mut usize, len: usize, direction: i8) {
    if len == 0 {
        *cursor = 0;
        return;
    }

    *cursor = (*cursor).min(len - 1);
    if direction < 0 {
        *cursor = if *cursor == 0 { len - 1 } else { *cursor - 1 };
    } else if direction > 0 {
        *cursor = (*cursor + 1) % len;
    }
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
    pub expected_contents: Option<Vec<u8>>,
    pub dialog_dismissed: bool,
}

#[derive(Debug, Clone)]
pub struct WatchedFileStamp {
    pub exists: bool,
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub content_hash: u64,
}

impl PartialEq for WatchedFileStamp {
    fn eq(&self, other: &Self) -> bool {
        self.exists == other.exists && self.content_hash == other.content_hash
    }
}

impl Eq for WatchedFileStamp {}

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
    pub(crate) cwd: Option<std::path::PathBuf>,
}

/// Bounded channels used by background workers that feed best-effort UI data.
/// Keeping them together makes their ownership and backpressure policy explicit
/// instead of spreading unrelated sender/receiver pairs across `App`.
struct BackgroundUiChannels {
    native_dialog_rx: mpsc::Receiver<app::native_dialog::NativeDialogResult>,
    native_dialog_tx: mpsc::Sender<app::native_dialog::NativeDialogResult>,
    container_usage_rx: mpsc::Receiver<ContainerUsageUpdate>,
    container_usage_tx: mpsc::Sender<ContainerUsageUpdate>,
    rules_scan_rx: mpsc::Receiver<Vec<(PathBuf, WatchedFileStamp)>>,
    rules_scan_tx: mpsc::Sender<Vec<(PathBuf, WatchedFileStamp)>>,
}

/// Top-level TUI application state and event loop ownership.
pub struct App {
    pub config: SharedConfig,
    pub loaded_config_path: PathBuf,
    pub token: String,
    pub session_registry: SessionRegistry,
    proxy_state: ProxyState,

    pub workspaces: Vec<WorkspaceStatus>,
    pub pending_stop: Vec<ContainerStopItem>,
    pub pending_exec: Vec<PendingItem>,
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
    pub active_settings_workspace: Option<usize>,
    pub settings_cursor: usize,

    pub(crate) container_picker: Option<ContainerPickerState>,
    pub build_container_idx: Option<usize>,
    pub build_workspace_idx: Option<usize>,
    pub build_session_group: Option<usize>,
    pub build_cursor: usize,
    pub pending_force_rebuild: bool,
    pub build_output: VecDeque<(String, bool)>,
    pub build_scroll: usize,
    pub sessions: Vec<ContainerSession>,
    pub new_workspace: Option<NewWorkspaceState>,
    pub remove_workspace_confirm: Option<RemoveWorkspaceConfirmState>,
    pub base_rules_changed: Option<BaseRulesChangedState>,

    pub exec_pending_rx: mpsc::Receiver<PendingItem>,
    pub stop_pending_rx: mpsc::Receiver<ContainerStopItem>,
    pub launch_pending_rx: mpsc::Receiver<WorkspaceLaunchItem>,
    pub(crate) workspace_launch_pending: Option<WorkspaceLaunchPending>,
    pub net_pending_rx: mpsc::Receiver<PendingNetworkItem>,
    // Native OS approval dialog (macOS): results of finished `hht __dialog`
    // subprocesses come back here; `inflight` holds the activity id of the one
    // dialog currently on screen so we never pop two at once.
    background_channels: BackgroundUiChannels,
    native_dialog_inflight: Option<app::native_dialog::NativeDialogTarget>,
    service_mode: bool,
    // Bounded (H12): a malicious in-container client streaming events at full
    // speed otherwise grows the backing Vec without limit. On full the
    // producers (proxy/server) `try_send` and drop the event with a debug log
    // — the TUI's view is best-effort already.
    pub activity_rx: mpsc::Receiver<ActivityEvent>,
    pub audit_rx: mpsc::Receiver<AuditEntry>,
    build_event_rx: mpsc::Receiver<BuildEvent>,
    build_event_tx: mpsc::Sender<BuildEvent>,
    build_task: Option<BuildTaskState>,
    /// Result of the most recent build, retained so the output pane (and its
    /// pass/fail banner) stays visible after the build task itself has ended.
    pub(crate) build_finished: Option<BuildFinished>,
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
        launch_workspace_idx: usize,
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
    command_display: String,
    /// Flipped to request cooperative cancellation of the spawned build task.
    cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Handle to the spawned build task so shutdown can wait for cleanup.
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

/// Frames the stdout writer thread may hold before new frames are dropped.
/// The writer normally drains a frame in well under one 50ms tick, so any
/// backlog means the terminal emulator has stopped reading the pty.
const FRAME_QUEUE_CAPACITY: usize = 4;

/// Non-blocking `Write` sink for the ratatui backend.
///
/// Bytes accumulate in `buf` until `flush()` — crossterm batches each frame
/// (and each `execute!`) into exactly one flush — then the complete batch is
/// handed to a dedicated writer thread over a bounded channel. If the channel
/// is full, the terminal emulator has stopped draining stdout (e.g. stalled
/// by screen capture); the frame is dropped whole and `dropped` is flagged so
/// the event loop forces a full repaint once the terminal recovers. Only
/// whole batches are ever dropped, so escape sequences never tear. This keeps
/// the TUI thread from ever blocking in `write(2)`, which previously froze
/// input handling for as long as the terminal was stalled.
struct FrameWriter {
    buf: Vec<u8>,
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    dropped: Arc<AtomicBool>,
    /// Set once the event loop has exited. The writes after that point are
    /// the terminal restore sequences (leave alternate screen, show cursor),
    /// which must never be dropped — a blocking send here can only stall if
    /// the terminal emulator still isn't draining stdout, in which case the
    /// restore lands as soon as it recovers. Shared with `run()` via an Arc
    /// because the writer is owned by the ratatui backend after construction.
    shutdown_blocking: Arc<AtomicBool>,
}

impl std::io::Write for FrameWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let frame = std::mem::take(&mut self.buf);
        if self.shutdown_blocking.load(Ordering::Relaxed) {
            return self.tx.send(frame).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdout writer thread exited",
                )
            });
        }
        match self.tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdout writer thread exited",
            )),
        }
    }
}

/// Owns the only blocking writes to the real stdout. Exits when every
/// `FrameWriter` clone of the sender is dropped (after draining the queue).
fn spawn_stdout_writer(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tui-stdout-writer".into())
        .spawn(move || {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            while let Ok(frame) = rx.recv() {
                if stdout.write_all(&frame).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        })
        .expect("failed to spawn tui-stdout-writer thread")
}

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
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
    let frames_dropped = Arc::new(AtomicBool::new(false));
    let shutdown_blocking = Arc::new(AtomicBool::new(false));
    let writer_handle = spawn_stdout_writer(frame_rx);
    let backend = CrosstermBackend::new(FrameWriter {
        buf: Vec::new(),
        tx: frame_tx,
        dropped: Arc::clone(&frames_dropped),
        shutdown_blocking: Arc::clone(&shutdown_blocking),
    });
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(
        &mut terminal,
        &mut app,
        &frames_dropped,
        &mut restore_guard.mouse_capture_enabled,
    )
    .await;

    disable_raw_mode()?;
    shutdown_blocking.store(true, Ordering::Relaxed);
    restore_terminal_output(terminal.backend_mut(), restore_guard.mouse_capture_enabled)?;
    restore_guard.mouse_capture_enabled = false;
    terminal.show_cursor()?;
    // Dropping the terminal drops the FrameWriter, which closes the frame
    // channel; the writer thread drains the queued restore sequences above
    // and exits, so joining it guarantees the terminal is restored before we
    // return.
    drop(terminal);
    let _ = writer_handle.join();
    restore_guard.disarm();

    result
}

/// Run the manager state machine without a terminal renderer. The installed
/// per-user agent uses this path and native dialogs for every approval.
pub async fn run_service(
    mut app: App,
    mut tui_rx: mpsc::Receiver<crate::server::TuiFrameItem>,
) -> Result<()> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let backend = CrosstermBackend::new(RemoteFrameWriter(Arc::clone(&output)));
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )?;
    loop {
        app.drain_channels();
        app.tick_base_rules_file_watch();
        while let Ok(item) = tui_rx.try_recv() {
            terminal.resize(Rect::new(0, 0, item.width, item.height))?;
            let (pty_cols, pty_rows) = (
                item.width.saturating_sub(38).max(20),
                item.height.saturating_sub(10).max(6),
            );
            for session in &mut app.sessions {
                let _ = session.resize(pty_rows, pty_cols);
            }
            if let Some(input) = item.input {
                apply_remote_input(&mut app, input);
            }
            terminal.draw(|frame| render::render(frame, &mut app))?;
            let frame = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
            let _ = item.response_tx.send(frame);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

struct RemoteFrameWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for RemoteFrameWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn apply_remote_input(app: &mut App, input: crate::server::TuiInput) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    match input {
        crate::server::TuiInput::Key { code, modifiers } => {
            let code = match code.as_str() {
                "backspace" => KeyCode::Backspace,
                "enter" => KeyCode::Enter,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "page_up" => KeyCode::PageUp,
                "page_down" => KeyCode::PageDown,
                "tab" => KeyCode::Tab,
                "back_tab" => KeyCode::BackTab,
                "delete" => KeyCode::Delete,
                "insert" => KeyCode::Insert,
                "esc" => KeyCode::Esc,
                value if value.len() == 1 => {
                    KeyCode::Char(value.chars().next().unwrap_or_default())
                }
                _ => return,
            };
            app.handle_key(KeyEvent::new(
                code,
                KeyModifiers::from_bits_truncate(modifiers),
            ));
        }
        crate::server::TuiInput::Paste { text } => app.append_remote_paste(&text),
        crate::server::TuiInput::Mouse {
            kind,
            column,
            row,
            modifiers,
        } => {
            use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
            let kind = match kind.as_str() {
                "down_left" => MouseEventKind::Down(MouseButton::Left),
                "down_right" => MouseEventKind::Down(MouseButton::Right),
                "down_middle" => MouseEventKind::Down(MouseButton::Middle),
                "up_left" => MouseEventKind::Up(MouseButton::Left),
                "up_right" => MouseEventKind::Up(MouseButton::Right),
                "up_middle" => MouseEventKind::Up(MouseButton::Middle),
                "drag_left" => MouseEventKind::Drag(MouseButton::Left),
                "drag_right" => MouseEventKind::Drag(MouseButton::Right),
                "drag_middle" => MouseEventKind::Drag(MouseButton::Middle),
                "moved" => MouseEventKind::Moved,
                "scroll_down" => MouseEventKind::ScrollDown,
                "scroll_up" => MouseEventKind::ScrollUp,
                "scroll_left" => MouseEventKind::ScrollLeft,
                "scroll_right" => MouseEventKind::ScrollRight,
                _ => return,
            };
            app.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::from_bits_truncate(modifiers),
            });
        }
    }
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
    mouse_capture_enabled: bool,
}

impl TerminalRestoreGuard {
    fn new() -> Self {
        Self {
            armed: true,
            mouse_capture_enabled: false,
        }
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
        let _ = restore_terminal_output(&mut stdout, self.mouse_capture_enabled);
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
    terminal: &mut Terminal<CrosstermBackend<FrameWriter>>,
    app: &mut App,
    frames_dropped: &AtomicBool,
    mouse_capture_enabled: &mut bool,
) -> Result<()> {
    let mut events = EventStream::new();
    let tick = tokio::time::Duration::from_millis(50);
    let mut approval_bell_rung = false;
    let mut session_bell_counts = Vec::new();
    crate::notifications::init();

    loop {
        sync_mouse_capture(terminal.backend_mut(), app, mouse_capture_enabled)?;
        app.drain_channels();
        app.tick_base_rules_file_watch();
        let modal_visible = app.has_pending_approval_modal();
        if modal_visible && !approval_bell_rung {
            ring_terminal_bell(terminal.backend_mut())?;
            if let Some(item) = app.pending_net.first() {
                crate::notifications::notify_pending_network_approval(
                    &item.host,
                    item.source_workspace.as_deref(),
                    app.pending_net.len(),
                );
            }
            approval_bell_rung = true;
        } else if !modal_visible {
            approval_bell_rung = false;
        }
        ring_new_session_bells(terminal.backend_mut(), app, &mut session_bell_counts)?;
        // A frame was dropped because the terminal emulator stopped draining
        // stdout (e.g. stalled by a screen share), so ratatui's back buffer
        // no longer matches what's on screen. Force a full repaint; clear()
        // goes through the same drop-instead-of-block writer, so this cannot
        // wedge the loop and converges once the terminal drains again.
        if frames_dropped.swap(false, Ordering::Relaxed) {
            terminal.clear()?;
        }
        terminal.draw(|frame| render::render(frame, app))?;

        if app.should_quit {
            app.cancel_docker_build_for_shutdown().await;
            app.terminate_all_sessions();
            break;
        }

        let timeout = tokio::time::sleep(tick);

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if should_handle_key_event(&key) => {
                        app.handle_key(key)
                    }
                    Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
                    Some(Ok(Event::Paste(text))) => {
                        if app.focus == Focus::NewWorkspace {
                            app.append_new_workspace_text(&text);
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
                sync_mouse_capture(terminal.backend_mut(), app, mouse_capture_enabled)?;
            }
            _ = timeout => {}
        }
    }

    Ok(())
}

fn should_handle_key_event(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn ring_terminal_bell<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn ring_new_session_bells<W: std::io::Write>(
    writer: &mut W,
    app: &App,
    last_seen: &mut Vec<u64>,
) -> std::io::Result<()> {
    if last_seen.len() < app.sessions.len() {
        last_seen.resize(app.sessions.len(), 0);
    } else if last_seen.len() > app.sessions.len() {
        last_seen.truncate(app.sessions.len());
    }

    let mut has_new_bell = false;
    for (idx, session) in app.sessions.iter().enumerate() {
        let current = session.bell_count();
        if current > last_seen[idx] {
            has_new_bell = true;
        }
        last_seen[idx] = current;
    }

    if has_new_bell {
        ring_terminal_bell(writer)?;
    }
    Ok(())
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

    #[test]
    fn restoring_terminal_without_mouse_capture_is_safe() {
        let mut buf = Vec::new();
        super::restore_terminal_output(&mut buf, false).expect("restore terminal");
        assert!(!buf.is_empty());
    }
}
