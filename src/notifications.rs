//! Native OS notifications for events that need user attention.
//!
//! Used to nudge the user toward the in-TUI approval modal when a network
//! approval becomes pending while the TUI may not be in focus. Notifications
//! are best-effort — any failure (no D-Bus session, macOS bundle quirks,
//! missing toast permission on Windows) is logged at debug level and ignored.

use std::sync::OnceLock;
use tracing::debug;

const APP_NAME: &str = "harness-hat";

/// macOS bundle id we associate with for notifications. Unbundled CLI tools
/// can't post NSUserNotifications under their own identity; piggy-backing on
/// Terminal.app lets the notification surface at all (under Terminal's icon).
#[cfg(target_os = "macos")]
const MACOS_APPLICATION: &str = "com.apple.Terminal";

/// Initialize platform-specific notification state. Idempotent and safe to
/// call from any thread; only the first call has any effect.
pub fn init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(platform_init);
}

#[cfg(target_os = "macos")]
fn platform_init() {
    if let Err(e) = notify_rust::set_application(MACOS_APPLICATION) {
        debug!(error = %e, "notify_rust::set_application failed; notifications may not surface");
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_init() {}

/// Fire a best-effort native notification telling the user a network approval
/// is waiting in the TUI. `queue_total` is the full pending count (including
/// this item), so callers don't have to compute "+N more" themselves.
///
/// The actual `show()` is done off-thread because notify-rust talks to D-Bus
/// on Linux and `osascript`/cocoa on macOS, either of which can block long
/// enough to stutter the TUI render loop.
pub fn notify_pending_network_approval(
    host: &str,
    source_workspace: Option<&str>,
    queue_total: usize,
) {
    let host = host.to_string();
    let workspace = source_workspace
        .unwrap_or("unknown workspace")
        .to_string();
    let extra = if queue_total > 1 {
        format!(" (+{} more pending)", queue_total - 1)
    } else {
        String::new()
    };
    std::thread::spawn(move || {
        let body = format!("{host} · {workspace}{extra}");
        if let Err(e) = notify_rust::Notification::new()
            .summary("Harness Hat: network approval")
            .body(&body)
            .appname(APP_NAME)
            .show()
        {
            debug!(error = %e, "failed to show native notification");
        }
    });
}
