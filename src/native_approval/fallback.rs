//! Non-macOS placeholder for the native approval dialog.
//!
//! Windows and Linux currently use the in-TUI approval flow. If this fallback
//! subprocess is invoked directly, it returns `Cancelled` so the request is
//! blocked without persisting a rule.

use tracing::debug;

use super::{ApprovalRequest, HostdoApprovalRequest, Outcome};

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    debug!(
        host = %req.host,
        "no native approval backend on this target; returning Cancelled"
    );
    Outcome::Cancelled
}

pub fn prompt_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    debug!(
        command = %req.command,
        "no native approval backend on this target; returning Cancelled"
    );
    Outcome::Cancelled
}
