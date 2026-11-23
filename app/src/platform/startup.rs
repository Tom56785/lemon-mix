//! Starting with Windows: an entry under the user's Run key opens Lemon Mix
//! in the notification area at sign-in. It needs no administrator rights,
//! and Task Manager's Startup tab shows it and can turn it off (which it
//! records under StartupApproved, read here too).
//!
//! Also the one-copy guard: a second start (from the Start menu while the
//! first sits in the notification area, say) shows the running window and
//! exits, rather than fight it for the USB/IP port and control pipe.

use std::path::Path;
use std::ptr::null;
use std::sync::OnceLock;

use cables::win::wide;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteKeyValueW, RegGetValueW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

/// The name of the Run entry, as Task Manager lists it.
const ENTRY: &str = "Lemon Mix";

/// Where the entry lives: the real keys, or a test's own.
#[derive(Clone, Copy)]
struct Keys<'a> {
    run: &'a str,
    approved: &'a str,
}

const KEYS: Keys<'static> = Keys {
    run: r"Software\Microsoft\Windows\CurrentVersion\Run",
    approved: r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupState {
    Off,
    On,
    /// The entry is there, but turned off in Task Manager.
    DisabledInTaskManager,
}

/// Whether Lemon Mix starts with Windows.
pub fn state() -> StartupState {
    state_in(KEYS)
}

/// Turns starting with Windows on or off. Turning it on also clears Task
/// Manager's turning it off: asking here is the newer choice.
pub fn set(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Could not find Lemon Mix's own path: {e}"))?;
    set_in(KEYS, on.then(|| command_line(&exe)).as_deref())
}

/// Points an existing entry at this copy of Lemon Mix, which may have moved
/// (a new release unpacked elsewhere, say). Returns whether it changed it.
pub fn refresh_path() -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    refresh_in(KEYS, &command_line(&exe))
}

/// The command the entry runs: this executable, in the notification area.
fn command_line(exe: &Path) -> String {
    format!("\"{}\" --minimized", exe.display())
}

fn state_in(keys: Keys) -> StartupState {
    if read_string(keys.run, ENTRY).is_none() {
        return StartupState::Off;
    }
    match read_binary(keys.approved, ENTRY) {
        Some(flags) if !approved(&flags) => StartupState::DisabledInTaskManager,
        _ => StartupState::On,
    }
}

fn set_in(keys: Keys, command: Option<&str>) -> Result<(), String> {
    delete_value(keys.approved, ENTRY)?;
    match command {
        Some(command) => write_string(keys.run, ENTRY, command),
        None => delete_value(keys.run, ENTRY),
    }
}

fn refresh_in(keys: Keys, command: &str) -> Result<bool, String> {
    match read_string(keys.run, ENTRY) {
        Some(current) if current != command => write_string(keys.run, ENTRY, command).map(|()| true),
        _ => Ok(false),
    }
}

/// Task Manager's record of an entry: the first byte is even while it may
/// run (2, or 6), odd once turned off (3, followed by when).
fn approved(flags: &[u8]) -> bool {
    flags.first().is_none_or(|flags| flags & 1 == 0)
}

fn read_string(key: &str, name: &str) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    let (key, name) = (wide(key), wide(name));
    // SAFETY: the names are NUL-terminated and the buffer holds `bytes`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = (bytes as usize / 2).min(buffer.len());
    let text = &buffer[..len];
    let end = text.iter().position(|&c| c == 0).unwrap_or(len);
    Some(String::from_utf16_lossy(&text[..end]))
}

fn read_binary(key: &str, name: &str) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 64];
    let mut bytes = buffer.len() as u32;
    let (key, name) = (wide(key), wide(name));
    // SAFETY: as in read_string.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    (status == ERROR_SUCCESS).then(|| buffer[..(bytes as usize).min(buffer.len())].to_vec())
}

fn write_string(key: &str, name: &str, value: &str) -> Result<(), String> {
    let wide_key = wide(key);
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: opens (creating if need be) a key under the user's hive.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide_key.as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut handle,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("Could not open the registry key {key} (error {status})."));
    }
    let data = wide(value);
    let (name, size) = (wide(name), (data.len() * 2) as u32);
    // SAFETY: writes the NUL-terminated string to the key opened above, then
    // closes it.
    let status = unsafe {
        let status = RegSetValueExW(handle, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), size);
        RegCloseKey(handle);
        status
    };
    if status != ERROR_SUCCESS {
        return Err(format!("Could not write the registry value under {key} (error {status})."));
    }
    Ok(())
}

/// Deletes a value; one already gone is fine.
fn delete_value(key: &str, name: &str) -> Result<(), String> {
    let (wide_key, name) = (wide(key), wide(name));
    // SAFETY: the names are NUL-terminated.
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide_key.as_ptr(), name.as_ptr()) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("Could not delete the registry value under {key} (error {status})."))
    }
}

// --- One copy at a time ---

const INSTANCE_MUTEX: &str = r"Local\LemonMix.Window";
const SHOW_EVENT: &str = r"Local\LemonMix.Show";

/// The event a second start sets to show this copy's window.
struct ShowEvent(HANDLE);
// SAFETY: an event handle may be waited on from any thread.
unsafe impl Send for ShowEvent {}
unsafe impl Sync for ShowEvent {}

static SHOW: OnceLock<ShowEvent> = OnceLock::new();

