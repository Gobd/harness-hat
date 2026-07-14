//! Native system approval dialogs, popped from a subprocess.
//!
//! The manager process owns the OS main thread for its tokio current-thread
//! executor and TUI, which leaves no run loop free for AppKit / Win32 modal
//! dialogs. Rather than restructure startup, we spawn a hidden
//! `hht __dialog network-approval ...` subprocess per prompt; the subprocess
//! has its own main thread and run loop, displays the native dialog, prints
//! a single machine-readable line to stdout, and exits.
//!
//! Wire format (one line, NL-terminated):
//!   `allow remember=true`
//!   `allow remember=false`
//!   `deny remember=true`
//!   `deny remember=false`
//!   `cancelled`
//!
//! On targets without a native backend (currently anything not macOS) the
//! subprocess returns `cancelled`, which the parent should interpret as
//! "deny once, don't persist" so the request is blocked cleanly.

use anyhow::{Context, Result, bail};

#[cfg(not(target_os = "macos"))]
mod fallback;
#[cfg(target_os = "macos")]
mod macos;

/// All the context a native dialog needs to render a useful prompt. Mirrors
/// the subset of `proxy::PendingNetworkItem` that the user actually reads.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub host: String,
    pub method: String,
    pub path: String,
    pub port: Option<u16>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HostdoApprovalRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub workspace: Option<String>,
    pub image: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// What the user picked. `remember` is the "Remember this decision" checkbox
/// state, which the parent uses to decide whether to write a persistent rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow {
        remember: bool,
    },
    Deny {
        remember: bool,
    },
    /// User dismissed the dialog without picking (Esc, close button, native
    /// backend missing). Parent should treat this as "deny once".
    Cancelled,
}

impl Outcome {
    /// Single-line wire encoding. Stable across versions; the subprocess
    /// writes this and the parent parses it.
    pub fn encode(self) -> String {
        match self {
            Outcome::Allow { remember } => format!("allow remember={remember}"),
            Outcome::Deny { remember } => format!("deny remember={remember}"),
            Outcome::Cancelled => "cancelled".to_string(),
        }
    }

    pub fn decode(line: &str) -> Result<Self> {
        let line = line.trim();
        if line == "cancelled" {
            return Ok(Outcome::Cancelled);
        }
        let (verb, rest) = line
            .split_once(' ')
            .with_context(|| format!("malformed dialog output: {line:?}"))?;
        let remember = match rest {
            "remember=true" => true,
            "remember=false" => false,
            other => bail!("malformed remember flag: {other:?}"),
        };
        match verb {
            "allow" => Ok(Outcome::Allow { remember }),
            "deny" => Ok(Outcome::Deny { remember }),
            other => bail!("unknown dialog verb: {other:?}"),
        }
    }
}

/// Entry point called from the `__dialog network-approval` subprocess. Blocks
/// the calling thread (the subprocess's main thread) on the native dialog,
/// returns the user's choice, and the caller prints `outcome.encode()` to
/// stdout.
///
/// Must be called from the OS main thread on macOS (NSAlert requires it).
pub fn run_network_approval(req: &ApprovalRequest) -> Outcome {
    #[cfg(target_os = "macos")]
    {
        macos::prompt_network_approval(req)
    }
    #[cfg(not(target_os = "macos"))]
    {
        fallback::prompt_network_approval(req)
    }
}

/// Entry point called from the `__dialog hostdo-approval ...` subprocess.
/// Returns `Allow`/`Deny`/`Cancelled` as a machine-readable single line.
pub fn run_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    #[cfg(target_os = "macos")]
    {
        macos::prompt_hostdo_approval(req)
    }
    #[cfg(not(target_os = "macos"))]
    {
        fallback::prompt_hostdo_approval(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_roundtrips_through_wire_format() {
        for o in [
            Outcome::Allow { remember: true },
            Outcome::Allow { remember: false },
            Outcome::Deny { remember: true },
            Outcome::Deny { remember: false },
            Outcome::Cancelled,
        ] {
            assert_eq!(Outcome::decode(&o.encode()).unwrap(), o);
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Outcome::decode("").is_err());
        assert!(Outcome::decode("allow").is_err());
        assert!(Outcome::decode("allow remember=maybe").is_err());
        assert!(Outcome::decode("approve remember=true").is_err());
    }

    #[test]
    fn decode_tolerates_trailing_newline() {
        assert_eq!(
            Outcome::decode("allow remember=true\n").unwrap(),
            Outcome::Allow { remember: true },
        );
    }
}
