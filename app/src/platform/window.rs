//! The main window's handle, for dialogs the other modules show on its
//! behalf (the administrator prompts in particular).

use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::HWND;

static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

pub fn set_main_window(hwnd: isize) {
    MAIN_WINDOW.store(hwnd, Ordering::SeqCst);
}

/// The main window, or null before it exists (and in the service).
pub fn main_window() -> HWND {
    MAIN_WINDOW.load(Ordering::SeqCst) as HWND
}
