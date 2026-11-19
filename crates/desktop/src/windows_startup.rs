//! Terminal diagnostics for a Windows GUI-subsystem executable.
use std::{io::Write as _, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    },
    UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW},
};

fn available(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

fn ensure_stream(stream: STD_HANDLE) -> bool {
    // Already redirected (e.g. to a terminal's pipe or to a file), or inherited
    // a console handle. Leave captured invocations completely alone.
    // SAFETY: these Win32 calls only query/set this process's standard handles.
    unsafe {
        if available(GetStdHandle(stream)) {
            return true;
        }
        let existing = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id)));
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }
        // Attaching can assign console handles. Keep any user-provided streams,
        // including a redirected stream other than the one we needed to attach.
        for (id, handle) in existing {
            if available(handle) {
                SetStdHandle(id, handle);
            }
        }
        available(GetStdHandle(stream))
    }
}

pub(super) fn ensure_stdout() {
    ensure_stream(STD_OUTPUT_HANDLE);
}

pub(super) fn ensure_stderr() {
    ensure_stream(STD_ERROR_HANDLE);
}

pub(super) fn report_launch_error(error: &anyhow::Error) {
    if ensure_stream(STD_ERROR_HANDLE)
        && writeln!(std::io::stderr().lock(), "Error: {error:?}").is_ok()
    {
        return;
    }
    let message: Vec<_> = format!("Could not start Zevria Desktop.\n\n{error:#}")
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: both strings are NUL-terminated and live through the synchronous call.
    unsafe {
        MessageBoxW(
            null_mut(),
            message.as_ptr(),
            windows_sys::w!("Zevria Desktop"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}
