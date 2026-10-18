//! Each machine's reading of the catalog's snapshot, after scanning and pushing: verified marks,
//! files to push again, deletions made elsewhere, fetching pinned and stale files, eviction.
//!
//! This is where an ordinary machine deletes local files, in two cases only, both requiring the
//! local copy to be exactly a version the store holds or held, unedited, and unchanged since the
//! scan: a file deleted on another machine (the store's trash keeps it), and eviction (the store
//! keeps it). Anything else is reported and left alone.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::{self, CatalogFile, Quota, RejectReason, Snapshot};
use crate::hash::{self, Stamp};
use crate::merge::remove_empty_parents;
use crate::paths::RelativePath;
use crate::state::{self, Change, FileRecord, State, StateError};
use crate::store::Store;

/// Under the data root: files being fetched, renamed into place once complete. Scans skip it.
pub const FETCH_DIR: &str = ".house-drive-fetch";

const CATALOG_SNAPSHOT: &str = "catalog.tsv";

#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Files newly found verified on the store.
    pub verified: usize,
    /// Rejected by the store machine (hash mismatch): pushed again next run.
    pub repush: Vec<RelativePath>,
    /// Refused by the store for lack of room: pushed again once there is room.
    pub over_quota: Vec<RelativePath>,
    /// The store's space at its last run.
    pub quota: Option<Quota>,
    pub fetched: Vec<RelativePath>,
    pub fetch_failed: Vec<(RelativePath, String)>,
    /// Deleted here because deleted on another machine.
    pub deleted: Vec<RelativePath>,
    /// Deleted on another machine but edited here: kept, and pushed back.
    pub kept_edited: Vec<RelativePath>,
    pub evicted: Vec<RelativePath>,
    /// Files a deletion or eviction left alone, with the reason.
    pub skipped: Vec<(RelativePath, String)>,
    /// The cache's size when eviction could not bring it under the limit.
    pub over_limit: Option<u64>,
    /// Unresolved conflict copies, from every machine.
    pub conflicts: Vec<RelativePath>,
    /// This machine's deletions the store refused, the file having changed meanwhile.
    pub refused: Vec<RelativePath>,
}

#[derive(Debug)]
pub enum ReconcileError {
    Io { path: PathBuf, source: io::Error },
    State(StateError),
    Snapshot(String),
    /// A request that cannot be carried out as asked, with the reason.
    Refused(String),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            ReconcileError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            ReconcileError::State(error) => write!(f, "{error}"),
            ReconcileError::Snapshot(message) => write!(f, "{message}"),
            ReconcileError::Refused(message) => write!(f, "{message}"),
        };
    }
}

impl std::error::Error for ReconcileError {}

impl From<StateError> for ReconcileError {
    fn from(error: StateError) -> ReconcileError {
        return ReconcileError::State(error);
    }
}

/// The store's current snapshot; `None` when there is none yet or it is cut short.
pub fn read_snapshot(store: &Store) -> Result<Option<Snapshot>, ReconcileError> {
    return read_snapshot_at(&store.root().join(CATALOG_SNAPSHOT));
}

/// A snapshot from a file: the store's, or this machine's copy of the last one read, which lets
/// `status`, `delete` and `resolve` work offline.
pub fn read_snapshot_at(path: &Path) -> Result<Option<Snapshot>, ReconcileError> {
    let path = path.to_path_buf();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReconcileError::Io { path, source: error }),
    };
    return catalog::parse_snapshot(&text).map_err(ReconcileError::Snapshot);
}

/// Keeps a copy of a snapshot on this machine, written whole then renamed into place.
pub fn save_snapshot(path: &Path, snapshot: &Snapshot) -> Result<(), ReconcileError> {
    let staging = path.with_extension("tsv.new");
    let io_error = |source| {
        return ReconcileError::Io { path: path.to_path_buf(), source };
    };
    fs::write(&staging, catalog::format_snapshot(snapshot)).map_err(io_error)?;
    fs::rename(&staging, path).map_err(io_error)?;
    return Ok(());
}

pub struct Limits {
    pub cache_bytes: u64,
    pub evict_to_percent: u32,
}

