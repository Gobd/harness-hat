use super::*;

struct BuildLog {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    write_error: Mutex<Option<String>>,
}

impl BuildLog {
    fn create(path: PathBuf, command_display: &str) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating build log directory {}", parent.display()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("creating build log {}", path.display()))?;
        if let Err(error) = (|| -> std::io::Result<()> {
            writeln!(file, "Harness Hat Docker build")?;
            writeln!(file, "$ {command_display}")?;
            writeln!(file)?;
            Ok(())
        })() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(Arc::new(Self {
            path,
            file: Mutex::new(file),
            write_error: Mutex::new(None),
        }))
    }

    fn write_line(&self, line: &str) {
        let result = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("build log lock poisoned"))
            .and_then(|mut file| writeln!(file, "{line}").map_err(Into::into));
        if let Err(error) = result
            && let Ok(mut first_error) = self.write_error.lock()
            && first_error.is_none()
        {
            *first_error = Some(error.to_string());
        }
    }

    fn finish(&self, retain: bool, status: &str) -> (Option<PathBuf>, Option<String>) {
        self.write_line(status);
        let mut errors = self.write_error.lock().ok().and_then(|error| error.clone());
        if let Ok(file) = self.file.lock()
            && let Err(error) = file.sync_all()
            && errors.is_none()
        {
            errors = Some(error.to_string());
        }

        if !retain {
            if let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
                && errors.is_none()
            {
                errors = Some(error.to_string());
            }
            return (None, errors);
        }

        if let Err(error) = retain_recent_build_logs(&self.path)
            && errors.is_none()
        {
            errors = Some(error.to_string());
        }
        (Some(self.path.clone()), errors)
    }
}

