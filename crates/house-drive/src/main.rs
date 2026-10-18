//! `house-drive.exe`: argument parsing and output only, over house-drive-core.

mod console;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use house_drive_core::catalog::{Catalog, Quota};
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
  join               set this machine up, once, interactively
  set-credentials    store the share's user name and password again
  networks           list the networks this machine is connected to
  config             check this machine's configuration and show it
  scan               record new, changed and deleted files in the data root (pushes nothing)
  push               scan, then push new, changed and deleted files to the store
  sync               scan, push, merge (store machine), then verify, fetch, delete and evict
  init-store <path>  create a store in an empty folder, once, on the store machine
  merge              on the store machine: merge pushed batches, empty the trash, write the catalog";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
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
