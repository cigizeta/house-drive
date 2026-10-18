//! Content hashes (BLAKE3) and the size-and-time stamps that decide when a hash is needed.

use std::fs::{Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const FILE_SHARE_READ: u32 = 0x1;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Size and modified time: a file whose stamp is unchanged is not hashed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub modified: i64,
}

impl Stamp {
    pub fn of(metadata: &Metadata) -> Stamp {
        let modified = metadata.modified().map(nanos).unwrap_or(0);
        return Stamp { size: metadata.len(), modified };
    }
}

/// Nanoseconds since the Unix epoch, 0 for anything earlier.
pub fn nanos(time: SystemTime) -> i64 {
    return match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i64,
        Err(_) => 0,
    };
}

#[derive(Debug, PartialEq, Eq)]
pub enum Hashed {
    Stable { hash: String, stamp: Stamp },
    /// Held by a writer, or changed while being read: left for a later run.
    Busy,
}

/// Hashes a file only if no one is writing it.
///
/// Opening a file locks it only as far as its opener chose. Opening it here sharing reading only
/// fails while anyone holds it open for writing, and keeps writers (and a rename over it) out
/// until the hash is done. Programs that hold no handle between saves are caught by comparing
/// the stamp before and after.
pub fn hash_stable(path: &Path) -> io::Result<Hashed> {
    let file = match OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path) {
        Ok(file) => file,
        Err(error) if is_busy(&error) => return Ok(Hashed::Busy),
        Err(error) => return Err(error),
    };
    let before = Stamp::of(&file.metadata()?);
    let hash = hash_reader(&file)?;
    let after = Stamp::of(&file.metadata()?);
    if before != after {
        return Ok(Hashed::Busy);
    }
    return Ok(Hashed::Stable { hash, stamp: after });
}

pub fn hash_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    return Ok(hasher.finalize().to_hex().to_string());
}

fn is_busy(error: &io::Error) -> bool {
    return matches!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_nothing_is_blake3s() {
        assert_eq!(
            hash_reader(&b""[..]).unwrap(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
