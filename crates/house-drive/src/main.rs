//! `house-drive.exe`: argument parsing and output only, over house-drive-core.

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use house_drive_core::config;
use house_drive_core::scan;
use house_drive_core::state::State;

const USAGE: &str = "usage: house-drive <command>

commands:
  config    check this machine's configuration and show it
  scan      record new, changed and deleted files in the data root (pushes nothing)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["config"] => show_config(),
        ["scan"] => run_scan(),
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
    return Ok(());
}

fn run_scan() -> Result<(), String> {
    let path = config::default_path().map_err(|error| error.to_string())?;
    let config = config::read(&path).map_err(|error| error.to_string())?;
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
    return Ok(());
}
