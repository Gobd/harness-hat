//! Bridge between the in-TUI pending-network queue and the native OS approval
//! dialog.
//!
//! The manager process owns the OS main thread (current-thread tokio + the
//! TUI), so it can't host an AppKit/Win32 modal directly. Instead it spawns a
//! short-lived `hht __dialog network-approval ...` subprocess that has its own
//! main thread, shows the dialog, and prints a single result line. This module
//! launches that subprocess for the front pending item, collects its result,
//! and feeds the decision back through the existing approve/deny paths.
//!
//! Network and host-command approvals use native dialogs for the background
//! service and on macOS. Rules-file tampering always opens a system dialog and
//! remains fail-closed if that dialog cannot be displayed.

use super::*;
use crate::native_approval::{
    ApprovalRequest, HostdoApprovalRequest, Outcome, RulesChangedRequest,
};

/// Message a finished native-dialog subprocess task sends back to the event
/// loop: the activity id it was prompting for, plus the user's choice.
pub(crate) type NativeDialogResult = (NativeDialogTarget, Outcome);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeDialogTarget {
    Exec(String),
    Network(String),
    RulesChanged(std::path::PathBuf),
}

impl App {
    /// Whether network-approval prompts are delegated to a native OS dialog
    /// rather than the in-TUI overlay.
    pub(crate) fn native_dialog_enabled(&self) -> bool {
        cfg!(target_os = "macos") || self.service_mode
    }

    /// Drain results from finished native-dialog subprocesses and apply each to
    /// its matching pending item. Called once per event-loop tick.
    pub(crate) fn drain_native_dialog_results(&mut self) {
        for _ in 0..16 {
            match self.background_channels.native_dialog_rx.try_recv() {
                Ok((target, outcome)) => self.apply_native_dialog_outcome(target, outcome),
                Err(_) => break,
            }
        }
    }

    /// If native dialogs are enabled, none is already on screen, and a request
    /// is waiting, pop a dialog for the front pending item. Only one modal is
    /// shown at a time, so a burst of requests queues behind it.
    pub(crate) fn maybe_launch_native_dialog(&mut self) {
        if self.native_dialog_inflight.is_some() {
            return;
        }
        let changed_rules_path = self.base_rules_changed.as_mut().and_then(|state| {
            (!state.dialog_dismissed).then(|| {
                state.dialog_dismissed = true;
                state.path.clone()
            })
        });
        if let Some(path) = changed_rules_path {
            let target = NativeDialogTarget::RulesChanged(path.clone());
            self.native_dialog_inflight = Some(target.clone());
            spawn_native_rules_changed_dialog(
                target,
                RulesChangedRequest { path },
                self.background_channels.native_dialog_tx.clone(),
            );
            return;
        }
        if !self.native_dialog_enabled() {
            return;
        }
        if let Some(item) = self.pending_exec.first() {
            let target = NativeDialogTarget::Exec(item.activity_id.clone());
            let req = HostdoApprovalRequest {
                command: item.argv.join(" "),
                reason: item.reason.clone(),
                cwd: Some(item.cwd.display().to_string()),
                workspace: Some(item.workspace_name.clone()),
                image: item.image.clone(),
                timeout_secs: Some(item.timeout_secs),
            };
            self.native_dialog_inflight = Some(target.clone());
            spawn_native_hostdo_dialog(
                target,
                req,
                self.background_channels.native_dialog_tx.clone(),
            );
            return;
        }
        let Some(item) = self.pending_net.first() else {
            return;
        };
        let target = NativeDialogTarget::Network(item.activity_id.clone());
        // Display-only attribution for the dialog body; persistence later uses
        // `unambiguous_pending_network_workspace`, never this.
        let workspace = item
            .source_workspace
            .clone()
            .or_else(|| item.source_container.clone());
        let req = ApprovalRequest {
            host: item.host.clone(),
            method: item.method.clone(),
            path: item.path.clone(),
            port: item.port,
            workspace,
        };
        self.native_dialog_inflight = Some(target.clone());
        spawn_native_network_dialog(
            target,
            req,
            self.background_channels.native_dialog_tx.clone(),
        );
    }

