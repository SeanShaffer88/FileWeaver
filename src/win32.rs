//! Tiny Windows FFI surface for the binary shell: parent-console attach
//! (so CLI output is visible from the Windows-subsystem release build) and
//! the single-instance mutex (Contract §3 #12). No `windows` crate is used;
//! the declarations are kept minimal and commented. Everything is a no-op
//! on other platforms so the shell still builds during development.

/// Name of the single-instance mutex. The GUID is FileWeaver's own,
/// generated for this purpose; it must never change between releases or
/// two builds could run side by side against one ledger.
pub const SINGLE_INSTANCE_MUTEX: &str = "Global\\FileWeaver-{9E1E458F-915B-4210-854B-9FE1AEF5E8AB}";

/// Attaches the process to the console of its parent process (the terminal
/// it was launched from), when one exists. Without this, output from the
/// Windows-subsystem release build would have nowhere to go.
pub fn attach_parent_console() {
    #[cfg(windows)]
    imp::attach_parent_console();
}

/// RAII guard holding the single-instance mutex. Dropping it closes the
/// handle, which releases the name for the next instance.
#[cfg(windows)]
pub struct InstanceGuard {
    handle: *mut std::ffi::c_void,
}

/// RAII guard holding the single-instance mutex (no-op off Windows).
#[cfg(not(windows))]
pub struct InstanceGuard;

/// Tries to claim the single-instance mutex. Returns `None` when another
/// FileWeaver process already holds it.
pub fn acquire_single_instance() -> Option<InstanceGuard> {
    imp::acquire_single_instance(SINGLE_INSTANCE_MUTEX)
}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle came from CreateMutexW in
            // `acquire_single_instance` and is closed exactly once, here.
            unsafe {
                imp::close_handle(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

#[cfg(not(windows))]
impl Drop for InstanceGuard {
    fn drop(&mut self) {}
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use super::InstanceGuard;

    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const ERROR_ALREADY_EXISTS: u32 = 183;

    // SAFETY for the whole block: these are the documented kernel32 entry
    // points with the exact signatures from the Windows SDK; every call
    // site below passes valid arguments as documented per function.
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn CreateMutexW(
            attributes: *mut c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    pub fn attach_parent_console() {
        // SAFETY: ATTACH_PARENT_PROCESS is the documented pseudo-process-id
        // for "the parent process of the current process". Fails harmlessly
        // when there is no parent console.
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }

    pub fn close_handle(handle: *mut c_void) {
        // SAFETY: caller guarantees a live handle from CreateMutexW.
        unsafe {
            CloseHandle(handle);
        }
    }

    pub fn acquire_single_instance(name: &str) -> Option<InstanceGuard> {
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives
        // the call; null attributes and no initial ownership are the
        // documented defaults. GetLastError is read immediately after, as
        // the API requires, before any other Win32 call can overwrite it.
        unsafe {
            let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
            if handle.is_null() {
                // The mutex could not even be created (exotic failure):
                // do not block startup over it.
                return Some(InstanceGuard {
                    handle: std::ptr::null_mut(),
                });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return None;
            }
            Some(InstanceGuard { handle })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::InstanceGuard;

    pub fn acquire_single_instance(_name: &str) -> Option<InstanceGuard> {
        Some(InstanceGuard)
    }
}
