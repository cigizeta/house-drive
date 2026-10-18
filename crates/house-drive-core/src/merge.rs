//! The store machine's run: merging every machine's complete batches into `files/`, verifying
//! each file by hashing it on the local disk, the trash, and the catalog's snapshot.
//!
//! Every operation can be repeated: a merge stopped halfway (a crash, a power cut) runs again over
//! the same batch and finishes it, since each step first checks whether it was already done.
//! Anything the store's state does not explain stops the run rather than guessing.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::catalog::{
    self, Catalog, CatalogError, CatalogFile, CatalogTransaction, Conflict, Quota, Refused, RejectReason, Rejected,
};
use crate::config;
use crate::dates;
use crate::hash;
use crate::journal::{self, Entry, JournalError};
use crate::paths::RelativePath;
use crate::store::{self, Store};

const CATALOG_SNAPSHOT: &str = "catalog.tsv";
const CATALOG_STAGING: &str = "catalog.tsv.new";
const MAX_CONFLICT_COPIES: u32 = 1000;

#[derive(Debug, Default)]
pub struct MergeReport {
    pub merged_batches: usize,
    pub discarded_batches: usize,
    /// Each machine's newest batch, still arriving.
    pub waiting: Vec<(String, String)>,
    pub placed: Vec<RelativePath>,
    pub replaced: Vec<RelativePath>,
    pub conflicts: Vec<RelativePath>,
    pub deleted: Vec<RelativePath>,
    pub refused: Vec<RelativePath>,
    pub rejected: Vec<(String, RelativePath)>,
    /// Not placed for lack of room (quota or free-space margin): pushed again later.
    pub over_quota: Vec<(String, RelativePath)>,
    pub quota: Option<Quota>,
    pub restored: Vec<RelativePath>,
    /// Restore requests with no trashed version to bring back.
    pub restore_missing: Vec<RelativePath>,
    /// Trashed versions not restored because a file of that name exists now.
    pub restore_skipped: Vec<RelativePath>,
    pub trash_emptied: Vec<String>,
    /// Folders on the store House Drive does not recognise, left alone.
    pub ignored: Vec<PathBuf>,
    /// False when the snapshot could not replace the old one (a reader holding it open); the
    /// next run tries again.
    pub snapshot_written: bool,
}

#[derive(Debug)]
pub enum MergeError {
    Io { path: PathBuf, source: io::Error },
    Catalog(CatalogError),
    Journal { path: PathBuf, error: JournalError },
    Unexpected(String),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            MergeError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            MergeError::Catalog(error) => write!(f, "{error}"),
            MergeError::Journal { path, error } => write!(f, "{}: {}", path.display(), error),
            MergeError::Unexpected(message) => write!(f, "unexpected state, stopped: {message}"),
        };
    }
}

impl std::error::Error for MergeError {}

impl From<CatalogError> for MergeError {
    fn from(error: CatalogError) -> MergeError {
        return MergeError::Catalog(error);
    }
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> MergeError + '_ {
    return move |source| MergeError::Io { path: path.to_path_buf(), source };
}

/// The store's space rules: one quota for the whole store, shared by every machine, and a margin
/// of free disk kept whatever the quota.
pub struct StoreLimits {
    pub quota: Option<u64>,
    pub free_margin: u64,
}

/// Room left during a merge. Placing a file from `incoming/` is a rename on the same disk, so it
/// takes no new disk space, but it does count toward the quota; refusing it frees its space.
struct Budget {
    used: u64,
    quota: Option<u64>,
    disk_ok: bool,
}

impl Budget {
    fn admit(&mut self, size: u64) -> bool {
        if !self.disk_ok {
            return false;
        }
        if let Some(quota) = self.quota
            && self.used + size > quota
        {
            return false;
        }
        self.used += size;
        return true;
    }
}

/// The store machine's whole run, in order: the trash (freeing room first), batches, snapshot.
/// `free_space` measures the store's disk, at the start and for the snapshot.
pub fn run(
    store: &Store,
    catalog: &mut Catalog,
    now: SystemTime,
    trash_days: u32,
    limits: &StoreLimits,
    free_space: &dyn Fn() -> u64,
) -> Result<MergeReport, MergeError> {
    let mut report = MergeReport::default();
    empty_trash(store, now, trash_days, &mut report)?;
    let used = catalog.total_size()? + folder_size(&store.trash_dir())?;
    let mut budget = Budget { used, quota: limits.quota, disk_ok: free_space() >= limits.free_margin };
    merge_batches(store, catalog, now, &mut budget, &mut report)?;
    let transaction = catalog.transaction()?;
    let day = 86_400_000_000_000i64;
    transaction.prune(hash::nanos(now) - trash_days as i64 * day)?;
    transaction.commit()?;
    let quota = Quota { used: budget.used, limit: limits.quota, free: free_space(), margin: limits.free_margin };
    report.quota = Some(quota);
    report.snapshot_written = write_snapshot(store, catalog, quota)?;
    return Ok(report);
}

