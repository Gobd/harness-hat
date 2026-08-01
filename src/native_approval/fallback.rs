//! Linux native approval dialogs used by the installed desktop agent.
//!
//! Interactive Linux TUI sessions retain their in-terminal overlay, but the
//! background agent has no terminal and therefore routes prompts here.
//! Any backend error, dismissal, or unrecognized response maps to deny.

use super::{ApprovalRequest, HostdoApprovalRequest, Outcome};

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    let port = req.port.map(|port| format!(":{port}")).unwrap_or_default();
    let workspace = req
        .workspace
        .as_deref()
        .map(|workspace| format!("\nWorkspace: {workspace}"))
        .unwrap_or_default();
    approval_outcome(
        "Harness Hat: Network Approval",
        format!(
            "Allow this network request?\n{} {}{}{}{}",
            req.method, req.host, port, req.path, workspace
        ),
    )
}

pub fn prompt_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    let workspace = req
        .workspace
        .as_deref()
        .map(|workspace| format!("\nWorkspace: {workspace}"))
        .unwrap_or_default();
    let command = format!("\nCommand: {}", req.command);
    let reason = req
        .reason
        .as_deref()
        .map(|reason| format!("\nReason: {reason}"))
        .unwrap_or_default();
    approval_outcome(
        "Harness Hat: Host Command Approval",
        format!(
            "Allow this host command?\n{}{}{}",
            reason, command, workspace
        ),
    )
}

fn approval_outcome(title: &str, description: String) -> Outcome {
    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Deny".to_string(),
            "Allow once".to_string(),
        ))
        .show();
    if matches!(result, rfd::MessageDialogResult::Custom(ref label) if label == "Allow once") {
        Outcome::Allow { remember: false }
    } else {
        Outcome::Deny { remember: false }
    }
}
