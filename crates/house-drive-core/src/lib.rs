//! House Drive's logic. Front ends (the command-line tool, later the tray and Explorer) stay thin
//! over this library, which never prints and never exits the process.

pub mod catalog;
pub mod config;
pub mod credentials;
pub mod dates;
pub mod disk;
pub mod hash;
pub mod journal;
pub mod lock;
pub mod merge;
pub mod network;
pub mod paths;
pub mod push;
pub mod reconcile;
pub mod scan;
pub mod schedule;
pub mod share;
pub mod state;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;
