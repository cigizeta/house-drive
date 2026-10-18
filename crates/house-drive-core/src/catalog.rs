//! The store's catalog, on the store machine only (`%LOCALAPPDATA%\house-drive\catalog.sqlite`),
//! written by it alone, and its snapshot `catalog.tsv` on the store for the other machines.
//!
//! A file is in the catalog only once the store machine has hashed its copy in `files/` and found
//! the hash its machine sent: being listed means being verified.
//!
//! Snapshot lines, then `end<TAB><count>` as in journals:
//!
//! ```text
//! file      <path>  <size>     <blake3>  <modified>
//! conflict  <copy>  <original> <machine>
//! refused   <path>  <machine>  <base>          a deletion refused: the file changed meanwhile
//! rejected  <machine> <path>   <blake3>        a pushed file whose hash did not match: push again
//! end       <count>
//! ```

use std::fmt::{self, Write};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::config;
use crate::hash;
use crate::paths::{PathError, RelativePath};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE files (
    path TEXT PRIMARY KEY COLLATE NOCASE,
    size INTEGER NOT NULL,
    hash TEXT NOT NULL,
    modified INTEGER NOT NULL,
    machine TEXT NOT NULL
) STRICT;
CREATE TABLE conflicts (
    copy TEXT PRIMARY KEY COLLATE NOCASE,
    original TEXT NOT NULL,
    machine TEXT NOT NULL
) STRICT;
CREATE TABLE refused (
    path TEXT NOT NULL COLLATE NOCASE,
    machine TEXT NOT NULL,
    base TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (path, machine)
) STRICT;
CREATE TABLE rejected (
    machine TEXT NOT NULL,
    path TEXT NOT NULL COLLATE NOCASE,
    hash TEXT NOT NULL,
    PRIMARY KEY (machine, path)
) STRICT;
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogFile {
    pub path: RelativePath,
    pub size: u64,
    pub hash: String,
    pub modified: i64,
    /// The machine whose push placed this version.
    pub machine: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub copy: RelativePath,
    pub original: RelativePath,
    pub machine: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: RelativePath,
    pub machine: String,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub machine: String,
    pub path: RelativePath,
    pub hash: String,
}

/// What the other machines read: the whole catalog at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub files: Vec<CatalogFile>,
    pub conflicts: Vec<Conflict>,
    pub refused: Vec<Refused>,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug)]
pub enum CatalogError {
    Sqlite(rusqlite::Error),
    BadPath(PathError),
    SchemaVersion(i64),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            CatalogError::Sqlite(error) => write!(f, "catalog: {error}"),
            CatalogError::BadPath(error) => write!(f, "catalog: {error}"),
            CatalogError::SchemaVersion(version) => {
                write!(f, "catalog: schema version {version}, expected {SCHEMA_VERSION}")
            }
        };
    }
}

impl std::error::Error for CatalogError {}

impl From<rusqlite::Error> for CatalogError {
    fn from(error: rusqlite::Error) -> CatalogError {
        return CatalogError::Sqlite(error);
    }
}

impl From<PathError> for CatalogError {
    fn from(error: PathError) -> CatalogError {
        return CatalogError::BadPath(error);
    }
}

pub struct Catalog {
    connection: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Catalog, CatalogError> {
        return Catalog::prepare(Connection::open(path)?);
    }

    pub fn open_in_memory() -> Result<Catalog, CatalogError> {
        return Catalog::prepare(Connection::open_in_memory()?);
    }

    fn prepare(connection: Connection) -> Result<Catalog, CatalogError> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            return Err(CatalogError::SchemaVersion(version));
        }
        return Ok(Catalog { connection });
    }

    pub fn transaction(&mut self) -> Result<CatalogTransaction<'_>, CatalogError> {
        return Ok(CatalogTransaction { transaction: self.connection.transaction()? });
    }

    pub fn get(&self, path: &RelativePath) -> Result<Option<CatalogFile>, CatalogError> {
        return get_file(&self.connection, path);
    }

    pub fn snapshot(&self) -> Result<Snapshot, CatalogError> {
        let connection = &self.connection;
        let mut snapshot = Snapshot::default();

        let mut statement =
            connection.prepare("SELECT path, size, hash, modified, machine FROM files ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            return Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ));
        })?;
        for row in rows {
            let (path, size, hash, modified, machine) = row?;
            snapshot.files.push(CatalogFile {
                path: RelativePath::parse(&path)?,
                size: size as u64,
                hash,
                modified,
                machine,
            });
        }

        let mut statement =
            connection.prepare("SELECT copy, original, machine FROM conflicts ORDER BY copy")?;
        let rows = statement.query_map([], |row| {
            return Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?));
        })?;
        for row in rows {
            let (copy, original, machine) = row?;
            snapshot.conflicts.push(Conflict {
                copy: RelativePath::parse(&copy)?,
                original: RelativePath::parse(&original)?,
                machine,
            });
        }

        let mut statement =
            connection.prepare("SELECT path, machine, base FROM refused ORDER BY path, machine")?;
        let rows = statement.query_map([], |row| {
            return Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?));
        })?;
        for row in rows {
            let (path, machine, base) = row?;
            snapshot.refused.push(Refused { path: RelativePath::parse(&path)?, machine, base });
        }

        let mut statement =
            connection.prepare("SELECT machine, path, hash FROM rejected ORDER BY machine, path")?;
        let rows = statement.query_map([], |row| {
            return Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?));
        })?;
        for row in rows {
            let (machine, path, hash) = row?;
            snapshot.rejected.push(Rejected { machine, path: RelativePath::parse(&path)?, hash });
        }
        return Ok(snapshot);
    }
}

