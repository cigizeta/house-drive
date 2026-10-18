//! A batch's journal, `journal/<machine_id>/<batch>.tsv`: one line per operation, then an `end`
//! line holding the count.
//!
//! The pushing machine may only create files on the store, never rename or rewrite them, so a
//! journal cannot be written under a temporary name and renamed. The `end` line is what tells a
//! complete journal from one cut short.
//!
//! ```text
//! put     <path>  <size>  <blake3>  <modified>  <base or ->
//! delete  <path>  -       -         -           <base>
//! end     <count>
//! ```

use std::fmt::{self, Write};

use crate::hash;
use crate::paths::RelativePath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Put {
        path: RelativePath,
        size: u64,
        hash: String,
        /// Nanoseconds since the Unix epoch.
        modified: i64,
        /// The store's version this file replaces, `None` for a file new to the store.
        base: Option<String>,
    },
    Delete {
        path: RelativePath,
        base: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return write!(f, "journal line {}: {}", self.line, self.message);
    }
}

impl std::error::Error for JournalError {}

pub fn format(entries: &[Entry]) -> String {
    let mut text = String::new();
    for entry in entries {
        let _ = match entry {
            Entry::Put { path, size, hash, modified, base } => writeln!(
                text,
                "put\t{path}\t{size}\t{hash}\t{modified}\t{}",
                base.as_deref().unwrap_or("-")
            ),
            Entry::Delete { path, base } => writeln!(text, "delete\t{path}\t-\t-\t-\t{base}"),
        };
    }
    let _ = writeln!(text, "end\t{}", entries.len());
    return text;
}

/// `Ok(None)` for a journal cut short (no `end` line yet); an error for anything malformed.
pub fn parse(text: &str) -> Result<Option<Vec<Entry>>, JournalError> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(last) = lines.last() else {
        return Ok(None);
    };
    let Some(count) = last.strip_prefix("end\t") else {
        return Ok(None);
    };
    let error = |line: usize, message: &str| {
        return JournalError { line, message: message.to_string() };
    };
    let count: usize = count.parse().map_err(|_| error(lines.len(), "bad count"))?;
    if count != lines.len() - 1 {
        return Err(error(lines.len(), "count does not match the lines"));
    }
    let mut entries = Vec::with_capacity(count);
    for (index, line) in lines[..count].iter().enumerate() {
        let number = index + 1;
        let fields: Vec<&str> = line.split('\t').collect();
        let &[operation, path, size, hash, modified, base] = fields.as_slice() else {
            return Err(error(number, "expected six fields"));
        };
        let path = RelativePath::parse(path).map_err(|e| error(number, &e.to_string()))?;
        let base_ok = base == "-" || hash::is_hash(base);
        if !base_ok {
            return Err(error(number, "bad base"));
        }
        let entry = match operation {
            "put" => {
                if !hash::is_hash(hash) {
                    return Err(error(number, "bad hash"));
                }
                Entry::Put {
                    path,
                    size: size.parse().map_err(|_| error(number, "bad size"))?,
                    hash: hash.to_string(),
                    modified: modified.parse().map_err(|_| error(number, "bad modified time"))?,
                    base: if base == "-" { None } else { Some(base.to_string()) },
                }
            }
            "delete" => {
                if base == "-" {
                    return Err(error(number, "a deletion needs a base"));
                }
                Entry::Delete { path, base: base.to_string() }
            }
            _ => return Err(error(number, "unknown operation")),
        };
        entries.push(entry);
    }
    return Ok(Some(entries));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> String {
        return c.to_string().repeat(64);
    }

    fn entries() -> Vec<Entry> {
        return vec![
            Entry::Put {
                path: RelativePath::parse("a/b.txt").unwrap(),
                size: 12,
                hash: h('a'),
                modified: 34,
                base: None,
            },
            Entry::Put {
                path: RelativePath::parse("c.txt").unwrap(),
                size: 0,
                hash: h('b'),
                modified: 0,
                base: Some(h('c')),
            },
            Entry::Delete { path: RelativePath::parse("d.txt").unwrap(), base: h('d') },
        ];
    }

    #[test]
    fn round_trips() {
        assert_eq!(parse(&format(&entries())).unwrap(), Some(entries()));
        assert_eq!(parse(&format(&[])).unwrap(), Some(vec![]));
    }

    #[test]
    fn a_journal_cut_short_is_incomplete() {
        let text = format(&entries());
        assert_eq!(parse(""), Ok(None));
        let without_end = text.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(parse(&without_end), Ok(None));
        assert_eq!(parse(&text[..text.len() - 4]), Ok(None));
    }

    #[test]
    fn malformed_journals_are_refused() {
        let text = format(&entries());
        assert!(parse(&text.replace("end\t3", "end\t2")).is_err());
        assert!(parse(&text.replace("a/b.txt", "../b.txt")).is_err());
        assert!(parse(&text.replace("put\tc.txt", "move\tc.txt")).is_err());
        assert!(parse(&text.replace(&h('a'), "xyz")).is_err());
    }
}