    /// Translate a native dialog `Outcome` into the existing approve/deny paths.
    /// `remember` maps to the persistent "forever" rules; when the request's
    /// source workspace is ambiguous we can't safely write a rule, so we log and
    /// fall back to a one-shot decision rather than re-prompting endlessly.
    fn apply_native_dialog_outcome(&mut self, target: NativeDialogTarget, outcome: Outcome) {
        if self.native_dialog_inflight.as_ref() == Some(&target) {
            self.native_dialog_inflight = None;
        }
        match target {
            NativeDialogTarget::Exec(activity_id) => {
                let Some(idx) = self
                    .pending_exec
                    .iter()
                    .position(|item| item.activity_id == activity_id)
                else {
                    return;
                };
                match outcome {
                    Outcome::Allow { remember: false } => self.approve_exec(idx, false),
                    Outcome::Allow { remember: true } => self.approve_exec(idx, true),
                    Outcome::Deny { remember: false } | Outcome::Cancelled => self.deny_exec(idx),
                    Outcome::Deny { remember: true } => self.deny_exec_forever(idx),
                }
            }
            NativeDialogTarget::Network(activity_id) => {
                let Some(idx) = self
                    .pending_net
                    .iter()
                    .position(|item| item.activity_id == activity_id)
                else {
                    return;
                };
                match outcome {
                    Outcome::Allow { remember: false } => self.approve_net(idx),
                    Outcome::Deny { remember: false } => self.deny_net(idx),
                    Outcome::Cancelled => self.deny_net(idx),
                    Outcome::Allow { remember: true } => {
                        if self.unambiguous_pending_network_workspace(idx).is_some() {
                            self.approve_net_forever(idx);
                        } else {
                            self.log_ambiguous_network_workspace_context(idx, "allow");
                            self.approve_net(idx);
                        }
                    }
                    Outcome::Deny { remember: true } => {
                        if self.unambiguous_pending_network_workspace(idx).is_some() {
                            self.deny_net_forever(idx);
                        } else {
                            self.log_ambiguous_network_workspace_context(idx, "deny");
                            self.deny_net(idx);
                        }
                    }
                }
            }
            NativeDialogTarget::RulesChanged(path) => match outcome {
                Outcome::Allow { .. } => {
                    let Some(expected_contents) = self
                        .base_rules_changed
                        .as_ref()
                        .filter(|state| state.path == path)
                        .map(|state| state.expected_contents.as_deref())
                    else {
                        self.config.block_rules_file(&path);
                        return;
                    };
                    match self
                        .config
                        .trust_rules_file_if_bytes(&path, expected_contents)
                    {
                        Ok(()) => {
                            self.base_rules_changed = None;
                            self.push_log(
                                format!("trusted reviewed rules file: {}", path.display()),
                                false,
                            );
                        }
                        Err(error) => {
                            self.config.block_rules_file(&path);
                            if let Some(state) = self.base_rules_changed.as_mut()
                                && state.path == path
                            {
                                state.dialog_dismissed = false;
                            }
                            self.push_log(
                                format!(
                                    "rules file changed before it could be trusted '{}': {error}",
                                    path.display()
                                ),
                                true,
                            );
                        }
                    }
                }
                Outcome::Deny { .. } | Outcome::Cancelled => {
                    self.config.block_rules_file(&path);
                    self.push_log(
                        format!("rules remain blocked until reviewed: {}", path.display()),
                        true,
                    );
                }
            },
        }
    }
}

fn spawn_native_network_dialog(
    target: NativeDialogTarget,
    req: ApprovalRequest,
    result_tx: mpsc::Sender<NativeDialogResult>,
) {
    tokio::spawn(async move {
        let outcome = run_native_dialog_subprocess(&req).await;
        let _ = result_tx.send((target, outcome)).await;
    });
}

fn spawn_native_hostdo_dialog(
    target: NativeDialogTarget,
    req: HostdoApprovalRequest,
    result_tx: mpsc::Sender<NativeDialogResult>,
) {
    tokio::spawn(async move {
        let outcome = run_native_hostdo_dialog_subprocess(&req).await;
        let _ = result_tx.send((target, outcome)).await;
    });
}

fn spawn_native_rules_changed_dialog(
    target: NativeDialogTarget,
    req: RulesChangedRequest,
    result_tx: mpsc::Sender<NativeDialogResult>,
) {
    tokio::spawn(async move {
        let outcome = run_native_rules_changed_dialog_subprocess(&req).await;
        let _ = result_tx.send((target, outcome)).await;
    });
}

