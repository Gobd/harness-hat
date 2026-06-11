//! Non-macOS placeholder for the native approval dialog.
//!
//! Windows will get a `TaskDialogIndirect`-backed implementation. Linux
//! support is best-effort and may stay as the TUI modal indefinitely. Until
//! then we deliberately return `Cancelled` so the parent blocks the request
//! without persisting a rule.

use tracing::debug;

use super::{ApprovalRequest, Outcome};

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    debug!(
        host = %req.host,
        "no native approval backend on this target; returning Cancelled"
    );
    Outcome::Cancelled
}
