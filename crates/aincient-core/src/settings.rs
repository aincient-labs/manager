//! The Manager's own per-user settings, next to the site registry
//! (`~/.atelier/manager.json`). Only the GUI reads them; the CLI takes flags.
//!
//! - `experimental` (m7-3, DECISIONS 0483): off by default. While off, the
//!   GUI hides everything engine-related and its engine commands refuse.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The settings file, in the same folder as `sites.toml`.
pub const SETTINGS_FILE: &str = "manager.json";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Experimental features (the engine sidecar). Off unless the user turned
    /// it on.
    #[serde(default)]
    pub experimental: bool,
}

pub fn path(user_home: &Path) -> PathBuf {
    crate::sites::registry_path(user_home).with_file_name(SETTINGS_FILE)
}

/// The settings; a missing or unreadable file is the defaults (everything
/// experimental off), never an error.
pub fn load(user_home: &Path) -> Settings {
    std::fs::read_to_string(path(user_home))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the settings (through a temp file, so a reader never sees half).
pub fn save(user_home: &Path, settings: &Settings) -> Result<()> {
    let p = path(user_home);
    let dir = p.parent().context("the settings path has a folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = p.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)?)
        .and_then(|_| std::fs::rename(&tmp, &p))
        .with_context(|| format!("could not write {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experimental_is_off_until_saved_on_and_junk_reads_as_off() {
        let home = std::env::temp_dir().join(format!("atelier-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(!load(&home).experimental);
        save(&home, &Settings { experimental: true }).unwrap();
        assert!(load(&home).experimental);
        assert!(path(&home).ends_with(".atelier/manager.json"));
        std::fs::write(path(&home), "{not json").unwrap();
        assert!(!load(&home).experimental, "unreadable means off");
        std::fs::write(path(&home), "{}").unwrap();
        assert!(!load(&home).experimental);
        let _ = std::fs::remove_dir_all(&home);
    }
}
