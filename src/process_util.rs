//! Cross-platform subprocess configuration shared by background operations.

/// Prevent a captured/background console application from creating a visible
/// console window when its parent is the Windows GUI-subsystem daemon.
pub(crate) fn hide_console_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    let _ = command;
}

/// Tokio equivalent of [`hide_console_window`].
pub(crate) fn hide_tokio_console_window(command: &mut tokio::process::Command) {
    hide_console_window(command.as_std_mut());
}

/// Terminate all Harness Hat daemon processes in the current Windows session.
///
/// The scheduled task can be missing already (for example after an interrupted
/// uninstall), so task deletion alone is not sufficient. Enumerating by image
/// name also catches daemons started by an older task definition.
#[cfg(windows)]
pub(crate) fn terminate_hht_daemons() -> std::io::Result<()> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
    };

    // SAFETY: the snapshot and process-entry APIs are called with their
    // documented initialized structures and handles are closed on every path.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut current_session_id = 0;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session_id) == 0 {
            let error = std::io::Error::last_os_error();
            CloseHandle(snapshot);
            return Err(error);
        }

        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..std::mem::zeroed()
        };
        let mut result = if Process32FirstW(snapshot, &mut entry) != 0 {
            terminate_matching_process(&entry, current_session_id)
        } else {
            Ok(())
        };
        while Process32NextW(snapshot, &mut entry) != 0 {
            if let Err(error) = terminate_matching_process(&entry, current_session_id) {
                result = Err(error);
            }
        }
        CloseHandle(snapshot);
        return result;
    }

    fn terminate_matching_process(
        entry: &PROCESSENTRY32W,
        current_session_id: u32,
    ) -> std::io::Result<()> {
        let end = entry
            .szExeFile
            .iter()
            .position(|&value| value == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if !name.eq_ignore_ascii_case("hht-daemon.exe") {
            return Ok(());
        }
        let mut process_session_id = 0;
        if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut process_session_id) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if process_session_id != current_session_id {
            return Ok(());
        }
        // SAFETY: the process handle is opened with terminate access, checked
        // for null, and closed after the termination attempt.
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
            if process.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let terminated = TerminateProcess(process, 0) != 0;
            let error = (!terminated).then(std::io::Error::last_os_error);
            CloseHandle(process);
            error.map_or(Ok(()), Err)
        }
    }
}
