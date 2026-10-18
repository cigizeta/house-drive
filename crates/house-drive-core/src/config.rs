//! Each machine's configuration, `%LOCALAPPDATA%\house-drive\config.toml`.
//!
//! The file holds home-specific values (paths, the store's ID, the home network's ID), so it lives
//! only on the machine; the repository holds `examples/config.toml` with placeholders. Credentials
//! are never here: they live in Windows Credential Manager. Every number a user may want to change
//! is here, with a default, never hard-coded elsewhere.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub machine_id: String,
    pub data_root: PathBuf,
    pub store_root: PathBuf,
    pub store_id: String,
    pub home_network_id: String,
    #[serde(default)]
    pub is_store: bool,
    #[serde(default = "default_sync_minutes")]
    pub sync_minutes: u32,
    #[serde(default = "default_cache_limit_gb")]
    pub cache_limit_gb: u64,
    #[serde(default = "default_evict_to_percent")]
    pub evict_to_percent: u32,
    #[serde(default = "default_trash_days")]
    pub trash_days: u32,
    #[serde(default = "default_settle_seconds")]
    pub settle_seconds: u64,
    #[serde(default = "default_batch_mb")]
    pub batch_mb: u64,
}

fn default_sync_minutes() -> u32 {
    return 10;
}

fn default_cache_limit_gb() -> u64 {
    return 256;
}

fn default_evict_to_percent() -> u32 {
    return 90;
}

fn default_trash_days() -> u32 {
    return 30;
}

fn default_settle_seconds() -> u64 {
    return 60;
}

fn default_batch_mb() -> u64 {
    return 1024;
}

#[derive(Debug)]
pub enum ConfigError {
    NoLocalAppData,
    Read { path: PathBuf, source: std::io::Error },
    Parse { path: PathBuf, message: String },
    Invalid { path: PathBuf, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return match self {
            ConfigError::NoLocalAppData => write!(f, "LOCALAPPDATA is not set"),
            ConfigError::Read { path, source } => {
                write!(f, "cannot read {}: {}", path.display(), source)
            }
            ConfigError::Parse { path, message } => {
                write!(f, "cannot parse {}: {}", path.display(), message)
            }
            ConfigError::Invalid { path, message } => write!(f, "{}: {}", path.display(), message),
        };
    }
}

impl std::error::Error for ConfigError {}

/// House Drive's own folder on this machine, holding the configuration and the local databases.
pub fn local_dir() -> Result<PathBuf, ConfigError> {
    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return Err(ConfigError::NoLocalAppData);
    };
    return Ok(PathBuf::from(local_app_data).join("house-drive"));
}

pub fn default_path() -> Result<PathBuf, ConfigError> {
    return Ok(local_dir()?.join("config.toml"));
}

pub fn read(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
    let config: Config = toml::from_str(&text)
        .map_err(|error| ConfigError::Parse { path: path.to_path_buf(), message: error.to_string() })?;
    config
        .validate()
        .map_err(|message| ConfigError::Invalid { path: path.to_path_buf(), message })?;
    return Ok(config);
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let id_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if self.machine_id.is_empty() || !self.machine_id.chars().all(id_ok) {
            return Err(format!(
                "machine_id {:?}: lowercase letters, digits and hyphens only",
                self.machine_id
            ));
        }
        if !self.data_root.is_absolute() {
            return Err(format!("data_root {} is not absolute", self.data_root.display()));
        }
        if !self.store_root.is_absolute() {
            return Err(format!("store_root {} is not absolute", self.store_root.display()));
        }
        if self.store_id.trim().is_empty() {
            return Err("store_id is empty".to_string());
        }
        if self.home_network_id.trim().is_empty() {
            return Err("home_network_id is empty".to_string());
        }
        if self.sync_minutes == 0 {
            return Err("sync_minutes must be at least 1".to_string());
        }
        if self.cache_limit_gb == 0 {
            return Err("cache_limit_gb must be at least 1".to_string());
        }
        if !(1..=99).contains(&self.evict_to_percent) {
            return Err("evict_to_percent must be between 1 and 99".to_string());
        }
        if self.trash_days == 0 {
            return Err("trash_days must be at least 1".to_string());
        }
        if self.batch_mb == 0 {
            return Err("batch_mb must be at least 1".to_string());
        }
        return Ok(());
    }

    pub fn cache_limit_bytes(&self) -> u64 {
        return self.cache_limit_gb * 1_000_000_000;
    }

    pub fn batch_bytes(&self) -> u64 {
        return self.batch_mb * 1_000_000;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/config.toml");

    const MINIMAL: &str = r#"
machine_id = "machine-b"
data_root = 'C:\example\data'
store_root = '\\store-machine\house-drive'
store_id = "id"
home_network_id = "{id}"
"#;

    fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
        config.validate()?;
        return Ok(config);
    }

    #[test]
    fn example_parses() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(config.machine_id, "machine-a");
        assert_eq!(config.data_root, PathBuf::from(r"C:\example\data"));
        assert_eq!(config.store_root, PathBuf::from(r"\\store-machine\house-drive"));
        assert!(!config.is_store);
    }

    #[test]
    fn defaults_apply() {
        let config = parse(MINIMAL).unwrap();
        assert!(!config.is_store);
        assert_eq!(config.sync_minutes, 10);
        assert_eq!(config.cache_limit_gb, 256);
        assert_eq!(config.evict_to_percent, 90);
        assert_eq!(config.trash_days, 30);
        assert_eq!(config.settle_seconds, 60);
        assert_eq!(config.batch_mb, 1024);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(parse(&format!("{MINIMAL}cache_limit = 1\n")).is_err());
    }

    #[test]
    fn invalid_values_are_refused() {
        let replace = |from: &str, to: &str| {
            return parse(&MINIMAL.replace(from, to)).unwrap_err();
        };
        assert!(replace("machine-b", "Machine B").contains("machine_id"));
        assert!(replace(r"C:\example\data", r"example\data").contains("data_root"));
        assert!(replace(r"\\store-machine\house-drive", "share").contains("store_root"));
        assert!(replace(r#"store_id = "id""#, r#"store_id = """#).contains("store_id"));
        assert!(parse(&format!("{MINIMAL}sync_minutes = 0\n")).unwrap_err().contains("sync_minutes"));
        assert!(
            parse(&format!("{MINIMAL}evict_to_percent = 100\n"))
                .unwrap_err()
                .contains("evict_to_percent")
        );
        assert!(parse(&format!("{MINIMAL}trash_days = 0\n")).unwrap_err().contains("trash_days"));
    }
}
