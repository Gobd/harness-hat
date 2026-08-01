//! Windows approval dialogs using the Common Controls task-dialog API.

use super::{ApprovalRequest, HostdoApprovalRequest, Outcome};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_WARNING_ICON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_SIZE_TO_CONTENT, TDN_CREATED, TaskDialogIndirect,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SW_SHOWNORMAL,
    SetForegroundWindow, ShowWindow,
};

const DENY_BUTTON_ID: i32 = 100;
const ALLOW_BUTTON_ID: i32 = 101;

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    let port = req.port.map(|port| format!(":{port}")).unwrap_or_default();
    let workspace = req
        .workspace
        .as_deref()
        .map(|workspace| format!("\nWorkspace: {workspace}"))
        .unwrap_or_default();
    prompt_approval(
        "Harness Hat: Network Approval",
        "Allow this network request?",
        &format!(
            "{} {}{}{}{}",
            req.method, req.host, port, req.path, workspace
        ),
    )
}

pub fn prompt_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    let mut details = String::new();
    if let Some(reason) = &req.reason {
        details.push_str(&format!("Reason: {reason}\n"));
    }
    details.push_str(&format!("Command: {}", req.command));
    if let Some(workspace) = &req.workspace {
        details.push_str(&format!("\nWorkspace: {workspace}"));
    }
    if let Some(cwd) = &req.cwd {
        details.push_str(&format!("\nWorking directory: {cwd}"));
    }
    if let Some(image) = &req.image {
        details.push_str(&format!("\nImage: {image}"));
    }
    if let Some(timeout_secs) = req.timeout_secs {
        details.push_str(&format!("\nTimeout: {timeout_secs}s"));
    }
    prompt_approval(
        "Harness Hat: Host Command Approval",
        "Allow this host command?",
        &details,
    )
}

fn prompt_approval(title: &str, instruction: &str, content: &str) -> Outcome {
    let title = wide(title);
    let instruction = wide(instruction);
    let content = wide(content);
    let deny_label = wide("Deny");
    let allow_label = wide("Allow");
    let remember_label = wide("Remember this decision");
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: DENY_BUTTON_ID,
            pszButtonText: deny_label.as_ptr(),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ALLOW_BUTTON_ID,
            pszButtonText: allow_label.as_ptr(),
        },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: std::ptr::null_mut(),
        hInstance: std::ptr::null_mut(),
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: 0,
        pszWindowTitle: title.as_ptr(),
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: TD_WARNING_ICON,
        },
        pszMainInstruction: instruction.as_ptr(),
        pszContent: content.as_ptr(),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: DENY_BUTTON_ID,
        cRadioButtons: 0,
        pRadioButtons: std::ptr::null(),
        nDefaultRadioButton: 0,
        pszVerificationText: remember_label.as_ptr(),
        pszExpandedInformation: std::ptr::null(),
        pszExpandedControlText: std::ptr::null(),
        pszCollapsedControlText: std::ptr::null(),
        Anonymous2: Default::default(),
        pszFooter: std::ptr::null(),
        pfCallback: Some(foreground_callback),
        lpCallbackData: 0,
        cxWidth: 0,
    };
    let mut clicked_button = 0;
    let mut verification_checked = 0;
    // SAFETY: every pointer in `config` refers to a live, NUL-terminated UTF-16
    // buffer (or the documented resource icon), and all output pointers remain
    // valid for the duration of this blocking call.
    let result = unsafe {
        TaskDialogIndirect(
            &config,
            &mut clicked_button,
            std::ptr::null_mut(),
            &mut verification_checked,
        )
    };
    if result < 0 {
        return Outcome::Cancelled;
    }
    let remember = verification_checked != 0;
    match clicked_button {
        ALLOW_BUTTON_ID => Outcome::Allow { remember },
        DENY_BUTTON_ID => Outcome::Deny { remember },
        _ => Outcome::Cancelled,
    }
}

unsafe extern "system" fn foreground_callback(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    _wparam: windows_sys::Win32::Foundation::WPARAM,
    _lparam: windows_sys::Win32::Foundation::LPARAM,
    _reference_data: isize,
) -> windows_sys::core::HRESULT {
    if message == TDN_CREATED as u32 {
        // The daemon is a background GUI process, so Windows may create the
        // modal behind the user's terminal. Explicitly show, raise, and
        // activate it as soon as the task-dialog window exists.
        // SAFETY: `hwnd` is supplied by TaskDialogIndirect for this callback.
        unsafe {
            let foreground = GetForegroundWindow();
            let current_thread = GetCurrentThreadId();
            let foreground_thread = if foreground.is_null() {
                0
            } else {
                GetWindowThreadProcessId(foreground, std::ptr::null_mut())
            };
            let attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, 1) != 0;
            ShowWindow(hwnd, SW_SHOWNORMAL);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
            if attached {
                AttachThreadInput(current_thread, foreground_thread, 0);
            }
        }
    }
    0
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::wide;

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(
            wide("Harness Hat"),
            [72, 97, 114, 110, 101, 115, 115, 32, 72, 97, 116, 0]
        );
    }
}
