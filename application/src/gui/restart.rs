//! Update handoff: the replacement waits for this process before opening the GUI.

use std::{io, path::Path, process::Child, time::Duration};

const UPDATE_PARENT_ENV: &str = "GOODSCANNER_UPDATE_PARENT_PID";

pub fn launch_updated_app(exe: &Path) -> io::Result<Child> {
    std::process::Command::new(exe)
        .env(UPDATE_PARENT_ENV, std::process::id().to_string())
        .spawn()
}

pub fn wait_for_update_parent() -> io::Result<()> {
    let Some(parent) = std::env::var_os(UPDATE_PARENT_ENV) else {
        return Ok(());
    };
    std::env::remove_var(UPDATE_PARENT_ENV);
    let pid = parent
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|&pid| pid != 0 && pid != std::process::id())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid update parent PID"))?;
    wait_for_parent_exit(pid, Duration::from_secs(30))
}

pub fn wait_for_parent_exit(pid: u32, timeout: Duration) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        // Open once so PID reuse cannot change which process we wait for.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            // The parent can already have exited before the replacement starts.
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let result = unsafe {
            WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128 - 1) as u32)
        };
        let error = match result {
            WAIT_OBJECT_0 => None,
            WAIT_TIMEOUT => Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "the previous application did not exit; close it before opening the update",
            )),
            _ => Some(io::Error::last_os_error()),
        };
        unsafe { CloseHandle(handle) };
        error.map_or(Ok(()), Err)
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, timeout);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "update handoff requires Windows",
        ))
    }
}

/// Called after application settings are persisted for an update.
pub fn exit_for_update() -> ! {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // ExitProcess runs DLL detach callbacks after stopping other threads.
        // OCR/WGC native threads can leave locks held, deadlocking those callbacks.
        // Terminate only ourselves, after application-level shutdown has completed.
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    }
    std::process::exit(0)
}