pub fn reconcile(
    store: &Store,
    snapshot: &Snapshot,
    machine_id: &str,
    data_root: &Path,
    state: &mut State,
    limits: &Limits,
    now: SystemTime,
) -> Result<ReconcileReport, ReconcileError> {
    let mut report = ReconcileReport::default();
    let fetch_dir = data_root.join(FETCH_DIR);
    if fetch_dir.exists() {
        fs::remove_dir_all(&fetch_dir)
            .map_err(|source| ReconcileError::Io { path: fetch_dir.clone(), source })?;
    }
    let pins = state.pins()?;
    let pinned = |path: &RelativePath| {
        return pins.iter().any(|pin| path.is_within(pin));
    };
    let pending_deletions: HashSet<String> =
        state.deletions()?.iter().map(|deletion| state::key(&deletion.path)).collect();
    let catalog: HashMap<String, &CatalogFile> =
        snapshot.files.iter().map(|file| (state::key(&file.path), file)).collect();
    let mut records: BTreeMap<String, FileRecord> =
        state.files()?.into_iter().map(|record| (state::key(&record.path), record)).collect();
    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut forgotten: Vec<RelativePath> = Vec::new();

    // Rejected pushes of this machine: push them again (those refused for room, once there is
    // room: pushing holds what does not fit).
    for rejected in snapshot.rejected.iter().filter(|rejected| rejected.machine == machine_id) {
        let key = state::key(&rejected.path);
        if let Some(record) = records.get_mut(&key)
            && record.hash == rejected.hash
            && record.pushed_hash.is_some()
        {
            record.pushed_hash = None;
            changed.insert(key);
            match rejected.reason {
                RejectReason::Hash => report.repush.push(rejected.path.clone()),
                RejectReason::Quota => report.over_quota.push(rejected.path.clone()),
            }
        }
    }
    report.quota = snapshot.quota;

    // Verified marks: the store holds this very version.
    for (key, record) in records.iter_mut() {
        match catalog.get(key) {
            Some(file) if file.hash == record.hash => {
                let hash = Some(file.hash.clone());
                if !record.verified || record.store_hash != hash || record.pushed_hash != hash {
                    record.verified = true;
                    record.store_hash = hash.clone();
                    record.pushed_hash = hash;
                    changed.insert(key.clone());
                    report.verified += 1;
                }
            }
            _ => {
                if record.verified {
                    record.verified = false;
                    changed.insert(key.clone());
                }
            }
        }
    }

    // Deleted on another machine: the store had this machine's version and no longer lists it.
    let keys: Vec<String> = records.keys().cloned().collect();
    for key in keys {
        let record = &records[&key];
        if catalog.contains_key(&key) {
            continue;
        }
        let Some(store_hash) = &record.store_hash else {
            continue;
        };
        if record.hash != *store_hash {
            report.kept_edited.push(record.path.clone());
            continue;
        }
        let path = record.path.clone();
        let target = path.under(data_root);
        match remove_if_unchanged(&target, record) {
            Ok(()) => {
                remove_empty_parents(&target, data_root);
                records.remove(&key);
                changed.remove(&key);
                forgotten.push(path.clone());
                report.deleted.push(path);
            }
            Err(reason) => report.skipped.push((path, reason)),
        }
    }

    // Fetching: pinned files missing here, and unedited copies of versions replaced elsewhere.
    for file in &snapshot.files {
        let key = state::key(&file.path);
        if pending_deletions.contains(&key) {
            continue;
        }
        let needed = match records.get(&key) {
            None => pinned(&file.path),
            Some(record) => {
                record.hash != file.hash && record.store_hash.as_deref() == Some(record.hash.as_str())
            }
        };
        if !needed {
            continue;
        }
        match fetch(store, data_root, &fetch_dir, file, records.get(&key), now) {
            Ok(record) => {
                records.insert(key.clone(), record);
                changed.insert(key);
                report.fetched.push(file.path.clone());
            }
            Err(reason) => report.fetch_failed.push((file.path.clone(), reason)),
        }
    }

    // Eviction: past the limit, least recently used first, down to the threshold.
    let mut total: u64 = records.values().map(|record| record.size).sum();
    if total > limits.cache_bytes {
        let goal = (limits.cache_bytes as u128 * limits.evict_to_percent as u128 / 100) as u64;
        let mut candidates: Vec<(i64, String)> = records
            .iter()
            .filter(|(_, record)| {
                return record.verified
                    && record.store_hash.as_deref() == Some(record.hash.as_str())
                    && !pinned(&record.path);
            })
            .map(|(key, record)| (record.last_use, key.clone()))
            .collect();
        candidates.sort();
        for (_, key) in candidates {
            if total <= goal {
                break;
            }
            let record = &records[&key];
            let path = record.path.clone();
            let target = path.under(data_root);
            match remove_if_unchanged(&target, record) {
                Ok(()) => {
                    total -= record.size;
                    remove_empty_parents(&target, data_root);
                    records.remove(&key);
                    changed.remove(&key);
                    forgotten.push(path.clone());
                    report.evicted.push(path);
                }
                Err(reason) => report.skipped.push((path, reason)),
            }
        }
        if total > limits.cache_bytes {
            report.over_limit = Some(total);
        }
    }

    let mut changes: Vec<Change> =
        changed.iter().map(|key| Change::Upsert(records[key].clone())).collect();
    // Removed without queueing a deletion: the store keeps (or trashed) these files.
    changes.extend(forgotten.into_iter().map(|path| Change::Deleted { path, base: None }));
    state.apply(&changes)?;

    report.conflicts = snapshot.conflicts.iter().map(|conflict| conflict.copy.clone()).collect();
    report.refused = snapshot
        .refused
        .iter()
        .filter(|refused| refused.machine == machine_id)
        .map(|refused| refused.path.clone())
        .collect();
    return Ok(report);
}

