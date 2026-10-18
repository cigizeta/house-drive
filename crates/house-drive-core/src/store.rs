//! The store's layout:
//!
//! ```text
//! <store_root>/
//!   store-id.txt                           written once by init-store
//!   files/<path>                           every file's current content
//!   trash/<date>/<path>                    deleted and replaced versions
//!   incoming/<machine_id>/<batch>/<path>   pushed files, created by their machine
//!   journal/<machine_id>/<batch>.tsv       each batch's journal, created once
//!   catalog.tsv                            the store machine's snapshot of the catalog
//! ```
//!
//! Pushing machines only ever create files, in their own `incoming/` and `journal/` folders;
//! everything else is the store machine's.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::hash;

const STORE_ID_FILE: &str = "store-id.txt";
const LAYOUT: [&str; 4] = ["files", "trash", "incoming", "journal"];

#[derive(Debug)]
pub enum StoreError {
    Io { path: PathBuf, source: io::Error },
    AlreadyInitialised(PathBuf),
    /// No store at this path (the share unreachable, or never initialised).
    Missing(PathBuf),
    WrongStore { expected: String, found: String },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            StoreError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            StoreError::AlreadyInitialised(path) => {
                write!(f, "{} already holds a store", path.display())
            }
            StoreError::Missing(path) => write!(f, "no store at {}", path.display()),
            StoreError::WrongStore { expected, found } => {
                write!(f, "wrong store: expected ID {expected}, found {found}; nothing done")
            }
        };
    }
}

impl std::error::Error for StoreError {}

fn io_error(path: &Path) -> impl Fn(io::Error) -> StoreError + '_ {
    return move |source| StoreError::Io { path: path.to_path_buf(), source };
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Creates the layout and a new store ID, which it returns. Refuses a folder that already
    /// holds a store.
    pub fn init(root: &Path) -> Result<String, StoreError> {
        let id_path = root.join(STORE_ID_FILE);
        if id_path.exists() {
            return Err(StoreError::AlreadyInitialised(root.to_path_buf()));
        }
        for folder in LAYOUT {
            let path = root.join(folder);
            fs::create_dir_all(&path).map_err(io_error(&path))?;
        }
        let id = new_id();
        let mut file =
            OpenOptions::new().write(true).create_new(true).open(&id_path).map_err(io_error(&id_path))?;
        file.write_all(format!("{id}\n").as_bytes()).map_err(io_error(&id_path))?;
        file.sync_all().map_err(io_error(&id_path))?;
        return Ok(id);
    }

    /// Opens the store at `root` only if its ID is the expected one: a different store (a
    /// look-alike share, a wrong path) stops everything before anything is sent or deleted.
    pub fn open(root: &Path, expected_id: &str) -> Result<Store, StoreError> {
        let found = read_id(root)?;
        if found != expected_id.trim() {
            return Err(StoreError::WrongStore { expected: expected_id.to_string(), found });
        }
        return Ok(Store { root: root.to_path_buf() });
    }

    pub fn root(&self) -> &Path {
        return &self.root;
    }

    pub fn files_dir(&self) -> PathBuf {
        return self.root.join("files");
    }

    pub fn trash_dir(&self) -> PathBuf {
        return self.root.join("trash");
    }

    pub fn incoming_dir(&self, machine_id: &str) -> PathBuf {
        return self.root.join("incoming").join(machine_id);
    }

    pub fn journal_dir(&self, machine_id: &str) -> PathBuf {
        return self.root.join("journal").join(machine_id);
    }

    /// The newest batch number of a machine, from its `incoming/` folders and its journals.
    pub fn newest_batch(&self, machine_id: &str) -> Result<Option<u128>, StoreError> {
        let mut newest = None;
        for folder in [self.incoming_dir(machine_id), self.journal_dir(machine_id)] {
            let entries = match fs::read_dir(&folder) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(StoreError::Io { path: folder, source: error }),
            };
            for entry in entries {
                let entry = entry.map_err(io_error(&folder))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let stem = name.strip_suffix(".tsv").unwrap_or(&name);
                if let Ok(number) = stem.parse::<u128>() {
                    newest = newest.max(Some(number));
                }
            }
        }
        return Ok(newest);
    }
}

