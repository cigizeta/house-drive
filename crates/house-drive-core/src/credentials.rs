//! The share's account, kept in Windows Credential Manager (a generic credential named
//! `house-drive`, this user only), never in a file.

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

pub const TARGET: &str = "house-drive";

pub struct Credentials {
    pub user: String,
    pub password: String,
}

fn wide(text: &str) -> Vec<u16> {
    return text.encode_utf16().chain(std::iter::once(0)).collect();
}

/// The stored credentials, `None` if none are stored.
pub fn read() -> Result<Option<Credentials>, String> {
    let target = wide(TARGET);
    let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `target` is NUL-terminated and alive for the call; on success `credential` points to
    // a block Windows allocated, read below and freed with CredFree.
    let result = unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut credential) };
    if let Err(error) = result {
        if error.code() == ERROR_NOT_FOUND.to_hresult() {
            return Ok(None);
        }
        return Err(format!("Credential Manager: {error}"));
    }
    // SAFETY: `credential` is valid until CredFree; the user name is NUL-terminated or null, and
    // the blob holds `CredentialBlobSize` bytes or is null when empty.
    let (user, password) = unsafe {
        let stored = &*credential;
        let user = if stored.UserName.is_null() { Ok(String::new()) } else { stored.UserName.to_string() };
        let units: Vec<u16> = if stored.CredentialBlob.is_null() || stored.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(stored.CredentialBlob, stored.CredentialBlobSize as usize)
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect()
        };
        let password = String::from_utf16(&units);
        CredFree(credential as *const _);
        (user, password)
    };
    let user = user.map_err(|_| "Credential Manager: the user name is not valid text".to_string())?;
    let password = password.map_err(|_| "Credential Manager: the password is not valid text".to_string())?;
    return Ok(Some(Credentials { user, password }));
}

/// Stores (or replaces) the credentials, for this user on this machine.
pub fn write(credentials: &Credentials) -> Result<(), String> {
    let mut target = wide(TARGET);
    let mut user = wide(&credentials.user);
    let mut blob: Vec<u8> = credentials.password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer in `credential` points into a buffer alive for the call.
    let result = unsafe { CredWriteW(&credential, 0) };
    blob.fill(0);
    return result.map_err(|error| format!("Credential Manager: {error}"));
}

pub fn delete() -> Result<(), String> {
    let target = wide(TARGET);
    // SAFETY: `target` is NUL-terminated and alive for the call.
    let result = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) };
    return match result {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        Err(error) => Err(format!("Credential Manager: {error}")),
    };
}
