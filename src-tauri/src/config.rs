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

static CACHE: OnceLock<PathBuf> = OnceLock::new();

/// Where things that can be made again are kept (addon icons): this machine's own folder, not the roaming one.
/// EVERLOOKDB_UPLOADER_DIR keeps them with the settings.
pub fn init_cache_dir(default: Option<PathBuf>) {
    let custom = std::env::var_os("EVERLOOKDB_UPLOADER_DIR").is_some_and(|s| !s.is_empty());
    if let Some(d) = default.filter(|_| !custom) {
        let _ = CACHE.set(d);
    }
}

/// The cache folder (the settings folder unless init_cache_dir chose another).
pub fn cache_dir() -> PathBuf {
    CACHE.get().cloned().unwrap_or_else(dir)
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Done {
    /// SHA-256 of the file content that was sent
    pub hash: String,
    /// false: the last try failed or was refused; it is tried again (see worker::backoff), and at once on start
    pub ok: bool,
}

/// What the window shows for one data file: the answer to its last upload. Kept across restarts so a failure
/// stays visible (and is retried) after the app starts again.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct FileState {
    /// "ok" (all taken), "partial" (some refused or skipped), "retry" (failed once, trying again), "failed"
    pub level: String,
    /// one sentence: the server's message, or one made from its counts
    pub headline: String,
    /// why scans were refused, one line each
    #[serde(default)]
    pub details: Vec<String>,
    /// when this answer came (unix seconds)
    #[serde(default)]
    pub at: i64,
    /// when it is tried again (unix seconds), while it keeps failing
    #[serde(default)]
    pub next_retry: Option<i64>,
    /// failed tries in a row
    #[serde(default)]
    pub failures: u32,
}

/// The server's receipt for an accepted upload (8 characters), for the giveaway code (see giveaway.rs).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Receipt {
    pub code: String,
    /// when it came (unix seconds)
    #[serde(default)]
    pub at: i64,
    /// the upload held a full auction house scan (auctions only)
    #[serde(default)]
    pub full_scan: bool,
}

/// The latest receipt per kind of file, over all accounts.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Receipts {
    /// EverlookDBAuctions.lua
    #[serde(default)]
    pub auctions: Option<Receipt>,
    /// EverlookDB.lua (the Collector addon's data)
    #[serde(default)]
    pub collector: Option<Receipt>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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
    /// same keys as `done`: the last answer for each file, for the window
    #[serde(default)]
    pub uploads: BTreeMap<String, FileState>,
    /// `World of Warcraft` folders the player added by hand
    #[serde(default)]
    pub extra_roots: Vec<PathBuf>,
    /// update the addons we installed by themselves (every `update_hours`)
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// how often the addon list is checked (and, with auto_update, addons updated); see `hours()`
    #[serde(default = "one")]
    pub update_hours: u32,
    /// the install the window shows (its flavor folder)
    #[serde(default)]
    pub selected: Option<String>,
    /// the server's receipts for the giveaway code
    #[serde(default)]
    pub receipts: Receipts,
    /// set once the first start has decided "start with Windows / open at login": on for a new install, left as it
    /// was for an install from before this setting, and never again after that, so a player who turns it off keeps it off
    #[serde(default)]
    pub autostart_decided: bool,
    /// GitHub repos ("owner/repo") the player installed an addon from by its link; checked for updates like the
    /// catalog (see github.rs)
    #[serde(default)]
    pub github_repos: Vec<String>,
    /// where the window was last (logical pixels), so it opens there again
    #[serde(default)]
    pub window: Option<WindowRect>,
    /// shared auction prices: write other players' newest scans of our auction houses into the game as the
    /// EverlookDBAuctions_Shared data addon (see shared.rs)
    #[serde(default = "yes")]
    pub shared_prices: bool,
}

/// A window's place and inner size, in logical pixels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct WindowRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl WindowRect {
    /// Whether enough of it (its title bar's middle) is on one of `screens` (x, y, w, h in logical pixels) to grab it.
    pub fn on_screen(&self, screens: &[(f64, f64, f64, f64)]) -> bool {
        let (cx, cy) = (self.x + self.w / 2.0, self.y + 16.0);
        self.w >= 200.0 && self.h >= 150.0 && screens.iter().any(|&(x, y, w, h)| cx >= x && cx < x + w && cy >= y && cy < y + h)
    }
}

/// Written just before the app installs its own update, read once at the next start: whether the window was open,
/// so the new version opens it again (without taking the focus) or stays in the tray.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Restart {
    pub window_open: bool,
    /// unix seconds
    pub at: i64,
}

