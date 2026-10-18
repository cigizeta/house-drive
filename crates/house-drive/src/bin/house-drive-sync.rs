//! `house-drive-sync.exe`, started by Task Scheduler: runs `house-drive sync` with no console
//! window (a console program would open one every few minutes) and appends what it printed to
//! `%LOCALAPPDATA%\house-drive\sync.log`, kept under about a megabyte.

#![windows_subsystem = "windows"]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use house_drive_core::{config, dates, hash};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const LOG_LIMIT: u64 = 1_000_000;

fn main() {
    let Ok(local_dir) = config::local_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&local_dir);
    let log = local_dir.join("sync.log");
    if fs::metadata(&log).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
        let _ = fs::rename(&log, local_dir.join("sync.log.1"));
    }
    let program: Option<PathBuf> = std::env::current_exe().ok().map(|exe| exe.with_file_name("house-drive.exe"));
    let started = dates::date_time_of(hash::nanos(SystemTime::now()));
    let text = match program {
        Some(program) => match Command::new(&program).arg("sync").creation_flags(CREATE_NO_WINDOW).output() {
            Ok(output) => format!(
                "== {started}, exit {}\n{}{}",
                output.status.code().map_or("?".to_string(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => format!("== {started}\ncannot start {}: {error}\n", program.display()),
        },
        None => format!("== {started}\ncannot find house-drive.exe\n"),
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log) {
        let _ = file.write_all(text.as_bytes());
    }
}