fn folder_size(folder: &Path) -> Result<u64, MergeError> {
    let mut total = 0;
    let mut folders = vec![folder.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(MergeError::Io { path: folder, source: error }),
        };
        for entry in entries {
            let entry = entry.map_err(io_error(&folder))?;
            let metadata = entry.metadata().map_err(io_error(&folder))?;
            if metadata.is_dir() {
                folders.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    return Ok(total);
}

#[derive(Default)]
struct BatchFiles {
    has_folder: bool,
    has_journal: bool,
}

fn merge_batches(
    store: &Store,
    catalog: &mut Catalog,
    now: SystemTime,
    budget: &mut Budget,
    report: &mut MergeReport,
) -> Result<(), MergeError> {
    // Every batch of every machine, merged oldest first; batch names are push times.
    let mut batches: BTreeMap<(u128, String), BatchFiles> = BTreeMap::new();
    let mut newest: BTreeMap<String, u128> = BTreeMap::new();
    for (area, is_journal) in [("incoming", false), ("journal", true)] {
        let area_dir = store.root().join(area);
        for machine_entry in fs::read_dir(&area_dir).map_err(io_error(&area_dir))? {
            let machine_entry = machine_entry.map_err(io_error(&area_dir))?;
            let machine_dir = machine_entry.path();
            let machine = machine_entry.file_name().to_string_lossy().to_string();
            if !config::is_valid_machine_id(&machine) || !machine_dir.is_dir() {
                report.ignored.push(machine_dir);
                continue;
            }
            for entry in fs::read_dir(&machine_dir).map_err(io_error(&machine_dir))? {
                let entry = entry.map_err(io_error(&machine_dir))?;
                let name = entry.file_name().to_string_lossy().to_string();
                let stem = if is_journal { name.strip_suffix(".tsv") } else { Some(name.as_str()) };
                let Some(number) = stem.and_then(|stem| stem.parse::<u128>().ok()) else {
                    report.ignored.push(entry.path());
                    continue;
                };
                let files = batches.entry((number, machine.clone())).or_default();
                if is_journal {
                    files.has_journal = true;
                } else {
                    files.has_folder = true;
                }
                let machine_newest = newest.entry(machine.clone()).or_insert(number);
                *machine_newest = (*machine_newest).max(number);
            }
        }
    }

    for ((number, machine), files) in batches {
        let batch = store::batch_name(number);
        let batch_dir = store.incoming_dir(&machine).join(&batch);
        let journal_path = store.journal_dir(&machine).join(format!("{batch}.tsv"));
        let entries = if files.has_journal {
            let text = fs::read_to_string(&journal_path).map_err(io_error(&journal_path))?;
            journal::parse(&text).map_err(|error| MergeError::Journal { path: journal_path.clone(), error })?
        } else {
            None
        };
        match entries {
            Some(entries) => {
                merge_batch(store, catalog, &machine, number, &entries, now, budget, report)?;
                report.merged_batches += 1;
            }
            // A machine pushes its batches one after another: an incomplete batch older than
            // its newest can never complete.
            None if number < newest[&machine] => report.discarded_batches += 1,
            None => {
                report.waiting.push((machine, batch));
                continue;
            }
        }
        if files.has_folder {
            fs::remove_dir_all(&batch_dir).map_err(io_error(&batch_dir))?;
        }
        if files.has_journal {
            fs::remove_file(&journal_path).map_err(io_error(&journal_path))?;
        }
    }
    return Ok(());
}

fn merge_batch(
    store: &Store,
    catalog: &mut Catalog,
    machine: &str,
    number: u128,
    entries: &[Entry],
    now: SystemTime,
    budget: &mut Budget,
    report: &mut MergeReport,
) -> Result<(), MergeError> {
    let batch = store::batch_name(number);
    let batch_dir = store.incoming_dir(machine).join(&batch);
    let files_dir = store.files_dir();
    let trash_dir = store.trash_dir().join(dates::date_of(hash::nanos(now))).join(&batch);
    let transaction = catalog.transaction()?;

    for entry in entries {
        match entry {
            Entry::Put { path, size, hash, modified, base } => {
                let source = path.under(&batch_dir);
                let target = path.under(&files_dir);
                let record = CatalogFile {
                    path: path.clone(),
                    size: *size,
                    hash: hash.clone(),
                    modified: *modified,
                    machine: machine.to_string(),
                };
                let rejected = Rejected {
                    machine: machine.to_string(),
                    path: path.clone(),
                    hash: hash.clone(),
                    reason: RejectReason::Hash,
                };
                transaction.clear_reports(machine, path)?;

                if !source.exists() {
                    // Placed by an earlier run of this merge that stopped before committing, or
                    // never sent.
                    if hash_of(&target)?.as_deref() == Some(hash.as_str()) {
                        transaction.put_file(&record)?;
                    } else if let Some(copy) = placed_conflict_copy(&files_dir, path, machine, number, hash)? {
                        transaction.put_file(&CatalogFile { path: copy.clone(), ..record })?;
                        transaction.add_conflict(&Conflict {
                            copy,
                            original: path.clone(),
                            machine: machine.to_string(),
                        })?;
                    } else {
                        transaction.add_rejected(&rejected)?;
                        report.rejected.push((machine.to_string(), path.clone()));
                    }
                    continue;
                }
                let found_size = fs::metadata(&source).map_err(io_error(&source))?.len();
                if found_size != *size || hash_of(&source)?.as_deref() != Some(hash.as_str()) {
                    transaction.add_rejected(&rejected)?;
                    report.rejected.push((machine.to_string(), path.clone()));
                    continue;
                }

                let current = transaction.get(path)?;
                let adds_bytes = !matches!(&current, Some(current) if current.hash == *hash);
                if adds_bytes && !budget.admit(*size) {
                    transaction.add_rejected(&Rejected { reason: RejectReason::Quota, ..rejected })?;
                    report.over_quota.push((machine.to_string(), path.clone()));
                    continue;
                }
                match current {
                    // Pushed again after a stop between journal and state: nothing to do.
                    Some(current) if current.hash == *hash => {}
                    Some(current) if base.as_deref() == Some(current.hash.as_str()) => {
                        move_to_trash(&target, path, &trash_dir)?;
                        place(&source, &target)?;
                        transaction.put_file(&record)?;
                        report.replaced.push(path.clone());
                    }
                    Some(_) => {
                        let copy = new_conflict_copy(&transaction, &files_dir, path, machine, number)?;
                        place(&source, &copy.under(&files_dir))?;
                        transaction.put_file(&CatalogFile { path: copy.clone(), ..record })?;
                        transaction.add_conflict(&Conflict {
                            copy: copy.clone(),
                            original: path.clone(),
                            machine: machine.to_string(),
                        })?;
                        report.conflicts.push(copy);
                    }
                    // New to the store, or edited here while deleted elsewhere: an edit is kept.
                    None => {
                        place(&source, &target)?;
                        transaction.put_file(&record)?;
                        report.placed.push(path.clone());
                    }
                }
            }
            Entry::Restore { path } => {
                restore(store, &transaction, machine, path, report)?;
            }
            Entry::Delete { path, base } => {
                transaction.clear_reports(machine, path)?;
                match transaction.get(path)? {
                    None => {}
                    Some(current) if current.hash == *base => {
                        let target = path.under(&files_dir);
                        move_to_trash(&target, path, &trash_dir)?;
                        remove_empty_parents(&target, &files_dir);
                        transaction.remove_file(path)?;
                        report.deleted.push(path.clone());
                    }
                    // Changed on the store since the deleting machine last saw it: kept.
                    Some(_) => {
                        let refused = Refused { path: path.clone(), machine: machine.to_string(), base: base.clone() };
                        transaction.add_refused(&refused, hash::nanos(now))?;
                        report.refused.push(path.clone());
                    }
                }
            }
        }
    }
    transaction.commit()?;
    return Ok(());
}

fn hash_of(path: &Path) -> Result<Option<String>, MergeError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(MergeError::Io { path: path.to_path_buf(), source: error }),
    };
    return Ok(Some(hash::hash_reader(file).map_err(io_error(path))?));
}

