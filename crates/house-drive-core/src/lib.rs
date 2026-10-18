//! House Drive's logic. Front ends (the command-line tool, later the tray and Explorer) stay thin
//! over this library, which never prints and never exits the process.

pub mod config;
pub mod hash;
pub mod journal;
pub mod paths;
pub mod push;
pub mod scan;
pub mod state;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;
