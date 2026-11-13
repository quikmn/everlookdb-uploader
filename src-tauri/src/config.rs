// Settings and upload memory in config.json, plus a small local log. Nothing in here is ever sent.
// Folder: %APPDATA%\com.everlookdb.uploader (Windows), ~/Library/Application Support/com.everlookdb.uploader
// (macOS), or EVERLOOKDB_UPLOADER_DIR.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init_dir(default: PathBuf) {
    let dir = std::env::var_os("EVERLOOKDB_UPLOADER_DIR").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or(default);
    let _ = DIR.set(dir);
}

pub fn dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_else(std::env::temp_dir)
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Done {
    /// SHA-256 of the file content that was sent
    pub hash: String,
    /// false: the server refused it (too big, not valid); not sent again until the file changes
    pub ok: bool,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Config {
    /// the `World of Warcraft/_forever_` folder
    #[serde(default)]
    pub wow_dir: Option<PathBuf>,
    #[serde(default)]
    pub paused: bool,
    /// "<server>|<file path>" -> the last content sent there, so an unchanged file isn't sent again
    #[serde(default)]
    pub done: BTreeMap<String, Done>,
    /// the tray's line about the last upload, kept across restarts
    #[serde(default)]
    pub last_status: Option<String>,
}

impl Config {
    fn path() -> PathBuf {
        dir().join("config.json")
    }

    pub fn exists() -> bool {
        Self::path().is_file()
    }

    pub fn load() -> Config {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::create_dir_all(dir());
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// A line in uploader.log (kept under 512 KB).
pub fn log(msg: &str) {
    let path = dir().join("uploader.log");
    let _ = std::fs::create_dir_all(dir());
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}
