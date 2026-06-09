use super::*;
use alacritty_terminal::term::TermMode;

#[allow(dead_code)]
pub(crate) fn maybe_encode_sgr_mouse_for_session(
    session: &crate::container::ContainerSession,
    mouse: MouseEvent,
) -> Option<Vec<u8>> {
    // Only forward mouse events when the terminal app has explicitly enabled mouse reporting.
    // Without this gating, shells and other apps would see raw escape sequences.
    let mode = *session.term.lock().mode();
    if !mode
        .intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    {
        return None;
    }

    // Only emit SGR mouse sequences for now; this matches most modern TUIs.
    if !mode.contains(TermMode::SGR_MOUSE) {
        return None;
    }

    encode_sgr_mouse(mouse)
}

#[allow(dead_code)]
pub(crate) fn encode_sgr_mouse(mouse: MouseEvent) -> Option<Vec<u8>> {
    let mut cb: u16 = 0;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb |= 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb |= 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb |= 16;
    }

    let (button_code, suffix): (u16, u8) = match mouse.kind {
        MouseEventKind::Down(button) => (button_to_code(button)?, b'M'),
        MouseEventKind::Up(button) => (button_to_code(button)?, b'm'),
        MouseEventKind::Drag(button) => (button_to_code(button)? + 32, b'M'),
        MouseEventKind::ScrollUp => (64, b'M'),
        MouseEventKind::ScrollDown => (65, b'M'),
        MouseEventKind::ScrollLeft => (66, b'M'),
        MouseEventKind::ScrollRight => (67, b'M'),
        MouseEventKind::Moved => return None,
    };

    let cb = cb + button_code;
    let x = mouse.column.saturating_add(1);
    let y = mouse.row.saturating_add(1);

    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"\x1b[<");
    out.extend_from_slice(cb.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(x.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(y.to_string().as_bytes());
    out.push(suffix);
    Some(out)
}

#[allow(dead_code)]
pub(crate) fn button_to_code(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
    }
}

pub(crate) fn shell_command_for_docker_args(args: &[String]) -> String {
    format!("docker {}", shell_words::join(args))
}

pub(crate) fn build_line_looks_like_error(line: &str) -> bool {
    // We want to flag genuine compiler/build failure lines (`error: …`,
    // `error!`, `error TS2304:`) without misfiring on benign output like
    // `Compiling: no error here`, `terror`, or `Exit code 0` (M23).
    //
    // Strategy:
    //   1. Lowercase + trim so leading whitespace doesn't hide the marker.
    //   2. Anchor the `error` needle at the start of the line and require the
    //      character after it to be a word break (`:`, `!`, ` `, or `\t`) —
    //      i.e. `error: ` / `error!` / leading `error ` only, never substring.
    //   3. Keep a small allow-list of additional unambiguous failure phrases.
    //      `"cannot find"` and `"exit code"` are intentionally dropped: they
    //      matched messages like `Exit code 0` and ordinary tool output that
    //      mentioned a missing optional dependency.
    let text = line.trim_start().to_ascii_lowercase();

    if let Some(rest) = text.strip_prefix("error") {
        let next = rest.chars().next();
        // `error:` / `error!` / `error ` / bare line that is exactly "error".
        if matches!(next, Some(':') | Some('!') | Some(' ') | Some('\t') | None) {
            return true;
        }
    }

    [
        "failed",
        "denied",
        "no such file",
        "permission denied",
        "unauthorized",
        "npm err",
        "did not complete",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

async fn forward_build_stream<R>(
    reader: R,
    prefix: &'static str,
    mark_stderr: bool,
    stderr_tail: Option<Arc<Mutex<VecDeque<String>>>>,
    output_tail: Arc<Mutex<VecDeque<String>>>,
    tx: mpsc::UnboundedSender<BuildEvent>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let is_error = mark_stderr && build_line_looks_like_error(&line);
                if is_error
                    && let Some(tail) = stderr_tail.as_ref()
                    && let Ok(mut lines) = tail.lock()
                {
                    lines.push_back(line.clone());
                    if lines.len() > 6 {
                        lines.pop_front();
                    }
                }
                // Always retain a tail of recent output so we can surface a
                // diagnostic even when no line matched the error heuristics.
                if !line.trim().is_empty()
                    && let Ok(mut lines) = output_tail.lock()
                {
                    lines.push_back(line.clone());
                    if lines.len() > 8 {
                        lines.pop_front();
                    }
                }
                let _ = tx.send(BuildEvent::Output {
                    line: format!("{prefix}{line}"),
                    is_error,
                });
            }
            Ok(None) | Err(_) => break,
        }
    }
}