/// What a `fetch` or `evict` by hand did.
#[derive(Debug, Default)]
pub struct ByHand {
    pub done: Vec<RelativePath>,
    pub refused: Vec<(RelativePath, String)>,
}

/// Fetches now every file of the snapshot at or under `folder` missing here (or an unedited copy
/// of an older version), except deletions still pending.
pub fn fetch_within(
    store: &Store,
    snapshot: &Snapshot,
    data_root: &Path,
    state: &mut State,
    folder: &RelativePath,
    now: SystemTime,
) -> Result<ByHand, ReconcileError> {
    let fetch_dir = data_root.join(FETCH_DIR);
    let pending: HashSet<String> = state.deletions()?.iter().map(|deletion| state::key(&deletion.path)).collect();
    let records: HashMap<String, FileRecord> =
        state.files()?.into_iter().map(|record| (state::key(&record.path), record)).collect();
    let mut result = ByHand::default();
    let mut changes = Vec::new();
    for file in snapshot.files.iter().filter(|file| file.path.is_within(folder)) {
        let key = state::key(&file.path);
        if pending.contains(&key) {
            result.refused.push((file.path.clone(), "deleted here, deletion not yet pushed".to_string()));
            continue;
        }
        let existing = records.get(&key);
        if let Some(record) = existing {
            if record.hash == file.hash {
                continue;
            }
            if record.store_hash.as_deref() != Some(record.hash.as_str()) {
                result.refused.push((file.path.clone(), "edited here, kept".to_string()));
                continue;
            }
        }
        match fetch(store, data_root, &fetch_dir, file, existing, now) {
            Ok(record) => {
                changes.push(Change::Upsert(record));
                result.done.push(file.path.clone());
            }
            Err(reason) => result.refused.push((file.path.clone(), reason)),
        }
    }
    state.apply(&changes)?;
    return Ok(result);
}

/// Evicts now every local file at or under `folder` the store holds verified, unedited and
/// unchanged since the scan; pinned ones are refused (unpin first).
pub fn evict_within(data_root: &Path, state: &mut State, folder: &RelativePath) -> Result<ByHand, ReconcileError> {
    let pins = state.pins()?;
    let mut result = ByHand::default();
    let mut changes = Vec::new();
    for record in state.files()?.into_iter().filter(|record| record.path.is_within(folder)) {
        if pins.iter().any(|pin| record.path.is_within(pin)) {
            result.refused.push((record.path, "pinned: unpin it first".to_string()));
            continue;
        }
        if !record.verified || record.store_hash.as_deref() != Some(record.hash.as_str()) {
            result.refused.push((record.path, "not verified on the store yet: sync first".to_string()));
            continue;
        }
        let target = record.path.under(data_root);
        match remove_if_unchanged(&target, &record) {
            Ok(()) => {
                remove_empty_parents(&target, data_root);
                changes.push(Change::Deleted { path: record.path.clone(), base: None });
                result.done.push(record.path);
            }
            Err(reason) => result.refused.push((record.path, reason)),
        }
    }
    state.apply(&changes)?;
    return Ok(result);
}