fn get_file(connection: &Connection, path: &RelativePath) -> Result<Option<CatalogFile>, CatalogError> {
    let row = connection
        .query_row(
            "SELECT path, size, hash, modified, machine FROM files WHERE path = ?1",
            params![path.as_str()],
            |row| {
                return Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ));
            },
        )
        .optional()?;
    let Some((path, size, hash, modified, machine)) = row else {
        return Ok(None);
    };
    return Ok(Some(CatalogFile { path: RelativePath::parse(&path)?, size: size as u64, hash, modified, machine }));
}

/// Changes to the catalog, committed together or not at all.
pub struct CatalogTransaction<'a> {
    transaction: Transaction<'a>,
}

impl CatalogTransaction<'_> {
    pub fn get(&self, path: &RelativePath) -> Result<Option<CatalogFile>, CatalogError> {
        return get_file(&self.transaction, path);
    }

    pub fn put_file(&self, file: &CatalogFile) -> Result<(), CatalogError> {
        self.transaction.execute(
            "INSERT OR REPLACE INTO files (path, size, hash, modified, machine)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file.path.as_str(), file.size as i64, file.hash, file.modified, file.machine],
        )?;
        return Ok(());
    }

    pub fn remove_file(&self, path: &RelativePath) -> Result<(), CatalogError> {
        self.transaction.execute("DELETE FROM files WHERE path = ?1", params![path.as_str()])?;
        return Ok(());
    }

    pub fn add_conflict(&self, conflict: &Conflict) -> Result<(), CatalogError> {
        self.transaction.execute(
            "INSERT OR REPLACE INTO conflicts (copy, original, machine) VALUES (?1, ?2, ?3)",
            params![conflict.copy.as_str(), conflict.original.as_str(), conflict.machine],
        )?;
        return Ok(());
    }

    pub fn add_refused(&self, refused: &Refused, at: i64) -> Result<(), CatalogError> {
        self.transaction.execute(
            "INSERT OR REPLACE INTO refused (path, machine, base, at) VALUES (?1, ?2, ?3, ?4)",
            params![refused.path.as_str(), refused.machine, refused.base, at],
        )?;
        return Ok(());
    }

    pub fn add_rejected(&self, rejected: &Rejected) -> Result<(), CatalogError> {
        self.transaction.execute(
            "INSERT OR REPLACE INTO rejected (machine, path, hash) VALUES (?1, ?2, ?3)",
            params![rejected.machine, rejected.path.as_str(), rejected.hash],
        )?;
        return Ok(());
    }

    /// A later operation of the same machine on the same path settles earlier reports.
    pub fn clear_reports(&self, machine: &str, path: &RelativePath) -> Result<(), CatalogError> {
        self.transaction.execute(
            "DELETE FROM rejected WHERE machine = ?1 AND path = ?2",
            params![machine, path.as_str()],
        )?;
        self.transaction.execute(
            "DELETE FROM refused WHERE machine = ?1 AND path = ?2",
            params![machine, path.as_str()],
        )?;
        return Ok(());
    }

    /// Forgets conflicts whose copy is gone (resolved) and refusals older than `before`.
    pub fn prune(&self, before: i64) -> Result<(), CatalogError> {
        self.transaction.execute(
            "DELETE FROM conflicts WHERE copy NOT IN (SELECT path FROM files)",
            [],
        )?;
        self.transaction.execute("DELETE FROM refused WHERE at < ?1", params![before])?;
        return Ok(());
    }

    pub fn commit(self) -> Result<(), CatalogError> {
        self.transaction.commit()?;
        return Ok(());
    }
}

pub fn format_snapshot(snapshot: &Snapshot) -> String {
    let mut text = String::new();
    let mut count = 0;
    for file in &snapshot.files {
        let _ = writeln!(
            text,
            "file\t{}\t{}\t{}\t{}\t{}",
            file.path, file.size, file.hash, file.modified, file.machine
        );
        count += 1;
    }
    for conflict in &snapshot.conflicts {
        let _ = writeln!(text, "conflict\t{}\t{}\t{}", conflict.copy, conflict.original, conflict.machine);
        count += 1;
    }
    for refused in &snapshot.refused {
        let _ = writeln!(text, "refused\t{}\t{}\t{}", refused.path, refused.machine, refused.base);
        count += 1;
    }
    for rejected in &snapshot.rejected {
        let _ = writeln!(text, "rejected\t{}\t{}\t{}", rejected.machine, rejected.path, rejected.hash);
        count += 1;
    }
    let _ = writeln!(text, "end\t{count}");
    return text;
}

