//! Scanning the data root against the state: new, changed and deleted files.
//!
//! A scan never pushes or deletes anything: it only records what it found, and its changes are
//! applied to the state in one transaction at the end, so an interrupted scan changes nothing. A
//! missing root or an unreadable folder stops it, since treating unseen files as deleted would
//! queue their deletion everywhere.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::hash::{self, Hashed, Stamp};
use crate::paths::RelativePath;
use crate::state::{self, Change, FileRecord, State, StateError};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Debug, Default)]
pub struct ScanReport {
    pub unchanged: usize,
    pub new: Vec<RelativePath>,
    pub changed: Vec<RelativePath>,
    pub deleted: Vec<RelativePath>,
    /// Modified too recently: left for a later run.
    pub settling: Vec<RelativePath>,
    /// Held by a writer or changed while hashed: left for a later run.
    pub busy: Vec<RelativePath>,
    /// Could not be read for another reason, with the error.
    pub unreadable: Vec<(RelativePath, String)>,
    /// Names House Drive refuses (see `RelativePath::parse`), with the reason.
    pub refused: Vec<String>,
    /// Symbolic links, junctions and cloud placeholders: never followed, never pushed.
    pub links: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum ScanError {
    RootMissing(PathBuf),
    Io { path: PathBuf, source: std::io::Error },
    State(StateError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            ScanError::RootMissing(path) => {
                write!(f, "data root {} is missing or not a folder", path.display())
            }
            ScanError::Io { path, source } => write!(f, "cannot read {}: {}", path.display(), source),
            ScanError::State(error) => write!(f, "{error}"),
        };
    }
}

impl std::error::Error for ScanError {}

impl From<StateError> for ScanError {
    fn from(error: StateError) -> ScanError {
        return ScanError::State(error);
    }
}

