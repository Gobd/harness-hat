//! Terminal transport for the daemon-owned interactive TUI.

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use std::time::Instant;

use crate::server::{TuiEventsResponse, TuiFrameRequest, TuiInput};

const TUI_FRAME_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const TUI_FRAME_RETRY_WINDOW: Duration = Duration::from_secs(30);
const TUI_FRAME_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Attach the current terminal to the daemon's existing App and renderer.
pub(crate) fn run_attached(config_path: PathBuf) -> Result<()> {
    let config = crate::config::load(&config_path)
        .with_context(|| format!("loading config at {}", config_path.display()))?;
    let token = read_token(&config.logging.log_dir.join("token"))?;
    let control_url = format!(
        "http://{}:{}",
        config.defaults.control.server_host, config.defaults.control.server_port
    );
    let client = reqwest::blocking::Client::builder()
        // The daemon deliberately returns `tui_busy` while its single TUI
        // thread is finishing a synchronous Docker/session operation. Keep
        // this above the daemon's 10-second handler deadline so the client
        // receives that useful response instead of a generic transport
        // timeout.
        .timeout(TUI_FRAME_REQUEST_TIMEOUT)
        .build()
        .context("building daemon TUI client")?;

    enable_raw_mode().context("enabling terminal raw mode")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        Clear(ClearType::All),
        cursor::Hide,
        crossterm::event::EnableMouseCapture,
        crossterm::terminal::SetTitle("Harness Hat")
    ) {
        let _ = disable_raw_mode();
        return Err(error).context("entering Harness Hat terminal UI");
    }

    let stop_events = Arc::new(AtomicBool::new(false));
    let (event_tx, event_rx) = mpsc::channel();
    spawn_event_poller(
        control_url.clone(),
        token.clone(),
        Arc::clone(&stop_events),
        event_tx,
    );
    let result = relay_loop(&client, &control_url, &token, &event_rx, &mut stdout);
    stop_events.store(true, Ordering::Relaxed);
    let restore = restore_terminal(&mut stdout);
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    }
}

fn relay_loop(
    client: &reqwest::blocking::Client,
    control_url: &str,
    token: &str,
    event_rx: &mpsc::Receiver<()>,
    stdout: &mut io::Stdout,
) -> Result<()> {
    let mut needs_render = true;
    let mut needs_full_frame = true;
    let mut last_frame: Vec<u8> = Vec::new();
    loop {
        let input = if poll_event(Duration::from_millis(50))? {
            match read_event()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    key_input(key)
                }
                Event::Paste(text) => Some(TuiInput::Paste { text }),
                Event::Mouse(mouse) => mouse_input(mouse),
                _ => None,
            }
        } else {
            None
        };
        while event_rx.try_recv().is_ok() {
            needs_render = true;
        }
        if input.is_none() && !needs_render {
            continue;
        }
        let (width, height) = terminal_size()?;
        let request = TuiFrameRequest {
            width,
            height,
            input,
            full_frame: needs_full_frame,
        };
        let retry_deadline = Instant::now() + TUI_FRAME_RETRY_WINDOW;
        let response = loop {
            match client
                .post(format!("{control_url}/tui/frame"))
                .bearer_auth(token)
                .json(&request)
                .send()
            {
                Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    let status = response.status();
                    let body = response.text().unwrap_or_default();
                    if body.contains("\"error\":\"tui_busy\"") && Instant::now() < retry_deadline {
                        std::thread::sleep(TUI_FRAME_RETRY_DELAY);
                        continue;
                    }
                    bail!("daemon returned {status}: {body}");
                }
                Ok(response) => break response,
                Err(error) if error.is_timeout() && Instant::now() < retry_deadline => {
                    std::thread::sleep(TUI_FRAME_RETRY_DELAY);
                }
                Err(error) => return Err(error).context("requesting daemon TUI frame"),
            }
        };
        let status = response.status();
        let server_sent_full_frame = response.headers().contains_key("x-harness-hat-full-frame");
        if !status.is_success() {
            bail!(
                "daemon returned {status}: {}",
                response.text().unwrap_or_default()
            );
        }
        let frame = response.bytes().context("reading daemon TUI frame")?;
        if frame.starts_with(b"\x1b[2J\x1b[H\x1b[31mHarness Hat could not render") {
            bail!(
                "{}",
                String::from_utf8_lossy(&frame).replace(['\r', '\x1b'], "")
            );
        }
        if request.full_frame || server_sent_full_frame || last_frame != frame {
            write_frame(stdout, &frame)?;
            last_frame = frame.to_vec();
        }
        needs_full_frame = false;
        needs_render = false;
    }
}