pub(crate) async fn run_build_shell_command(
    label: String,
    shell_command: String,
    launch_project_idx: usize,
    launch_container_idx: usize,
    launch_session_group: Option<usize>,
    cancel_flag: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<BuildEvent>,
) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-lc")
        .arg(&shell_command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::setpgid(0, 0);
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = tx.send(BuildEvent::Finished {
                label,
                launch_project_idx,
                launch_container_idx,
                launch_session_group,
                success: false,
                cancelled: false,
                exit_code: None,
                error: Some(e.to_string()),
                diagnostic: None,
            });
            return;
        }
    };

    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let output_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let stdout_task = child.stdout.take().map(|stdout| {
        let tx = tx.clone();
        let output_tail = output_tail.clone();
        tokio::spawn(async move {
            forward_build_stream(stdout, "build: ", false, None, output_tail, tx).await;
        })
    });
    let stderr_task = child.stderr.take().map(|stderr| {
        let tx = tx.clone();
        let stderr_tail = stderr_tail.clone();
        let output_tail = output_tail.clone();
        tokio::spawn(async move {
            forward_build_stream(stderr, "build: ", true, Some(stderr_tail), output_tail, tx).await;
        })
    });

    let mut cancelled = false;
    let status = loop {
        if cancel_flag.load(Ordering::SeqCst) {
            cancelled = true;
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let pgid = format!("-{}", pid);
                let _ = tokio::process::Command::new("kill")
                    .args(["-TERM", &pgid])
                    .status()
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let _ = tokio::process::Command::new("kill")
                    .args(["-KILL", &pgid])
                    .status()
                    .await;
            }
            let _ = child.start_kill();
            break child.wait().await.ok();
        }

        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            Err(_) => break None,
        }
    };

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    let success = !cancelled && status.map(|s| s.success()).unwrap_or(false);
    let exit_code = status.and_then(|s| s.code());
    let join_tail = |tail: &Arc<Mutex<VecDeque<String>>>| {
        tail.lock().ok().and_then(|lines| {
            (!lines.is_empty()).then(|| lines.iter().cloned().collect::<Vec<_>>().join(" | "))
        })
    };
    // Prefer lines that looked like errors; otherwise fall back to the tail of
    // raw output so a failure is never reported with an empty diagnostic.
    let diagnostic = join_tail(&stderr_tail).or_else(|| join_tail(&output_tail));
    let _ = tx.send(BuildEvent::Finished {
        label,
        launch_project_idx,
        launch_container_idx,
        launch_session_group,
        success,
        cancelled,
        exit_code,
        error: None,
        diagnostic,
    });
}

pub(crate) fn control_hotkey_char(key: KeyEvent) -> Option<char> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(ch) = key.code
    {
        return Some(ch.to_ascii_lowercase());
    }

    if key.modifiers.is_empty()
        && let KeyCode::Char(ch) = key.code
        && ch.is_ascii_control()
    {
        let code = ch as u32;
        if (1..=26).contains(&code) {
            return char::from_u32((code - 1) + ('a' as u32));
        }
    }

    None
}

pub(crate) fn is_scroll_mode_toggle_key(key: KeyEvent) -> bool {
    (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('\u{13}') && key.modifiers.is_empty())
}

pub(crate) fn oneshot_dummy() -> tokio::sync::oneshot::Sender<NetworkDecision> {
    let (tx, _) = tokio::sync::oneshot::channel();
    tx
}

// ── Key → PTY bytes (Streamlined mapping) ────────────────────────────────────
