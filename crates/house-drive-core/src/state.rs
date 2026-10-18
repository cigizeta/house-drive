//! This machine's own state, `%LOCALAPPDATA%\house-drive\state.sqlite`, written by this machine
//! alone and never shared over the network (SQLite over SMB is unsafe with several writers).
//!
//! Paths are compared ignoring case, as Windows does (`COLLATE NOCASE`, ASCII only, which NTFS
//! makes sufficient for names that coexist on one disk).

use std::fmt;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::paths::{PathError, RelativePath};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE files (
    path TEXT PRIMARY KEY COLLATE NOCASE,
    size INTEGER NOT NULL,
    modified INTEGER NOT NULL,
    hash TEXT NOT NULL,
    store_hash TEXT,
    pushed_hash TEXT,
    verified INTEGER NOT NULL,
    last_use INTEGER NOT NULL
) STRICT;
CREATE TABLE deletions (
    path TEXT PRIMARY KEY COLLATE NOCASE,
    base TEXT NOT NULL
) STRICT;
";

/// One local file as last scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: RelativePath,
    pub size: u64,
    pub modified: i64,
    pub hash: String,
    /// The store's version this local copy started from (fetched or verified), the `base` of
    /// its next push; `None` for a file the store has never had from this machine's view.
    pub store_hash: Option<String>,
    /// The hash last pushed; the file needs pushing while it differs from `hash`.
    pub pushed_hash: Option<String>,
    /// The store holds a verified copy with this very hash: the local copy may be evicted.
    pub verified: bool,
    /// Nanoseconds since the Unix epoch, for eviction.
    pub last_use: i64,
}

/// A deletion made on this machine, waiting to be pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDeletion {
    pub path: RelativePath,
    /// The store's version the deletion was made against.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Upsert(FileRecord),
    /// Gone locally; `base` set when the store may hold it, so the deletion must be pushed.
    Deleted { path: RelativePath, base: Option<String> },
}

#[derive(Debug)]
pub enum StateError {
    Sqlite(rusqlite::Error),
    BadPath(PathError),
    SchemaVersion(i64),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            StateError::Sqlite(error) => write!(f, "state database: {error}"),
            StateError::BadPath(error) => write!(f, "state database: {error}"),
            StateError::SchemaVersion(version) => {
                write!(f, "state database: schema version {version}, expected {SCHEMA_VERSION}")
            }
        };
    }
}

impl std::error::Error for StateError {}

impl From<rusqlite::Error> for StateError {
    fn from(error: rusqlite::Error) -> StateError {
        return StateError::Sqlite(error);
    }
}

impl From<PathError> for StateError {
    fn from(error: PathError) -> StateError {
        return StateError::BadPath(error);
    }
}

pub struct State {
    connection: Connection,
}

impl State {
    pub fn open(path: &Path) -> Result<State, StateError> {
        return State::prepare(Connection::open(path)?);
    }

    pub fn open_in_memory() -> Result<State, StateError> {
        return State::prepare(Connection::open_in_memory()?);
    }

    fn prepare(connection: Connection) -> Result<State, StateError> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            return Err(StateError::SchemaVersion(version));
        }
        return Ok(State { connection });
    }

    pub fn files(&self) -> Result<Vec<FileRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT path, size, modified, hash, store_hash, pushed_hash, verified, last_use
             FROM files ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| {
            return Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ));
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (path, size, modified, hash, store_hash, pushed_hash, verified, last_use) = row?;
            records.push(FileRecord {
                path: RelativePath::parse(&path)?,
                size: size as u64,
                modified,
                hash,
                store_hash,
                pushed_hash,
                verified: verified != 0,
                last_use,
            });
        }
        return Ok(records);
    }

    pub fn deletions(&self) -> Result<Vec<PendingDeletion>, StateError> {
        let mut statement =
            self.connection.prepare("SELECT path, base FROM deletions ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            return Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
        })?;
        let mut deletions = Vec::new();
        for row in rows {
            let (path, base) = row?;
            deletions.push(PendingDeletion { path: RelativePath::parse(&path)?, base });
        }
        return Ok(deletions);
    }

    /// Applies a scan's changes all at once, or none of them.
    pub fn apply(&mut self, changes: &[Change]) -> Result<(), StateError> {
        let transaction = self.connection.transaction()?;
        for change in changes {
            match change {
                Change::Upsert(record) => {
                    transaction.execute(
                        "INSERT OR REPLACE INTO files
                         (path, size, modified, hash, store_hash, pushed_hash, verified, last_use)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            record.path.as_str(),
                            record.size as i64,
                            record.modified,
                            record.hash,
                            record.store_hash,
                            record.pushed_hash,
                            record.verified as i64,
                            record.last_use,
                        ],
                    )?;
                    transaction.execute(
                        "DELETE FROM deletions WHERE path = ?1",
                        params![record.path.as_str()],
                    )?;
                }
                Change::Deleted { path, base } => {
                    transaction.execute("DELETE FROM files WHERE path = ?1", params![path.as_str()])?;
                    if let Some(base) = base {
                        transaction.execute(
                            "INSERT OR REPLACE INTO deletions (path, base) VALUES (?1, ?2)",
                            params![path.as_str(), base],
                        )?;
                    }
                }
            }
        }
        transaction.commit()?;
        return Ok(());
    }
}

/// The key under which a path is compared, matching `COLLATE NOCASE`.
pub fn key(path: &RelativePath) -> String {
    return path.as_str().to_ascii_lowercase();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, hash: &str) -> FileRecord {
        return FileRecord {
            path: RelativePath::parse(path).unwrap(),
            size: 3,
            modified: 7,
            hash: hash.to_string(),
            store_hash: None,
            pushed_hash: Some("p".to_string()),
            verified: true,
            last_use: 9,
        };
    }

    #[test]
    fn upsert_reads_back_and_ignores_case() {
        let mut state = State::open_in_memory().unwrap();
        state.apply(&[Change::Upsert(record("a/B.txt", "h1"))]).unwrap();
        state.apply(&[Change::Upsert(record("A/b.txt", "h2"))]).unwrap();
        assert_eq!(state.files().unwrap(), vec![record("A/b.txt", "h2")]);
    }

    #[test]
    fn deletion_queued_only_with_a_base() {
        let mut state = State::open_in_memory().unwrap();
        let a = RelativePath::parse("a").unwrap();
        let b = RelativePath::parse("b").unwrap();
        state
            .apply(&[Change::Upsert(record("a", "h")), Change::Upsert(record("b", "h"))])
            .unwrap();
        state
            .apply(&[
                Change::Deleted { path: a.clone(), base: Some("h".to_string()) },
                Change::Deleted { path: b, base: None },
            ])
            .unwrap();
        assert!(state.files().unwrap().is_empty());
        assert_eq!(state.deletions().unwrap(), vec![PendingDeletion { path: a, base: "h".to_string() }]);
    }

    #[test]
    fn a_recreated_file_cancels_its_pending_deletion() {
        let mut state = State::open_in_memory().unwrap();
        let a = RelativePath::parse("a").unwrap();
        state.apply(&[Change::Deleted { path: a, base: Some("h".to_string()) }]).unwrap();
        state.apply(&[Change::Upsert(record("a", "h2"))]).unwrap();
        assert!(state.deletions().unwrap().is_empty());
    }
}
