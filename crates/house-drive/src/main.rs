//! `house-drive.exe`: argument parsing and output only, over house-drive-core.

use std::process::ExitCode;

use house_drive_core::config;

const USAGE: &str = "usage: house-drive <command>

commands:
  config    check this machine's configuration and show it";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["config"] => show_config(),
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
    return Ok(());
}
