//! Pushing: every file whose hash differs from the one last pushed, and every pending deletion,
//! sent to the store in batches.
//!
//! A batch is its files under `incoming/<machine_id>/<batch>/`, then its journal. A file counts
//! as pushed only once its batch's journal is complete, so a push cut short leaves the state as
//! it was and its files go again in a later batch. Batches are pushed one after another with
//! increasing names, so the store machine discards a batch whose journal never completed once a
//! newer batch of the same machine exists; a pushing machine cannot do it, being allowed to
//! create files only.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::hash::{self, Stamp};
use crate::journal::{self, Entry};
use crate::paths::RelativePath;
use crate::state::{FileRecord, PendingDeletion, State, StateError};
use crate::store::{self, Store, StoreError};

#[derive(Debug, Default)]
pub struct PushReport {
    pub batches: Vec<String>,
    pub pushed: Vec<RelativePath>,
    pub bytes: u64,
    pub deletions: usize,
    /// Changed, busy or gone since the scan: left for a later run.
    pub skipped: Vec<RelativePath>,
    /// Not pushed for lack of room on the store, as its last snapshot reported.
    pub held: Vec<RelativePath>,
}

#[derive(Debug)]
pub enum PushError {
    Io { path: PathBuf, source: io::Error },
    Store(StoreError),
    State(StateError),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            PushError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            PushError::Store(error) => write!(f, "{error}"),
            PushError::State(error) => write!(f, "{error}"),
        };
    }
}

impl std::error::Error for PushError {}

impl From<StoreError> for PushError {
    fn from(error: StoreError) -> PushError {
        return PushError::Store(error);
    }
}

impl From<StateError> for PushError {
    fn from(error: StateError) -> PushError {
        return PushError::State(error);
    }
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> PushError + '_ {
    return move |source| PushError::Io { path: path.to_path_buf(), source };
}

/// `room`: bytes the store can still take (the quota and its free-space margin, from its last
/// snapshot); files that do not fit are held here, while deletions always go.
pub fn push(
    store: &Store,
    machine_id: &str,
    data_root: &Path,
    state: &mut State,
    batch_bytes: u64,
    room: u64,
    now: SystemTime,
) -> Result<PushReport, PushError> {
    let mut room = room;
    let files: Vec<FileRecord> = state
        .files()?
        .into_iter()
        .filter(|record| record.pushed_hash.as_deref() != Some(record.hash.as_str()))
        .collect();
    let mut deletions = Some(state.deletions()?);
    let mut report = PushReport::default();
    if files.is_empty() && deletions.as_ref().is_some_and(Vec::is_empty) {
        return Ok(report);
    }
    let incoming_dir = store.incoming_dir(machine_id);
    let journal_dir = store.journal_dir(machine_id);
    fs::create_dir_all(&incoming_dir).map_err(io_error(&incoming_dir))?;
    fs::create_dir_all(&journal_dir).map_err(io_error(&journal_dir))?;

    let mut newest = store.newest_batch(machine_id)?;
    let mut queue = files.into_iter().peekable();
    loop {
        let number = store::next_batch(now, newest);
        newest = Some(number);
        let name = store::batch_name(number);
        let batch_dir = incoming_dir.join(&name);

        let mut entries = Vec::new();
        let mut pushed = Vec::new();
        let mut bytes = 0u64;
        // Deletions are small: all of them go in the first batch.
        let batch_deletions: Vec<PendingDeletion> = deletions.take().unwrap_or_default();
        for deletion in &batch_deletions {
            entries.push(Entry::Delete { path: deletion.path.clone(), base: deletion.base.clone() });
        }
        while let Some(record) = queue.peek() {
            if bytes > 0 && bytes + record.size > batch_bytes {
                break;
            }
            let record = queue.next().unwrap();
            if record.size > room {
                report.held.push(record.path);
                continue;
            }
            if !copy_into(&record, data_root, &batch_dir)? {
                report.skipped.push(record.path);
                continue;
            }
            room -= record.size;
            bytes += record.size;
            entries.push(Entry::Put {
                path: record.path.clone(),
                size: record.size,
                hash: record.hash.clone(),
                modified: record.modified,
                base: record.store_hash.clone(),
            });
            pushed.push((record.path, record.hash));
        }
        if entries.is_empty() {
            break;
        }

        let journal_path = journal_dir.join(format!("{name}.tsv"));
        let mut journal_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&journal_path)
            .map_err(io_error(&journal_path))?;
        journal_file
            .write_all(journal::format(&entries).as_bytes())
            .map_err(io_error(&journal_path))?;
        journal_file.sync_all().map_err(io_error(&journal_path))?;
        state.mark_pushed(&pushed, &batch_deletions)?;

        report.batches.push(name);
        report.bytes += bytes;
        report.deletions += batch_deletions.len();
        report.pushed.extend(pushed.into_iter().map(|(path, _)| path));
        if queue.peek().is_none() {
            break;
        }
    }
    return Ok(report);
}

