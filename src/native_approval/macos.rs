//! macOS native approval dialog using NSAlert.
//!
//! Called from inside the `__dialog network-approval` subprocess. That
//! subprocess owns its main thread and has no other event loop competing for
//! it, so we can safely bring up `NSApplication`, set an activation policy,
//! and block on `runModal`.

#![cfg(target_os = "macos")]

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSAlert, NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn};
use objc2_foundation::{MainThreadMarker, NSString};

use super::{ApprovalRequest, HostdoApprovalRequest, Outcome};

// NSAlert button returns are stable AppKit ABI values; using the raw ints
// avoids depending on whether the binding crate re-exports the named
// constants.
const NS_ALERT_FIRST_BUTTON_RETURN: isize = 1000;
const NS_ALERT_SECOND_BUTTON_RETURN: isize = 1001;

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    autoreleasepool(|_| {
        // NSAlert::runModal must execute on the OS main thread. The
        // subprocess's whole job is to be that thread; if we ever get here
        // off-thread the caller invoked us wrong.
        let Some(mtm) = MainThreadMarker::new() else {
            return Outcome::Cancelled;
        };

        let app = NSApplication::sharedApplication(mtm);
        // Accessory: appears in the process list so the alert can come
        // forward, but does not add a Dock icon for the brief subprocess
        // lifetime.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let alert = NSAlert::new(mtm);
        let title = NSString::from_str("Harness Hat — network approval");
        let body = NSString::from_str(&format_body(req));
        let allow_label = NSString::from_str("Allow");
        let deny_label = NSString::from_str("Deny");
        let remember_label = NSString::from_str("Remember this decision");

        alert.setMessageText(&title);
        alert.setInformativeText(&body);
        // First button added becomes the default (return key, right
        // side). We add Allow first to match the rfd preview shown to
        // the user; revisit if we decide "safer default = Deny" later.
        let _ = alert.addButtonWithTitle(&allow_label);
        let _ = alert.addButtonWithTitle(&deny_label);
        alert.setShowsSuppressionButton(true);
        if let Some(supp) = alert.suppressionButton() {
            supp.setTitle(&remember_label);
        }

        // Bring the subprocess forward so the alert isn't buried behind the
        // terminal that spawned it. `activate` (Sonoma+) is preferred; the
        // older `activateIgnoringOtherApps` still works back to 10.5.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let response: isize = alert.runModal();
        let remember = alert
            .suppressionButton()
            .map(|b| b.state() == NSControlStateValueOn)
            .unwrap_or(false);

        match response {
            NS_ALERT_FIRST_BUTTON_RETURN => Outcome::Allow { remember },
            NS_ALERT_SECOND_BUTTON_RETURN => Outcome::Deny { remember },
            _ => Outcome::Cancelled,
        }
    })
}

fn format_body(req: &ApprovalRequest) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "Method: {}\n",
        if req.method.is_empty() {
            "-"
        } else {
            &req.method
        }
    ));
    body.push_str(&format!("Host: {}\n", req.host));
    body.push_str(&format!(
        "Path: {}\n",
        if req.path.is_empty() { "-" } else { &req.path }
    ));
    if let Some(port) = req.port {
        body.push_str(&format!("Port: {port}\n"));
    }
    if let Some(ws) = &req.workspace {
        body.push_str(&format!("Workspace: {ws}\n"));
    }
    body
}

pub fn prompt_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    autoreleasepool(|_| {
        let Some(mtm) = MainThreadMarker::new() else {
            return Outcome::Cancelled;
        };

        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let alert = NSAlert::new(mtm);
        let title = NSString::from_str("Harness Hat — host command approval");
        let body = NSString::from_str(&format_hostdo_body(req));
        let allow_label = NSString::from_str("Allow");
        let deny_label = NSString::from_str("Deny");
        let remember_label = NSString::from_str("Remember this decision");

        alert.setMessageText(&title);
        alert.setInformativeText(&body);
        let _ = alert.addButtonWithTitle(&allow_label);
        let _ = alert.addButtonWithTitle(&deny_label);
        alert.setShowsSuppressionButton(true);
        if let Some(supp) = alert.suppressionButton() {
            supp.setTitle(&remember_label);
        }

        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let response: isize = alert.runModal();
        let remember = alert
            .suppressionButton()
            .map(|b| b.state() == NSControlStateValueOn)
            .unwrap_or(false);

        match response {
            NS_ALERT_FIRST_BUTTON_RETURN => Outcome::Allow { remember },
            NS_ALERT_SECOND_BUTTON_RETURN => Outcome::Deny { remember },
            _ => Outcome::Cancelled,
        }
    })
}

fn format_hostdo_body(req: &HostdoApprovalRequest) -> String {
    let mut body = String::new();
    body.push_str(&format!("Command: {}\n", req.command));
    if let Some(cwd) = &req.cwd {
        body.push_str(&format!("CWD: {cwd}\n"));
    }
    if let Some(image) = &req.image {
        body.push_str(&format!("Image: {image}\n"));
    }
    if let Some(ws) = &req.workspace {
        body.push_str(&format!("Workspace: {ws}\n"));
    }
    if let Some(timeout_secs) = req.timeout_secs {
        body.push_str(&format!("Timeout: {timeout_secs}\n"));
    }
    body
}
