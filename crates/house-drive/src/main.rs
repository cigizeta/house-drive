//! `house-drive.exe`: argument parsing and output only, over house-drive-core.

mod console;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use house_drive_core::catalog::{Catalog, Quota, Snapshot};
use house_drive_core::paths::RelativePath;
use house_drive_core::reconcile::{ByHand, Resolution};
use house_drive_core::config::{self, Config};
use house_drive_core::credentials::{self, Credentials};
use house_drive_core::disk;
use house_drive_core::lock::{self, RunLock};
use house_drive_core::merge::{self, StoreLimits};
use house_drive_core::network;
use house_drive_core::push;
use house_drive_core::reconcile::{self, Limits};
use house_drive_core::scan;
use house_drive_core::share::{self, ConnectError};
use house_drive_core::state::State;
use house_drive_core::store::{self, Store};

const USAGE: &str = "usage: house-drive <command>

commands:
  status             this machine's cache, pending work, conflicts and the store's space
  sync               scan, push, merge (store machine), then verify, fetch, delete and evict
  pin <path>         keep a file or folder on this machine, later additions too
  unpin <path>       stop keeping it (it may then be evicted)
  fetch <path>       bring a file or folder from the store now
  evict <path>       free a file or folder's local copies, kept on the store
  delete <path>      delete a file or folder everywhere (the store keeps it in its trash)
  restore <path>     bring a file or folder back from the store's trash
  resolve <copy> keep-original | keep-copy | keep-both <new path>
                     settle a conflict
  schedule           sync by itself every sync_minutes and at logon (Task Scheduler)
  unschedule         stop syncing by itself
  join               set this machine up, once, interactively
  set-credentials    store the share's user name and password again
  networks           list the networks this machine is connected to
  config             check this machine's configuration and show it
  scan               record new, changed and deleted files in the data root (pushes nothing)
  push               scan, then push new, changed and deleted files to the store
  init-store <path>  create a store in an empty folder, once, on the store machine
  merge              on the store machine: merge pushed batches, empty the trash, write the catalog
  empty-trash [--older-than <days>]
                     on the store machine: empty the trash now, irreversibly

<path> is inside the data folder: absolute, relative to the current folder, or to the data folder.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["status"] => status(),
        ["pin", path] => pin(path, true),
        ["unpin", path] => pin(path, false),
        ["fetch", path] => fetch(path),
        ["evict", path] => evict(path),
        ["delete", path] => delete(path),
        ["restore", path] => restore(path),
        ["resolve", copy, "keep-original"] => resolve(copy, None, false),
        ["resolve", copy, "keep-copy"] => resolve(copy, None, true),
        ["resolve", copy, "keep-both", new_path] => resolve(copy, Some(*new_path), false),
        ["empty-trash"] => empty_trash(None),
        ["empty-trash", "--older-than", days] => match days.parse::<i64>() {
            Ok(days) if days >= 0 => empty_trash(Some(days)),
            _ => Err(format!("--older-than takes a number of days, not {days}")),
        },
        ["schedule"] => schedule(),
        ["unschedule"] => unschedule(),
        ["join"] => join(),
        ["set-credentials"] => set_credentials(),
        ["networks"] => show_networks(),
        ["config"] => show_config(),
        ["scan"] => run_scan_only(),
        ["push"] => run_sync(false),
        ["sync"] => run_sync(true),
        ["init-store", path] => init_store(Path::new(path)),
        ["merge"] => run_merge(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    return match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("house-drive: {message}");
            ExitCode::FAILURE
        }
    };
}

fn load_config() -> Result<Config, String> {
    let path = config::default_path().map_err(|error| error.to_string())?;
    return config::read(&path).map_err(|error| error.to_string());
}

fn show_config() -> Result<(), String> {
    let path = config::default_path().map_err(|error| error.to_string())?;
    let config = config::read(&path).map_err(|error| error.to_string())?;
    println!("configuration    {}", path.display());
    println!("machine_id       {}", config.machine_id);
    println!("data_root        {}", config.data_root.display());
    println!("store_root       {}", config.store_root.display());
    println!("is_store         {}", config.is_store);
    println!("sync_minutes     {}", config.sync_minutes);
    println!("cache_limit_gb   {}", config.cache_limit_gb);
    println!("evict_to_percent {}", config.evict_to_percent);
    println!("trash_days       {}", config.trash_days);
    println!("settle_seconds   {}", config.settle_seconds);
    println!("batch_mb         {}", config.batch_mb);
    return Ok(());
}

