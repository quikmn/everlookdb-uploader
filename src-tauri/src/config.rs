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
    /// "Update my addons by itself": update the addons we installed, every CHECK_EVERY, without asking
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// the player set `auto_update` with its switch (so a false is theirs and stays); see migrate()
    #[serde(default)]
    pub auto_update_chosen: bool,
    /// "Update the app by itself": install new versions of the app quietly (see main.rs spawn_updater)
    #[serde(default = "yes")]
    pub app_auto_update: bool,
    /// the player set `app_auto_update` with its switch; a false without this is not a choice and is turned on
    #[serde(default)]
    pub app_auto_update_chosen: bool,
    /// the last updates done by themselves (newest first, at most RECENT_MAX), for the window's quiet list
    #[serde(default)]
    pub recent_updates: Vec<RecentUpdate>,
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
    /// the player said "Not now" to the welcome card that installs our addons (ours.rs); it never shows again
    #[serde(default)]
    pub ours_card_dismissed: bool,
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

/// One update done by itself: an addon (`install` is its game) or the app (`install` empty).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RecentUpdate {
    /// local time, RFC 3339
    pub at: String,
    #[serde(default)]
    pub install: String,
    pub name: String,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
}

pub const RECENT_MAX: usize = 20;

/// How often the app looks for updates of itself and of the addons (and once soon after it starts).
pub const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// CHECK_EVERY, or EVERLOOKDB_CHECK_EVERY (seconds, at least 10) in a test instance (EVERLOOKDB_UPLOADER_DIR set),
/// so a short test run sees several checks. The real app always uses CHECK_EVERY.
pub fn check_every() -> std::time::Duration {
    let test = std::env::var_os("EVERLOOKDB_UPLOADER_DIR").is_some_and(|s| !s.is_empty());
    let over = std::env::var("EVERLOOKDB_CHECK_EVERY").ok().and_then(|s| s.trim().parse::<u64>().ok());
    match over {
        Some(secs) if test => std::time::Duration::from_secs(secs.max(10)),
        _ => CHECK_EVERY,
    }
}

/// When the next check is due: soon after the start (`first`), then `every` after the last one. A clock set back
/// (the last check "in the future") counts from now.
pub fn next_check(
    started: chrono::DateTime<chrono::Local>,
    last: Option<chrono::DateTime<chrono::Local>>,
    now: chrono::DateTime<chrono::Local>,
    first: std::time::Duration,
    every: std::time::Duration,
) -> chrono::DateTime<chrono::Local> {
    let every = chrono::Duration::from_std(every).unwrap_or(chrono::Duration::minutes(10));
    match last {
        None => started + chrono::Duration::from_std(first).unwrap_or_default(),
        Some(t) if t > now + chrono::Duration::minutes(1) => now + every,
        Some(t) => t + every,
    }
}

/// Written just before the app installs its own update, read once at the next start, so the new version knows it
/// was started by the update (it then stays in the tray: the window never opens by itself).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Restart {
    /// kept so notes written by older versions still read; never used to open the window
    #[serde(default)]
    pub window_open: bool,
    /// the version being installed (empty in notes from older versions)
    #[serde(default)]
    pub to: String,
    /// unix seconds
    pub at: i64,
}