/// Claims the window for this copy, for the life of the process. False when
/// another copy has it.
pub fn claim_window() -> bool {
    let name = wide(INSTANCE_MUTEX);
    // SAFETY: creates or opens a named mutex. Its handle is kept open for
    // the life of the process, which is what holds the claim.
    let (mutex, error) = unsafe {
        let mutex = CreateMutexW(null(), 0, name.as_ptr());
        (mutex, GetLastError())
    };
    // An elevated copy's mutex cannot even be opened: it is running too.
    if mutex.is_null() {
        return error != ERROR_ACCESS_DENIED;
    }
    if error == ERROR_ALREADY_EXISTS {
        // SAFETY: closes the handle opened above.
        unsafe { CloseHandle(mutex) };
        return false;
    }
    let event_name = wide(SHOW_EVENT);
    // SAFETY: creates an auto-reset event, kept open for the process's life.
    let event = unsafe { CreateEventW(null(), 0, 0, event_name.as_ptr()) };
    if !event.is_null() {
        let _ = SHOW.set(ShowEvent(event));
    }
    true
}

/// Asks the copy that has the window to show it.
pub fn show_running_copy() {
    let name = wide(SHOW_EVENT);
    // SAFETY: opens the running copy's event, sets it and closes it. Letting
    // any process take the foreground lets that copy bring its window up
    // over this one, which the user just started.
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if !event.is_null() {
            AllowSetForegroundWindow(ASFW_ANY);
            SetEvent(event);
            CloseHandle(event);
        }
    }
}

/// Calls `show` whenever a second start asks for the window.
pub fn on_show_request(show: fn()) {
    let Some(event) = SHOW.get() else {
        return;
    };
    let event = event.0 as usize;
    let _ = std::thread::Builder::new().name("show requests".into()).spawn(move || {
        loop {
            // SAFETY: waits on the event, which stays open for the process.
            if unsafe { WaitForSingleObject(event as HANDLE, INFINITE) } != 0 {
                return;
            }
            show();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW};

    /// Keys of the test's own under the user's hive, removed afterwards.
    struct TestKeys {
        root: String,
        run: String,
        approved: String,
    }

    impl TestKeys {
        fn new(name: &str) -> TestKeys {
            let root = format!(r"Software\Lemon Mix Tests\{name}-{}", std::process::id());
            TestKeys { run: format!(r"{root}\Run"), approved: format!(r"{root}\Approved"), root }
        }

        fn keys(&self) -> Keys<'_> {
            Keys { run: &self.run, approved: &self.approved }
        }
    }

    impl Drop for TestKeys {
        fn drop(&mut self) {
            let root = wide(&self.root);
            let parent = wide(r"Software\Lemon Mix Tests");
            // SAFETY: removes the test's own key tree, then the tests' parent
            // key, which RegDeleteKeyW leaves while another test is using it.
            unsafe {
                RegDeleteTreeW(HKEY_CURRENT_USER, root.as_ptr());
                RegDeleteKeyW(HKEY_CURRENT_USER, parent.as_ptr());
            }
        }
    }

    fn write_binary(key: &str, name: &str, value: &[u8]) {
        let wide_key = wide(key);
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: test-only, under the test's own key.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide_key.as_ptr(),
                0,
                null(),
                0,
                KEY_SET_VALUE,
                null(),
                &mut handle,
                std::ptr::null_mut(),
            );
            let name = wide(name);
            RegSetValueExW(
                handle,
                name.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::REG_BINARY,
                value.as_ptr(),
                value.len() as u32,
            );
            RegCloseKey(handle);
        }
    }

    #[test]
    fn the_command_starts_this_executable_in_the_notification_area() {
        assert_eq!(
            command_line(Path::new(r"C:\Program Files\Lemon Mix\LemonMix.exe")),
            r#""C:\Program Files\Lemon Mix\LemonMix.exe" --minimized"#
        );
    }

    #[test]
    fn task_manager_marks_entries_turned_off_with_an_odd_first_byte() {
        assert!(approved(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(approved(&[6, 0, 0, 0]));
        assert!(!approved(&[3, 0, 0, 0, 0x10, 0x20, 0x30, 0x40, 0, 0, 0, 0]));
        assert!(approved(&[]));
    }

    #[test]
    fn the_entry_turns_on_and_off_and_follows_task_manager() {
        let test = TestKeys::new("startup");
        let keys = test.keys();
        assert_eq!(state_in(keys), StartupState::Off);

        set_in(keys, Some(r#""C:\a\LemonMix.exe" --minimized"#)).unwrap();
        assert_eq!(state_in(keys), StartupState::On);
        assert_eq!(read_string(keys.run, ENTRY).as_deref(), Some(r#""C:\a\LemonMix.exe" --minimized"#));

        // Turned off in Task Manager; turning it on here clears that.
        write_binary(keys.approved, ENTRY, &[3, 0, 0, 0, 1, 2, 3, 4, 0, 0, 0, 0]);
        assert_eq!(state_in(keys), StartupState::DisabledInTaskManager);
        set_in(keys, Some(r#""C:\a\LemonMix.exe" --minimized"#)).unwrap();
        assert_eq!(state_in(keys), StartupState::On);

        // A moved executable: the entry follows it, and only when it is on.
        assert!(refresh_in(keys, r#""C:\b\LemonMix.exe" --minimized"#).unwrap());
        assert!(!refresh_in(keys, r#""C:\b\LemonMix.exe" --minimized"#).unwrap());
        assert_eq!(read_string(keys.run, ENTRY).as_deref(), Some(r#""C:\b\LemonMix.exe" --minimized"#));

        set_in(keys, None).unwrap();
        assert_eq!(state_in(keys), StartupState::Off);
        assert!(!refresh_in(keys, r#""C:\c\LemonMix.exe" --minimized"#).unwrap(), "off stays off");
        assert_eq!(state_in(keys), StartupState::Off);
        set_in(keys, None).unwrap();
    }
}