impl Restart {
    fn path() -> PathBuf {
        dir().join("restart.json")
    }
    pub fn write(window_open: bool) {
        let r = Restart { window_open, at: chrono::Local::now().timestamp() };
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(Self::path(), serde_json::to_vec(&r).unwrap_or_default());
    }
    /// The note from just before an update (at most 30 minutes old), removed as it is read.
    pub fn take() -> Option<Restart> {
        let r: Option<Restart> = std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let _ = std::fs::remove_file(Self::path());
        r.filter(|r| (chrono::Local::now().timestamp() - r.at).abs() < 30 * 60)
    }
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

/// The update interval's range, in hours: never more often than hourly (the server's catalog changes about that
/// often), at least once a week.
pub const MIN_HOURS: u32 = 1;
pub const MAX_HOURS: u32 = 168;

pub fn clamp_hours(h: f64) -> u32 {
    if h.is_nan() {
        return MIN_HOURS;
    }
    h.round().clamp(MIN_HOURS as f64, MAX_HOURS as f64) as u32
}

impl Default for Config {
    fn default() -> Self {
        Config { wow_dir: None, paused: false, done: BTreeMap::new(), last_status: None, uploads: BTreeMap::new(), extra_roots: Vec::new(), auto_update: true, update_hours: 1, selected: None, receipts: Receipts::default(), autostart_decided: false, github_repos: Vec::new(), window: None, shared_prices: true }
    }
}

impl Config {
    pub(crate) fn path() -> PathBuf {
        dir().join("config.json")
    }

    /// The update interval, kept in range whatever the file says.
    pub fn hours(&self) -> u32 {
        clamp_hours(self.update_hours as f64)
    }

    /// The interval from the window: a number, or the text typed (clamped; text that isn't a number keeps the
    /// current one). Returns the interval now.
    pub fn set_hours(&mut self, v: &serde_json::Value) -> u32 {
        if let Some(h) = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) {
            self.update_hours = clamp_hours(h);
        }
        self.hours()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_is_decided_once() {
        // a settings file from before the setting: not decided yet, so the first start decides (and leaves an existing
        // install's choice alone, see main.rs run)
        let old: Config = serde_json::from_str(r#"{"paused":false}"#).unwrap();
        assert!(!old.autostart_decided);
        // once decided, it stays decided: a player who turned it off is never switched back on
        let mut c = Config::default();
        c.autostart_decided = true;
        let back: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(back.autostart_decided);
    }

    #[test]
    fn update_hours_are_clamped() {
        assert_eq!(clamp_hours(0.0), 1);
        assert_eq!(clamp_hours(-5.0), 1);
        assert_eq!(clamp_hours(1.0), 1);
        assert_eq!(clamp_hours(2.4), 2);
        assert_eq!(clamp_hours(2.6), 3);
        assert_eq!(clamp_hours(168.0), 168);
        assert_eq!(clamp_hours(1000.0), 168);
        assert_eq!(clamp_hours(f64::NAN), 1);
        assert_eq!(clamp_hours(f64::INFINITY), 168);
    }

    #[test]
    fn update_hours_persist() {
        // an old config.json without the field: hourly
        let old: Config = serde_json::from_str(r#"{"auto_update": false}"#).unwrap();
        assert_eq!((old.update_hours, old.hours(), old.auto_update), (1, 1, false));
        // a hand-edited file out of range: kept in range
        let wild: Config = serde_json::from_str(r#"{"update_hours": 9999}"#).unwrap();
        assert_eq!(wild.hours(), 168);
        let zero: Config = serde_json::from_str(r#"{"update_hours": 0}"#).unwrap();
        assert_eq!(zero.hours(), 1);
        // what save() writes, load() reads back
        let c = Config { update_hours: 6, ..Default::default() };
        let back: Config = serde_json::from_slice(&serde_json::to_vec_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.hours(), 6);
        // from the window
        let mut c = Config::default();
        assert_eq!(c.set_hours(&serde_json::json!(12)), 12);
        assert_eq!(c.set_hours(&serde_json::json!(" 3 ")), 3);
        assert_eq!(c.set_hours(&serde_json::json!("lots")), 3);
        assert_eq!(c.set_hours(&serde_json::json!(500)), 168);
        assert_eq!(c.set_hours(&serde_json::json!(0)), 1);
        assert_eq!(c.update_hours, 1);
    }

    #[test]
    fn window_place_is_kept_only_when_on_a_screen() {
        let screens = [(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 1280.0, 1024.0)];
        let r = WindowRect { x: 2000.0, y: 100.0, w: 1100.0, h: 740.0 };
        assert!(r.on_screen(&screens));
        assert!(!r.on_screen(&screens[..1]), "the second screen was unplugged");
        assert!(!WindowRect { x: -5000.0, ..r }.on_screen(&screens));
        assert!(!WindowRect { w: 10.0, ..r }.on_screen(&screens));
        let c = Config { window: Some(r), ..Default::default() };
        let back: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back.window, Some(r));
        let old: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(old.window, None);
    }

    #[test]
    fn receipts_persist() {
        // an old config.json has none
        let old: Config = serde_json::from_str(r#"{"auto_update": true}"#).unwrap();
        assert_eq!(old.receipts, Receipts::default());
        let c = Config {
            receipts: Receipts {
                auctions: Some(Receipt { code: "7K3M9QXZ".into(), at: 5, full_scan: true }),
                collector: Some(Receipt { code: "00000001".into(), at: 6, full_scan: false }),
            },
            ..Default::default()
        };
        let back: Config = serde_json::from_slice(&serde_json::to_vec_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.receipts, c.receipts);
    }
}
