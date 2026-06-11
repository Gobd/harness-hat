//! Bridge between the in-TUI pending-network queue and the native OS approval
//! dialog.
//!
//! The manager process owns the OS main thread (current-thread tokio + the
//! TUI), so it can't host an AppKit/Win32 modal directly. Instead it spawns a
//! short-lived `hh __dialog network-approval ...` subprocess that has its own
//! main thread, shows the dialog, and prints a single result line. This module
//! launches that subprocess for the front pending item, collects its result,
//! and feeds the decision back through the existing approve/deny paths.
//!
//! macOS only — every other platform falls back to the in-TUI overlay because
//! the subprocess has no native backend there (it would just return
//! `cancelled`, silently denying every request).

use super::*;
use crate::native_approval::{ApprovalRequest, Outcome};

/// Message a finished native-dialog subprocess task sends back to the event
/// loop: the activity id it was prompting for, plus the user's choice.
pub(crate) type NativeDialogResult = (String, Outcome);

impl App {
    /// Whether network-approval prompts are delegated to a native OS dialog
    /// rather than the in-TUI overlay.
    pub(crate) fn native_dialog_enabled() -> bool {
        cfg!(target_os = "macos")
    }

    /// Drain results from finished native-dialog subprocesses and apply each to
    /// its matching pending item. Called once per event-loop tick.
    pub(crate) fn drain_native_dialog_results(&mut self) {
        for _ in 0..16 {
            match self.native_dialog_rx.try_recv() {
                Ok((activity_id, outcome)) => {
                    self.apply_native_dialog_outcome(&activity_id, outcome)
                }
                Err(_) => break,
            }
        }
    }

    /// If native dialogs are enabled, none is already on screen, and a request
    /// is waiting, pop a dialog for the front pending item. Only one modal is
    /// shown at a time, so a burst of requests queues behind it.
    pub(crate) fn maybe_launch_native_dialog(&mut self) {
        if !Self::native_dialog_enabled() || self.native_dialog_inflight.is_some() {
            return;
        }
        let Some(item) = self.pending_net.first() else {
            return;
        };
        let activity_id = item.activity_id.clone();
        // Display-only attribution for the dialog body; persistence later uses
        // `unambiguous_pending_network_project`, never this.
        let workspace = item
            .source_project
            .clone()
            .or_else(|| item.source_container.clone());
        let req = ApprovalRequest {
            host: item.host.clone(),
            method: item.method.clone(),
            path: item.path.clone(),
            port: item.port,
            workspace,
        };
        self.native_dialog_inflight = Some(activity_id.clone());
        spawn_native_dialog(activity_id, req, self.native_dialog_tx.clone());
    }

    /// Translate a native dialog `Outcome` into the existing approve/deny paths.
    /// `remember` maps to the persistent "forever" rules; when the request's
    /// source workspace is ambiguous we can't safely write a rule, so we log and
    /// fall back to a one-shot decision rather than re-prompting endlessly.
    fn apply_native_dialog_outcome(&mut self, activity_id: &str, outcome: Outcome) {
        if self.native_dialog_inflight.as_deref() == Some(activity_id) {
            self.native_dialog_inflight = None;
        }
        let Some(idx) = self
            .pending_net
            .iter()
            .position(|item| item.activity_id == activity_id)
        else {
            // Already resolved — e.g. the source session was torn down, or a
            // merged duplicate answered it. Nothing to do.
            return;
        };
        match outcome {
            Outcome::Allow { remember: false } => self.approve_net(idx),
            Outcome::Deny { remember: false } => self.deny_net(idx),
            // Esc / closed window / missing backend: deny once, persist nothing.
            Outcome::Cancelled => self.deny_net(idx),
            Outcome::Allow { remember: true } => {
                if self.unambiguous_pending_network_project(idx).is_some() {
                    self.approve_net_forever(idx);
                } else {
                    self.log_ambiguous_network_project_context(idx, "allow");
                    self.approve_net(idx);
                }
            }
            Outcome::Deny { remember: true } => {
                if self.unambiguous_pending_network_project(idx).is_some() {
                    self.deny_net_forever(idx);
                } else {
                    self.log_ambiguous_network_project_context(idx, "deny");
                    self.deny_net(idx);
                }
            }
        }
    }
}

/// Spawn the `hh __dialog network-approval` subprocess on the current runtime
/// and return immediately. When the user answers (or the subprocess fails) the
/// outcome is delivered to `result_tx`, which the event loop drains on its next
/// tick. The task captures only owned `String`s plus the sender, so it is
/// `Send` and safe to `tokio::spawn` even though `App` itself is not.
fn spawn_native_dialog(
    activity_id: String,
    req: ApprovalRequest,
    result_tx: mpsc::UnboundedSender<NativeDialogResult>,
) {
    tokio::spawn(async move {
        let outcome = run_native_dialog_subprocess(&req).await;
        let _ = result_tx.send((activity_id, outcome));
    });
}

/// Run the dialog subprocess and parse its single result line. Any failure
/// (can't find our own exe, spawn error, unparseable output) maps to
/// `Cancelled`, which the caller treats as "deny once" — a request is never
/// silently allowed because the dialog misbehaved.
async fn run_native_dialog_subprocess(req: &ApprovalRequest) -> Outcome {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return Outcome::Cancelled,
    };
    let mut cmd = tokio::process::Command::new(exe);
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
