//! Invocation-scoped Windows process ownership. Workloads start suspended and
//! are assigned before their primary thread can create any descendants. The job
//! handle is never inherited. Waiting still observes the shell leader, not the job.
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    },
};

pub fn system_executable(name: &str) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = [0u16; 32768];
    let len = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])).join(name))
}

/// The console delivers Ctrl-C/Break to both launcher and child. Keep the
/// launcher alive to observe the child's status without suppressing the child's
/// handler (unlike SetConsoleCtrlHandler(NULL, TRUE), this is not inherited).
pub struct ForwardConsoleInterrupts;
unsafe extern "system" fn console_handler(event: u32) -> i32 {
    i32::from(event == 0 || event == 1)
}
impl ForwardConsoleInterrupts {
    pub fn install() -> io::Result<Self> {
        if unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_handler), 1)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
}
impl Drop for ForwardConsoleInterrupts {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_handler), 0);
        }
    }
}

#[derive(Debug)]
pub struct Job(OwnedHandle);
impl Job {
    pub fn new() -> io::Result<Self> {
        // SAFETY: null security attributes make a non-inheritable owned handle.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.handle(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
    pub fn kill(&self) {
        unsafe {
            TerminateJobObject(self.handle(), 1);
        }
    }

    /// Only accepts a just-created suspended child from `spawn`. A failed
    /// assignment/resume is fatal; the caller terminates the suspended child.
    fn assign_and_resume(&self, process: HANDLE, pid: u32) -> io::Result<()> {
        if unsafe { AssignProcessToJobObject(self.handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // std/tokio own the process handle but not its primary thread handle.
        // A CREATE_SUSPENDED process has exactly one thread and has run no code.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        Err(io::Error::other(
            "could not locate suspended child primary thread",
        ))
    }
    pub fn spawn(
        command: &mut tokio::process::Command,
    ) -> io::Result<(tokio::process::Child, Self)> {
        let job = Self::new()?;
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        let mut child = command.spawn()?;
        let result = job.assign_and_resume(
            child.raw_handle().expect("live child"),
            child.id().expect("live child"),
        );
        if let Err(error) = result {
            job.kill();
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((child, job))
    }
}