/// Copies one file into the batch, hashing as it goes. False when the file is not as the scan
/// found it (changed, busy, gone, or hashing differently); a partial copy then stays in the
/// batch, unlisted by the journal, and the store machine discards it with the batch's folder.
fn copy_into(record: &FileRecord, data_root: &Path, batch_dir: &Path) -> Result<bool, PushError> {
    let source_path = record.path.under(data_root);
    let source = match hash::open_unwritten(&source_path) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(PushError::Io { path: source_path, source: error }),
    };
    let scanned = Stamp { size: record.size, modified: record.modified };
    let before = Stamp::of(&source.metadata().map_err(io_error(&source_path))?);
    if before != scanned {
        return Ok(false);
    }
    let target_path = record.path.under(batch_dir);
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    let target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target_path)
        .map_err(io_error(&target_path))?;
    let hash = hash::copy_hashing(&source, &target).map_err(io_error(&source_path))?;
    target.sync_all().map_err(io_error(&target_path))?;
    let after = Stamp::of(&source.metadata().map_err(io_error(&source_path))?);
    return Ok(hash == record.hash && after == scanned);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use crate::test_support::TempDir;
    use std::time::Duration;

    const MACHINE: &str = "machine-a";

    struct Setup {
        data: TempDir,
        _store_root: TempDir,
        store: Store,
        state: State,
    }

    fn setup(name: &str) -> Setup {
        let data = TempDir::new(&format!("{name}-data"));
        let store_root = TempDir::new(&format!("{name}-store"));
        let id = Store::init(&store_root.0).unwrap();
        let store = Store::open(&store_root.0, &id).unwrap();
        return Setup { data, _store_root: store_root, store, state: State::open_in_memory().unwrap() };
    }

    fn later() -> SystemTime {
        return SystemTime::now() + Duration::from_secs(3600);
    }

    fn scan(setup: &mut Setup) {
        scan::scan(&setup.data.0, &mut setup.state, later(), Duration::from_secs(60)).unwrap();
    }

    fn push(setup: &mut Setup, batch_bytes: u64) -> PushReport {
        return push_with_room(setup, batch_bytes, u64::MAX);
    }

    fn push_with_room(setup: &mut Setup, batch_bytes: u64, room: u64) -> PushReport {
        return super::push(&setup.store, MACHINE, &setup.data.0, &mut setup.state, batch_bytes, room, later())
            .unwrap();
    }

    fn journal(setup: &Setup, batch: &str) -> Vec<Entry> {
        let path = setup.store.journal_dir(MACHINE).join(format!("{batch}.tsv"));
        return journal::parse(&fs::read_to_string(path).unwrap()).unwrap().unwrap();
    }

    #[test]
    fn pushes_new_files_once() {
        let mut setup = setup("push-new");
        fs::create_dir_all(setup.data.0.join("a")).unwrap();
        fs::write(setup.data.0.join("a").join("one.txt"), b"one").unwrap();
        scan(&mut setup);

        let report = push(&mut setup, 1 << 30);
        assert_eq!(report.batches.len(), 1);
        assert_eq!(report.pushed, vec![RelativePath::parse("a/one.txt").unwrap()]);
        let batch = &report.batches[0];
        let copy = setup.store.incoming_dir(MACHINE).join(batch).join("a").join("one.txt");
        assert_eq!(fs::read(copy).unwrap(), b"one");
        let entries = journal(&setup, batch);
        assert!(matches!(&entries[..], [Entry::Put { size: 3, base: None, .. }]));
        let record = setup.state.files().unwrap().remove(0);
        assert_eq!(record.pushed_hash, Some(record.hash));

        let again = push(&mut setup, 1 << 30);
        assert!(again.batches.is_empty());
    }

    #[test]
    fn splits_into_increasing_batches() {
        let mut setup = setup("push-split");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(setup.data.0.join(name), b"0123456789").unwrap();
        }
        scan(&mut setup);
        let report = push(&mut setup, 15);
        assert_eq!(report.batches.len(), 3);
        assert!(report.batches.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(report.bytes, 30);
    }

    #[test]
    fn pushes_deletions_with_their_base() {
        let mut setup = setup("push-delete");
        fs::write(setup.data.0.join("a.txt"), b"a").unwrap();
        scan(&mut setup);
        push(&mut setup, 1 << 30);
        let hash = setup.state.files().unwrap()[0].hash.clone();
        fs::remove_file(setup.data.0.join("a.txt")).unwrap();
        scan(&mut setup);

        let report = push(&mut setup, 1 << 30);
        assert_eq!(report.deletions, 1);
        let entries = journal(&setup, &report.batches[0]);
        assert_eq!(
            entries,
            vec![Entry::Delete { path: RelativePath::parse("a.txt").unwrap(), base: hash }]
        );
        assert!(setup.state.deletions().unwrap().is_empty());
    }

    #[test]
    fn a_file_changed_since_the_scan_waits() {
        let mut setup = setup("push-changed");
        fs::write(setup.data.0.join("a.txt"), b"a").unwrap();
        scan(&mut setup);
        fs::write(setup.data.0.join("a.txt"), b"changed").unwrap();

        let report = push(&mut setup, 1 << 30);
        assert!(report.batches.is_empty());
        assert_eq!(report.skipped, vec![RelativePath::parse("a.txt").unwrap()]);
        assert_eq!(setup.state.files().unwrap()[0].pushed_hash, None);
    }

    #[test]
    fn files_that_do_not_fit_are_held_deletions_still_go() {
        let mut setup = setup("push-room");
        fs::write(setup.data.0.join("gone.txt"), b"g").unwrap();
        scan(&mut setup);
        push(&mut setup, 1 << 30);
        fs::remove_file(setup.data.0.join("gone.txt")).unwrap();
        fs::write(setup.data.0.join("big.bin"), b"0123456789").unwrap();
        fs::write(setup.data.0.join("small.bin"), b"0123").unwrap();
        scan(&mut setup);

        let report = push_with_room(&mut setup, 1 << 30, 5);
        assert_eq!(report.held, vec![RelativePath::parse("big.bin").unwrap()]);
        assert_eq!(report.pushed, vec![RelativePath::parse("small.bin").unwrap()]);
        assert_eq!(report.deletions, 1);

        let report = push_with_room(&mut setup, 1 << 30, 0);
        assert_eq!(report.held, vec![RelativePath::parse("big.bin").unwrap()]);
        assert!(report.batches.is_empty());
    }

    #[test]
    fn a_new_batch_follows_an_interrupted_one() {
        let mut setup = setup("push-interrupted");
        let stale = store::batch_name(store::next_batch(later(), None) + 1_000_000_000);
        fs::create_dir_all(setup.store.incoming_dir(MACHINE).join(&stale)).unwrap();
        fs::write(setup.data.0.join("a.txt"), b"a").unwrap();
        scan(&mut setup);
        let report = push(&mut setup, 1 << 30);
        assert!(report.batches[0] > stale);
    }
}
