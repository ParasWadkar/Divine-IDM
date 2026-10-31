//! Starting with Windows.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, which needs no
//! elevation and belongs to this user alone. Not a scheduled task and not
//! the machine-wide key: either would mean an installer with administrator
//! rights, and either would outlive the user who asked for it.
//!
//! This lives in the application rather than the engine deliberately. The
//! setting is stored where every client can read it, but only the app knows
//! which executable should actually be started — the command line changing
//! the setting would otherwise register `idm.exe`, which has no window and
//! would sit in the user's startup doing nothing visible.

use std::path::Path;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "DivinePulse";

/// Adds or removes this executable from the current user's startup.
pub fn apply(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    platform::write(enabled, &exe)
}

/// What the Run entry would be: the app, told to come up hidden.
///
/// Quoted because the path contains spaces in any normal install, and an
/// unquoted path with a space is read by Windows as a different program
/// followed by arguments.
fn command_for(exe: &Path) -> String {
    format!("\"{}\" --hidden", exe.display())
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn write(enabled: bool, exe: &Path) -> Result<(), String> {
        let path = wide(super::RUN_KEY);
        let name = wide(super::VALUE);

        let mut key = HKEY::default();
        // SAFETY: NUL-terminated wide inputs that outlive the call; `key` is
        // a valid out-pointer.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &raw mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("could not open the startup key: {status:?}"));
        }

        let result = if enabled {
            let command = wide(&super::command_for(exe));
            // Bytes, not characters, and including the NUL: REG_SZ is
            // measured in bytes and a value written without its terminator
            // reads back with whatever follows it in the hive.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    command.as_ptr() as *const u8,
                    command.len() * std::mem::size_of::<u16>(),
                )
            };
            // SAFETY: `key` is open for writing; `bytes` describes the
            // buffer `command` owns for the duration of the call.
            let status =
                unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) };
            if status == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("could not write the startup entry: {status:?}"))
            }
        } else {
            // SAFETY: `key` is open; `name` is NUL-terminated.
            let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
            // Already absent is the state that was asked for.
            if status == ERROR_SUCCESS || status.0 == 2 {
                Ok(())
            } else {
                Err(format!("could not remove the startup entry: {status:?}"))
            }
        };

        // SAFETY: opened above, not used after this.
        unsafe {
            let _ = RegCloseKey(key);
        }
        result
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    pub(super) fn write(_enabled: bool, _exe: &Path) -> Result<(), String> {
        Err("starting with the system is only implemented on Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_quoted_and_starts_hidden() {
        // An unquoted path with a space is read as a different program
        // followed by arguments, which is how a startup entry silently
        // stops working the moment it lives under "Program Files".
        let command = command_for(Path::new(r"C:\Program Files\DivinePulse\divinepulse.exe"));
        assert!(command.starts_with('"'), "{command}");
        assert!(command.contains(r#""C:\Program Files\DivinePulse\divinepulse.exe""#), "{command}");
        assert!(command.ends_with(" --hidden"), "{command}");
    }

    #[cfg(windows)]
    #[test]
    fn enabling_then_disabling_leaves_no_trace() {
        // Writes to the real Run key, under this crate's own value name, and
        // removes it again. A leftover entry would start this test binary at
        // login, so the removal half matters as much as the writing half.
        apply(true).expect("enable");
        assert!(present(), "the entry should exist once enabled");

        apply(false).expect("disable");
        assert!(!present(), "the entry should be gone once disabled");

        // Removing something already absent is the state that was asked for.
        apply(false).expect("disabling twice should not be an error");
    }

    #[cfg(windows)]
    fn present() -> bool {
        let output = std::process::Command::new("reg")
            .args(["query", &format!(r"HKCU\{RUN_KEY}"), "/v", VALUE])
            .output();
        matches!(output, Ok(output) if output.status.success())
    }
}