fn place(source: &Path, target: &Path) -> Result<(), MergeError> {
    if target.exists() {
        return Err(MergeError::Unexpected(format!(
            "{} exists but the catalog does not list it",
            target.display()
        )));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::rename(source, target).map_err(io_error(target))?;
    return Ok(());
}

/// Moves a file of `files/` into the trash; already gone (moved by an earlier run) is fine.
fn move_to_trash(target: &Path, path: &RelativePath, trash_dir: &Path) -> Result<(), MergeError> {
    if !target.exists() {
        return Ok(());
    }
    let trashed = path.under(trash_dir);
    if trashed.exists() {
        return Err(MergeError::Unexpected(format!("{} is already in the trash", trashed.display())));
    }
    if let Some(parent) = trashed.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::rename(target, &trashed).map_err(io_error(target))?;
    return Ok(());
}

/// Removes the folders a deletion left empty, up to (not including) `root`.
pub(crate) fn remove_empty_parents(target: &Path, root: &Path) {
    let mut folder = target.parent();
    while let Some(path) = folder {
        if path == root || fs::remove_dir(path).is_err() {
            break;
        }
        folder = path.parent();
    }
}

/// `report.txt` becomes `report (conflict machine-b 2026-10-18).txt`, then ` 2`, ` 3` if taken;
/// the date is the batch's, so a repeated merge finds the same name.
fn conflict_copy(path: &RelativePath, machine: &str, number: u128, n: u32) -> RelativePath {
    let mut components: Vec<&str> = path.components().collect();
    let name = components.pop().unwrap_or_default();
    let (stem, extension) = match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    };
    let date = dates::date_of(number.min(i64::MAX as u128) as i64);
    let counter = if n == 1 { String::new() } else { format!(" {n}") };
    let copy_name = format!("{stem} (conflict {machine} {date}{counter}){extension}");
    components.push(&copy_name);
    // The pieces were valid and the added text is: parsing cannot fail.
    return RelativePath::parse(&components.join("/")).expect("a conflict name is a valid path");
}

