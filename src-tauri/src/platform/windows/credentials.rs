use std::{ffi::c_void, ptr};

use windows::{
    Win32::{
        Foundation::ERROR_NOT_FOUND,
        Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
            CredReadW, CredWriteW,
        },
    },
    core::{Error, HRESULT, PCWSTR, PWSTR},
};

pub const WEBDAV_CREDENTIAL_TARGET: &str = "ClipboardAssistant.WebDAV";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCredential {
    pub username: String,
    pub password: String,
}

pub fn write_webdav_credential(username: &str, password: &str) -> windows::core::Result<()> {
    let mut target = wide(WEBDAV_CREDENTIAL_TARGET);
    let mut username = wide(username);
    let mut password = password.encode_utf16().collect::<Vec<_>>();
    let password_bytes = password
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(Error::from_win32)?;
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: password_bytes,
        CredentialBlob: password.as_mut_ptr().cast::<u8>(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(username.as_mut_ptr()),
        ..Default::default()
    };
    let result = unsafe { CredWriteW(&credential, 0) };
    password.fill(0);
    result
}

pub fn read_webdav_credential() -> windows::core::Result<Option<StoredCredential>> {
    let target = wide(WEBDAV_CREDENTIAL_TARGET);
    let mut raw = ptr::null_mut::<CREDENTIALW>();
    if let Err(error) =
        unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) }
    {
        if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Ok(None);
        }
        return Err(error);
    }
    let result = (|| {
        let credential = unsafe { raw.as_ref() }.ok_or_else(Error::from_win32)?;
        let username = unsafe { credential.UserName.to_string()? };
        if !(credential.CredentialBlobSize as usize).is_multiple_of(size_of::<u16>()) {
            return Err(Error::from_win32());
        }
        let password_units = credential.CredentialBlobSize as usize / size_of::<u16>();
        let password = if password_units == 0 {
            String::new()
        } else {
            let units = unsafe {
                std::slice::from_raw_parts(credential.CredentialBlob.cast::<u16>(), password_units)
            };
            String::from_utf16(units).map_err(|_| Error::from_win32())?
        };
        Ok(Some(StoredCredential { username, password }))
    })();
    unsafe { CredFree(raw.cast::<c_void>()) };
    result
}

pub fn delete_webdav_credential() -> windows::core::Result<()> {
    let target = wide(WEBDAV_CREDENTIAL_TARGET);
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(()),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(()),
        Err(error) => Err(error),
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::WEBDAV_CREDENTIAL_TARGET;

    #[test]
    fn webdav_credential_uses_a_stable_windows_target() {
        assert_eq!(WEBDAV_CREDENTIAL_TARGET, "ClipboardAssistant.WebDAV");
    }
}
