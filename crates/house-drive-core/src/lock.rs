//! One run at a time on a machine: the scheduled sync and one started by hand must not interleave.
//!
//! The lock is a file held open without sharing. Windows closes it when the process ends, however
//! it ends, so a crash never leaves a stale lock behind.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

const ERROR_SHARING_VIOLATION: i32 = 32;

/// Held for as long as it lives.
pub struct RunLock {
    _file: File,
}

/// The lock, or `None` while another run holds it.
pub fn try_lock(path: &Path) -> io::Result<Option<RunLock>> {
    return match OpenOptions::new().write(true).create(true).truncate(false).share_mode(0).open(path) {
        Ok(file) => Ok(Some(RunLock { _file: file })),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(error) => Err(error),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn a_second_run_is_refused_until_the_first_ends() {
        let dir = TempDir::new("lock");
        let path = dir.0.join("sync.lock");
        let first = try_lock(&path).unwrap();
        assert!(first.is_some());
        assert!(try_lock(&path).unwrap().is_none());
        drop(first);
        assert!(try_lock(&path).unwrap().is_some());
    }
}