fn new_conflict_copy(
    transaction: &CatalogTransaction,
    files_dir: &Path,
    path: &RelativePath,
    machine: &str,
    number: u128,
) -> Result<RelativePath, MergeError> {
    for n in 1..=MAX_CONFLICT_COPIES {
        let copy = conflict_copy(path, machine, number, n);
        if transaction.get(&copy)?.is_none() && !copy.under(files_dir).exists() {
            return Ok(copy);
        }
    }
    return Err(MergeError::Unexpected(format!("too many conflict copies of {path}")));
}

/// The conflict copy an earlier run of this merge already placed, found by its hash.
fn placed_conflict_copy(
    files_dir: &Path,
    path: &RelativePath,
    machine: &str,
    number: u128,
    hash: &str,
) -> Result<Option<RelativePath>, MergeError> {
    for n in 1..=MAX_CONFLICT_COPIES {
        let copy = conflict_copy(path, machine, number, n);
        let found = hash_of(&copy.under(files_dir))?;
        match found {
            None => return Ok(None),
            Some(found) if found == hash => return Ok(Some(copy)),
            Some(_) => {}
        }
    }
    return Ok(None);
}

/// Every trashed file, newest version per path: `trash/<date>/<batch>/<path>`, ordered by date
/// then batch.
fn newest_trashed(store: &Store) -> Result<BTreeMap<String, (RelativePath, PathBuf)>, MergeError> {
    let trash_dir = store.trash_dir();
    let mut versions: BTreeMap<String, ((i64, String), RelativePath, PathBuf)> = BTreeMap::new();
    for date_entry in fs::read_dir(&trash_dir).map_err(io_error(&trash_dir))? {
        let date_entry = date_entry.map_err(io_error(&trash_dir))?;
        let Some(day) = dates::parse_date(&date_entry.file_name().to_string_lossy()) else {
            continue;
        };
        let date_dir = date_entry.path();
        if !date_dir.is_dir() {
            continue;
        }
        for batch_entry in fs::read_dir(&date_dir).map_err(io_error(&date_dir))? {
            let batch_entry = batch_entry.map_err(io_error(&date_dir))?;
            let batch = batch_entry.file_name().to_string_lossy().to_string();
            let batch_dir = batch_entry.path();
            let mut folders = vec![(batch_dir.clone(), String::new())];
            while let Some((folder, prefix)) = folders.pop() {
                let Ok(entries) = fs::read_dir(&folder) else {
                    continue;
                };
                for entry in entries {
                    let entry = entry.map_err(io_error(&folder))?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let text = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                    if entry.path().is_dir() {
                        folders.push((entry.path(), text));
                        continue;
                    }
                    let Ok(path) = RelativePath::parse(&text) else {
                        continue;
                    };
                    let order = (day, batch.clone());
                    let key = crate::state::key(&path);
                    let newer = versions.get(&key).is_none_or(|(existing, _, _)| order > *existing);
                    if newer {
                        versions.insert(key, (order, path, entry.path()));
                    }
                }
            }
        }
    }
    return Ok(versions.into_iter().map(|(key, (_, path, file))| (key, (path, file))).collect());
}

/// Brings back the newest trashed version of `requested`, or of every file under it, where no
/// file of that name exists now. A rename on the store's own disk: the quota is unchanged, the
/// trash counting toward it as well.
fn restore(
    store: &Store,
    transaction: &CatalogTransaction,
    machine: &str,
    requested: &RelativePath,
    report: &mut MergeReport,
) -> Result<(), MergeError> {
    let files_dir = store.files_dir();
    let mut found = false;
    for (_, (path, trashed)) in newest_trashed(store)? {
        if !path.is_within(requested) {
            continue;
        }
        found = true;
        if transaction.get(&path)?.is_some() {
            report.restore_skipped.push(path);
            continue;
        }
        let Some(hash) = hash_of(&trashed)? else {
            continue;
        };
        let metadata = fs::metadata(&trashed).map_err(io_error(&trashed))?;
        let modified = metadata.modified().map(hash::nanos).unwrap_or(0);
        place(&trashed, &path.under(&files_dir))?;
        transaction.put_file(&CatalogFile {
            path: path.clone(),
            size: metadata.len(),
            hash,
            modified,
            machine: machine.to_string(),
        })?;
        report.restored.push(path);
    }
    if !found {
        report.restore_missing.push(requested.clone());
    }
    return Ok(());
}