/// Deletes `folder` (a file or a folder) everywhere: local copies now, and the deletions queued
/// for the next push; files only on the store (from the last snapshot read) get their deletion
/// queued too. On the store, deleted files go to the trash.
pub fn delete_within(
    data_root: &Path,
    state: &mut State,
    snapshot: Option<&Snapshot>,
    folder: &RelativePath,
) -> Result<ByHand, ReconcileError> {
    let mut result = ByHand::default();
    let mut changes = Vec::new();
    let mut local = HashSet::new();
    for record in state.files()?.into_iter().filter(|record| record.path.is_within(folder)) {
        local.insert(state::key(&record.path));
        let target = record.path.under(data_root);
        match fs::remove_file(&target) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                result.refused.push((record.path, error.to_string()));
                continue;
            }
            _ => {}
        }
        remove_empty_parents(&target, data_root);
        let base = record.pushed_hash.clone().or_else(|| record.store_hash.clone());
        changes.push(Change::Deleted { path: record.path.clone(), base });
        result.done.push(record.path);
    }
    if let Some(snapshot) = snapshot {
        for file in snapshot.files.iter().filter(|file| file.path.is_within(folder)) {
            if !local.contains(&state::key(&file.path)) {
                changes.push(Change::Deleted { path: file.path.clone(), base: Some(file.hash.clone()) });
                result.done.push(file.path.clone());
            }
        }
    }
    state.apply(&changes)?;
    return Ok(result);
}

pub enum Resolution {
    /// Delete the conflict copy.
    KeepOriginal,
    /// The copy's content replaces the original; the copy is deleted.
    KeepCopy,
    /// The copy is renamed and kept as a file of its own.
    KeepBoth(RelativePath),
}

/// Settles a conflict with ordinary operations, which the next sync pushes: deleting the copy,
/// replacing the original with it, or renaming it. Replacing and renaming need the copy here.
pub fn resolve(
    data_root: &Path,
    state: &mut State,
    snapshot: &Snapshot,
    copy: &RelativePath,
    resolution: &Resolution,
) -> Result<(), ReconcileError> {
    let refuse = |message: String| {
        return Err(ReconcileError::Refused(message));
    };
    let Some(conflict) = snapshot.conflicts.iter().find(|conflict| state::key(&conflict.copy) == state::key(copy)) else {
        return refuse(format!("{copy} is not a conflict copy in the last catalog read"));
    };
    if let Resolution::KeepOriginal = resolution {
        delete_within(data_root, state, Some(snapshot), &conflict.copy)?;
        return Ok(());
    }
    let records: HashMap<String, FileRecord> =
        state.files()?.into_iter().map(|record| (state::key(&record.path), record)).collect();
    let copy_target = conflict.copy.under(data_root);
    if !records.contains_key(&state::key(&conflict.copy)) || !copy_target.exists() {
        return refuse(format!("{} is not on this machine: fetch it first", conflict.copy));
    }
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        return move |source: io::Error| ReconcileError::Io { path, source };
    };
    match resolution {
        Resolution::KeepOriginal => {}
        Resolution::KeepCopy => {
            let original_target = conflict.original.under(data_root);
            if let Some(parent) = original_target.parent() {
                fs::create_dir_all(parent).map_err(io_error(parent))?;
            }
            fs::copy(&copy_target, &original_target).map_err(io_error(&original_target))?;
            // Not here before: record the store's version as its base, with a stamp that makes
            // the next scan hash it, so its push replaces that version instead of conflicting.
            if !records.contains_key(&state::key(&conflict.original))
                && let Some(current) = snapshot.files.iter().find(|file| file.path == conflict.original)
            {
                state.apply(&[Change::Upsert(FileRecord {
                    path: conflict.original.clone(),
                    size: 0,
                    modified: 0,
                    hash: current.hash.clone(),
                    store_hash: Some(current.hash.clone()),
                    pushed_hash: Some(current.hash.clone()),
                    verified: false,
                    last_use: 0,
                })])?;
            }
            delete_within(data_root, state, Some(snapshot), &conflict.copy)?;
        }
        Resolution::KeepBoth(new_path) => {
            let new_target = new_path.under(data_root);
            if new_target.exists() || snapshot.files.iter().any(|file| state::key(&file.path) == state::key(new_path)) {
                return refuse(format!("{new_path} already exists"));
            }
            if let Some(parent) = new_target.parent() {
                fs::create_dir_all(parent).map_err(io_error(parent))?;
            }
            // The next scan sees the copy gone (its deletion pushed) and a new file.
            fs::rename(&copy_target, &new_target).map_err(io_error(&new_target))?;
        }
    }
    return Ok(());
}

