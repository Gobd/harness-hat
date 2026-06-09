//! `hh shell` — open an interactive shell in a running session.
//!
//! This is a pure-Docker passthrough: it discovers sessions purely from the
//! discovery labels stamped at launch (`harness-hat.alias` etc.) and attaches
//! with `docker exec`, never touching the proxy, control server, or config
//! loading.
//!
//! Sessions only exist while the manager is running. Each session container is
//! launched as `docker run --rm -it` owned by the manager's PTY, so quitting
//! the manager (or that session's terminal) tears the container down and
//! `--rm` removes it. `hh shell` therefore only finds a session while the
//! manager that launched it is still running — run it from a second terminal
//! alongside the live manager.

use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::container::{
    LABEL_ALIAS, LABEL_TEMPLATE, LABEL_WORKSPACE, SHELL_HOME, SHELL_USER, parse_docker_label,
};

/// Entry point for the `shell` subcommand. With an id, attaches to that
/// session; without one, prints the running sessions and their ids.
pub fn run(id: Option<String>) -> Result<i32> {
    if which::which("docker").is_err() {
        bail!("docker not found in PATH — `hh shell` requires Docker");
    }
    match id {
        Some(id) => attach(&id),
        None => {
            list()?;
            Ok(0)
        }
    }
}

/// A running harness-hat session, as seen through docker labels.
struct Session {
    alias: String,
    workspace: String,
    template: String,
    name: String,
}

fn running_sessions() -> Result<Vec<Session>> {
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
    sessions.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(sessions)
}

/// Normalize a user-supplied id to the stored form: numeric ids are
/// zero-padded to the 4-digit width that's shown in the UI, so `hh shell 42`
/// matches the displayed `0042`. Non-numeric or longer ids pass through.
fn normalize_id(id: &str) -> String {
    if !id.is_empty() && id.len() < 4 && id.bytes().all(|b| b.is_ascii_digit()) {
        format!("{id:0>4}")
    } else {
        id.to_string()
    }
}

fn attach(id: &str) -> Result<i32> {
    let wanted = normalize_id(id);
    let sessions = running_sessions()?;
    let matches: Vec<&Session> = sessions.iter().filter(|s| s.alias == wanted).collect();

    let name = match matches.as_slice() {
        [] => bail!(
            "no running session with id '{wanted}'. Run `hh shell` to list sessions. \
             (Sessions only exist while the manager is running — is it still open?)"
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

    let mut command = Command::new("docker");
    command.args([
        "exec",
        "-it",
        "-u",
        SHELL_USER,
        "-e",
        &format!("HOME={SHELL_HOME}"),
        &name,
        "/bin/bash",
    ]);

    #[cfg(unix)]
    {
        // Replace this process with docker exec so the user gets a native TTY
        // with no intermediate PTY. exec() only returns on failure.
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        Err(err).context("exec docker")
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("running docker exec")?;
        Ok(status.code().unwrap_or(1))
    }
}

fn list() -> Result<()> {
    let sessions = running_sessions()?;
    if sessions.is_empty() {
        println!(
            "No running harness-hat sessions. Sessions only exist while the manager is \
             running — launch one in the manager, then run `hh shell` from another terminal."
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

    println!("{:<6}{:<ws$}  {:<tpl$}", "ID", "WORKSPACE", "TEMPLATE", ws = ws_width + 2, tpl = tpl_width);
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
    println!("\nAttach with: hh shell <ID>");
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