fn empty_trash(
    store: &Store,
    now: SystemTime,
    trash_days: u32,
    report: &mut MergeReport,
) -> Result<(), MergeError> {
    let (emptied, ignored) = empty_trash_older_than(store, now, trash_days as i64)?;
    report.trash_emptied.extend(emptied);
    report.ignored.extend(ignored);
    return Ok(());
}

/// Removes trash folders more than `days` days old (`-1`: all of them, today's too), returning
/// the dates removed and the folders ignored. Only folders named as dates are touched.
pub fn empty_trash_older_than(
    store: &Store,
    now: SystemTime,
    days: i64,
) -> Result<(Vec<String>, Vec<PathBuf>), MergeError> {
    let trash_dir = store.trash_dir();
    let today = dates::day_of(hash::nanos(now));
    let mut emptied = Vec::new();
    let mut ignored = Vec::new();
    for entry in fs::read_dir(&trash_dir).map_err(io_error(&trash_dir))? {
        let entry = entry.map_err(io_error(&trash_dir))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(day) = dates::parse_date(&name) else {
            ignored.push(entry.path());
            continue;
        };
        if today - day > days {
            let path = entry.path();
            fs::remove_dir_all(&path).map_err(io_error(&path))?;
            emptied.push(name);
        }
    }
    return Ok((emptied, ignored));
}

