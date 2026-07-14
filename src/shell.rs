//! `hht shell` — open an interactive shell in a running session.
//!
//! This is a pure-Docker passthrough: it discovers sessions purely from the
//! discovery labels stamped at launch (`harness-hat.alias` etc.) and attaches
//! with `docker exec`, never touching the proxy, control server, or config
//! loading.
//!
//! Sessions only exist while the manager is running. Each session container is
//! launched as `docker run --rm -it` owned by the manager's PTY, so quitting
//! the manager (or that session's terminal) tears the container down and
//! `--rm` removes it. `hht shell` therefore only finds a session while the
//! manager that launched it is still running — run it from a second terminal
//! alongside the live manager.

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::process::Command;

use crate::container::{
    LABEL_ALIAS, LABEL_TEMPLATE, LABEL_WORKSPACE, SHELL_HOME, SHELL_USER, parse_docker_label,
};

/// Entry point for the `shell` subcommand. With an id, attaches to that
/// session, optionally running `args` as a command instead of bash; without
/// an id, prints the running sessions and their ids (and `args` is ignored).
pub fn run(id: Option<String>, args: Vec<OsString>) -> Result<i32> {
    if which::which("docker").is_err() {
        bail!(
            "docker not found in PATH — `{} shell` requires Docker",
            crate::cli::COMMAND_NAME
        );
    }
    match id {
        Some(id) => attach(&id, &args),
        None => {
            list()?;
            Ok(0)
        }
    }
}

/// A running harness-hat session, as seen through docker labels.
pub(crate) struct Session {
    pub(crate) alias: String,
    pub(crate) workspace: String,
    pub(crate) template: String,
    pub(crate) name: String,
}

/// Enumerate running harness-hat sessions via `docker ps`. Returns containers
/// in docker's natural order (newest first), so callers wanting most-recent
/// behavior can take the first element.
pub(crate) fn running_sessions() -> Result<Vec<Session>> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label={LABEL_ALIAS}"),
            "--format",
            "{{.Names}}\t{{.Labels}}",
        ])
        .output()
        .context("running docker ps")?;
    if !output.status.success() {
        bail!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut sessions = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((name, labels)) = line.split_once('\t') else {
            continue;
        };
        let Some(alias) = parse_docker_label(labels, LABEL_ALIAS) else {
            continue;
        };
        sessions.push(Session {
            alias,
            workspace: parse_docker_label(labels, LABEL_WORKSPACE).unwrap_or_default(),
            template: parse_docker_label(labels, LABEL_TEMPLATE).unwrap_or_default(),
            name: name.trim().to_string(),
        });
    }
    Ok(sessions)
}

/// Normalize a user-supplied id to the stored form: numeric ids are
/// zero-padded to the 4-digit width that's shown in the UI, so `hht shell 42`
/// matches the displayed `0042`. Non-numeric or longer ids pass through.
fn normalize_id(id: &str) -> String {
    if !id.is_empty() && id.len() < 4 && id.bytes().all(|b| b.is_ascii_digit()) {
        format!("{id:0>4}")
    } else {
        id.to_string()
    }
}

fn attach(id: &str, extra_args: &[OsString]) -> Result<i32> {
    let wanted = normalize_id(id);
    let sessions = running_sessions()?;
    let matches: Vec<&Session> = sessions.iter().filter(|s| s.alias == wanted).collect();

    let name = match matches.as_slice() {
        [] => bail!(
            "no running session with id '{wanted}'. Run `{} shell` to list sessions. \
             (Sessions only exist while the manager is running — is it still open?)",
            crate::cli::COMMAND_NAME
        ),
        [session] => session.name.clone(),
        many => {
            // Aliases are deduped at launch, so this should not happen; pick the
            // first deterministically rather than failing the user outright.
            eprintln!(
                "warning: {} running sessions share id '{wanted}'; attaching to the first",
                many.len()
            );
            many[0].name.clone()
        }
    };

    exec_into_container(&name, extra_args)
}

/// Run `docker exec` against a container with optional trailing argv (or
/// `/bin/bash` when empty), with the same TTY auto-detect + signal guarding +
/// host-terminal cleanup the interactive shell flow needs. Reused by
/// `hht workspace`.
///
/// Don't `exec()` into docker exec. If the manager exits while this shell is
/// attached, the `docker run --rm` container dies and `docker exec` is ripped
/// out before any program inside (bash readline, vim, fzf, the user's
/// prompt) can send its DECRST cleanup sequences. The host terminal is then
/// stuck with focus reporting / bracketed paste / mouse mode / hidden cursor
/// left on. Staying as the parent lets us emit the disable sequences
/// ourselves on the way out.
///
/// Because we're no longer the docker exec process itself, Ctrl-C in the
/// terminal would be delivered to both us and the child via the foreground
/// process group. Ignore the terminal-driven signals here so they're handled
/// by `docker exec` (which forwards them through the PTY) and we survive to
/// run the cleanup.
pub(crate) fn exec_into_container(container_name: &str, extra_args: &[OsString]) -> Result<i32> {
    // `-it` requires a TTY on stdin; without one (e.g. piped input) docker
    // exec refuses to start. Drop `-t` when stdin isn't a terminal so
    // `echo ... | hht shell ID cat` works.
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut command = Command::new("docker");
    command.args([
        "exec",
        if stdin_is_tty { "-it" } else { "-i" },
        "-u",
        SHELL_USER,
        "-e",
        &format!("HOME={SHELL_HOME}"),
        container_name,
    ]);
    if extra_args.is_empty() {
        command.arg("/bin/bash");
    } else {
        command.args(extra_args);
    }

    let _signal_guard = IgnoreTerminalSignals::install();
    let _terminal_reset = TerminalModesResetGuard::new();
    let status = command.status().context("running docker exec")?;
    Ok(status.code().unwrap_or(1))
}

