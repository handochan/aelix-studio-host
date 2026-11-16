//! Conservative PID probe for legacy disk artifacts that predate file leases.

/// False only when the OS establishes that this positive PID does not exist.
/// Permission errors, unsupported platforms and recycled/live PIDs are retained.
/// This probe never signals or terminates a process.
pub fn process_may_exist(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return true;
    }
    #[cfg(unix)]
    {
        // Signal zero performs a liveness/permission check, without delivery.
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn CloseHandle(handle: *mut c_void) -> i32;
            fn GetLastError() -> u32;
        }
        // PROCESS_QUERY_LIMITED_INFORMATION; access denied means "may exist".
        let handle = unsafe { OpenProcess(0x1000, 0, pid) };
        if handle.is_null() {
            // ERROR_INVALID_PARAMETER is returned for an absent positive PID.
            return unsafe { GetLastError() } != 87;
        }
        unsafe { CloseHandle(handle) };
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_and_unprobeable_pids_are_retained() {
        assert!(process_may_exist(std::process::id()));
        assert!(process_may_exist(0));
        assert!(process_may_exist(u32::MAX));
    }
}