fn local_dir() -> Result<PathBuf, String> {
    let local_dir = config::local_dir().map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&local_dir)
        .map_err(|error| format!("cannot create {}: {error}", local_dir.display()))?;
    return Ok(local_dir);
}

/// One run at a time on this machine; `None` while another holds the lock.
fn take_lock() -> Result<Option<RunLock>, String> {
    let path = local_dir()?.join("sync.lock");
    return lock::try_lock(&path).map_err(|error| format!("{}: {error}", path.display()));
}

/// The store, if this run may and can reach it: the home network gate, the share connection
/// with the stored credentials, then the store's ID. `None` (nothing sent) away from home or
/// with the store off, which are ordinary states.
fn reach_store(config: &Config) -> Result<Option<Store>, String> {
    if !config.is_store {
        let networks = network::connected_networks()?;
        if !network::on_home_network(&networks, &config.home_network_id) {
            println!("not on the home network: nothing sent");
            return Ok(None);
        }
        let Some(credentials) = credentials::read()? else {
            return Err("no credentials stored for the share: run house-drive set-credentials".to_string());
        };
        match share::connect(&config.store_root, &credentials) {
            Ok(()) => {}
            Err(ConnectError::Unreachable(_)) => {
                println!("the store is unreachable (off?): nothing sent");
                return Ok(None);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let store = Store::open(&config.store_root, &config.store_id).map_err(|error| error.to_string())?;
    return Ok(Some(store));
}

fn run_scan_only() -> Result<(), String> {
    let config = load_config()?;
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    run_scan(&config)?;
    return Ok(());
}

/// Scans and prints the report, returning the state for what follows.
fn run_scan(config: &Config) -> Result<State, String> {
    let mut state = State::open(&local_dir()?.join("state.sqlite")).map_err(|error| error.to_string())?;
    let settle = Duration::from_secs(config.settle_seconds);
    let report = scan::scan(&config.data_root, &mut state, SystemTime::now(), settle)
        .map_err(|error| error.to_string())?;
    println!("unchanged  {}", report.unchanged);
    println!("new        {}", report.new.len());
    println!("changed    {}", report.changed.len());
    println!("deleted    {}", report.deleted.len());
    println!("settling   {}", report.settling.len());
    for path in &report.busy {
        println!("busy       {path}");
    }
    for (path, error) in &report.unreadable {
        println!("unreadable {path}: {error}");
    }
    for refused in &report.refused {
        println!("refused    {refused}");
    }
    for link in &report.links {
        println!("link       {}", link.display());
    }
    return Ok(state);
}

fn push_and_print(config: &Config, store: &Store, state: &mut State) -> Result<(), String> {
    // The store's room as of its last snapshot; none yet means no limit known.
    let snapshot = reconcile::read_snapshot(store).map_err(|error| error.to_string())?;
    let room = snapshot.and_then(|snapshot| snapshot.quota).map_or(u64::MAX, |quota| quota.room());
    let report = push::push(
        store,
        &config.machine_id,
        &config.data_root,
        state,
        config.batch_bytes(),
        room,
        SystemTime::now(),
    )
    .map_err(|error| error.to_string())?;
    println!("batches    {}", report.batches.len());
    println!("pushed     {} files, {} bytes", report.pushed.len(), report.bytes);
    println!("deletions  {}", report.deletions);
    for path in &report.skipped {
        println!("skipped    {path}");
    }
    if !report.held.is_empty() {
        println!("held       {} files: the store is full", report.held.len());
    }
    return Ok(());
}

/// `push` (`full` false) or `sync`: lock, gate, connection, scan, push, and for `sync` the
/// store machine's merge and the reading of the catalog.
fn run_sync(full: bool) -> Result<(), String> {
    let config = load_config()?;
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let Some(store) = reach_store(&config)? else {
        return Ok(());
    };
    let mut state = run_scan(&config)?;
    push_and_print(&config, &store, &mut state)?;
    if !full {
        return Ok(());
    }
    if config.is_store {
        merge_and_print(&config, &store)?;
    }
    let Some(snapshot) = reconcile::read_snapshot(&store).map_err(|error| error.to_string())? else {
        println!("no complete catalog on the store yet");
        return Ok(());
    };
    reconcile::save_snapshot(&cached_snapshot_path()?, &snapshot).map_err(|error| error.to_string())?;
    let limits = Limits { cache_bytes: config.cache_limit_bytes(), evict_to_percent: config.evict_to_percent };
    let report = reconcile::reconcile(
        &store,
        &snapshot,
        &config.machine_id,
        &config.data_root,
        &mut state,
        &limits,
        SystemTime::now(),
    )
    .map_err(|error| error.to_string())?;
    println!("verified   {}", report.verified);
    println!("fetched    {}", report.fetched.len());
    println!("deleted    {} (deleted on other machines)", report.deleted.len());
    println!("evicted    {}", report.evicted.len());
    for path in &report.repush {
        println!("repush     {path}: rejected by the store, pushed again next run");
    }
    for path in &report.over_quota {
        println!("waiting    {path}: the store was full, pushed again once there is room");
    }
    if let Some(quota) = report.quota {
        print_quota(&quota);
    }
    for (path, reason) in &report.fetch_failed {
        println!("not fetched {path}: {reason}");
    }
    for path in &report.kept_edited {
        println!("kept       {path}: deleted elsewhere, edited here");
    }
    for (path, reason) in &report.skipped {
        println!("left alone {path}: {reason}");
    }
    for path in &report.conflicts {
        println!("conflict   {path}");
    }
    for path in &report.refused {
        println!("refused    deletion of {path}: changed elsewhere since");
    }
    if let Some(total) = report.over_limit {
        println!(
            "WARNING    cache at {total} bytes, over its {} GB limit: nothing more can be evicted",
            config.cache_limit_gb
        );
    }
    return Ok(());
}

fn run_merge() -> Result<(), String> {
    let config = load_config()?;
    if !config.is_store {
        return Err("merge runs on the store machine only (is_store = true)".to_string());
    }
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let store = Store::open(&config.store_root, &config.store_id).map_err(|error| error.to_string())?;
    return merge_and_print(&config, &store);
}

fn merge_and_print(config: &Config, store: &Store) -> Result<(), String> {
    let mut catalog =
        Catalog::open(&local_dir()?.join("catalog.sqlite")).map_err(|error| error.to_string())?;
    let limits = StoreLimits { quota: config.store_quota_bytes(), free_margin: config.free_margin_bytes() };
    // A failed measurement counts as a full disk: new files wait rather than risk filling it.
    let free_space = || disk::free_space(store.root()).unwrap_or(0);
    let report = merge::run(store, &mut catalog, SystemTime::now(), config.trash_days, &limits, &free_space)
        .map_err(|error| error.to_string())?;
    if let Some(quota) = report.quota {
        print_quota(&quota);
    }
    for (machine, path) in &report.over_quota {
        println!("refused    {machine} {path}: no room, to be pushed again later");
    }
    println!("batches    {} merged, {} discarded", report.merged_batches, report.discarded_batches);
    println!("placed     {}", report.placed.len());
    println!("replaced   {}", report.replaced.len());
    println!("deleted    {}", report.deleted.len());
    for path in &report.restored {
        println!("restored   {path}");
    }
    for path in &report.restore_skipped {
        println!("not restored {path}: a file of that name exists");
    }
    for path in &report.restore_missing {
        println!("not restored {path}: nothing in the trash");
    }
    for (machine, batch) in &report.waiting {
        println!("waiting    {machine} {batch}");
    }
    for path in &report.conflicts {
        println!("conflict   {path}");
    }
    for path in &report.refused {
        println!("refused    deletion of {path}: changed since");
    }
    for (machine, path) in &report.rejected {
        println!("rejected   {machine} {path}: hash mismatch, to be pushed again");
    }
    for date in &report.trash_emptied {
        println!("emptied    trash {date}");
    }
    for path in &report.ignored {
        println!("ignored    {}", path.display());
    }
    if !report.snapshot_written {
        println!("catalog.tsv not replaced (held open?), next run retries");
    }
    return Ok(());
}

fn print_quota(quota: &Quota) {
    let gb = |bytes: u64| {
        return bytes as f64 / 1e9;
    };
    match quota.limit {
        Some(limit) => println!("store      {:.1} of {:.1} GB used", gb(quota.used), gb(limit)),
        None => println!("store      {:.1} GB used, no quota", gb(quota.used)),
    }
    println!("store disk {:.1} GB free, {:.1} GB kept free", gb(quota.free), gb(quota.margin));
    if quota.room() == 0 {
        println!("WARNING    the store is full: new files wait on their machines");
    }
}

fn init_store(root: &Path) -> Result<(), String> {
    let id = Store::init(root).map_err(|error| error.to_string())?;
    println!("store created at {}", root.display());
    println!("store_id = \"{id}\"");
    return Ok(());
}

fn open_state() -> Result<State, String> {
    return State::open(&local_dir()?.join("state.sqlite")).map_err(|error| error.to_string());
}

/// This machine's copy of the last catalog snapshot read, for working offline.
fn cached_snapshot_path() -> Result<PathBuf, String> {
    return Ok(local_dir()?.join("catalog.tsv"));
}

fn cached_snapshot() -> Result<Option<Snapshot>, String> {
    return reconcile::read_snapshot_at(&cached_snapshot_path()?).map_err(|error| error.to_string());
}

/// A path given on the command line, inside the data folder: absolute, relative to the current
/// folder when that lies inside the data folder, or relative to the data folder itself.
fn relative(config: &Config, text: &str) -> Result<RelativePath, String> {
    let given = Path::new(text);
    let absolute = if given.is_absolute() {
        given.to_path_buf()
    } else {
        std::env::current_dir().map_err(|error| error.to_string())?.join(given)
    };
    let inside = match absolute.strip_prefix(&config.data_root) {
        Ok(inside) => inside.to_string_lossy().to_string(),
        Err(_) if !given.is_absolute() => text.to_string(),
        Err(_) => return Err(format!("{text} is not inside the data folder {}", config.data_root.display())),
    };
    return RelativePath::parse(&inside).map_err(|error| error.to_string());
}

fn print_by_hand(verb: &str, result: &ByHand) {
    for path in &result.done {
        println!("{verb:<10} {path}");
    }
    for (path, reason) in &result.refused {
        println!("left alone {path}: {reason}");
    }
}

fn pin(text: &str, pinned: bool) -> Result<(), String> {
    let config = load_config()?;
    let path = relative(&config, text)?;
    let mut state = open_state()?;
    if pinned {
        state.add_pin(&path).map_err(|error| error.to_string())?;
        println!("pinned {path}: missing files arrive at the next sync");
    } else {
        state.remove_pin(&path).map_err(|error| error.to_string())?;
        println!("unpinned {path}");
    }
    return Ok(());
}

fn fetch(text: &str) -> Result<(), String> {
    let config = load_config()?;
    let path = relative(&config, text)?;
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let Some(store) = reach_store(&config)? else {
        return Ok(());
    };
    let Some(snapshot) = reconcile::read_snapshot(&store).map_err(|error| error.to_string())? else {
        return Err("no complete catalog on the store yet".to_string());
    };
    let mut state = open_state()?;
    let result = reconcile::fetch_within(&store, &snapshot, &config.data_root, &mut state, &path, SystemTime::now())
        .map_err(|error| error.to_string())?;
    print_by_hand("fetched", &result);
    if result.done.is_empty() && result.refused.is_empty() {
        println!("nothing to fetch under {path}");
    }
    return Ok(());
}

fn evict(text: &str) -> Result<(), String> {
    let config = load_config()?;
    let path = relative(&config, text)?;
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let mut state = open_state()?;
    let result = reconcile::evict_within(&config.data_root, &mut state, &path).map_err(|error| error.to_string())?;
    print_by_hand("evicted", &result);
    return Ok(());
}

fn delete(text: &str) -> Result<(), String> {
    let config = load_config()?;
    let path = relative(&config, text)?;
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let mut state = open_state()?;
    let snapshot = cached_snapshot()?;
    let result = reconcile::delete_within(&config.data_root, &mut state, snapshot.as_ref(), &path)
        .map_err(|error| error.to_string())?;
    print_by_hand("deleted", &result);
    if !result.done.is_empty() {
        println!("the store moves them to its trash after the next sync");
    }
    return Ok(());
}

fn restore(text: &str) -> Result<(), String> {
    let config = load_config()?;
    let path = relative(&config, text)?;
    let mut state = open_state()?;
    state.add_restore(&path).map_err(|error| error.to_string())?;
    println!("restore of {path} requested: the store brings it back after the next sync");
    return Ok(());
}

fn resolve(copy: &str, new_path: Option<&str>, keep_copy: bool) -> Result<(), String> {
    let config = load_config()?;
    let copy = relative(&config, copy)?;
    let resolution = match new_path {
        Some(new_path) => Resolution::KeepBoth(relative(&config, new_path)?),
        None if keep_copy => Resolution::KeepCopy,
        None => Resolution::KeepOriginal,
    };
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let Some(snapshot) = cached_snapshot()? else {
        return Err("no catalog read yet: sync first".to_string());
    };
    let mut state = open_state()?;
    reconcile::resolve(&config.data_root, &mut state, &snapshot, &copy, &resolution)
        .map_err(|error| error.to_string())?;
    println!("resolved {copy}: the next sync carries it to the store");
    return Ok(());
}

fn empty_trash(older_than: Option<i64>) -> Result<(), String> {
    let config = load_config()?;
    if !config.is_store {
        return Err("empty-trash runs on the store machine only".to_string());
    }
    let question = match older_than {
        Some(days) => format!("permanently delete trashed files older than {days} days"),
        None => "permanently delete everything in the trash".to_string(),
    };
    if !console::ask_yes_no(&question)? {
        return Ok(());
    }
    let Some(_lock) = take_lock()? else {
        println!("another run is in progress");
        return Ok(());
    };
    let store = Store::open(&config.store_root, &config.store_id).map_err(|error| error.to_string())?;
    let (emptied, _) = merge::empty_trash_older_than(&store, SystemTime::now(), older_than.unwrap_or(-1))
        .map_err(|error| error.to_string())?;
    for date in &emptied {
        println!("emptied    trash {date}");
    }
    // Refreshes the store's usage in the catalog.
    return merge_and_print(&config, &store);
}

fn status() -> Result<(), String> {
    let config = load_config()?;
    println!("machine    {}{}", config.machine_id, if config.is_store { " (store machine)" } else { "" });
    if !config.is_store {
        let home = network::on_home_network(&network::connected_networks()?, &config.home_network_id);
        println!("network    {}", if home { "home" } else { "away from home: syncs wait" });
    }
    let state = open_state()?;
    let files = state.files().map_err(|error| error.to_string())?;
    let local_bytes: u64 = files.iter().map(|record| record.size).sum();
    let pending: Vec<_> = files.iter().filter(|record| record.pushed_hash.as_deref() != Some(record.hash.as_str())).collect();
    let unverified = files.iter().filter(|record| !record.verified).count();
    let deletions = state.deletions().map_err(|error| error.to_string())?;
    let restores = state.restores().map_err(|error| error.to_string())?;
    let gb = |bytes: u64| {
        return bytes as f64 / 1e9;
    };
    println!(
        "cache      {} files, {:.1} of {} GB",
        files.len(),
        gb(local_bytes),
        config.cache_limit_gb
    );
    println!(
        "to push    {} files ({:.1} GB), {} deletions, {} restores",
        pending.len(),
        gb(pending.iter().map(|record| record.size).sum()),
        deletions.len(),
        restores.len()
    );
    println!("unverified {unverified} (never evicted until the store holds them)");
    for pin in state.pins().map_err(|error| error.to_string())? {
        println!("pinned     {pin}");
    }
    if local_bytes > config.cache_limit_bytes() {
        println!("WARNING    the cache is over its limit");
    }

    let snapshot_path = cached_snapshot_path()?;
    let Some(snapshot) = cached_snapshot()? else {
        println!("catalog    none read yet: sync on the home network");
        return Ok(());
    };
    if let Ok(modified) = std::fs::metadata(&snapshot_path).and_then(|metadata| metadata.modified())
        && let Ok(age) = SystemTime::now().duration_since(modified)
    {
        println!("catalog    read {} minutes ago", age.as_secs() / 60);
    }
    let local: std::collections::HashSet<String> =
        files.iter().map(|record| house_drive_core::state::key(&record.path)).collect();
    let store_only: Vec<_> =
        snapshot.files.iter().filter(|file| !local.contains(&house_drive_core::state::key(&file.path))).collect();
    println!(
        "store only {} files ({:.1} GB)",
        store_only.len(),
        gb(store_only.iter().map(|file| file.size).sum())
    );
    if let Some(quota) = &snapshot.quota {
        print_quota(quota);
    }
    for conflict in &snapshot.conflicts {
        println!("conflict   {} (from {}, copy of {})", conflict.copy, conflict.machine, conflict.original);
    }
    for refused in snapshot.refused.iter().filter(|refused| refused.machine == config.machine_id) {
        println!("refused    deletion of {}: changed elsewhere since", refused.path);
    }
    return Ok(());
}

/// Copies both programs into House Drive's own folder, so the task survives rebuilds, then
/// registers the task from the configuration's `sync_minutes`.
fn schedule() -> Result<(), String> {
    let config = load_config()?;
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let built_with = current.with_file_name("house-drive-sync.exe");
    if !built_with.exists() {
        return Err(format!("{} is missing: build both programs (cargo build --release)", built_with.display()));
    }
    let bin = local_dir()?.join("bin");
    std::fs::create_dir_all(&bin).map_err(|error| format!("{}: {error}", bin.display()))?;
    for (source, name) in [(&current, "house-drive.exe"), (&built_with, "house-drive-sync.exe")] {
        let target = bin.join(name);
        if *source != target {
            std::fs::copy(source, &target).map_err(|error| {
                return format!("cannot copy to {} (a sync running?): {error}", target.display());
            })?;
        }
    }
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) => format!(r"{domain}\{name}"),
        _ => return Err("USERDOMAIN or USERNAME is not set".to_string()),
    };
    let xml = house_drive_core::schedule::task_xml(&user, config.sync_minutes, &bin.join("house-drive-sync.exe"));
    house_drive_core::schedule::register(&xml, &local_dir()?.join("task.xml"))?;
    println!(
        "scheduled: \"{}\" runs every {} minutes and at logon",
        house_drive_core::schedule::TASK_NAME,
        config.sync_minutes
    );
    println!("programs copied to {}", bin.display());
    println!("each run's output goes to {}", local_dir()?.join("sync.log").display());
    println!("after changing sync_minutes, run schedule again");
    return Ok(());
}

fn unschedule() -> Result<(), String> {
    house_drive_core::schedule::unregister()?;
    println!("unscheduled: syncs now run only by hand");
    return Ok(());
}

fn show_networks() -> Result<(), String> {
    for network in network::connected_networks()? {
        let profile = if network.private { "Private" } else { "Public " };
        println!("{profile}  {}  {}", network.id, network.name);
    }
    return Ok(());
}

/// Asks for the share's account, checks it by connecting, then stores it.
fn ask_credentials(store_root: &Path) -> Result<Credentials, String> {
    let user = console::ask("the share's user name")?;
    let password = console::ask_hidden("its password")?;
    let credentials = Credentials { user, password };
    share::connect(store_root, &credentials).map_err(|error| error.to_string())?;
    credentials::write(&credentials)?;
    println!("credentials stored in Credential Manager under \"{}\"", credentials::TARGET);
    return Ok(credentials);
}

fn set_credentials() -> Result<(), String> {
    let config = load_config()?;
    if config.is_store {
        return Err("the store machine reaches its store locally: no credentials needed".to_string());
    }
    ask_credentials(&config.store_root)?;
    return Ok(());
}

/// Sets this machine up: its ID, folders, the home network, the share's account, and the store's
/// ID read from the store itself (recorded now, checked at every run after).
fn join() -> Result<(), String> {
    let path = config::default_path().map_err(|error| error.to_string())?;
    if path.exists() {
        return Err(format!("{} exists: this machine has already joined (edit it by hand)", path.display()));
    }
    let machine_id = loop {
        let answer = console::ask("this machine's ID (lowercase letters, digits and hyphens)")?;
        if config::is_valid_machine_id(&answer) {
            break answer;
        }
    };
    let is_store = console::ask_yes_no("is this the store machine")?;
    let data_root = PathBuf::from(console::ask("this machine's data folder (its cache)")?);
    let store_root = PathBuf::from(if is_store {
        console::ask("the store's folder on this machine (as given to init-store)")?
    } else {
        console::ask(r"the store's share (\\server\share)")?
    });

    let mut home_network_id = String::new();
    if !is_store {
        let private: Vec<_> = network::connected_networks()?.into_iter().filter(|network| network.private).collect();
        home_network_id = match private.len() {
            0 => {
                return Err("not connected to any Private network: connect to the home network, set its \
                            profile to Private in Windows' settings, and join again"
                    .to_string());
            }
            1 => private[0].id.clone(),
            _ => loop {
                for (index, network) in private.iter().enumerate() {
                    println!("  {}. {}  {}", index + 1, network.name, network.id);
                }
                let answer = console::ask("which one is the home network")?;
                if let Ok(choice) = answer.parse::<usize>()
                    && (1..=private.len()).contains(&choice)
                {
                    break private[choice - 1].id.clone();
                }
            },
        };
        ask_credentials(&store_root)?;
    }

    let store_id = store::read_id(&store_root).map_err(|error| error.to_string())?;
    let config = Config::new(machine_id, data_root, store_root, store_id, home_network_id, is_store);
    config::write_new(&path, &config).map_err(|error| error.to_string())?;
    println!("joined: configuration written to {}", path.display());
    println!("store ID {} recorded; every run checks it", config.store_id);
    println!("next: house-drive sync");
    return Ok(());
}
