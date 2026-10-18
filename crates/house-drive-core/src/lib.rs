//! House Drive's logic. Front ends (the command-line tool, later the tray and Explorer) stay thin
//! over this library, which never prints and never exits the process.

pub mod config;
pub mod paths;