/// Run the dialog subprocess and parse its single result line. Any failure
/// (can't find our own exe, spawn error, unparseable output) maps to
/// `Cancelled`, which the caller treats as "deny once" — a request is never
/// silently allowed because the dialog misbehaved.
async fn run_native_dialog_subprocess(req: &ApprovalRequest) -> Outcome {
    let exe = match dialog_executable() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "cannot launch native network approval dialog");
            return Outcome::Cancelled;
        }
    };
    let mut cmd = tokio::process::Command::new(exe);
    crate::process_util::hide_tokio_console_window(&mut cmd);
    cmd.arg("__dialog").arg("network-approval");
    cmd.arg("--host").arg(&req.host);
    if !req.method.is_empty() {
        cmd.arg("--method").arg(&req.method);
    }
    if !req.path.is_empty() {
        cmd.arg("--path").arg(&req.path);
    }
    if let Some(port) = req.port {
        cmd.arg("--port").arg(port.to_string());
    }
    if let Some(workspace) = &req.workspace {
        cmd.arg("--workspace").arg(workspace);
    }
    // The dialog reads nothing from stdin; inheriting the manager's would let it
    // compete for terminal input.
    cmd.stdin(std::process::Stdio::null());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(_) => return Outcome::Cancelled,
    };
    // Parse the last non-empty line so any stray stdout noise can't break
    // decoding of the result line the subprocess prints last.
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    Outcome::decode(line).unwrap_or(Outcome::Cancelled)
}

async fn run_native_hostdo_dialog_subprocess(req: &HostdoApprovalRequest) -> Outcome {
    let exe = match dialog_executable() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "cannot launch native host-command approval dialog");
            return Outcome::Cancelled;
        }
    };
    let mut cmd = tokio::process::Command::new(exe);
    crate::process_util::hide_tokio_console_window(&mut cmd);
    cmd.arg("__dialog").arg("hostdo-approval");
    cmd.arg("--command").arg(&req.command);
    if let Some(reason) = &req.reason {
        cmd.arg("--reason").arg(reason);
    }
    if let Some(cwd) = &req.cwd {
        cmd.arg("--cwd").arg(cwd);
    }
    if let Some(image) = &req.image {
        cmd.arg("--image").arg(image);
    }
    if let Some(workspace) = &req.workspace {
        cmd.arg("--workspace").arg(workspace);
    }
    if let Some(timeout_secs) = &req.timeout_secs {
        cmd.arg("--timeout").arg(timeout_secs.to_string());
    }
    cmd.stdin(std::process::Stdio::null());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(_) => return Outcome::Cancelled,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    Outcome::decode(line).unwrap_or(Outcome::Cancelled)
}

async fn run_native_rules_changed_dialog_subprocess(req: &RulesChangedRequest) -> Outcome {
    let exe = match dialog_executable() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "cannot launch native rules-change dialog");
            return Outcome::Cancelled;
        }
    };
    let mut cmd = tokio::process::Command::new(exe);
    crate::process_util::hide_tokio_console_window(&mut cmd);
    cmd.arg("__dialog")
        .arg("rules-changed")
        .arg("--path")
        .arg(&req.path)
        .stdin(std::process::Stdio::null());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(_) => return Outcome::Cancelled,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    Outcome::decode(line).unwrap_or(Outcome::Cancelled)
}

/// Native dialog subcommands belong to the user-facing `hht` binary, not the
/// `hht-daemon` service binary. The daemon is deliberately a narrow process
/// that accepts only its service configuration, so using `current_exe()` here
/// would turn every prompt into an unknown-argument failure and fail closed.
fn dialog_executable() -> Result<std::path::PathBuf, String> {
    let current =
        std::env::current_exe().map_err(|error| format!("locating current executable: {error}"))?;
    dialog_executable_next_to(&current)
}

fn dialog_executable_next_to(current: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let parent = current.parent().ok_or_else(|| {
        format!(
            "cannot locate hht next to current executable {}",
            current.display()
        )
    })?;
    let name = if cfg!(target_os = "windows") {
        "hht.exe"
    } else {
        "hht"
    };
    let candidate = parent.join(name);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "hht dialog executable was not found next to {}; reinstall Harness Hat so hht and hht-daemon are installed together",
            current.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::dialog_executable_next_to;

    #[test]
    fn dialog_executable_uses_sibling_hht_binary() {
        let temp = tempfile::tempdir().expect("temp dir");
        let daemon = temp.path().join(if cfg!(target_os = "windows") {
            "hht-daemon.exe"
        } else {
            "hht-daemon"
        });
        let hht = temp.path().join(if cfg!(target_os = "windows") {
            "hht.exe"
        } else {
            "hht"
        });
        std::fs::write(&daemon, "daemon").expect("daemon binary placeholder");
        std::fs::write(&hht, "hht").expect("hht binary placeholder");

        assert_eq!(dialog_executable_next_to(&daemon).unwrap(), hht);
    }

    #[test]
    fn dialog_executable_requires_sibling_hht_binary() {
        let temp = tempfile::tempdir().expect("temp dir");
        let daemon = temp.path().join("hht-daemon");
        std::fs::write(&daemon, "daemon").expect("daemon binary placeholder");

        assert!(dialog_executable_next_to(&daemon).is_err());
    }
}