struct TerminalModesResetGuard {
    enabled: bool,
}

impl TerminalModesResetGuard {
    fn new() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal(),
        }
    }
}

impl Drop for TerminalModesResetGuard {
    fn drop(&mut self) {
        if self.enabled {
            restore_host_terminal_modes();
        }
    }
}

#[cfg(unix)]
struct IgnoreTerminalSignals {
    prev: [(libc::c_int, libc::sighandler_t); 3],
}

#[cfg(unix)]
impl IgnoreTerminalSignals {
    fn install() -> Self {
        let signals = [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP];
        let mut prev = [(0, 0 as libc::sighandler_t); 3];
        for (slot, sig) in prev.iter_mut().zip(signals) {
            // SAFETY: setting SIG_IGN on terminal-driven signals; the returned
            // sighandler_t is stored verbatim for later restoration.
            let old = unsafe { libc::signal(sig, libc::SIG_IGN) };
            *slot = (sig, old);
        }
        Self { prev }
    }
}

#[cfg(unix)]
impl Drop for IgnoreTerminalSignals {
    fn drop(&mut self) {
        for &(sig, old) in &self.prev {
            // SAFETY: restoring the previously-saved disposition for `sig`.
            unsafe {
                libc::signal(sig, old);
            }
        }
    }
}

#[cfg(not(unix))]
struct IgnoreTerminalSignals;

#[cfg(not(unix))]
impl IgnoreTerminalSignals {
    fn install() -> Self {
        Self
    }
}

/// Reset the terminal modes a program inside `docker exec` may have enabled but
/// never gotten to disable. We can't know which modes the inner program set, so
/// we unconditionally disable the common ones whose "stuck" state is visible to
/// the user (focus reporting prints `^[[I` / `^[[O` on focus changes; bracketed
/// paste wraps every paste in `^[[200~`; mouse reporting eats clicks; kitty key
/// encoding leaves `CSI u` text in the app when legacy tools resume; hidden
/// cursor makes shells look frozen). Sequences that have no effect when already
/// off are harmless, so this is safe to run on every exit.
fn restore_host_terminal_modes() {
    const RESET: &[u8] = concat!(
        "\x1b<u",      // kitty keyboard protocol: pop mode
        "\x1b[>4;0m",  // xterm: disable modifyOtherKeys
        "\x1b[?1004l", // focus reporting
        "\x1b[?2004l", // bracketed paste
        "\x1b[?1000l", // mouse: X10
        "\x1b[?1002l", // mouse: button-event
        "\x1b[?1003l", // mouse: any-event
        "\x1b[?1006l", // mouse: SGR encoding
        "\x1b[?1015l", // mouse: urxvt encoding
        "\x1b[?25h",   // cursor visible
        "\x1b[?1049l", // leave alternate screen
        "\x1b[?7h",    // line wrap on
        "\x1b[0m",     // reset SGR
    )
    .as_bytes();
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(RESET);
    let _ = stdout.flush();
}

fn list() -> Result<()> {
    let mut sessions = running_sessions()?;
    sessions.sort_by(|a, b| a.alias.cmp(&b.alias));
    if sessions.is_empty() {
        println!(
            "No running harness-hat sessions. Sessions only exist while the manager is \
             running — launch one in the manager, then run `{} shell` from another terminal.",
            crate::cli::COMMAND_NAME
        );
        return Ok(());
    }

    let ws_width = sessions
        .iter()
        .map(|s| s.workspace.len())
        .chain(std::iter::once("WORKSPACE".len()))
        .max()
        .unwrap_or(0);
    let tpl_width = sessions
        .iter()
        .map(|s| s.template.len())
        .chain(std::iter::once("TEMPLATE".len()))
        .max()
        .unwrap_or(0);

    println!(
        "{:<6}{:<ws$}  {:<tpl$}",
        "ID",
        "WORKSPACE",
        "TEMPLATE",
        ws = ws_width + 2,
        tpl = tpl_width
    );
    for session in &sessions {
        println!(
            "{:<6}{:<ws$}  {:<tpl$}",
            session.alias,
            session.workspace,
            session.template,
            ws = ws_width + 2,
            tpl = tpl_width
        );
    }
    println!("\nAttach with: {} shell <ID>", crate::cli::COMMAND_NAME);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_id;

    #[test]
    fn numeric_ids_are_zero_padded_to_width_four() {
        assert_eq!(normalize_id("42"), "0042");
        assert_eq!(normalize_id("7"), "0007");
        assert_eq!(normalize_id("0042"), "0042");
    }

    #[test]
    fn non_numeric_or_long_ids_pass_through() {
        assert_eq!(normalize_id("12345"), "12345");
        assert_eq!(normalize_id("abcd"), "abcd");
        assert_eq!(normalize_id(""), "");
    }
}
