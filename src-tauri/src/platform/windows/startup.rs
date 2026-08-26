use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
            RegOpenKeyExW, RegSetValueExW,
        },
    },
    core::PCWSTR,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "ClipboardAssistant";

pub fn set_start_at_sign_in(enabled: bool, executable: &Path) -> Result<(), String> {
    let key_path = wide(RUN_KEY);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    check(opened, "open Windows startup settings")?;
    let result = if enabled {
        let value_name = wide(VALUE_NAME);
        let command = wide(format!("\"{}\" --autostart", executable.display()));
        let bytes =
            unsafe { std::slice::from_raw_parts(command.as_ptr().cast::<u8>(), command.len() * 2) };
        unsafe { RegSetValueExW(key, PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(bytes)) }
    } else {
        let value_name = wide(VALUE_NAME);
        let deleted = unsafe { RegDeleteValueW(key, PCWSTR(value_name.as_ptr())) };
        if deleted == ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            deleted
        }
    };
    let _ = unsafe { RegCloseKey(key) };
    check(result, "update Windows startup settings")
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn check(result: WIN32_ERROR, operation: &str) -> Result<(), String> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "{operation} failed with Windows error {}",
            result.0
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::wide;

    #[test]
    fn registry_strings_are_null_terminated() {
        assert_eq!(wide("abc"), [97, 98, 99, 0]);
    }
}