fn spawn_event_poller(
    control_url: String,
    token: String,
    stop: Arc<AtomicBool>,
    changed: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            // The server waits up to 25 seconds, but this shorter client
            // timeout keeps Ctrl+C/quit responsive even if the daemon dies.
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        let mut after = 0_u64;
        while !stop.load(Ordering::Relaxed) {
            let response = client
                .get(format!("{control_url}/tui/events?after={after}"))
                .bearer_auth(&token)
                .send();
            let Ok(response) = response else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(events) = response.json::<TuiEventsResponse>() else {
                continue;
            };
            after = events.latest;
            if events.reset_required || !events.events.is_empty() {
                if changed.send(()).is_err() {
                    return;
                }
            }
        }
    });
}

fn poll_event(timeout: Duration) -> Result<bool> {
    loop {
        match event::poll(timeout) {
            Ok(ready) => return Ok(ready),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("polling terminal input"),
        }
    }
}

fn read_event() -> Result<Event> {
    loop {
        match event::read() {
            Ok(event) => return Ok(event),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("reading terminal input"),
        }
    }
}

fn terminal_size() -> Result<(u16, u16)> {
    loop {
        match crossterm::terminal::size() {
            Ok(size) => return Ok(size),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("reading terminal size"),
        }
    }
}

fn write_frame(stdout: &mut io::Stdout, frame: &[u8]) -> Result<()> {
    let mut remaining = frame;
    while !remaining.is_empty() {
        match stdout.write(remaining) {
            Ok(0) => anyhow::bail!("writing daemon TUI frame returned zero bytes"),
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("writing daemon TUI frame"),
        }
    }
    loop {
        match stdout.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("flushing daemon TUI frame"),
        }
    }
}

fn key_input(key: KeyEvent) -> Option<TuiInput> {
    let code = match key.code {
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page_up".to_string(),
        KeyCode::PageDown => "page_down".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "back_tab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Char(value) => value.to_string(),
        _ => return None,
    };
    Some(TuiInput::Key {
        code,
        modifiers: key.modifiers.bits(),
    })
}

fn mouse_input(mouse: MouseEvent) -> Option<TuiInput> {
    let kind = match mouse.kind {
        MouseEventKind::Down(button) => format!("down_{}", mouse_button_name(button)?),
        MouseEventKind::Up(button) => format!("up_{}", mouse_button_name(button)?),
        MouseEventKind::Drag(button) => format!("drag_{}", mouse_button_name(button)?),
        MouseEventKind::Moved => "moved".to_string(),
        MouseEventKind::ScrollDown => "scroll_down".to_string(),
        MouseEventKind::ScrollUp => "scroll_up".to_string(),
        MouseEventKind::ScrollLeft => "scroll_left".to_string(),
        MouseEventKind::ScrollRight => "scroll_right".to_string(),
    };
    Some(TuiInput::Mouse {
        kind,
        column: mouse.column,
        row: mouse.row,
        modifiers: mouse.modifiers.bits(),
    })
}

fn mouse_button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Right => Some("right"),
        MouseButton::Middle => Some("middle"),
    }
}

fn restore_terminal(stdout: &mut io::Stdout) -> Result<()> {
    loop {
        match disable_raw_mode() {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("restoring terminal mode"),
        }
    }
    // Keep cleanup independent of crossterm's output writer so a temporarily
    // full macOS PTY cannot strand the user in raw mode or the alt screen.
    write_frame(
        stdout,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1049l\x1b[?25h\x1b[0m",
    )
}

fn read_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading daemon token at {}", path.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("daemon token at {} is empty", path.display());
    }
    Ok(token)
}