pub fn scan(
    root: &Path,
    state: &mut State,
    now: SystemTime,
    settle: Duration,
) -> Result<ScanReport, ScanError> {
    if !root.is_dir() {
        return Err(ScanError::RootMissing(root.to_path_buf()));
    }
    let known: HashMap<String, FileRecord> =
        state.files()?.into_iter().map(|record| (state::key(&record.path), record)).collect();
    let pending_deletions: HashMap<String, String> = state
        .deletions()?
        .into_iter()
        .map(|deletion| (state::key(&deletion.path), deletion.base))
        .collect();
    let mut report = ScanReport::default();
    let mut changes = Vec::new();
    let mut seen = HashSet::new();

    let mut folders = vec![(root.to_path_buf(), String::new())];
    while let Some((folder, prefix)) = folders.pop() {
        let io_error = |source| {
            return ScanError::Io { path: folder.clone(), source };
        };
        for entry in fs::read_dir(&folder).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            // On Windows a directory entry's metadata does not follow links.
            let metadata = entry.metadata().map_err(io_error)?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                report.refused.push(format!("{}: name is not valid Unicode", entry.path().display()));
                continue;
            };
            let text = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                report.links.push(entry.path());
                continue;
            }
            let path = match RelativePath::parse(&text) {
                Ok(path) => path,
                Err(error) => {
                    report.refused.push(error.to_string());
                    continue;
                }
            };
            if metadata.is_dir() {
                folders.push((entry.path(), text));
                continue;
            }
            let key = state::key(&path);
            seen.insert(key.clone());
            let stamp = Stamp::of(&metadata);
            let accessed = metadata.accessed().map(hash::nanos).unwrap_or(0);
            let last_use = accessed.max(stamp.modified);
            let previous = known.get(&key);

            if let Some(previous) = previous
                && previous.size == stamp.size
                && previous.modified == stamp.modified
            {
                report.unchanged += 1;
                if last_use > previous.last_use {
                    changes.push(Change::Upsert(FileRecord { last_use, ..previous.clone() }));
                }
                continue;
            }
            let modified_at = SystemTime::UNIX_EPOCH + Duration::from_nanos(stamp.modified as u64);
            if let Ok(age) = now.duration_since(modified_at)
                && age < settle
            {
                report.settling.push(path);
                continue;
            }
            let (hash, stamp) = match hash::hash_stable(&entry.path()) {
                Ok(Hashed::Stable { hash, stamp }) => (hash, stamp),
                Ok(Hashed::Busy) => {
                    report.busy.push(path);
                    continue;
                }
                // Removed since listed: the next scan sees it gone.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    report.unreadable.push((path, error.to_string()));
                    continue;
                }
            };
            let record = match previous {
                Some(previous) if previous.hash == hash => {
                    report.unchanged += 1;
                    FileRecord { size: stamp.size, modified: stamp.modified, last_use, ..previous.clone() }
                }
                Some(previous) => {
                    report.changed.push(path.clone());
                    FileRecord {
                        path,
                        size: stamp.size,
                        modified: stamp.modified,
                        hash,
                        verified: false,
                        last_use,
                        ..previous.clone()
                    }
                }
                None => {
                    report.new.push(path.clone());
                    // A file recreated where a deletion is still pending replaces the store's
                    // version, so it is pushed against that version, and the deletion dropped.
                    FileRecord {
                        path,
                        size: stamp.size,
                        modified: stamp.modified,
                        hash,
                        store_hash: pending_deletions.get(&key).cloned(),
                        pushed_hash: None,
                        verified: false,
                        last_use,
                    }
                }
            };
            changes.push(Change::Upsert(record));
        }
    }

    for (key, record) in &known {
        if !seen.contains(key) {
            report.deleted.push(record.path.clone());
            // The store may hold it once pushed or fetched; otherwise there is nothing to tell.
            let base = record.pushed_hash.clone().or_else(|| record.store_hash.clone());
            changes.push(Change::Deleted { path: record.path.clone(), base });
        }
    }
    state.apply(&changes)?;
    return Ok(report);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let path =
                std::env::temp_dir().join(format!("house-drive-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            return TempDir(path);
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn later() -> SystemTime {
        return SystemTime::now() + Duration::from_secs(3600);
    }

    const SETTLE: Duration = Duration::from_secs(60);

    fn path(text: &str) -> RelativePath {
        return RelativePath::parse(text).unwrap();
    }

    #[test]
    fn finds_new_files_then_nothing_new() {
        let root = TempDir::new("new");
        fs::create_dir_all(root.0.join("a")).unwrap();
        fs::write(root.0.join("a").join("one.txt"), b"one").unwrap();
        fs::write(root.0.join("two.txt"), b"two").unwrap();
        let mut state = State::open_in_memory().unwrap();

        let mut report = scan(&root.0, &mut state, later(), SETTLE).unwrap();
        report.new.sort();
        assert_eq!(report.new, vec![path("a/one.txt"), path("two.txt")]);
        let files = state.files().unwrap();
        assert_eq!(files[0].hash, hash::hash_reader(&b"one"[..]).unwrap());
        assert!(files.iter().all(|file| !file.verified && file.pushed_hash.is_none()));

        let report = scan(&root.0, &mut state, later(), SETTLE).unwrap();
        assert_eq!(report.unchanged, 2);
        assert!(report.new.is_empty() && report.changed.is_empty() && report.deleted.is_empty());
    }

    #[test]
    fn a_changed_file_loses_its_verified_mark() {
        let root = TempDir::new("changed");
        fs::write(root.0.join("a.txt"), b"one").unwrap();
        let mut state = State::open_in_memory().unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        let mut record = state.files().unwrap().remove(0);
        record.verified = true;
        record.store_hash = Some(record.hash.clone());
        state.apply(&[Change::Upsert(record.clone())]).unwrap();

        fs::write(root.0.join("a.txt"), b"longer").unwrap();
        let report = scan(&root.0, &mut state, later(), SETTLE).unwrap();
        assert_eq!(report.changed, vec![path("a.txt")]);
        let after = state.files().unwrap().remove(0);
        assert!(!after.verified);
        assert_eq!(after.store_hash, record.store_hash);
        assert_eq!(after.hash, hash::hash_reader(&b"longer"[..]).unwrap());
    }

    #[test]
    fn deletions_are_queued_only_for_files_the_store_may_hold() {
        let root = TempDir::new("deleted");
        fs::write(root.0.join("local.txt"), b"l").unwrap();
        fs::write(root.0.join("pushed.txt"), b"p").unwrap();
        let mut state = State::open_in_memory().unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        let mut pushed = state.files().unwrap().remove(1);
        pushed.pushed_hash = Some(pushed.hash.clone());
        state.apply(&[Change::Upsert(pushed.clone())]).unwrap();

        fs::remove_file(root.0.join("local.txt")).unwrap();
        fs::remove_file(root.0.join("pushed.txt")).unwrap();
        let mut report = scan(&root.0, &mut state, later(), SETTLE).unwrap();
        report.deleted.sort();
        assert_eq!(report.deleted, vec![path("local.txt"), path("pushed.txt")]);
        let deletions = state.deletions().unwrap();
        assert_eq!(deletions.len(), 1);
        assert_eq!(deletions[0].path, path("pushed.txt"));
        assert_eq!(deletions[0].base, pushed.hash);
    }

    #[test]
    fn a_recreated_file_is_pushed_against_the_deleted_version() {
        let root = TempDir::new("recreated");
        fs::write(root.0.join("a.txt"), b"old").unwrap();
        let mut state = State::open_in_memory().unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        let mut record = state.files().unwrap().remove(0);
        record.pushed_hash = Some(record.hash.clone());
        state.apply(&[Change::Upsert(record.clone())]).unwrap();
        fs::remove_file(root.0.join("a.txt")).unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();

        fs::write(root.0.join("a.txt"), b"new").unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        assert!(state.deletions().unwrap().is_empty());
        assert_eq!(state.files().unwrap()[0].store_hash, Some(record.hash));
    }

    #[test]
    fn recent_files_settle_first() {
        let root = TempDir::new("settling");
        fs::write(root.0.join("a.txt"), b"a").unwrap();
        let mut state = State::open_in_memory().unwrap();
        let report = scan(&root.0, &mut state, SystemTime::now(), SETTLE).unwrap();
        assert_eq!(report.settling, vec![path("a.txt")]);
        assert!(state.files().unwrap().is_empty());
    }

    #[test]
    fn a_file_held_by_a_writer_is_neither_new_nor_deleted() {
        let root = TempDir::new("busy");
        fs::write(root.0.join("a.txt"), b"a").unwrap();
        let mut state = State::open_in_memory().unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        fs::write(root.0.join("b.txt"), b"b").unwrap();

        let _writer = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(root.0.join("b.txt"))
            .unwrap();
        let report = scan(&root.0, &mut state, later(), SETTLE).unwrap();
        assert_eq!(report.busy, vec![path("b.txt")]);
        assert!(report.new.is_empty() && report.deleted.is_empty());
    }

    #[test]
    fn a_missing_root_stops_and_changes_nothing() {
        let root = TempDir::new("missing");
        fs::write(root.0.join("a.txt"), b"a").unwrap();
        let mut state = State::open_in_memory().unwrap();
        scan(&root.0, &mut state, later(), SETTLE).unwrap();
        let gone = root.0.join("not-here");
        assert!(matches!(
            scan(&gone, &mut state, later(), SETTLE),
            Err(ScanError::RootMissing(_))
        ));
        assert_eq!(state.files().unwrap().len(), 1);
        assert!(state.deletions().unwrap().is_empty());
    }
}