fn retain_recent_build_logs(current: &Path) -> Result<()> {
    let Some(parent) = current.parent() else {
        return Ok(());
    };
    let mut paths = std::fs::read_dir(parent)
        .with_context(|| format!("reading build log directory {}", parent.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("docker-build-") && name.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort();
    let remove_count = paths.len().saturating_sub(10);
    for path in paths.into_iter().take(remove_count) {
        if path != current {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing old build log {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod build_log_tests {
    use super::*;

    #[test]
    fn build_log_keeps_complete_output_beyond_live_buffer_limit() {
        let dir = tempfile::tempdir().expect("temporary build log directory");
        let path = dir.path().join("docker-build-test.log");
        let log = BuildLog::create(path.clone(), "docker build example").expect("create log");
        for line in 0..500 {
            log.write_line(&format!("build: line {line}"));
        }
        let (saved_path, error) = log.finish(true, "BUILD FAILED (exit code 1)");

        assert_eq!(saved_path, Some(path.clone()));
        assert!(error.is_none(), "unexpected log error: {error:?}");
        let contents = std::fs::read_to_string(path).expect("read saved build log");
        assert!(contents.contains("build: line 0"));
        assert!(contents.contains("build: line 499"));
        assert!(contents.contains("BUILD FAILED (exit code 1)"));
    }

    #[test]
    fn build_log_retention_keeps_only_the_ten_newest_logs() {
        let dir = tempfile::tempdir().expect("temporary build log directory");
        for index in 0..11 {
            std::fs::write(
                dir.path().join(format!("docker-build-{index:02}.log")),
                "failure",
            )
            .expect("write old build log");
        }
        let current = dir.path().join("docker-build-10.log");
        retain_recent_build_logs(&current).expect("retain recent logs");

        let count = std::fs::read_dir(dir.path())
            .expect("read build log directory")
            .count();
        assert_eq!(count, 10);
        assert!(!dir.path().join("docker-build-00.log").exists());
        assert!(current.exists());
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
    build_log: Option<Arc<BuildLog>>,
    tx: mpsc::Sender<BuildEvent>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(build_log) = build_log.as_ref() {
                    build_log.write_line(&format!("build: {line}"));
                }
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
                if tx
                    .send(BuildEvent::Output {
                        line: format!("{prefix}{line}"),
                        is_error,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
}

pub(crate) async fn run_build_docker_commands(
    label: String,
    docker_commands: Vec<Vec<String>>,
    command_display: String,
    build_log_path: PathBuf,
    launch_workspace_idx: usize,
    launch_container_idx: usize,
    launch_session_group: Option<usize>,
    cancel_flag: Arc<AtomicBool>,
    tx: mpsc::Sender<BuildEvent>,
) {
    let (build_log, mut log_error) = match BuildLog::create(build_log_path, &command_display) {
        Ok(build_log) => (Some(build_log), None),
        Err(error) => (None, Some(error.to_string())),
    };

    if docker_commands.is_empty() {
        let (log_path, finish_error) = build_log
            .as_ref()
            .map(|log| log.finish(true, "BUILD FAILED: no docker build commands were provided"))
            .unwrap_or((None, None));
        if log_error.is_none() {
            log_error = finish_error;
        }
        let _ = tx
            .send(BuildEvent::Finished {
                label,
                launch_workspace_idx,
                launch_container_idx,
                launch_session_group,
                success: false,
                cancelled: false,
                exit_code: None,
                error: Some("no docker build commands were provided".to_string()),
                diagnostic: None,
                log_path,
                log_error,
            })
            .await;
        return;
    }

    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let output_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

    let mut cancelled = false;
    let mut final_status = None;
    let mut spawn_error = None;

    for docker_args in docker_commands {
        let mut cmd = tokio::process::Command::new("docker");
        crate::process_util::hide_tokio_console_window(&mut cmd);
        cmd.args(&docker_args)
            .env("BUILDKIT_PROGRESS", "plain")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                spawn_error = Some(e.to_string());
                break;
            }
        };

        let stdout_task = child.stdout.take().map(|stdout| {
            let tx = tx.clone();
            let output_tail = output_tail.clone();
            let build_log = build_log.clone();
            tokio::spawn(async move {
                forward_build_stream(stdout, "build: ", false, None, output_tail, build_log, tx)
                    .await;
            })
        });
        let stderr_task = child.stderr.take().map(|stderr| {
            let tx = tx.clone();
            let stderr_tail = stderr_tail.clone();
            let output_tail = output_tail.clone();
            let build_log = build_log.clone();
            tokio::spawn(async move {
                forward_build_stream(
                    stderr,
                    "build: ",
                    true,
                    Some(stderr_tail),
                    output_tail,
                    build_log,
                    tx,
                )
                .await;
            })
        });

        let (status, command_cancelled) =
            wait_for_build_child(&mut child, Arc::clone(&cancel_flag)).await;
        cancelled = command_cancelled;

        if let Some(task) = stdout_task {
            let _ = task.await;
        }
        if let Some(task) = stderr_task {
            let _ = task.await;
        }

        final_status = status;
        if cancelled || !final_status.as_ref().map(|s| s.success()).unwrap_or(false) {
            break;
        }
    }

    if let Some(error) = spawn_error {
        let diagnostic = join_build_tail(&stderr_tail).or_else(|| join_build_tail(&output_tail));
        let status = format!("BUILD FAILED: {error}");
        let (log_path, finish_error) = build_log
            .as_ref()
            .map(|log| log.finish(true, &status))
            .unwrap_or((None, None));
        if log_error.is_none() {
            log_error = finish_error;
        }
        let _ = tx
            .send(BuildEvent::Finished {
                label,
                launch_workspace_idx,
                launch_container_idx,
                launch_session_group,
                success: false,
                cancelled: false,
                exit_code: None,
                error: Some(error),
                diagnostic,
                log_path,
                log_error,
            })
            .await;
        return;
    }

    let success = !cancelled && final_status.as_ref().map(|s| s.success()).unwrap_or(false);
    let exit_code = final_status.and_then(|s| s.code());
    // Prefer lines that looked like errors; otherwise fall back to the tail of
    // raw output so a failure is never reported with an empty diagnostic.
    let diagnostic = join_build_tail(&stderr_tail).or_else(|| join_build_tail(&output_tail));
    let status = if cancelled {
        "BUILD CANCELLED".to_string()
    } else if success {
        "BUILD SUCCEEDED".to_string()
    } else {
        format!(
            "BUILD FAILED{}",
            exit_code
                .map(|code| format!(" (exit code {code})"))
                .unwrap_or_default()
        )
    };
    let (log_path, finish_error) = build_log
        .as_ref()
        .map(|log| log.finish(!success && !cancelled, &status))
        .unwrap_or((None, None));
    if log_error.is_none() {
        log_error = finish_error;
    }
    let _ = tx
        .send(BuildEvent::Finished {
            label,
            launch_workspace_idx,
            launch_container_idx,
            launch_session_group,
            success,
            cancelled,
            exit_code,
            error: None,
            diagnostic,
            log_path,
            log_error,
        })
        .await;
}

async fn wait_for_build_child(
    child: &mut tokio::process::Child,
    cancel_flag: Arc<AtomicBool>,
) -> (Option<std::process::ExitStatus>, bool) {
    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            kill_build_child_tree(child).await;
            return (child.wait().await.ok(), true);
        }

        match child.try_wait() {
            Ok(Some(status)) => return (Some(status), false),
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            Err(_) => return (None, false),
        }
    }
}

async fn kill_build_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let pid = pid.to_string();
        let mut command = tokio::process::Command::new("taskkill");
        crate::process_util::hide_tokio_console_window(&mut command);
        let _ = command
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }

    let _ = child.start_kill();
}

fn join_build_tail(tail: &Arc<Mutex<VecDeque<String>>>) -> Option<String> {
    tail.lock().ok().and_then(|lines| {
        (!lines.is_empty()).then(|| lines.iter().cloned().collect::<Vec<_>>().join(" | "))
    })
}

pub(crate) fn is_scroll_mode_toggle_key(key: KeyEvent) -> bool {
    (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('\u{13}') && key.modifiers.is_empty())
}

// ── Key → PTY bytes (Streamlined mapping) ────────────────────────────────────