impl Restart {
    fn path() -> PathBuf {
        dir().join("restart.json")
    }
    pub fn write(to: &str) {
        let r = Restart { window_open: false, to: to.to_string(), at: chrono::Local::now().timestamp() };
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

impl Default for Config {
    fn default() -> Self {
        Config { wow_dir: None, paused: false, done: BTreeMap::new(), last_status: None, uploads: BTreeMap::new(), extra_roots: Vec::new(), auto_update: true, auto_update_chosen: false, app_auto_update: true, app_auto_update_chosen: false, recent_updates: Vec::new(), selected: None, receipts: Receipts::default(), autostart_decided: false, github_repos: Vec::new(), window: None, shared_prices: true, ours_card_dismissed: false }
    }
}

impl Config {
    pub(crate) fn path() -> PathBuf {
        dir().join("config.json")
    }

    pub fn load() -> Config {
        let mut c: Config = std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if c.migrate() {
            log("settings: \"Update the app by itself\" turned on (it was off only as an old default)");
        }
        c
    }

    /// Settings from older versions. Returns whether the app update switch was turned on.
    /// - `app_auto_update` false without `app_auto_update_chosen` is not the player's choice: on.
    /// - `auto_update` (the addons) has defaulted to on since the first version that had it (7349211), so a false in
    ///   the file was always written by its switch: it is the player's and stays, now marked as chosen.
    pub fn migrate(&mut self) -> bool {
        if !self.auto_update && !self.auto_update_chosen {
            self.auto_update_chosen = true;
        }
        if !self.app_auto_update && !self.app_auto_update_chosen {
            self.app_auto_update = true;
            return true;
        }
        false
    }

    /// The window's switches: the value, remembered as the player's choice.
    pub fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
        self.auto_update_chosen = true;
    }

    pub fn set_app_auto_update(&mut self, on: bool) {
        self.app_auto_update = on;
        self.app_auto_update_chosen = true;
    }

    /// Note an update done by itself (newest first, the list kept short).
    pub fn note_recent(&mut self, u: RecentUpdate) {
        self.recent_updates.insert(0, u);
        self.recent_updates.truncate(RECENT_MAX);
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
    fn updates_are_on_by_default() {
        let c = Config::default();
        assert!(c.auto_update && c.app_auto_update);
        assert!(!c.auto_update_chosen && !c.app_auto_update_chosen);
        // a settings file from before either switch (or an empty one): both on
        let old: Config = serde_json::from_str(r#"{"autostart_decided":true}"#).unwrap();
        assert!(old.auto_update && old.app_auto_update);
        assert_eq!(CHECK_EVERY, std::time::Duration::from_secs(600));
    }

    #[test]
    fn migration_turns_on_only_what_the_player_did_not_choose() {
        // an app-update false nobody chose (no _chosen): on
        let mut c: Config = serde_json::from_str(r#"{"app_auto_update": false}"#).unwrap();
        assert!(c.migrate());
        assert!(c.app_auto_update && !c.app_auto_update_chosen);
        assert!(!c.migrate(), "once is enough");
        // the player switched it off in the window: stays off, also after a save and load
        let mut c = Config::default();
        c.set_app_auto_update(false);
        let mut back: Config = serde_json::from_slice(&serde_json::to_vec_pretty(&c).unwrap()).unwrap();
        assert!(!back.migrate());
        assert!(!back.app_auto_update && back.app_auto_update_chosen);
        // the addon switch always defaulted to on, so an old false is the player's: kept, and marked as chosen
        let mut old: Config = serde_json::from_str(r#"{"auto_update": false}"#).unwrap();
        assert!(!old.migrate());
        assert!(!old.auto_update && old.auto_update_chosen);
        let mut on: Config = serde_json::from_str(r#"{"auto_update": true}"#).unwrap();
        on.migrate();
        assert!(on.auto_update && !on.auto_update_chosen);
        // switched back on in the window: on, and still chosen
        old.set_auto_update(true);
        assert!(old.auto_update && old.auto_update_chosen);
        // an old file with the hours setting this version dropped still reads
        let hours: Config = serde_json::from_str(r#"{"update_hours": 6, "auto_update": true}"#).unwrap();
        assert!(hours.auto_update);
    }

    #[test]
    fn checks_run_every_ten_minutes() {
        use chrono::TimeZone;
        let t0 = chrono::Local.with_ymd_and_hms(2026, 11, 21, 12, 0, 0).unwrap();
        let first = std::time::Duration::from_secs(5);
        let m = |n: i64| chrono::Duration::minutes(n);
        // the first check soon after the start, then every ten minutes after the last one
        assert_eq!(next_check(t0, None, t0, first, CHECK_EVERY), t0 + chrono::Duration::seconds(5));
        assert_eq!(next_check(t0, Some(t0), t0 + m(3), first, CHECK_EVERY), t0 + m(10));
        assert_eq!(next_check(t0, Some(t0 + m(10)), t0 + m(11), first, CHECK_EVERY), t0 + m(20));
        // a PC that slept: due at once when it wakes
        assert!(next_check(t0, Some(t0), t0 + m(90), first, CHECK_EVERY) < t0 + m(90));
        // the clock set back an hour: ten minutes from now, not an hour and ten
        assert_eq!(next_check(t0, Some(t0 + m(60)), t0, first, CHECK_EVERY), t0 + m(10));
        // the real app ignores the test override
        assert_eq!(check_every(), CHECK_EVERY);
    }

    #[test]
    fn recent_updates_stay_short() {
        let mut c = Config::default();
        for i in 0..30 {
            c.note_recent(RecentUpdate { at: String::new(), install: "WoW Forever".into(), name: format!("A{i}"), from: None, to: "1".into() });
        }
        assert_eq!(c.recent_updates.len(), RECENT_MAX);
        assert_eq!(c.recent_updates[0].name, "A29");
        let back: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back.recent_updates, c.recent_updates);
    }

    #[test]
    fn restart_note_from_an_older_version_still_reads() {
        let r: Restart = serde_json::from_str(r#"{"window_open":true,"at":5}"#).unwrap();
        assert!(r.window_open && r.to.is_empty());
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
