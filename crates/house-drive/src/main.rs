//! `house-drive.exe`: argument parsing and output only, over house-drive-core.

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use std::path::Path;

use house_drive_core::catalog::{Catalog, Quota};
use house_drive_core::config::{self, Config};
use house_drive_core::disk;
use house_drive_core::merge::{self, StoreLimits};
use house_drive_core::push;
use house_drive_core::reconcile::{self, Limits};
use house_drive_core::scan;
use house_drive_core::state::State;
use house_drive_core::store::Store;

const USAGE: &str = "usage: house-drive <command>

commands:
  config             check this machine's configuration and show it
  scan               record new, changed and deleted files in the data root (pushes nothing)
  push               scan, then push new, changed and deleted files to the store
  sync               scan, push, merge (store machine), then verify, fetch, delete and evict
  init-store <path>  create a store in an empty folder, once, on the store machine
  merge              on the store machine: merge pushed batches, empty the trash, write the catalog";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["config"] => show_config(),
        ["scan"] => run_scan().map(|_| ()),
        ["push"] => run_push().map(|_| ()),
        ["sync"] => run_sync(),
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

/// Scans and prints the report, returning the configuration and the state for what follows.
fn run_scan() -> Result<(Config, State), String> {
    let config = load_config()?;
    let local_dir = config::local_dir().map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&local_dir)
        .map_err(|error| format!("cannot create {}: {error}", local_dir.display()))?;
    let mut state = State::open(&local_dir.join("state.sqlite")).map_err(|error| error.to_string())?;
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
    return Ok((config, state));
}

/// Scans and pushes, returning what the rest of a sync needs.
fn run_push() -> Result<(Config, State, Store), String> {
    let (config, mut state) = run_scan()?;
    let store = Store::open(&config.store_root, &config.store_id).map_err(|error| error.to_string())?;
    // The store's room as of its last snapshot; none yet means no limit known.
    let snapshot = reconcile::read_snapshot(&store).map_err(|error| error.to_string())?;
    let room = snapshot.and_then(|snapshot| snapshot.quota).map_or(u64::MAX, |quota| quota.room());
    let report = push::push(
        &store,
        &config.machine_id,
        &config.data_root,
        &mut state,
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
    return Ok((config, state, store));
}

fn run_sync() -> Result<(), String> {
    let (config, mut state, store) = run_push()?;
    if config.is_store {
        merge_and_print(&config, &store)?;
    }
    let Some(snapshot) = reconcile::read_snapshot(&store).map_err(|error| error.to_string())? else {
        println!("no complete catalog on the store yet");
        return Ok(());
    };
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
    let store = Store::open(&config.store_root, &config.store_id).map_err(|error| error.to_string())?;
    return merge_and_print(&config, &store);
}

fn merge_and_print(config: &Config, store: &Store) -> Result<(), String> {
    let local_dir = config::local_dir().map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&local_dir)
        .map_err(|error| format!("cannot create {}: {error}", local_dir.display()))?;
    let mut catalog =
        Catalog::open(&local_dir.join("catalog.sqlite")).map_err(|error| error.to_string())?;
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
