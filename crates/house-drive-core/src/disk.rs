//! Disk space, from Windows itself: one call, declared here rather than pulling in a crate.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        directory: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> i32;
}

/// Bytes free to this user on the disk holding `path` (a local folder or a share).
pub fn free_space(path: &Path) -> io::Result<u64> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the call; the out pointers are
    // either valid or null, as the function allows.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    return Ok(free);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temporary_folder_has_some_free_space() {
        assert!(free_space(&std::env::temp_dir()).unwrap() > 0);
        assert!(free_space(Path::new(r"C:\no\such\folder\anywhere")).is_err());
    }
}
