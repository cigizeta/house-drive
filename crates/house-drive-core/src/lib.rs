//! House Drive's logic. Front ends (the command-line tool, later the tray and Explorer) stay thin
//! over this library, which never prints and never exits the process.

pub mod catalog;
pub mod config;
pub mod dates;
pub mod hash;
pub mod journal;
pub mod merge;
pub mod paths;
pub mod push;
pub mod reconcile;
pub mod scan;
pub mod state;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;