/// `Ok(None)` for a snapshot cut short; an error message for anything malformed.
pub fn parse_snapshot(text: &str) -> Result<Option<Snapshot>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(count) = lines.last().and_then(|last| last.strip_prefix("end\t")) else {
        return Ok(None);
    };
    let count: usize = count.parse().map_err(|_| "snapshot: bad count".to_string())?;
    if count != lines.len() - 1 {
        return Err("snapshot: count does not match the lines".to_string());
    }
    let mut snapshot = Snapshot::default();
    for (index, line) in lines[..count].iter().enumerate() {
        let bad = |what: &str| {
            return format!("snapshot line {}: {what}", index + 1);
        };
        let path = |text: &str| {
            return RelativePath::parse(text).map_err(|error| bad(&error.to_string()));
        };
        let machine = |text: &str| {
            if config::is_valid_machine_id(text) {
                return Ok(text.to_string());
            }
            return Err(bad("bad machine ID"));
        };
        let hash_field = |text: &str| {
            if hash::is_hash(text) {
                return Ok(text.to_string());
            }
            return Err(bad("bad hash"));
        };
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            &["file", p, size, h, modified, m] => snapshot.files.push(CatalogFile {
                path: path(p)?,
                size: size.parse().map_err(|_| bad("bad size"))?,
                hash: hash_field(h)?,
                modified: modified.parse().map_err(|_| bad("bad modified time"))?,
                machine: machine(m)?,
            }),
            &["conflict", copy, original, m] => snapshot.conflicts.push(Conflict {
                copy: path(copy)?,
                original: path(original)?,
                machine: machine(m)?,
            }),
            &["refused", p, m, base] => snapshot.refused.push(Refused {
                path: path(p)?,
                machine: machine(m)?,
                base: hash_field(base)?,
            }),
            &["rejected", m, p, h] => snapshot.rejected.push(Rejected {
                machine: machine(m)?,
                path: path(p)?,
                hash: hash_field(h)?,
            }),
            _ => return Err(bad("unknown line")),
        }
    }
    return Ok(Some(snapshot));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> String {
        return c.to_string().repeat(64);
    }

    fn path(text: &str) -> RelativePath {
        return RelativePath::parse(text).unwrap();
    }

    #[test]
    fn snapshot_round_trips() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        let transaction = catalog.transaction().unwrap();
        let file = CatalogFile { path: path("a/b.txt"), size: 3, hash: h('a'), modified: 5, machine: "machine-a".into() };
        transaction.put_file(&file).unwrap();
        transaction
            .add_conflict(&Conflict { copy: path("a/b (conflict).txt"), original: path("a/b.txt"), machine: "machine-b".into() })
            .unwrap();
        transaction.put_file(&CatalogFile { path: path("a/b (conflict).txt"), ..file.clone() }).unwrap();
        transaction.add_refused(&Refused { path: path("c"), machine: "machine-b".into(), base: h('c') }, 1).unwrap();
        transaction.add_rejected(&Rejected { machine: "machine-a".into(), path: path("d"), hash: h('d') }).unwrap();
        transaction.commit().unwrap();

        let snapshot = catalog.snapshot().unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(parse_snapshot(&format_snapshot(&snapshot)).unwrap(), Some(snapshot));
    }

    #[test]
    fn a_snapshot_cut_short_is_incomplete() {
        let text = format_snapshot(&Snapshot::default());
        assert_eq!(parse_snapshot(&text).unwrap(), Some(Snapshot::default()));
        assert_eq!(parse_snapshot("file\ta\t1"), Ok(None));
    }

    #[test]
    fn prune_forgets_resolved_conflicts_and_old_refusals() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        let transaction = catalog.transaction().unwrap();
        transaction
            .add_conflict(&Conflict { copy: path("gone"), original: path("a"), machine: "machine-b".into() })
            .unwrap();
        transaction.add_refused(&Refused { path: path("old"), machine: "machine-b".into(), base: h('a') }, 1).unwrap();
        transaction.add_refused(&Refused { path: path("new"), machine: "machine-b".into(), base: h('a') }, 9).unwrap();
        transaction.prune(5).unwrap();
        transaction.commit().unwrap();
        let snapshot = catalog.snapshot().unwrap();
        assert!(snapshot.conflicts.is_empty());
        assert_eq!(snapshot.refused.len(), 1);
        assert_eq!(snapshot.refused[0].path, path("new"));
    }
}
