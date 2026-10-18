//! Connecting to the store's share with the dedicated account.

use std::fmt;
use std::path::Path;

use windows::Win32::NetworkManagement::WNet::{NET_CONNECT_FLAGS, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W};
use windows::core::{PCWSTR, PWSTR};

use crate::credentials::Credentials;

#[derive(Debug)]
pub enum ConnectError {
    /// `store_root` is not a `\\server\share` path.
    NotAShare(String),
    /// The store machine is off or out of reach: an ordinary state, nothing to fix.
    Unreachable(u32),
    /// Reached but refused (wrong password, a conflicting connection, ...).
    Refused { code: u32, message: String },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            ConnectError::NotAShare(path) => write!(f, "{path} is not a \\\\server\\share path"),
            ConnectError::Unreachable(code) => write!(f, "the store is unreachable (error {code})"),
            ConnectError::Refused { code, message } => write!(f, "the share refused the connection: {message} (error {code})"),
        };
    }
}

impl std::error::Error for ConnectError {}

/// `\\server\share` of a path on a share, which is what Windows connects to.
pub fn share_root(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let rest = text.strip_prefix(r"\\")?;
    let mut parts = rest.split('\\');
    let server = parts.next().filter(|part| !part.is_empty() && *part != "?" && *part != ".")?;
    let share = parts.next().filter(|part| !part.is_empty())?;
    return Some(format!(r"\\{server}\{share}"));
}

/// Connects this user's session to the share (no drive letter). Already connected with the same
/// account is success.
pub fn connect(store_root: &Path, credentials: &Credentials) -> Result<(), ConnectError> {
    let Some(root) = share_root(store_root) else {
        return Err(ConnectError::NotAShare(store_root.display().to_string()));
    };
    let wide = |text: &str| {
        return text.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    };
    let mut remote = wide(&root);
    let user = wide(&credentials.user);
    let mut password = wide(&credentials.password);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(remote.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer points into a NUL-terminated buffer alive for the call.
    let code = unsafe {
        WNetAddConnection2W(&resource, PCWSTR(password.as_ptr()), PCWSTR(user.as_ptr()), NET_CONNECT_FLAGS(0))
    }
    .0;
    password.fill(0);
    return match code {
        0 => Ok(()),
        // Bad network path, network gone, bad network name, timeout, no network, unreachable.
        53 | 64 | 67 | 121 | 1222 | 1231 => Err(ConnectError::Unreachable(code)),
        1219 => Err(ConnectError::Refused {
            code,
            message: "this user is already connected to the store machine with another account; \
                      disconnect it (net use) and try again"
                .to_string(),
        }),
        86 | 1326 => Err(ConnectError::Refused {
            code,
            message: "wrong user name or password; store them again with set-credentials".to_string(),
        }),
        _ => Err(ConnectError::Refused { code, message: "see the error code".to_string() }),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_root_of_paths() {
        assert_eq!(share_root(Path::new(r"\\store-machine\house-drive")).as_deref(), Some(r"\\store-machine\house-drive"));
        assert_eq!(share_root(Path::new(r"\\store-machine\house-drive\sub")).as_deref(), Some(r"\\store-machine\house-drive"));
        assert_eq!(share_root(Path::new(r"\\store-machine")), None);
        assert_eq!(share_root(Path::new(r"\\?\C:\x")), None);
        assert_eq!(share_root(Path::new(r"C:\example")), None);
    }
}
