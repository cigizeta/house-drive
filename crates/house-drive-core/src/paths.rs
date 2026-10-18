//! Relative paths inside a root (the data root, the store's `files/`, a batch folder).
//!
//! Every path read from a journal, the catalog or the share goes through `RelativePath::parse`
//! before it touches the disk: a hostile or corrupt entry must never reach outside its root, so
//! absolute paths, `..`, drive letters and alternate data streams are refused, as are names Windows
//! would silently alter or treat as devices.

use std::fmt;
use std::path::{Path, PathBuf};

/// A path checked to stay inside its root, stored with `/` between components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelativePath(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    pub text: String,
    pub reason: &'static str,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return write!(f, "refused path {:?}: {}", self.text, self.reason);
    }
}

impl std::error::Error for PathError {}

const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

impl RelativePath {
    /// Accepts `/` or `\` between components.
    pub fn parse(text: &str) -> Result<RelativePath, PathError> {
        let refuse = |reason| {
            return Err(PathError { text: text.to_string(), reason });
        };
        if text.is_empty() {
            return refuse("empty");
        }
        if text.starts_with('/') || text.starts_with('\\') {
            return refuse("absolute");
        }
        let mut components = Vec::new();
        for component in text.split(['/', '\\']) {
            if let Err(reason) = check_component(component) {
                return refuse(reason);
            }
            components.push(component);
        }
        return Ok(RelativePath(components.join("/")));
    }

    pub fn as_str(&self) -> &str {
        return &self.0;
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        return self.0.split('/');
    }

    /// The path on disk under `root`.
    pub fn under(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for component in self.components() {
            path.push(component);
        }
        return path;
    }

    /// True when `self` is `folder` or lies inside it, comparing whole components and ignoring
    /// case, as Windows does: a pinned folder pins everything under it.
    pub fn is_within(&self, folder: &RelativePath) -> bool {
        let mut mine = self.components();
        for theirs in folder.components() {
            match mine.next() {
                Some(component) if component.eq_ignore_ascii_case(theirs) => {}
                _ => return false,
            }
        }
        return true;
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return f.write_str(&self.0);
    }
}

fn check_component(component: &str) -> Result<(), &'static str> {
    if component.is_empty() {
        return Err("empty component");
    }
    if component == "." || component == ".." {
        return Err("`.` or `..` component");
    }
    // `:` covers both drive letters and alternate data streams.
    if component.chars().any(|c| (c as u32) < 32 || "<>:\"|?*".contains(c)) {
        return Err("character not allowed in a Windows name");
    }
    // Windows strips a trailing dot or space, so `a.` would silently become `a`.
    if component.ends_with('.') || component.ends_with(' ') {
        return Err("trailing dot or space");
    }
    let stem = component.split('.').next().unwrap_or(component);
    if RESERVED_NAMES.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        return Err("reserved device name");
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(text: &str) -> &'static str {
        return RelativePath::parse(text).unwrap_err().reason;
    }

    #[test]
    fn accepts_ordinary_paths_and_normalises_separators() {
        assert_eq!(RelativePath::parse("a/b/c.txt").unwrap().as_str(), "a/b/c.txt");
        assert_eq!(RelativePath::parse(r"a\b\c.txt").unwrap().as_str(), "a/b/c.txt");
        assert_eq!(RelativePath::parse(".hidden/x").unwrap().as_str(), ".hidden/x");
        assert_eq!(RelativePath::parse("a..b").unwrap().as_str(), "a..b");
    }

    #[test]
    fn refuses_escapes() {
        assert_eq!(reason(""), "empty");
        assert_eq!(reason("/a"), "absolute");
        assert_eq!(reason(r"\a"), "absolute");
        assert_eq!(reason(r"\\server\share\a"), "absolute");
        assert_eq!(reason("../a"), "`.` or `..` component");
        assert_eq!(reason(r"a\..\..\b"), "`.` or `..` component");
        assert_eq!(reason("a/./b"), "`.` or `..` component");
        assert_eq!(reason("a//b"), "empty component");
        assert_eq!(reason("a/"), "empty component");
    }

    #[test]
    fn refuses_drive_letters_and_streams() {
        assert_eq!(reason(r"C:\a"), "character not allowed in a Windows name");
        assert_eq!(reason("C:a"), "character not allowed in a Windows name");
        assert_eq!(reason("a/b.txt:hidden"), "character not allowed in a Windows name");
        assert_eq!(reason("a/b.txt::$DATA"), "character not allowed in a Windows name");
    }

    #[test]
    fn refuses_names_windows_would_alter() {
        assert_eq!(reason("a/b."), "trailing dot or space");
        assert_eq!(reason("a /b"), "trailing dot or space");
        assert_eq!(reason("a\tb"), "character not allowed in a Windows name");
        assert_eq!(reason("nul"), "reserved device name");
        assert_eq!(reason("a/Com1.txt"), "reserved device name");
        assert!(RelativePath::parse("console.txt").is_ok());
    }

    #[test]
    fn under_joins_components() {
        let path = RelativePath::parse("a/b.txt").unwrap();
        assert_eq!(path.under(Path::new(r"C:\root")), PathBuf::from(r"C:\root\a\b.txt"));
    }

    #[test]
    fn is_within_compares_whole_components() {
        let folder = RelativePath::parse("photos/2026").unwrap();
        assert!(RelativePath::parse("photos/2026").unwrap().is_within(&folder));
        assert!(RelativePath::parse("photos/2026/a.jpg").unwrap().is_within(&folder));
        assert!(RelativePath::parse("Photos/2026/a.jpg").unwrap().is_within(&folder));
        assert!(!RelativePath::parse("photos/20261/a.jpg").unwrap().is_within(&folder));
        assert!(!RelativePath::parse("photos").unwrap().is_within(&folder));
    }
}