/// Writes the snapshot under a staging name, then renames it over the old one, so a reader sees
/// either whole snapshot, never half of one.
fn write_snapshot(store: &Store, catalog: &Catalog, quota: Quota) -> Result<bool, MergeError> {
    let mut snapshot = catalog.snapshot()?;
    snapshot.quota = Some(quota);
    let text = catalog::format_snapshot(&snapshot);
    let staging = store.root().join(CATALOG_STAGING);
    let snapshot = store.root().join(CATALOG_SNAPSHOT);
    let file = File::create(&staging).map_err(io_error(&staging))?;
    {
        use std::io::Write;
        let mut writer = io::BufWriter::new(&file);
        writer.write_all(text.as_bytes()).map_err(io_error(&staging))?;
        writer.flush().map_err(io_error(&staging))?;
    }
    file.sync_all().map_err(io_error(&staging))?;
    drop(file);
    return Ok(fs::rename(&staging, &snapshot).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::time::Duration;

    const A: &str = "machine-a";
    const B: &str = "machine-b";
    const DAY: u64 = 86_400;

    struct Setup {
        _root: TempDir,
        store: Store,
        catalog: Catalog,
        next: u128,
    }

    fn setup(name: &str) -> Setup {
        let root = TempDir::new(name);
        let id = Store::init(&root.0).unwrap();
        let store = Store::open(&root.0, &id).unwrap();
        return Setup { _root: root, store, catalog: Catalog::open_in_memory().unwrap(), next: 1_000_000_000_000_000_000 };
    }

    fn now() -> SystemTime {
        return SystemTime::UNIX_EPOCH + Duration::from_secs(20_000 * DAY);
    }

    fn h(content: &[u8]) -> String {
        return hash::hash_reader(content).unwrap();
    }

    fn path(text: &str) -> RelativePath {
        return RelativePath::parse(text).unwrap();
    }

    fn put(text: &str, content: &[u8], base: Option<&[u8]>) -> (Entry, Vec<u8>) {
        let entry = Entry::Put {
            path: path(text),
            size: content.len() as u64,
            hash: h(content),
            modified: 1,
            base: base.map(h),
        };
        return (entry, content.to_vec());
    }

    /// Writes a batch as a pushing machine would; returns its number.
    fn push_batch(setup: &mut Setup, machine: &str, puts: &[(Entry, Vec<u8>)], deletes: &[Entry], complete: bool) -> u128 {
        let number = setup.next;
        setup.next += 1;
        let batch = store::batch_name(number);
        let batch_dir = setup.store.incoming_dir(machine).join(&batch);
        let mut entries = Vec::new();
        for (entry, content) in puts {
            let Entry::Put { path, .. } = entry else { unreachable!() };
            let file = path.under(&batch_dir);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
            entries.push(entry.clone());
        }
        entries.extend(deletes.iter().cloned());
        let journal_dir = setup.store.journal_dir(machine);
        fs::create_dir_all(&journal_dir).unwrap();
        let mut text = journal::format(&entries);
        if !complete {
            text = text.lines().take(entries.len()).collect::<Vec<_>>().join("\n");
        }
        fs::write(journal_dir.join(format!("{batch}.tsv")), text).unwrap();
        return number;
    }

    const NO_LIMITS: StoreLimits = StoreLimits { quota: None, free_margin: 0 };

    fn run(setup: &mut Setup) -> MergeReport {
        return run_with(setup, &NO_LIMITS, 1 << 40);
    }

    fn run_with(setup: &mut Setup, limits: &StoreLimits, free: u64) -> MergeReport {
        return super::run(&setup.store, &mut setup.catalog, now(), 30, limits, &|| free).unwrap();
    }

    fn stored(setup: &Setup, text: &str) -> Option<Vec<u8>> {
        return fs::read(path(text).under(&setup.store.files_dir())).ok();
    }

    fn trashed(setup: &Setup) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        let mut folders = vec![setup.store.trash_dir()];
        while let Some(folder) = folders.pop() {
            for entry in fs::read_dir(folder).unwrap() {
                let entry = entry.unwrap();
                if entry.path().is_dir() {
                    folders.push(entry.path());
                } else {
                    found.push(fs::read(entry.path()).unwrap());
                }
            }
        }
        return found;
    }

    #[test]
    fn places_new_files_and_clears_the_batch() {
        let mut setup = setup("merge-new");
        push_batch(&mut setup, A, &[put("a/one.txt", b"one", None)], &[], true);
        let report = run(&mut setup);
        assert_eq!(report.merged_batches, 1);
        assert_eq!(report.placed, vec![path("a/one.txt")]);
        assert_eq!(stored(&setup, "a/one.txt"), Some(b"one".to_vec()));
        assert_eq!(fs::read_dir(setup.store.incoming_dir(A)).unwrap().count(), 0);
        assert_eq!(fs::read_dir(setup.store.journal_dir(A)).unwrap().count(), 0);
        let text = fs::read_to_string(setup.store.root().join(CATALOG_SNAPSHOT)).unwrap();
        let snapshot = catalog::parse_snapshot(&text).unwrap().unwrap();
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].hash, h(b"one"));
        assert!(report.snapshot_written);
    }

    #[test]
    fn replaces_on_the_right_base_keeping_the_old_version_in_the_trash() {
        let mut setup = setup("merge-replace");
        push_batch(&mut setup, A, &[put("a.txt", b"v1", None)], &[], true);
        run(&mut setup);
        push_batch(&mut setup, A, &[put("a.txt", b"v2", Some(b"v1"))], &[], true);
        let report = run(&mut setup);
        assert_eq!(report.replaced, vec![path("a.txt")]);
        assert_eq!(stored(&setup, "a.txt"), Some(b"v2".to_vec()));
        assert_eq!(trashed(&setup), vec![b"v1".to_vec()]);
    }

    #[test]
    fn a_stale_base_makes_a_conflict_copy() {
        let mut setup = setup("merge-conflict");
        push_batch(&mut setup, A, &[put("r/report.txt", b"v1", None)], &[], true);
        run(&mut setup);
        push_batch(&mut setup, A, &[put("r/report.txt", b"v2a", Some(b"v1"))], &[], true);
        push_batch(&mut setup, B, &[put("r/report.txt", b"v2b", Some(b"v1"))], &[], true);
        let report = run(&mut setup);
        let date = dates::date_of(1_000_000_000_000_000_000);
        let copy = path(&format!("r/report (conflict machine-b {date}).txt"));
        assert_eq!(report.conflicts, vec![copy.clone()]);
        assert_eq!(stored(&setup, "r/report.txt"), Some(b"v2a".to_vec()));
        assert_eq!(stored(&setup, copy.as_str()), Some(b"v2b".to_vec()));
        let snapshot = setup.catalog.snapshot().unwrap();
        assert_eq!(snapshot.conflicts, vec![Conflict { copy, original: path("r/report.txt"), machine: B.into() }]);
    }

    #[test]
    fn the_same_content_again_is_no_conflict() {
        let mut setup = setup("merge-again");
        push_batch(&mut setup, A, &[put("a.txt", b"v1", None)], &[], true);
        run(&mut setup);
        push_batch(&mut setup, A, &[put("a.txt", b"v1", None)], &[], true);
        let report = run(&mut setup);
        assert!(report.conflicts.is_empty() && report.replaced.is_empty() && report.placed.is_empty());
        assert!(trashed(&setup).is_empty());
    }

    #[test]
    fn deletes_on_the_right_base_and_refuses_on_a_stale_one() {
        let mut setup = setup("merge-delete");
        push_batch(&mut setup, A, &[put("d/a.txt", b"a1", None), put("b.txt", b"b2", None)], &[], true);
        run(&mut setup);
        let deletes = [
            Entry::Delete { path: path("d/a.txt"), base: h(b"a1") },
            Entry::Delete { path: path("b.txt"), base: h(b"b1") },
        ];
        push_batch(&mut setup, B, &[], &deletes, true);
        let report = run(&mut setup);
        assert_eq!(report.deleted, vec![path("d/a.txt")]);
        assert_eq!(report.refused, vec![path("b.txt")]);
        assert_eq!(stored(&setup, "d/a.txt"), None);
        assert!(!setup.store.files_dir().join("d").exists());
        assert_eq!(stored(&setup, "b.txt"), Some(b"b2".to_vec()));
        assert_eq!(trashed(&setup), vec![b"a1".to_vec()]);
        assert_eq!(setup.catalog.snapshot().unwrap().refused.len(), 1);
    }

    #[test]
    fn a_hash_mismatch_is_rejected_and_not_placed() {
        let mut setup = setup("merge-mismatch");
        let (entry, _) = put("a.txt", b"sent", None);
        push_batch(&mut setup, A, &[(entry, b"corrupt".to_vec())], &[], true);
        let report = run(&mut setup);
        assert_eq!(report.rejected, vec![(A.to_string(), path("a.txt"))]);
        assert_eq!(stored(&setup, "a.txt"), None);
        assert_eq!(setup.catalog.snapshot().unwrap().rejected.len(), 1);

        push_batch(&mut setup, A, &[put("a.txt", b"sent", None)], &[], true);
        run(&mut setup);
        assert!(setup.catalog.snapshot().unwrap().rejected.is_empty());
    }

    #[test]
    fn the_newest_incomplete_batch_waits_an_older_one_is_discarded() {
        let mut setup = setup("merge-incomplete");
        push_batch(&mut setup, A, &[put("old.txt", b"o", None)], &[], false);
        let report = run(&mut setup);
        assert_eq!(report.waiting.len(), 1);
        assert_eq!(stored(&setup, "old.txt"), None);

        push_batch(&mut setup, A, &[put("new.txt", b"n", None)], &[], true);
        let report = run(&mut setup);
        assert_eq!(report.discarded_batches, 1);
        assert_eq!(report.placed, vec![path("new.txt")]);
        assert_eq!(stored(&setup, "old.txt"), None);
        assert_eq!(fs::read_dir(setup.store.incoming_dir(A)).unwrap().count(), 0);
    }

    #[test]
    fn a_merge_stopped_after_moving_finishes_on_the_next_run() {
        let mut setup = setup("merge-resume");
        let number = push_batch(&mut setup, A, &[put("a.txt", b"v1", None)], &[], true);
        // As if an earlier run had moved the file, then stopped before committing the catalog.
        let batch_dir = setup.store.incoming_dir(A).join(store::batch_name(number));
        fs::rename(batch_dir.join("a.txt"), setup.store.files_dir().join("a.txt")).unwrap();
        let report = run(&mut setup);
        assert!(report.rejected.is_empty());
        assert_eq!(setup.catalog.get(&path("a.txt")).unwrap().unwrap().hash, h(b"v1"));
    }

    #[test]
    fn a_file_on_disk_the_catalog_does_not_list_stops_the_run() {
        let mut setup = setup("merge-unexpected");
        fs::write(setup.store.files_dir().join("a.txt"), b"stray").unwrap();
        push_batch(&mut setup, A, &[put("a.txt", b"v1", None)], &[], true);
        let result = super::run(&setup.store, &mut setup.catalog, now(), 30, &NO_LIMITS, &|| 1 << 40);
        assert!(matches!(result, Err(MergeError::Unexpected(_))));
        assert_eq!(fs::read(setup.store.files_dir().join("a.txt")).unwrap(), b"stray");
    }

    #[test]
    fn files_past_the_quota_are_refused_and_listed() {
        let mut setup = setup("merge-quota");
        let limits = StoreLimits { quota: Some(10), free_margin: 0 };
        push_batch(&mut setup, A, &[put("a.bin", b"123456", None), put("b.bin", b"123456", None)], &[], true);
        let report = run_with(&mut setup, &limits, 1 << 40);
        assert_eq!(report.placed, vec![path("a.bin")]);
        assert_eq!(report.over_quota, vec![(A.to_string(), path("b.bin"))]);
        assert_eq!(stored(&setup, "b.bin"), None);
        assert_eq!(fs::read_dir(setup.store.incoming_dir(A)).unwrap().count(), 0);
        let snapshot = setup.catalog.snapshot().unwrap();
        assert_eq!(snapshot.rejected[0].reason, RejectReason::Quota);
        assert_eq!(report.quota.unwrap().used, 6);

        // The same content again takes no room; a replacement's old version stays counted.
        push_batch(&mut setup, A, &[put("a.bin", b"123456", None)], &[], true);
        assert!(run_with(&mut setup, &limits, 1 << 40).over_quota.is_empty());
        push_batch(&mut setup, A, &[put("a.bin", b"1234567", Some(b"123456"))], &[], true);
        assert_eq!(run_with(&mut setup, &limits, 1 << 40).over_quota.len(), 1);
    }

    #[test]
    fn the_trash_counts_toward_the_quota() {
        let mut setup = setup("merge-quota-trash");
        let today = dates::date_of(hash::nanos(now()));
        fs::create_dir_all(setup.store.trash_dir().join(&today)).unwrap();
        fs::write(setup.store.trash_dir().join(&today).join("old"), b"12345678").unwrap();
        push_batch(&mut setup, A, &[put("a.bin", b"123", None)], &[], true);
        let report = run_with(&mut setup, &StoreLimits { quota: Some(10), free_margin: 0 }, 1 << 40);
        assert_eq!(report.over_quota.len(), 1);
        assert_eq!(report.quota.unwrap().used, 8);
    }

    #[test]
    fn under_the_free_space_margin_new_files_wait_but_deletions_pass() {
        let mut setup = setup("merge-margin");
        push_batch(&mut setup, A, &[put("a.txt", b"a", None)], &[], true);
        run(&mut setup);
        let limits = StoreLimits { quota: None, free_margin: 100 };
        push_batch(&mut setup, A, &[put("b.txt", b"b", None)], &[Entry::Delete { path: path("a.txt"), base: h(b"a") }], true);
        let report = run_with(&mut setup, &limits, 50);
        assert_eq!(report.deleted, vec![path("a.txt")]);
        assert_eq!(report.over_quota, vec![(A.to_string(), path("b.txt"))]);
        let quota = report.quota.unwrap();
        assert_eq!((quota.free, quota.margin, quota.room()), (50, 100, 0));
    }

    #[test]
    fn restore_brings_back_the_newest_trashed_version() {
        let mut setup = setup("merge-restore");
        push_batch(&mut setup, A, &[put("d/a.txt", b"v1", None), put("d/b.txt", b"b", None)], &[], true);
        run(&mut setup);
        push_batch(&mut setup, A, &[put("d/a.txt", b"v2", Some(b"v1"))], &[], true);
        run(&mut setup);
        let deletes = [
            Entry::Delete { path: path("d/a.txt"), base: h(b"v2") },
            Entry::Delete { path: path("d/b.txt"), base: h(b"b") },
        ];
        push_batch(&mut setup, A, &[], &deletes, true);
        run(&mut setup);
        assert_eq!(stored(&setup, "d/a.txt"), None);

        push_batch(&mut setup, B, &[], &[Entry::Restore { path: path("d") }], true);
        let report = run(&mut setup);
        let mut restored = report.restored.clone();
        restored.sort();
        assert_eq!(restored, vec![path("d/a.txt"), path("d/b.txt")]);
        assert_eq!(stored(&setup, "d/a.txt"), Some(b"v2".to_vec()));
        assert_eq!(setup.catalog.get(&path("d/a.txt")).unwrap().unwrap().hash, h(b"v2"));

        // Again: the v1 version is still in the trash, but d/a.txt exists now.
        push_batch(&mut setup, B, &[], &[Entry::Restore { path: path("d/a.txt") }], true);
        let report = run(&mut setup);
        assert_eq!(report.restore_skipped, vec![path("d/a.txt")]);
        assert_eq!(stored(&setup, "d/a.txt"), Some(b"v2".to_vec()));

        push_batch(&mut setup, B, &[], &[Entry::Restore { path: path("never") }], true);
        assert_eq!(run(&mut setup).restore_missing, vec![path("never")]);
    }

    #[test]
    fn emptying_the_whole_trash_includes_today() {
        let setup = setup("merge-empty-all");
        let today = dates::date_of(hash::nanos(now()));
        fs::create_dir_all(setup.store.trash_dir().join(&today)).unwrap();
        let (emptied, _) = empty_trash_older_than(&setup.store, now(), -1).unwrap();
        assert_eq!(emptied, vec![today]);
    }

    #[test]
    fn the_trash_is_emptied_after_trash_days_only() {
        let mut setup = setup("merge-trash");
        let today = dates::day_of(hash::nanos(now()));
        let day_nanos = 86_400_000_000_000i64;
        let old = dates::date_of((today - 31) * day_nanos);
        let recent = dates::date_of((today - 30) * day_nanos);
        for date in [&old, &recent] {
            fs::create_dir_all(setup.store.trash_dir().join(date)).unwrap();
            fs::write(setup.store.trash_dir().join(date).join("x"), b"x").unwrap();
        }
        fs::create_dir_all(setup.store.trash_dir().join("notes")).unwrap();
        let report = run(&mut setup);
        assert_eq!(report.trash_emptied, vec![old.clone()]);
        assert!(!setup.store.trash_dir().join(&old).exists());
        assert!(setup.store.trash_dir().join(&recent).exists());
        assert!(setup.store.trash_dir().join("notes").exists());
    }
}