/// Deletes a local file only if it is still as last scanned.
fn remove_if_unchanged(target: &Path, record: &FileRecord) -> Result<(), String> {
    let metadata = match fs::metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if Stamp::of(&metadata) != (Stamp { size: record.size, modified: record.modified }) {
        return Err("changed since the scan".to_string());
    }
    return fs::remove_file(target).map_err(|error| error.to_string());
}

/// Copies a file from the store into the fetch folder, checks its hash, keeps the store's
/// modified time, and renames it into place, unless the local file changed since the scan.
fn fetch(
    store: &Store,
    data_root: &Path,
    fetch_dir: &Path,
    file: &CatalogFile,
    existing: Option<&FileRecord>,
    now: SystemTime,
) -> Result<FileRecord, String> {
    let text = |error: io::Error| {
        return error.to_string();
    };
    let source = File::open(file.path.under(&store.files_dir())).map_err(text)?;
    fs::create_dir_all(fetch_dir).map_err(text)?;
    let part = fetch_dir.join(format!("{}.part", file.hash));
    let copy = File::create(&part).map_err(text)?;
    let hash = hash::copy_hashing(&source, &copy).map_err(text)?;
    if hash != file.hash {
        return Err("the store's copy does not match the catalog".to_string());
    }
    copy.set_modified(UNIX_EPOCH + Duration::from_nanos(file.modified.max(0) as u64)).map_err(text)?;
    copy.sync_all().map_err(text)?;
    drop(copy);

    let target = file.path.under(data_root);
    match existing {
        Some(record) => match fs::metadata(&target) {
            Ok(metadata) if Stamp::of(&metadata) == (Stamp { size: record.size, modified: record.modified }) => {}
            Ok(_) => return Err("changed here since the scan, kept".to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        },
        None => {
            if target.exists() {
                return Err("a file not yet scanned is in the way".to_string());
            }
        }
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(text)?;
    }
    fs::rename(&part, &target).map_err(text)?;
    let stamp = Stamp::of(&fs::metadata(&target).map_err(text)?);
    return Ok(FileRecord {
        path: file.path.clone(),
        size: stamp.size,
        modified: stamp.modified,
        hash: file.hash.clone(),
        store_hash: Some(file.hash.clone()),
        pushed_hash: Some(file.hash.clone()),
        verified: true,
        last_use: hash::nanos(now),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Rejected;
    use crate::state::PendingDeletion;
    use crate::test_support::TempDir;

    const ME: &str = "machine-a";

    struct Setup {
        data: TempDir,
        _store_root: TempDir,
        store: Store,
        state: State,
        snapshot: Snapshot,
    }

    fn setup(name: &str) -> Setup {
        let data = TempDir::new(&format!("{name}-data"));
        let store_root = TempDir::new(&format!("{name}-store"));
        let id = Store::init(&store_root.0).unwrap();
        let store = Store::open(&store_root.0, &id).unwrap();
        return Setup {
            data,
            _store_root: store_root,
            store,
            state: State::open_in_memory().unwrap(),
            snapshot: Snapshot::default(),
        };
    }

    fn path(text: &str) -> RelativePath {
        return RelativePath::parse(text).unwrap();
    }

    fn h(content: &[u8]) -> String {
        return hash::hash_reader(content).unwrap();
    }

    /// A file in the store's `files/`, listed in the snapshot.
    fn on_store(setup: &mut Setup, text: &str, content: &[u8]) {
        let target = path(text).under(&setup.store.files_dir());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, content).unwrap();
        setup.snapshot.files.push(CatalogFile {
            path: path(text),
            size: content.len() as u64,
            hash: h(content),
            modified: 1_000_000_000_000_000_000,
            machine: "machine-b".into(),
        });
    }

    /// A local file and its record; `store` is the version the store held when last seen.
    fn local(setup: &mut Setup, text: &str, content: &[u8], store: Option<&[u8]>, last_use: i64) {
        let target = path(text).under(&setup.data.0);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, content).unwrap();
        let stamp = Stamp::of(&fs::metadata(&target).unwrap());
        let store_hash = store.map(h);
        let record = FileRecord {
            path: path(text),
            size: stamp.size,
            modified: stamp.modified,
            hash: h(content),
            verified: store_hash.as_deref() == Some(h(content).as_str()),
            pushed_hash: store_hash.clone(),
            store_hash,
            last_use,
        };
        setup.state.apply(&[Change::Upsert(record)]).unwrap();
    }

    fn run(setup: &mut Setup, cache_bytes: u64) -> ReconcileReport {
        let limits = Limits { cache_bytes, evict_to_percent: 50 };
        return reconcile(&setup.store, &setup.snapshot, ME, &setup.data.0, &mut setup.state, &limits, SystemTime::now())
            .unwrap();
    }

    fn record(setup: &Setup, text: &str) -> Option<FileRecord> {
        return setup.state.files().unwrap().into_iter().find(|record| record.path == path(text));
    }

    #[test]
    fn a_pushed_file_listed_by_the_store_is_verified() {
        let mut setup = setup("rec-verified");
        local(&mut setup, "a.txt", b"a", None, 0);
        on_store(&mut setup, "a.txt", b"a");
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.verified, 1);
        let record = record(&setup, "a.txt").unwrap();
        assert!(record.verified);
        assert_eq!(record.store_hash, Some(h(b"a")));
        assert_eq!(record.pushed_hash, Some(h(b"a")));
    }

    #[test]
    fn a_rejected_push_is_pushed_again() {
        let mut setup = setup("rec-rejected");
        local(&mut setup, "a.txt", b"a", None, 0);
        let mut record = record(&setup, "a.txt").unwrap();
        record.pushed_hash = Some(record.hash.clone());
        setup.state.apply(&[Change::Upsert(record)]).unwrap();
        setup.snapshot.rejected.push(Rejected {
            machine: ME.into(),
            path: path("a.txt"),
            hash: h(b"a"),
            reason: RejectReason::Hash,
        });
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.repush, vec![path("a.txt")]);
        assert_eq!(self::record(&setup, "a.txt").unwrap().pushed_hash, None);
    }

    #[test]
    fn a_push_refused_for_room_waits_to_be_pushed_again() {
        let mut setup = setup("rec-quota");
        local(&mut setup, "a.txt", b"a", None, 0);
        let mut record = record(&setup, "a.txt").unwrap();
        record.pushed_hash = Some(record.hash.clone());
        setup.state.apply(&[Change::Upsert(record)]).unwrap();
        setup.snapshot.rejected.push(Rejected {
            machine: ME.into(),
            path: path("a.txt"),
            hash: h(b"a"),
            reason: RejectReason::Quota,
        });
        setup.snapshot.quota = Some(Quota { used: 10, limit: Some(10), free: 100, margin: 0 });
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.over_quota, vec![path("a.txt")]);
        assert!(report.repush.is_empty());
        assert_eq!(report.quota.unwrap().room(), 0);
        assert_eq!(self::record(&setup, "a.txt").unwrap().pushed_hash, None);
    }

    #[test]
    fn pinned_files_are_fetched_others_are_not() {
        let mut setup = setup("rec-fetch");
        on_store(&mut setup, "photos/a.jpg", b"jpeg");
        on_store(&mut setup, "other/b.txt", b"b");
        setup.state.add_pin(&path("photos")).unwrap();
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.fetched, vec![path("photos/a.jpg")]);
        let local = path("photos/a.jpg").under(&setup.data.0);
        assert_eq!(fs::read(&local).unwrap(), b"jpeg");
        let modified = Stamp::of(&fs::metadata(&local).unwrap()).modified;
        assert_eq!(modified, 1_000_000_000_000_000_000);
        assert!(record(&setup, "photos/a.jpg").unwrap().verified);
        assert!(!path("other/b.txt").under(&setup.data.0).exists());
        assert!(!setup.data.0.join(FETCH_DIR).join(format!("{}.part", h(b"jpeg"))).exists());
    }

    #[test]
    fn an_unedited_copy_is_refreshed_an_edited_one_is_not() {
        let mut setup = setup("rec-refresh");
        local(&mut setup, "clean.txt", b"v1", Some(b"v1"), 0);
        local(&mut setup, "edited.txt", b"mine", Some(b"v1"), 0);
        on_store(&mut setup, "clean.txt", b"v2");
        on_store(&mut setup, "edited.txt", b"v2");
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.fetched, vec![path("clean.txt")]);
        assert_eq!(fs::read(setup.data.0.join("clean.txt")).unwrap(), b"v2");
        assert_eq!(fs::read(setup.data.0.join("edited.txt")).unwrap(), b"mine");
    }

    #[test]
    fn a_file_deleted_elsewhere_goes_unless_edited_here() {
        let mut setup = setup("rec-deleted");
        local(&mut setup, "d/clean.txt", b"v1", Some(b"v1"), 0);
        local(&mut setup, "edited.txt", b"mine", Some(b"v1"), 0);
        local(&mut setup, "never-pushed.txt", b"n", None, 0);
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.deleted, vec![path("d/clean.txt")]);
        assert_eq!(report.kept_edited, vec![path("edited.txt")]);
        assert!(!setup.data.0.join("d").exists());
        assert!(setup.data.0.join("edited.txt").exists());
        assert!(setup.data.0.join("never-pushed.txt").exists());
        assert!(record(&setup, "d/clean.txt").is_none());
        assert!(setup.state.deletions().unwrap().is_empty());
    }

    #[test]
    fn a_pending_deletion_is_not_fetched_back() {
        let mut setup = setup("rec-pending");
        on_store(&mut setup, "photos/a.jpg", b"jpeg");
        setup.state.add_pin(&path("photos")).unwrap();
        setup
            .state
            .apply(&[Change::Deleted { path: path("photos/a.jpg"), base: Some(h(b"jpeg")) }])
            .unwrap();
        let report = run(&mut setup, u64::MAX);
        assert!(report.fetched.is_empty());
        assert_eq!(
            setup.state.deletions().unwrap(),
            vec![PendingDeletion { path: path("photos/a.jpg"), base: h(b"jpeg") }]
        );
    }

    #[test]
    fn eviction_spares_pinned_unverified_and_recent_files() {
        let mut setup = setup("rec-evict");
        for (name, last_use) in [("old.bin", 1), ("mid.bin", 2), ("new.bin", 3)] {
            local(&mut setup, name, b"0123456789", Some(b"0123456789"), last_use);
            on_store(&mut setup, name, b"0123456789");
        }
        local(&mut setup, "pinned/p.bin", b"0123456789", Some(b"0123456789"), 0);
        on_store(&mut setup, "pinned/p.bin", b"0123456789");
        setup.state.add_pin(&path("pinned")).unwrap();
        local(&mut setup, "unpushed.bin", b"abcdefghij", None, 0);

        // 50 bytes against a 40-byte limit: down to 50% of it, 20 bytes.
        let report = run(&mut setup, 40);
        assert_eq!(report.evicted, vec![path("old.bin"), path("mid.bin"), path("new.bin")]);
        assert_eq!(report.over_limit, None);
        assert!(setup.data.0.join("pinned").join("p.bin").exists());
        assert!(setup.data.0.join("unpushed.bin").exists());
        assert!(setup.state.deletions().unwrap().is_empty());
    }

    #[test]
    fn eviction_warns_when_it_cannot_reach_the_limit() {
        let mut setup = setup("rec-over");
        local(&mut setup, "unpushed.bin", b"abcdefghij", None, 0);
        let report = run(&mut setup, 5);
        assert!(report.evicted.is_empty());
        assert_eq!(report.over_limit, Some(10));
    }

    #[test]
    fn fetch_by_hand_takes_a_folder() {
        let mut setup = setup("rec-fetch-hand");
        on_store(&mut setup, "docs/a.txt", b"a");
        on_store(&mut setup, "docs/sub/b.txt", b"b");
        on_store(&mut setup, "other.txt", b"o");
        local(&mut setup, "docs/edited.txt", b"mine", Some(b"v1"), 0);
        on_store(&mut setup, "docs/edited.txt", b"v2");
        let snapshot = setup.snapshot.clone();
        let result = fetch_within(&setup.store, &snapshot, &setup.data.0, &mut setup.state, &path("docs"), SystemTime::now())
            .unwrap();
        let mut done = result.done.clone();
        done.sort();
        assert_eq!(done, vec![path("docs/a.txt"), path("docs/sub/b.txt")]);
        assert_eq!(result.refused.len(), 1);
        assert!(!setup.data.0.join("other.txt").exists());
        assert!(record(&setup, "docs/sub/b.txt").unwrap().verified);
    }

    #[test]
    fn evict_by_hand_refuses_pinned_and_unverified_files() {
        let mut setup = setup("rec-evict-hand");
        local(&mut setup, "docs/a.txt", b"a", Some(b"a"), 0);
        local(&mut setup, "docs/new.txt", b"n", None, 0);
        local(&mut setup, "docs/keep/k.txt", b"k", Some(b"k"), 0);
        setup.state.add_pin(&path("docs/keep")).unwrap();
        let result = evict_within(&setup.data.0, &mut setup.state, &path("docs")).unwrap();
        assert_eq!(result.done, vec![path("docs/a.txt")]);
        assert_eq!(result.refused.len(), 2);
        assert!(!setup.data.0.join("docs").join("a.txt").exists());
        assert!(setup.data.0.join("docs").join("new.txt").exists());
        assert!(setup.state.deletions().unwrap().is_empty());
    }

    #[test]
    fn delete_reaches_local_and_store_only_files() {
        let mut setup = setup("rec-delete");
        local(&mut setup, "docs/a.txt", b"a", Some(b"a"), 0);
        local(&mut setup, "docs/new.txt", b"n", None, 0);
        on_store(&mut setup, "docs/a.txt", b"a");
        on_store(&mut setup, "docs/remote.txt", b"r");
        let snapshot = setup.snapshot.clone();
        let result = delete_within(&setup.data.0, &mut setup.state, Some(&snapshot), &path("docs")).unwrap();
        assert_eq!(result.done.len(), 3);
        assert!(!setup.data.0.join("docs").exists());
        let mut deletions: Vec<String> =
            setup.state.deletions().unwrap().into_iter().map(|deletion| deletion.path.to_string()).collect();
        deletions.sort();
        // The never-pushed file has nothing to tell the store.
        assert_eq!(deletions, vec!["docs/a.txt", "docs/remote.txt"]);
    }

    fn with_conflict(name: &str) -> Setup {
        let mut setup = setup(name);
        on_store(&mut setup, "r.txt", b"mine");
        on_store(&mut setup, "r (conflict machine-b 2026-10-18).txt", b"theirs");
        local(&mut setup, "r (conflict machine-b 2026-10-18).txt", b"theirs", Some(b"theirs"), 0);
        setup.snapshot.conflicts.push(crate::catalog::Conflict {
            copy: path("r (conflict machine-b 2026-10-18).txt"),
            original: path("r.txt"),
            machine: "machine-b".into(),
        });
        return setup;
    }

    #[test]
    fn keep_copy_replaces_the_original_against_the_stores_version() {
        let mut setup = with_conflict("rec-keep-copy");
        let snapshot = setup.snapshot.clone();
        let copy = path("r (conflict machine-b 2026-10-18).txt");
        resolve(&setup.data.0, &mut setup.state, &snapshot, &copy, &Resolution::KeepCopy).unwrap();
        assert_eq!(fs::read(setup.data.0.join("r.txt")).unwrap(), b"theirs");
        assert!(!copy.under(&setup.data.0).exists());
        assert_eq!(record(&setup, "r.txt").unwrap().store_hash, Some(h(b"mine")));

        // The next scan hashes the original and finds it changed, pushed against "mine".
        let later = SystemTime::now() + Duration::from_secs(3600);
        crate::scan::scan(&setup.data.0, &mut setup.state, later, Duration::from_secs(60)).unwrap();
        let original = record(&setup, "r.txt").unwrap();
        assert_eq!(original.hash, h(b"theirs"));
        assert_eq!(original.store_hash, Some(h(b"mine")));
    }

    #[test]
    fn keep_both_renames_the_copy() {
        let mut setup = with_conflict("rec-keep-both");
        let snapshot = setup.snapshot.clone();
        let copy = path("r (conflict machine-b 2026-10-18).txt");
        resolve(&setup.data.0, &mut setup.state, &snapshot, &copy, &Resolution::KeepBoth(path("r.txt"))).unwrap_err();
        resolve(&setup.data.0, &mut setup.state, &snapshot, &copy, &Resolution::KeepBoth(path("r-theirs.txt"))).unwrap();
        assert_eq!(fs::read(setup.data.0.join("r-theirs.txt")).unwrap(), b"theirs");
        assert!(!copy.under(&setup.data.0).exists());
    }

    #[test]
    fn a_corrupt_store_copy_is_not_fetched() {
        let mut setup = setup("rec-corrupt");
        on_store(&mut setup, "photos/a.jpg", b"jpeg");
        fs::write(path("photos/a.jpg").under(&setup.store.files_dir()), b"rot").unwrap();
        setup.state.add_pin(&path("photos")).unwrap();
        let report = run(&mut setup, u64::MAX);
        assert_eq!(report.fetch_failed.len(), 1);
        assert!(!path("photos/a.jpg").under(&setup.data.0).exists());
    }
}
