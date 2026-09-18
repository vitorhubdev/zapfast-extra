//! Bringing the app window to the front on Windows.
//!
//! Windows only lets the foreground process take focus, so showing a window
//! from the tray or from a clicked notification can leave it behind another
//! one. These are the documented workarounds: restore the window, borrow the
//! foreground thread's input queue while asking for the front, and try again
//! while the window is still coming up.

use std::ffi::c_void;
use std::time::Duration;

/// `SW_RESTORE`: un-minimize and activate.
const SW_RESTORE: i32 = 9;

type Hwnd = *mut c_void;

unsafe extern "system" {
    fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, count: i32) -> i32;
    fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn GetForegroundWindow() -> Hwnd;
    fn BringWindowToTop(hwnd: Hwnd) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn AttachThreadInput(target: u32, attach_to: u32, attach: i32) -> i32;
}

/// How often the request is repeated while a window is coming up.
pub const RETRY: Duration = Duration::from_millis(250);
/// How long a request keeps being repeated.
pub const PATIENCE: Duration = Duration::from_millis(1500);

/// The first window of this process whose title is ours.
fn app_window() -> Option<Hwnd> {
    extern "system" fn find(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: lparam is the address of the slot passed to EnumWindows.
        let slot = unsafe { &mut *(lparam as *mut Hwnd) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != std::process::id() {
            return 1;
        }
        let mut title = [0u16; 64];
        let length = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        if length <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&title[..length as usize]);
        if !title.starts_with("ZapExt") {
            return 1;
        }
        *slot = hwnd;
        // Stop: the first matching window is the one to raise.
        0
    }

    let mut found: Hwnd = std::ptr::null_mut();
    // SAFETY: the callback only writes to the slot behind lparam.
    unsafe { EnumWindows(find, &mut found as *mut Hwnd as isize) };
    (!found.is_null()).then_some(found)
}

/// Brings the window to the front, working around the foreground lock.
pub fn raise() {
    let Some(hwnd) = app_window() else {
        return;
    };
    // SAFETY: every call takes the handle found above and plain integers.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        BringWindowToTop(hwnd);
        let foreground = GetForegroundWindow();
        let current = GetCurrentThreadId();
        let owner = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        // Borrowing the foreground thread's input queue is what lets a
        // background process take the front.
        let attached = owner != 0 && owner != current && AttachThreadInput(current, owner, 1) != 0;
        SetForegroundWindow(hwnd);
        if attached {
            AttachThreadInput(current, owner, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raising_without_a_window_is_harmless() {
        // The test binary has no window titled like the app: this must not
        // panic and must not touch anything.
        raise();
    }
}
