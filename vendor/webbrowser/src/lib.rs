//! A minimal stand-in for the `webbrowser` crate: only `open`, which is all
//! egui-winit uses. URLs are opened with the shell's default handler.

use std::io;

/// Opens `url` in the default browser. Only http, https and mailto links are
/// opened, as the shell would also run local programs.
pub fn open(url: &str) -> io::Result<()> {
    let lower = url.trim().to_ascii_lowercase();
    if !["http://", "https://", "mailto:"].iter().any(|scheme| lower.starts_with(scheme)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "only web links can be opened"));
    }
    imp::open(url.trim())
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::ptr::null;

    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    pub fn open(url: &str) -> io::Result<()> {
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(url));
        // SAFETY: NUL-terminated strings that outlive the call.
        let result =
            unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) };
        if result as isize > 32 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn open(url: &str) -> io::Result<()> {
        let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(program).arg(url).spawn().map(|_| ())
    }
}