/// The ID of the store at `root`, as `join` records it the first time.
pub fn read_id(root: &Path) -> Result<String, StoreError> {
    let id_path = root.join(STORE_ID_FILE);
    return match fs::read_to_string(&id_path) {
        Ok(text) => Ok(text.trim().to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(StoreError::Missing(root.to_path_buf())),
        Err(error) => Err(StoreError::Io { path: id_path, source: error }),
    };
}

/// Batch names are zero-padded numbers, nanoseconds since the Unix epoch, always above the
/// machine's newest batch even if the clock went back: a batch's order is its name's order.
pub fn next_batch(now: SystemTime, newest: Option<u128>) -> u128 {
    let from_clock = hash::nanos(now) as u128;
    return match newest {
        Some(newest) if newest >= from_clock => newest + 1,
        _ => from_clock,
    };
}

pub fn batch_name(number: u128) -> String {
    return format!("{number:020}");
}

/// A random ID in the usual 8-4-4-4-12 form. It needs to be unique, not secret: it tells this
/// store from any other. The standard library's hash keys come from the operating system's
/// random generator, which spares a dependency.
fn new_id() -> String {
    let mut hasher = blake3::Hasher::new();
    for round in 0u64..4 {
        let mut keyed = std::collections::hash_map::RandomState::new().build_hasher();
        keyed.write_u64(round);
        hasher.update(&keyed.finish().to_le_bytes());
    }
    hasher.update(&hash::nanos(SystemTime::now()).to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    let hex = hasher.finalize().to_hex();
    let hex = hex.as_str();
    return format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::time::Duration;

    #[test]
    fn init_then_open() {
        let root = TempDir::new("store-init");
        let id = Store::init(&root.0).unwrap();
        assert_eq!(id.len(), 36);
        for folder in LAYOUT {
            assert!(root.0.join(folder).is_dir());
        }
        assert!(matches!(Store::init(&root.0), Err(StoreError::AlreadyInitialised(_))));
        assert!(Store::open(&root.0, &id).is_ok());
    }

    #[test]
    fn a_wrong_or_missing_store_is_refused() {
        let root = TempDir::new("store-wrong");
        assert!(matches!(Store::open(&root.0, "x"), Err(StoreError::Missing(_))));
        Store::init(&root.0).unwrap();
        assert!(matches!(Store::open(&root.0, "x"), Err(StoreError::WrongStore { .. })));
    }

    #[test]
    fn ids_differ() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn batches_always_increase() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(next_batch(now, None), 100_000_000_000);
        assert_eq!(next_batch(now, Some(5)), 100_000_000_000);
        assert_eq!(next_batch(now, Some(200_000_000_000)), 200_000_000_001);
        assert_eq!(batch_name(42), "00000000000000000042");
    }

    #[test]
    fn newest_batch_reads_folders_and_journals() {
        let root = TempDir::new("store-newest");
        let id = Store::init(&root.0).unwrap();
        let store = Store::open(&root.0, &id).unwrap();
        assert_eq!(store.newest_batch("machine-a").unwrap(), None);
        fs::create_dir_all(store.incoming_dir("machine-a").join(batch_name(7))).unwrap();
        fs::create_dir_all(store.journal_dir("machine-a")).unwrap();
        fs::write(store.journal_dir("machine-a").join(format!("{}.tsv", batch_name(9))), "").unwrap();
        assert_eq!(store.newest_batch("machine-a").unwrap(), Some(9));
        assert_eq!(store.newest_batch("machine-b").unwrap(), None);
    }
}
