//! Terminal transport for the daemon-owned interactive TUI.

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::server::{TuiFrameRequest, TuiInput};

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
        .timeout(Duration::from_secs(3))
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

    let result = relay_loop(&client, &control_url, &token, &mut stdout);
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
    stdout: &mut io::Stdout,
) -> Result<()> {
    loop {
        let input = if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if key.code == KeyCode::Char('q')
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
        let (width, height) = crossterm::terminal::size().context("reading terminal size")?;
        let response = client
            .post(format!("{control_url}/tui/frame"))
            .bearer_auth(token)
            .json(&TuiFrameRequest {
                width,
                height,
                input,
            })
            .send()
            .context("requesting daemon TUI frame")?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "daemon returned {status}: {}",
                response.text().unwrap_or_default()
            );
        }
        let frame = response.bytes().context("reading daemon TUI frame")?;
        stdout
            .write_all(&frame)
            .context("writing daemon TUI frame")?;
        stdout.flush().context("flushing daemon TUI frame")?;
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
    disable_raw_mode().context("restoring terminal mode")?;
    execute!(
        stdout,
        LeaveAlternateScreen,
        cursor::Show,
        crossterm::event::DisableMouseCapture,
        crossterm::style::ResetColor
    )
    .context("restoring terminal screen")
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
