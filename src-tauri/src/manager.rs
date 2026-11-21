// Installing, updating, rolling back and removing addons.
//
// Download to a temp file, check its size, unzip into a staging folder (every entry must stay inside it: no `..`,
// no absolute paths), then swap the folders in. The previous version's zip is kept for one-click rollback. Folders
// we didn't install are never deleted: when the player updates an addon another app installed, its old folders are
// copied to the app's backups folder first. What we installed is recorded in the app's data folder (addons.json),
// never in the game folder.

use crate::addons::{self, Folder};
use crate::icons;
use crate::catalog::{version_label, Addon, Catalog};
use crate::config;
use crate::wow::Install;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOO_LARGE: &str = "The download is larger than 400 MB, so EverlookDB didn't install it.";

/// Largest zip we download, and the most an archive may unpack to.
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;

/// One write at a time, across every install.
static LOCK: Mutex<()> = Mutex::new(());

/// Jobs that change addons running now (an update run counts as one from start to end, gaps between addons too).
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts a job as running while it lives.
pub struct Busy(());

impl Busy {
    pub fn start() -> Busy {
        ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Busy(())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// No install, update, rollback or uninstall is running.
pub fn idle() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::SeqCst) == 0 && LOCK.try_lock().is_ok()
}

/// Hold off every addon change (the app's self-update holds it while the installer takes over). None while busy.
pub fn hold() -> Option<std::sync::MutexGuard<'static, ()>> {
    if ACTIVE.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        return None;
    }
    LOCK.try_lock().ok()
}

/// How often a download reports its progress (about ten times a second).
pub const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Lets progress through at most once per `every`; the first report and the last always go through.
pub struct Throttle {
    every: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(every: Duration) -> Throttle {
        Throttle { every, last: None }
    }

    pub fn ready(&mut self, now: Instant, last_report: bool) -> bool {
        let due = last_report || self.last.is_none_or(|t| now.saturating_duration_since(t) >= self.every);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// Called with (bytes done, bytes in all; 0 when neither the server nor the catalog says).
pub type OnProgress<'a> = &'a mut dyn FnMut(u64, u64);

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Prev {
    pub version: String,
    pub zip: PathBuf,
}

/// An addon we installed into one install.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub version: String,
    pub folders: Vec<String>,
    #[serde(default)]
    pub zip: Option<PathBuf>,
    #[serde(default)]
    pub previous: Option<Prev>,
    #[serde(default)]
    pub installed: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Records {
    /// install id (its flavor folder) -> addon id -> record
    #[serde(default)]
    pub installs: BTreeMap<String, BTreeMap<String, Record>>,
}

impl Records {
    fn path() -> PathBuf {
        config::dir().join("addons.json")
    }
    pub fn load() -> Records {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) -> Result<(), String> {
        let path = Self::path();
        let tmp = path.with_extension("json.tmp");
        const SAVE: &str = "EverlookDB couldn't save its list of installed addons in its data folder. Check that the disk isn't full.";
        std::fs::create_dir_all(config::dir()).map_err(logged(SAVE))?;
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(logged(SAVE))?).map_err(logged(SAVE))?;
        std::fs::rename(&tmp, &path).map_err(logged(SAVE))
    }
    pub fn of(&self, install: &str) -> BTreeMap<String, Record> {
        self.installs.get(install).cloned().unwrap_or_default()
    }
}

/// An installed addon as the window shows it.
#[derive(Serialize, Clone, Debug)]
pub struct Item {
    /// catalog id, or "folder:<main folder>" for addons we can't match
    pub key: String,
    pub name: String,
    pub author: Option<String>,
    pub notes: Option<String>,
    pub version: Option<String>,
    pub folders: Vec<String>,
    /// matched to the catalog
    pub catalog: bool,
    /// installed by this app (we may update and remove it)
    pub managed: bool,
    pub ours: bool,
    /// from a GitHub repo the player added by its link (not the catalog)
    pub custom: bool,
    pub latest: Option<String>,
    /// `version` and `latest` as the window shows them ("Plater-v664" -> "v664"; catalog::version_label)
    pub version_label: Option<String>,
    pub latest_label: Option<String>,
    /// when the newest version was released (the catalog's time, RFC 3339), shown next to its Update button
    pub latest_released: Option<String>,
    /// what changed in the newest version (release notes), when there is a link
    pub changelog: Option<String>,
    /// the folder whose .toc speaks for the addon ("Open folder" opens it)
    pub main: String,
    pub update: bool,
    /// folders in AddOns (not ours) that are in the way of the newest version: "Replace (backup kept)"
    pub conflict: Vec<String>,
    /// the automatic update (and Update all) may take it: EverlookDB installed it, or its version and the catalog's
    /// are both plain numbers and the catalog's is higher. Otherwise an update waits for the player's click.
    pub auto: bool,
    pub previous: Option<String>,
    pub url: Option<String>,
    /// the texture its .toc names as its icon, when that is a file in AddOns (the window asks for it separately)
    #[serde(skip)]
    pub icon_file: Option<PathBuf>,
    /// where it comes from, for the copied addon list: (label, value)
    #[serde(skip)]
    pub links: Vec<(&'static str, String)>,
}

/// Why an install or update didn't happen, said plainly (the details go to the log).
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Failure {
    pub message: String,
    /// folders in AddOns that EverlookDB didn't install and the new version needs: "Replace (backup kept)" takes them
    /// over (the copies there now go to the backups folder first)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict: Vec<String>,
}

impl From<String> for Failure {
    fn from(message: String) -> Failure {
        Failure { message, conflict: Vec::new() }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Failure {
        Failure::from(message.to_string())
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One addon an update run couldn't update.
#[derive(Serialize, Clone, Debug)]
pub struct Failed {
    pub id: String,
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict: Vec<String>,
}

/// The download was damaged: what the player is told.
pub const DAMAGED: &str = "The download was damaged (it doesn't match what the addon list says), so nothing was changed. Try again.";
const UNPACK: &str = "The addon couldn't be unpacked (the download may be damaged, or the disk is full), so nothing was changed. Try again.";
const SAVE_ZIP: &str = "EverlookDB couldn't save the download in its data folder. Check that the disk isn't full, then try again.";
const NEXT_TO_ADDONS: &str = "EverlookDB can't write next to your AddOns folder. Check that the game folder isn't read-only and the disk isn't full, then try again.";

/// An error mapper: logs the raw error with `plain`, and gives `plain` (the sentence the player sees).
fn logged<E: std::fmt::Display>(plain: impl Into<String>) -> impl FnOnce(E) -> String {
    let plain = plain.into();
    move |e| {
        config::log(&format!("{plain} [{e}]"));
        plain
    }
}

/// A network error as a sentence.
fn net_text(e: &reqwest::Error) -> String {
    config::log(&format!("download: {e:?}"));
    if e.is_timeout() {
        "The download stalled (no data for a minute). Check your internet connection, then try again.".into()
    } else if e.is_connect() {
        "No connection to the download server. Check your internet connection, then try again.".into()
    } else {
        "The download was cut off. Check your internet connection, then try again.".into()
    }
}

/// A download server's refusal as a sentence.
pub fn status_text(code: u16) -> String {
    match code {
        404 | 410 => "The download isn't there any more (the author may have replaced it). Check again later.".into(),
        403 | 429 => "The download server is limiting downloads right now. Try again in a few minutes.".into(),
        500..=599 => "The download server is having trouble. Try again in a few minutes.".into(),
        c => format!("The download server refused the download (error {c}). Try again later."),
    }
}

/// Folders in the way, as a sentence.
pub fn conflict_text(folders: &[String]) -> String {
    let list = match folders {
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
        [] => String::new(),
    };
    let (is, it) = if folders.len() == 1 { ("is", "it") } else { ("are", "them") };
    format!("{list} {is} already in your AddOns folder, from another app or put there by hand, so EverlookDB left {it} alone. Replace {it} to update (a backup is kept).")
}

/// Windows wouldn't move a folder: someone is using its files.
fn in_use_text(f: &str, e: &std::io::Error) -> String {
    config::log(&format!("can't move {f}: {e}"));
    format!("WoW or another program is using {f}'s files. Close WoW (and anything that has the AddOns folder open), then try again.")
}

#[derive(Serialize, Clone, Debug)]
pub struct Change {
    pub install: String,
    pub name: String,
    pub from: Option<String>,
    pub to: String,
    /// the game was running when it changed: a /reload loads it
    #[serde(default)]
    pub running: bool,
}

/// "bundle/v12.9.0+v1.1.1" -> "12.9.0": no tag prefix before '/', no leading v, no build metadata after '+'.
fn norm(v: &str) -> String {
    let v = v.trim();
    let v = v.rsplit('/').next().unwrap_or(v).trim_start_matches(['v', 'V']);
    v.split('+').next().unwrap_or(v).trim().to_ascii_lowercase()
}

/// Dotted numbers, when the whole version is that ("4.18.4.1" yes, "1.0-beta" no).
fn numbers(v: &str) -> Option<Vec<u64>> {
    v.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Whether `latest` is an update for what is installed: different, and not older when both are plain numbers.
pub fn differs(installed: Option<&str>, latest: &str) -> bool {
    let Some(i) = installed else { return false };
    let (i, l) = (norm(i), norm(latest));
    if l.is_empty() || i == l {
        return false;
    }
    match (numbers(&i), numbers(&l)) {
        (Some(a), Some(b)) => b > a,
        _ => true,
    }
}

/// Whether `latest` is certainly newer than what is installed: both are plain dotted numbers and `latest` is higher.
/// "1.2-forever" against "1.2" can't be told, so it isn't.
pub fn strictly_newer(installed: Option<&str>, latest: &str) -> bool {
    let Some(i) = installed else { return false };
    match (numbers(&norm(i)), numbers(&norm(latest))) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// The addons in an install, grouped, matched to records and the catalog.
pub fn list(install: &Install, catalog: Option<&Catalog>) -> Vec<Item> {
    // the shared-prices data addon is the app's own output (shared.rs), not an addon to manage
    let folders: Vec<Folder> = addons::scan(&install.addons_dir, &install.folder).into_iter().filter(|f| f.name != crate::shared::FOLDER).collect();
    let records = Records::load().of(&install.id);
    let conflicts = Conflicts::load();
    let mut known: Vec<(String, Vec<String>)> = records.values().map(|r| (r.id.clone(), r.folders.clone())).collect();
    if let Some(c) = catalog {
        known.extend(c.addons.iter().filter(|a| !records.contains_key(&a.id) && !a.folders.is_empty()).map(|a| (a.id.clone(), a.folders.clone())));
    }
    let by_name: BTreeMap<&str, &Folder> = folders.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut items: Vec<Item> = addons::group(&folders, &known)
        .into_iter()
        .map(|g| {
            let toc = by_name.get(g.main.as_str()).map(|f| f.toc.clone()).unwrap_or_default();
            let entry: Option<&Addon> = g.key.as_deref().and_then(|k| catalog.and_then(|c| c.get(k)));
            let record = g.key.as_deref().and_then(|k| records.get(k));
            let version = record.map(|r| r.version.clone()).or(toc.version.clone());
            // only offer what runs here
            let latest = entry.filter(|a| a.file_for(&install.accepts).is_some()).map(|a| a.version.clone());
            let name = record.map(|r| r.name.clone()).or(entry.map(|a| a.name.clone())).or(toc.title.clone()).unwrap_or_else(|| g.main.clone());
            // the main folder's toc first, then the others'
            let tocs: Vec<&addons::Toc> = std::iter::once(g.main.as_str())
                .chain(g.folders.iter().map(String::as_str).filter(|f| *f != g.main))
                .filter_map(|f| by_name.get(f).map(|x| &x.toc))
                .collect();
            let first = |get: fn(&addons::Toc) -> &Option<String>| tocs.iter().find_map(|t| get(t).clone());
            let icon_file = tocs.iter().find_map(|t| t.icon.as_deref().and_then(|v| icons::resolve(&install.addons_dir, v)));
            let website = first(|t| &t.website);
            let mut links: Vec<(&'static str, String)> = Vec::new();
            if let Some(w) = &website {
                links.push(("website", w.clone()));
            }
            if let Some(src) = entry.map(|a| a.url.clone()).filter(|u| !u.is_empty() && Some(u) != website.as_ref()) {
                links.push(("source", src));
            }
            for (label, value) in [("CurseForge", first(|t| &t.curse_id)), ("WoWInterface", first(|t| &t.wowi_id)), ("Wago", first(|t| &t.wago_id))] {
                if let Some(v) = value {
                    links.push((label, v));
                }
            }
            Item {
                key: g.key.clone().unwrap_or_else(|| format!("folder:{}", g.main)),
                update: latest.as_deref().is_some_and(|l| differs(version.as_deref(), l)),
                conflict: g
                    .key
                    .as_deref()
                    .zip(latest.as_deref())
                    .and_then(|(k, l)| conflicts.get(&install.id, k, l))
                    .map(|c| c.folders.clone())
                    .unwrap_or_default(),
                auto: false,
                name,
                author: entry.map(|a| a.author.clone()).filter(|a| !a.is_empty()).or(toc.author.clone()),
                notes: entry.map(|a| a.summary.clone()).filter(|s| !s.is_empty()).or(toc.notes.clone()),
                version_label: version.as_deref().map(version_label),
                version,
                folders: g.folders.clone(),
                catalog: entry.is_some(),
                managed: record.is_some(),
                ours: entry.is_some_and(|a| a.ours),
                custom: entry.is_some_and(|a| a.custom),
                latest_label: latest.as_deref().map(version_label),
                latest_released: entry.filter(|_| latest.is_some()).map(|a| a.released.clone()).filter(|r| !r.is_empty()),
                changelog: entry.filter(|_| latest.is_some()).and_then(Addon::changelog_url),
                main: g.main.clone(),
                latest,
                previous: record.and_then(|r| r.previous.as_ref().filter(|p| p.zip.is_file()).map(|p| p.version.clone())),
                url: entry.map(|a| a.url.clone()).or(toc.website.clone()),
                icon_file,
                links,
            }
        })
        .collect();
    for it in items.iter_mut() {
        it.auto = it.update && it.catalog && (it.managed || (adoptable(install, it) && it.latest.as_deref().is_some_and(|l| strictly_newer(it.version.as_deref(), l))));
    }
    items.sort_by_key(|i| (!i.update, i.name.to_ascii_lowercase()));
    items
}

/// The installed addons as plain text, one per line: name, version, author, folders, and where each comes from
/// (X-Website, the catalog's source, CurseForge / WoWInterface / Wago ids). For pasting in a chat or keeping.
pub fn export_text(install: &Install, items: &[Item], date: &str) -> String {
    let game = match &install.version {
        Some(v) => format!("{} {v}", install.name),
        None => install.name.clone(),
    };
    let n = items.len();
    let mut out = format!("{game}: {n} addon{}, listed by EverlookDB on {date}\n\n", if n == 1 { "" } else { "s" });
    let mut sorted: Vec<&Item> = items.iter().collect();
    sorted.sort_by_key(|i| i.name.to_ascii_lowercase());
    for it in sorted {
        let mut line = it.name.clone();
        // some titles carry the version already ("Titan Panel [_Core_] 9.3.6")
        if let Some(v) = it.version.as_deref().filter(|v| !v.is_empty() && !it.name.ends_with(v)) {
            line += &format!(" {v}");
        }
        if let Some(a) = it.author.as_deref().filter(|a| !a.is_empty()) {
            line += &format!(" by {a}");
        }
        line += &format!(" | folder{}: {}", if it.folders.len() == 1 { "" } else { "s" }, it.folders.join(", "));
        for (label, value) in &it.links {
            line += &format!(" | {label}: {value}");
        }
        out += line.replace(['\r', '\n'], " ").trim_end();
        out.push('\n');
    }
    out
}

fn short_hash(s: &str) -> String {
    let h = Sha256::digest(s.as_bytes());
    h.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Download a release zip into `dest`. https only; size must match the catalog when it gives one. Reports its
/// progress to `progress`, throttled to PROGRESS_EVERY; the last report has done == total.
pub fn download(url: &str, expect: u64, dest: &Path, progress: OnProgress) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("EverlookDB only downloads over https, so this addon wasn't installed.".into());
    }
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB/", env!("CARGO_PKG_VERSION")))
        // reqwest's blocking timeout counts per step (connecting, each read), not for the whole download: a big
        // addon on a slow line is fine, a download that stops sending for a minute is given up
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(logged("EverlookDB couldn't start the download. Try again."))?;
    let mut resp = http.get(url).send().map_err(|e| net_text(&e))?;
    if !resp.status().is_success() {
        config::log(&format!("download {url}: {}", resp.status()));
        return Err(status_text(resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_DOWNLOAD) {
        return Err(TOO_LARGE.into());
    }
    let size = resp.content_length().filter(|n| *n > 0).unwrap_or(expect);
    let mut throttle = Throttle::new(PROGRESS_EVERY);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(logged(SAVE_ZIP))?;
    }
    let mut out = std::fs::File::create(dest).map_err(logged(SAVE_ZIP))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    if throttle.ready(Instant::now(), false) {
        progress(0, size);
    }
    loop {
        let n = resp.read(&mut buf).map_err(|e| {
            config::log(&format!("download {url}: {e}"));
            "The download was cut off. Check your internet connection, then try again.".to_string()
        })?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_DOWNLOAD {
            drop(out);
            let _ = std::fs::remove_file(dest);
            return Err(TOO_LARGE.into());
        }
        out.write_all(&buf[..n]).map_err(logged(SAVE_ZIP))?;
        if throttle.ready(Instant::now(), false) {
            progress(total, size.max(total));
        }
    }
    out.flush().map_err(logged(SAVE_ZIP))?;
    if expect > 0 && total != expect {
        drop(out);
        let _ = std::fs::remove_file(dest);
        config::log(&format!("download {url}: {total} bytes, the catalog says {expect}"));
        return Err(DAMAGED.into());
    }
    throttle.ready(Instant::now(), true);
    progress(total, total);
    Ok(())
}

/// Whether a zip entry's name is safe to unpack: relative, no `..`, no drive letters. Returns its parts.
pub fn safe_parts(name: &str) -> Option<Vec<String>> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') || name.contains('\0') {
        return None;
    }
    let parts: Vec<String> = name.split('/').filter(|p| !p.is_empty() && *p != ".").map(str::to_string).collect();
    if parts.is_empty() || parts.iter().any(|p| p == "..") {
        return None;
    }
    Some(parts)
}

const UNSAFE: &str = "The addon's zip has a file that would land outside the AddOns folder, so EverlookDB didn't install it.";

/// Unpack a zip into `into`; returns the top-level folders. Any unsafe entry fails the whole thing.
pub fn unzip(zip: &Path, into: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(zip).map_err(logged(DAMAGED))?;
    let mut archive = zip::ZipArchive::new(file).map_err(logged(DAMAGED))?;
    // check every name before writing anything
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(logged(DAMAGED))?;
        let name = entry.name().map_err(logged(DAMAGED))?.into_owned();
        if safe_parts(&name).is_none() || entry.enclosed_name().is_none() {
            config::log(&format!("unsafe path in {}: {name}", zip.display()));
            return Err(UNSAFE.into());
        }
    }
    std::fs::create_dir_all(into).map_err(logged(UNPACK))?;
    let mut tops: Vec<String> = Vec::new();
    let mut unpacked: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(logged(DAMAGED))?;
        let parts = safe_parts(&entry.name().map_err(logged(DAMAGED))?).ok_or(UNSAFE)?;
        let is_dir = entry.is_dir();
        // loose files next to the folders (a readme) aren't addons
        if parts.len() == 1 && !is_dir {
            continue;
        }
        if !tops.contains(&parts[0]) {
            tops.push(parts[0].clone());
        }
        let target = parts.iter().fold(into.to_path_buf(), |p, part| p.join(part));
        if !target.starts_with(into) {
            return Err(UNSAFE.into());
        }
        if is_dir {
            std::fs::create_dir_all(&target).map_err(logged(UNPACK))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(logged(UNPACK))?;
        }
        let mut out = std::fs::File::create(&target).map_err(logged(UNPACK))?;
        let mut limited = (&mut entry).take(MAX_UNPACKED - unpacked + 1);
        let n = std::io::copy(&mut limited, &mut out).map_err(logged(UNPACK))?;
        unpacked += n;
        if unpacked > MAX_UNPACKED {
            return Err("The addon unpacks to more than 2 GB, so EverlookDB didn't install it.".into());
        }
    }
    tops.sort();
    Ok(tops)
}

fn has_toc(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".toc"))).unwrap_or(false)
}

/// The folder holding the addon folders: the staging folder, or the one folder the zip wraps them in.
fn addon_root(staging: &Path, tops: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    if tops.iter().any(|t| has_toc(&staging.join(t))) {
        return Ok((staging.to_path_buf(), tops.to_vec()));
    }
    if let [only] = tops {
        let inner = staging.join(only);
        let mut subs: Vec<String> = std::fs::read_dir(&inner)
            .map_err(logged(UNPACK))?
            .flatten()
            .filter(|e| e.path().is_dir() && has_toc(&e.path()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        subs.sort();
        if !subs.is_empty() {
            return Ok((inner, subs));
        }
    }
    Err("The download holds no addon (no .toc file), so nothing was installed.".into())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

/// A staging folder next to AddOns (same drive, so swapping is a rename; outside AddOns, so the game never sees it).
fn staging_for(addons_dir: &Path) -> Result<PathBuf, String> {
    let parent = addons_dir.parent().ok_or("no Interface folder")?;
    let dir = parent.join(format!("{STAGING_PREFIX}{}-{}", std::process::id(), stamp()));
    std::fs::create_dir_all(&dir).map_err(logged(NEXT_TO_ADDONS))?;
    Ok(dir)
}

/// What a swap has done so far, kept in the staging folder (journal.json) so a swap cut short (a crash, a power cut,
/// an undo that failed) can be put right the next time: see `recover`.
#[derive(Serialize, Deserialize, Default, Debug)]
struct Journal {
    /// folders moved from AddOns into staging/old (the version being replaced)
    moved: Vec<String>,
    /// folders of the new version put into AddOns
    placed: Vec<String>,
    /// every new folder is in place: the old copies may go
    done: bool,
    /// where the replaced folders are copied before they go (taking over another app's addon)
    #[serde(default)]
    backup: Option<PathBuf>,
}

const JOURNAL: &str = "journal.json";
const STAGING_PREFIX: &str = ".everlookdb-staging-";

fn write_journal(staging: &Path, j: &Journal) -> std::io::Result<()> {
    let tmp = staging.join("journal.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(j).unwrap_or_default())?;
    std::fs::rename(&tmp, staging.join(JOURNAL))
}

/// Moves a folder (a rename; injectable so tests can make it fail).
type Mover<'a> = &'a mut dyn FnMut(&Path, &Path) -> std::io::Result<()>;

/// Unpack `zip` and swap its folders into `addons_dir`. `allowed` are the existing folders we may replace (ours, or
/// the ones the player asked us to take over). Returns the new folders. The old folders are deleted only once every
/// new one is in place; when anything fails they are put back, and when even that fails they stay in the staging
/// folder, which `recover` puts right (at the next install, and when the app starts).
fn install_zip(addons_dir: &Path, zip: &Path, allowed: &HashSet<String>, backup_to: Option<&Path>) -> Result<Vec<String>, Failure> {
    install_zip_with(addons_dir, zip, allowed, backup_to, &mut |a: &Path, b: &Path| std::fs::rename(a, b))
}

fn install_zip_with(addons_dir: &Path, zip: &Path, allowed: &HashSet<String>, backup_to: Option<&Path>, mv: Mover) -> Result<Vec<String>, Failure> {
    std::fs::create_dir_all(addons_dir).map_err(logged(NEXT_TO_ADDONS))?;
    // anything an earlier swap left half done is put right first
    recover(addons_dir);
    let staging = staging_for(addons_dir)?;
    let unpack = staging.join("new");
    let prepared = (|| -> Result<_, Failure> {
        let tops = unzip(zip, &unpack)?;
        let (root, folders) = addon_root(&unpack, &tops)?;
        let blocking: Vec<String> = folders.iter().filter(|f| addons_dir.join(f).exists() && !allowed.contains(*f)).cloned().collect();
        if !blocking.is_empty() {
            return Err(Failure { message: conflict_text(&blocking), conflict: blocking });
        }
        let old = staging.join("old");
        std::fs::create_dir_all(&old).map_err(logged(NEXT_TO_ADDONS))?;
        Ok((root, folders, old))
    })();
    let (root, folders, old) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let mut j = Journal { backup: backup_to.map(Path::to_path_buf), ..Default::default() };
    if let Err(e) = write_journal(&staging, &j) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(logged(NEXT_TO_ADDONS)(e).into());
    }
    // move the old folders aside: the ones being replaced and the ones the old version had that the new one drops
    let mut aside: Vec<&String> = allowed.iter().collect();
    aside.sort();
    for f in aside {
        let p = addons_dir.join(f);
        if p.exists() {
            if let Err(e) = mv(&p, &old.join(f)) {
                return Err(undo(addons_dir, &staging, &root, &mut j, mv, in_use_text(f, &e)).into());
            }
            j.moved.push(f.clone());
            let _ = write_journal(&staging, &j);
        }
    }
    for f in &folders {
        if let Err(e) = mv(&root.join(f), &addons_dir.join(f)) {
            return Err(undo(addons_dir, &staging, &root, &mut j, mv, in_use_text(f, &e)).into());
        }
        j.placed.push(f.clone());
        let _ = write_journal(&staging, &j);
    }
    j.done = true;
    let _ = write_journal(&staging, &j);
    finish(&staging, &j);
    Ok(folders)
}

/// Put things back as they were after a failed swap: the new folders out of AddOns, the old ones back in. Returns
/// the error to show. When something can't be put back, the staging folder stays (with its journal) for `recover`.
fn undo(addons_dir: &Path, staging: &Path, root: &Path, j: &mut Journal, mv: Mover, cause: String) -> String {
    let old = staging.join("old");
    while let Some(p) = j.placed.pop() {
        if let Err(e) = mv(&addons_dir.join(&p), &root.join(&p)) {
            config::log(&format!("undo: can't take the new {p} out of AddOns: {e}"));
            j.placed.push(p);
            break;
        }
        let _ = write_journal(staging, j);
    }
    if j.placed.is_empty() {
        let mut left = Vec::new();
        for m in std::mem::take(&mut j.moved) {
            if let Err(e) = mv(&old.join(&m), &addons_dir.join(&m)) {
                config::log(&format!("undo: can't put {m} back: {e}"));
                left.push(m);
            }
        }
        j.moved = left;
    }
    let _ = write_journal(staging, j);
    if j.moved.is_empty() && j.placed.is_empty() {
        let _ = std::fs::remove_dir_all(staging);
        return cause;
    }
    config::log(&format!("undo left {} for later: placed {:?}, moved {:?}", staging.display(), j.placed, j.moved));
    format!(
        "{cause} The earlier copy of {} is kept safe in {} and EverlookDB puts it back the next time it starts.",
        j.moved.join(", "),
        staging.display()
    )
}

/// After a swap in which every new folder is in place: copy the replaced folders to the backup (when taking over),
/// then delete the staging folder. If the backup can't be made, the staging folder stays and `recover` tries again.
fn finish(staging: &Path, j: &Journal) {
    let old = staging.join("old");
    if let Some(dest) = &j.backup {
        for f in &j.moved {
            if !old.join(f).exists() {
                continue;
            }
            if let Err(e) = copy_dir(&old.join(f), &dest.join(f)) {
                config::log(&format!("backup of {f} to {} failed ({e}); kept in {}", dest.display(), staging.display()));
                return;
            }
        }
    }
    let _ = std::fs::remove_dir_all(staging);
}

/// Put right whatever a swap left in staging folders next to `addons_dir` (see `Journal`): a swap that finished has
/// its old copies deleted (after the backup, when there is one); one that didn't is undone, the new folders out and
/// the old ones back. An old copy is never deleted unless the new one is fully in place: when that can't be told (a
/// staging folder from before journals), an old folder whose place in AddOns is taken goes to the backups folder.
pub fn recover(addons_dir: &Path) {
    recover_in(addons_dir, &config::dir().join("backups"));
}

/// `recover`, at app start: under the install lock.
pub fn recover_leftovers(addons_dir: &Path) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    recover(addons_dir);
}

fn recover_in(addons_dir: &Path, backups: &Path) {
    let Some(parent) = addons_dir.parent() else { return };
    let Ok(rd) = std::fs::read_dir(parent) else { return };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with(STAGING_PREFIX) && e.path().is_dir() {
            recover_one(addons_dir, &e.path(), backups);
        }
    }
}

fn recover_one(addons_dir: &Path, staging: &Path, backups: &Path) {
    let old = staging.join("old");
    let journal: Option<Journal> = std::fs::read(staging.join(JOURNAL)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    match journal {
        Some(j) if j.done => {
            config::log(&format!("recover: finishing {}", staging.display()));
            finish(staging, &j);
            return;
        }
        Some(j) => {
            config::log(&format!("recover: undoing {} (placed {:?}, moved {:?})", staging.display(), j.placed, j.moved));
            for p in &j.placed {
                // the new copy is taken out only when the old one is there to go back (or there was none)
                if old.join(p).exists() || !j.moved.contains(p) {
                    if let Err(e) = std::fs::remove_dir_all(addons_dir.join(p)) {
                        config::log(&format!("recover: can't take out the new {p}: {e}"));
                    }
                }
            }
        }
        None => config::log(&format!("recover: {} has no journal; putting back what is missing", staging.display())),
    }
    let olds: Vec<String> = std::fs::read_dir(&old).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    let mut left = false;
    for f in olds {
        let here = addons_dir.join(&f);
        let from = old.join(&f);
        let r = if here.exists() {
            // its place is taken by a copy that can't be told complete: kept in the backups folder, never deleted
            let dest = backups.join(format!("{}-recovered-{}", sanitize(&f), stamp())).join(&f);
            std::fs::create_dir_all(dest.parent().unwrap_or(backups))
                .and_then(|_| std::fs::rename(&from, &dest).or_else(|_| copy_dir(&from, &dest).and_then(|_| std::fs::remove_dir_all(&from))))
        } else {
            std::fs::rename(&from, &here)
        };
        if let Err(e) = r {
            config::log(&format!("recover: can't put {f} back from {}: {e}", staging.display()));
            left = true;
        }
    }
    if !left {
        let _ = std::fs::remove_dir_all(staging);
    }
}

fn zip_dir(install: &Install, id: &str) -> PathBuf {
    config::dir().join("zips").join(short_hash(&format!("{}|{id}", install.id)))
}

/// Install or update `addon` in `install`. `take_over`: folders of this addon another app installed may be replaced
/// (they are copied to the backups folder first).
pub fn install(install: &Install, addon: &Addon, take_over: &[String], progress: OnProgress) -> Result<Change, Failure> {
    let _busy = Busy::start();
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = addon.file_for(&install.accepts).ok_or_else(|| format!("{} has no download for {}.", addon.name, install.name))?;
    // from a repo the player added: only a zip attached to one of its GitHub releases
    if addon.custom && !crate::github::safe_download(&file.url) {
        return Err(format!("{}'s download isn't a GitHub release file, so EverlookDB didn't install it.", addon.name).into());
    }
    let mut records = Records::load();
    let old = records.of(&install.id).get(&addon.id).cloned();
    let dir = zip_dir(install, &addon.id);
    let name = if file.name.is_empty() { format!("{}.zip", short_hash(&file.url)) } else { file.name.replace(['/', '\\', ':'], "_") };
    let zip = dir.join(&name);
    let tmp = dir.join(format!("{name}.part"));
    download(&file.url, file.size, &tmp, progress)?;
    std::fs::rename(&tmp, &zip).map_err(logged(SAVE_ZIP))?;

    let mut allowed: HashSet<String> = old.as_ref().map(|r| r.folders.iter().cloned().collect()).unwrap_or_default();
    let take: HashSet<String> = take_over.iter().filter(|f| !allowed.contains(*f)).cloned().collect();
    allowed.extend(take.iter().cloned());
    let backup = (!take.is_empty() && take.iter().any(|f| install.addons_dir.join(f).exists()))
        .then(|| config::dir().join("backups").join(format!("{}-{}", sanitize(&addon.name), stamp())));
    let folders = match install_zip(&install.addons_dir, &zip, &allowed, backup.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            if old.as_ref().and_then(|r| r.zip.as_ref()) != Some(&zip) {
                let _ = std::fs::remove_file(&zip);
            }
            return Err(e);
        }
    };
    let previous = match &old {
        Some(r) if r.zip.as_ref().is_some_and(|z| z != &zip && z.is_file()) => Some(Prev { version: r.version.clone(), zip: r.zip.clone().unwrap() }),
        Some(r) => r.previous.clone(),
        None => None,
    };
    keep_only(&dir, &[Some(&zip), previous.as_ref().map(|p| &p.zip)]);
    if backup.is_some() {
        prune_backups(&config::dir().join("backups"), &sanitize(&addon.name), KEEP_BACKUPS);
    }
    let rec = Record {
        id: addon.id.clone(),
        name: addon.name.clone(),
        version: addon.version.clone(),
        folders,
        zip: Some(zip),
        previous,
        installed: chrono::Local::now().to_rfc3339(),
    };
    records.installs.entry(install.id.clone()).or_default().insert(addon.id.clone(), rec);
    records.save()?;
    Conflicts::clear(&install.id, &addon.id);
    config::log(&format!("installed {} {} into {}", addon.id, addon.version, install.id));
    Ok(Change { install: install.name.clone(), name: addon.name.clone(), from: old.map(|r| r.version), to: addon.version.clone(), running: crate::wow::game_running(install) })
}

/// How many backups of one addon are kept (the newest).
const KEEP_BACKUPS: usize = 3;

/// Delete all but the newest `keep` backups named `<name>-YYYYMMDD-HHMMSS` in `dir`. Other folders are left alone.
fn prune_backups(dir: &Path, name: &str, keep: usize) {
    let stamp_of = |file: &str| -> Option<String> {
        let rest = file.strip_prefix(name)?.strip_prefix('-')?;
        let ok = rest.len() == 15 && rest.as_bytes()[8] == b'-' && rest.chars().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit());
        ok.then(|| rest.to_string())
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut found: Vec<(String, PathBuf)> = rd.flatten().filter_map(|e| stamp_of(&e.file_name().to_string_lossy()).map(|s| (s, e.path()))).collect();
    found.sort();
    let n = found.len();
    for (_, p) in found.into_iter().take(n.saturating_sub(keep)) {
        if let Err(e) = std::fs::remove_dir_all(&p) {
            config::log(&format!("can't remove the old backup {}: {e}", p.display()));
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Delete every file in `dir` except `keep`.
fn keep_only(dir: &Path, keep: &[Option<&PathBuf>]) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !keep.iter().any(|k| k.is_some_and(|k| k == &p)) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Put the previous version back (from its kept zip).
pub fn rollback(install: &Install, id: &str) -> Result<Change, Failure> {
    let _busy = Busy::start();
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = Records::load();
    let rec = records.of(&install.id).get(id).cloned().ok_or("EverlookDB didn't install this addon, so there's no earlier version to go back to.")?;
    let prev = rec.previous.clone().filter(|p| p.zip.is_file()).ok_or("No earlier version of it is kept any more.")?;
    let allowed: HashSet<String> = rec.folders.iter().cloned().collect();
    let folders = install_zip(&install.addons_dir, &prev.zip, &allowed, None)?;
    let new = Record {
        version: prev.version.clone(),
        folders,
        zip: Some(prev.zip.clone()),
        previous: rec.zip.clone().filter(|z| z.is_file()).map(|z| Prev { version: rec.version.clone(), zip: z }),
        installed: chrono::Local::now().to_rfc3339(),
        ..rec.clone()
    };
    records.installs.entry(install.id.clone()).or_default().insert(id.to_string(), new);
    records.save()?;
    config::log(&format!("rolled back {id} to {} in {}", prev.version, install.id));
    Ok(Change { install: install.name.clone(), name: rec.name, from: Some(rec.version), to: prev.version, running: crate::wow::game_running(install) })
}

/// What uninstalling one listed addon does: the folders that go to the Recycle Bin, and the ones that stay because
/// another addon lists them too.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub key: String,
    pub name: String,
    pub folders: Vec<String>,
    pub kept: Vec<String>,
    /// installed by EverlookDB: its record and kept zips go too
    pub managed: bool,
}

/// One plain folder name, as a direct child of AddOns: not empty, not `.` or `..`, no separators, no drive, no
/// trailing dot or space (Windows would drop them and name another folder).
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':', '\0'])
        && !name.ends_with(['.', ' '])
        && matches!(Path::new(name).components().collect::<Vec<_>>()[..], [std::path::Component::Normal(_)])
}

/// A link: a symlink or a junction (any reparse point), which is never followed or removed.
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// The path of folder `name` in `addons_dir`, when it may be removed: a real folder directly in AddOns (never AddOns
/// itself, nothing outside it), and not a link or junction.
pub fn removable(addons_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if !plain_name(name) {
        return Err(format!("“{name}” isn't a folder in AddOns, so nothing was removed."));
    }
    let p = addons_dir.join(name);
    if p.parent() != Some(addons_dir) {
        return Err(format!("“{name}” isn't a folder in AddOns, so nothing was removed."));
    }
    let meta = std::fs::symlink_metadata(&p).map_err(logged(format!("Can't read {name} in AddOns, so nothing was removed.")))?;
    if is_link(&meta) {
        return Err(format!("{name} is a link to another folder (a junction or shortcut). EverlookDB doesn't remove links, so nothing was removed."));
    }
    if !meta.is_dir() {
        return Err(format!("{name} isn't a folder, so nothing was removed."));
    }
    Ok(p)
}

/// Split an addon's folders into the ones that go and the ones another addon lists too (`others`, lower case).
fn split_shared(folders: &[String], others: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    folders.iter().cloned().partition(|f| !others.contains(&f.to_ascii_lowercase()))
}

/// What uninstalling the listed addon `key` would remove. Every folder that goes is checked (`removable`) first.
pub fn plan(install: &Install, catalog: Option<&Catalog>, key: &str) -> Result<Plan, String> {
    let items = list(install, catalog);
    let it = items.iter().find(|i| i.key == key).ok_or("That addon isn't in the AddOns folder any more.")?;
    let records = Records::load().of(&install.id);
    // folders another listed addon has, or another addon EverlookDB installed lists, are kept
    let others: HashSet<String> = items
        .iter()
        .filter(|o| o.key != key)
        .flat_map(|o| o.folders.iter())
        .chain(records.iter().filter(|(k, _)| k.as_str() != key).flat_map(|(_, r)| r.folders.iter()))
        .map(|f| f.to_ascii_lowercase())
        .collect();
    let (folders, kept) = split_shared(&it.folders, &others);
    if folders.is_empty() {
        return Err(format!("Every folder of {} belongs to another addon too, so there's nothing to remove on its own.", it.name));
    }
    for f in &folders {
        removable(&install.addons_dir, f)?;
    }
    Ok(Plan { key: key.to_string(), name: it.name.clone(), folders, kept, managed: it.managed })
}

/// Moves one folder out of the way for good (the Recycle Bin, in the app; a test folder in tests).
pub type Trash<'a> = &'a mut dyn FnMut(&Path) -> Result<(), String>;

/// Uninstall a listed addon, whoever installed it: exactly the folders the player confirmed (`confirmed`, from
/// `plan`) go to the Recycle Bin. For one EverlookDB installed, its kept zips and its record go too. Nothing else is
/// recorded or changed.
pub fn uninstall(install: &Install, catalog: Option<&Catalog>, key: &str, confirmed: &[String], trash: Trash) -> Result<Plan, String> {
    let _busy = Busy::start();
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = plan(install, catalog, key)?;
    let same = |a: &[String], b: &[String]| {
        let set = |x: &[String]| x.iter().map(|f| f.to_ascii_lowercase()).collect::<HashSet<_>>();
        set(a) == set(b)
    };
    if !same(&p.folders, confirmed) {
        return Err(format!("{}'s folders changed since you asked. Nothing was removed; try again.", p.name));
    }
    // all checked again right before anything moves
    let paths: Vec<PathBuf> = p.folders.iter().map(|f| removable(&install.addons_dir, f)).collect::<Result<_, _>>()?;
    let mut failed = Vec::new();
    for (f, path) in p.folders.iter().zip(&paths) {
        if let Err(e) = trash(path) {
            failed.push(format!("{f} ({e})"));
        }
    }
    if !failed.is_empty() {
        config::log(&format!("uninstalling {key} from {}: {}", install.id, failed.join("; ")));
        return Err(format!("Windows couldn't move {} to the Recycle Bin: WoW or another program may be using its files. Close WoW, then try again.", p.name));
    }
    if p.managed {
        let _ = std::fs::remove_dir_all(zip_dir(install, key));
        let mut records = Records::load();
        if let Some(m) = records.installs.get_mut(&install.id) {
            m.remove(key);
        }
        records.save()?;
    }
    config::log(&format!("uninstalled {key} from {} (to the Recycle Bin): {}", install.id, p.folders.join(", ")));
    Ok(p)
}

/// Move a folder to the Recycle Bin (Windows shell; links inside are moved as links, never followed). Never a
/// silent permanent delete: when the Recycle Bin can't take it, Windows asks first.
#[cfg(windows)]
pub fn recycle(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_NORECURSEREPARSE, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE,
        SHFILEOPSTRUCTW,
    };
    if !path.is_absolute() {
        return Err("not a full path".into());
    }
    // a list of paths, each ending in a NUL, the list in another
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING | FOF_NORECURSEREPARSE) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!("Windows error {code:#x}"));
    }
    if op.fAnyOperationsAborted != 0 || std::fs::symlink_metadata(path).is_ok() {
        return Err("it's still there".into());
    }
    Ok(())
}

/// Move a folder to the Trash (macOS and others: ~/.Trash, under a new name when one is taken).
#[cfg(not(windows))]
pub fn recycle(path: &Path) -> Result<(), String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("no home folder")?;
    let bin = home.join(".Trash");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("no folder name")?.to_string_lossy().into_owned();
    let mut dest = bin.join(&name);
    let mut n = 1;
    while dest.exists() {
        n += 1;
        dest = bin.join(format!("{name} {n}"));
    }
    std::fs::rename(path, &dest).map_err(|e| e.to_string())
}

/// Why nothing is installed or updated in a folder without the game (leftover files of an uninstalled game).
pub const NO_GAME: &str = "This game isn't installed here (only leftover files), so EverlookDB doesn't install or update addons in it.";

/// The addons we installed in `install` that have a newer version in the catalog. None when the game isn't there:
/// nothing is downloaded into a folder no game uses.
pub fn pending<'a>(install: &Install, catalog: &'a Catalog) -> Vec<(&'a Addon, Vec<String>)> {
    if !install.game {
        return Vec::new();
    }
    // Every addon matched to the catalog with a newer version, also ones another app or the player put there
    // (players expect "update by itself" to mean all of them). Those are taken over only when the catalog's version
    // is certainly higher (both plain numbers), so a build like "1.2-forever" is never swapped for "1.2"; their
    // folders are passed as consent, and install() backs up the old copy first.
    list(install, Some(catalog))
        .into_iter()
        .filter(|it| it.auto)
        .filter_map(|it| catalog.get(&it.key).map(|a| (a, if it.managed { Vec::new() } else { it.folders.clone() })))
        .collect()
}

/// An addon we didn't install that auto-update may replace: not a developer's git checkout, and a real version
/// (not a packager placeholder like "@project-version@", which would always look outdated).
fn adoptable(install: &Install, it: &Item) -> bool {
    let real_version = it.version.as_deref().is_some_and(|v| !v.contains('@'));
    real_version && !it.folders.iter().any(|f| install.addons_dir.join(f).join(".git").exists())
}

/// Update every addon we know (see `pending`) that has a newer version in the catalog. `progress` gets the addon's id with
/// each download report.
pub fn update_all(install: &Install, catalog: &Catalog, progress: &mut dyn FnMut(&str, u64, u64)) -> (Vec<Change>, Vec<Failed>) {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for (addon, consent) in pending(install, catalog) {
        // a folder in the way was reported once for this version; it waits for the player's "Replace"
        if Conflicts::known(&install.id, &addon.id, &addon.version) {
            continue;
        }
        match self::install(install, addon, &consent, &mut |d, t| progress(&addon.id, d, t)) {
            Ok(c) => done.push(c),
            Err(e) => {
                if !e.conflict.is_empty() {
                    Conflicts::remember(&install.id, &addon.id, &addon.version, &e.conflict);
                }
                failed.push(Failed { id: addon.id.clone(), name: addon.name.clone(), message: e.message, conflict: e.conflict });
            }
        }
    }
    (done, failed)
}

/// Folders in the way of an update (another app's or the player's), per install and addon, for the version that
/// met them: reported once, then the addon waits for the player's "Replace (backup kept)" (conflicts.json).
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Conflicts {
    #[serde(default)]
    pub seen: BTreeMap<String, Conflict>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Conflict {
    pub version: String,
    pub folders: Vec<String>,
}

impl Conflicts {
    fn path() -> PathBuf {
        config::dir().join("conflicts.json")
    }
    fn key(install: &str, id: &str) -> String {
        format!("{install}|{id}")
    }
    pub fn load() -> Conflicts {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(config::dir());
        let _ = std::fs::write(Self::path(), serde_json::to_vec_pretty(self).unwrap_or_default());
    }
    /// The folders in the way of this version, if it met any.
    pub fn get(&self, install: &str, id: &str, version: &str) -> Option<&Conflict> {
        self.seen.get(&Self::key(install, id)).filter(|c| c.version == version)
    }
    pub fn known(install: &str, id: &str, version: &str) -> bool {
        Self::load().get(install, id, version).is_some()
    }
    pub fn remember(install: &str, id: &str, version: &str, folders: &[String]) {
        let mut c = Self::load();
        c.seen.insert(Self::key(install, id), Conflict { version: version.into(), folders: folders.to_vec() });
        c.save();
    }
    pub fn clear(install: &str, id: &str) {
        let mut c = Self::load();
        if c.seen.remove(&Self::key(install, id)).is_some() {
            c.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::File as CFile;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("edb-mgr-{name}-{}-{}", std::process::id(), stamp()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(path: &Path, files: &[(&str, &str)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in files {
            z.start_file(*name, opts).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn reads_release_tags() {
        assert!(!differs(Some("12.9.0"), "bundle/v12.9.0+v1.1.1"));
        assert!(differs(Some("12.8.0"), "bundle/v12.9.0+v1.1.1"));
        assert!(!differs(Some("12.9.0"), "v12.8.0"));
        assert!(!differs(Some("v427.9"), "427.9"));
    }

    #[test]
    fn never_adopts_an_addon_whose_version_cant_be_compared() {
        assert!(strictly_newer(Some("1.2"), "1.3"));
        assert!(strictly_newer(Some("v1.2"), "1.2.1"));
        assert!(!strictly_newer(Some("1.2-forever"), "1.2"), "a different build of the same version");
        assert!(!strictly_newer(Some("1.2-forever"), "1.3"), "can't be told");
        assert!(!strictly_newer(Some("1.3"), "1.2"), "never a downgrade");
        assert!(!strictly_newer(Some("1.2"), "1.2"));
        assert!(!strictly_newer(None, "1.2"));

        let (d, mut install) = test_install(
            "adopt",
            &[
                ("Forever", "## Title: Forever\n## Version: 1.2-forever\n"),
                ("Plain", "## Title: Plain\n## Version: 1.2\n"),
                ("Ahead", "## Title: Ahead\n## Version: 2.0\n"),
            ],
        );
        install.game = true;
        let entry = |id: &str, folder: &str, version: &str| Addon {
            id: id.into(),
            name: folder.into(),
            version: version.into(),
            folders: vec![folder.into()],
            files: vec![CFile { flavors: vec!["forever".into()], url: "https://example.test/x.zip".into(), ..Default::default() }],
            ..Default::default()
        };
        let cat = Catalog {
            version: 1,
            addons: vec![entry("github:x/Forever", "Forever", "1.2"), entry("github:x/Plain", "Plain", "1.3"), entry("github:x/Ahead", "Ahead", "1.9")],
            ..Default::default()
        };
        let items = list(&install, Some(&cat));
        let get = |n: &str| items.iter().find(|i| i.name == n).unwrap();
        // can't be told: a click may update it, the automatic update never does
        assert!(get("Forever").update && !get("Forever").auto);
        assert!(get("Plain").update && get("Plain").auto);
        // the catalog is behind what is installed: no update at all
        assert!(!get("Ahead").update && !get("Ahead").auto);
        let ids: Vec<&str> = pending(&install, &cat).iter().map(|(a, _)| a.id.as_str()).collect();
        assert_eq!(ids, vec!["github:x/Plain"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plain_sentences_and_conflicts_once() {
        assert!(status_text(404).contains("isn't there any more"));
        assert!(status_text(503).contains("trouble"));
        assert_eq!(conflict_text(&["A".into()]), "A is already in your AddOns folder, from another app or put there by hand, so EverlookDB left it alone. Replace it to update (a backup is kept).");
        assert!(conflict_text(&["A".into(), "B".into(), "C".into()]).starts_with("A, B and C are already"));
        let e = std::io::Error::from_raw_os_error(5);
        let t = in_use_text("Foo", &e);
        assert!(!t.contains("os error") && t.contains("Close WoW"), "{t}");

        let _ = config::dir(); // the tests' data folder
        let (install, id) = (format!("test-{}", stamp()), "github:x/Clash");
        assert!(!Conflicts::known(&install, id, "1.0"));
        Conflicts::remember(&install, id, "1.0", &["Theirs".into()]);
        assert!(Conflicts::known(&install, id, "1.0"));
        // a newer version is tried (and reported) again
        assert!(!Conflicts::known(&install, id, "1.1"));
        Conflicts::clear(&install, id);
        assert!(!Conflicts::known(&install, id, "1.0"));
    }

    #[test]
    fn busy_jobs_hold_off_the_self_update() {
        // other tests may be installing at the same moment; this checks the counting only
        let before = ACTIVE.load(std::sync::atomic::Ordering::SeqCst);
        {
            let _a = Busy::start();
            let _b = Busy::start();
            assert_eq!(ACTIVE.load(std::sync::atomic::Ordering::SeqCst), before + 2);
            assert!(!idle());
            assert!(hold().is_none());
        }
    }

    #[test]
    fn keeps_the_newest_three_backups_per_addon() {
        let d = tmp("prune");
        for n in ["Foo-20261101-100000", "Foo-20261102-100000", "Foo-20261103-100000", "Foo-20261104-100000", "Foo-20261105-090000", "Foo_Bar-20261101-100000", "Foo-recovered-20261101-100000", "Foo-x"] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        prune_backups(&d, "Foo", 3);
        assert_eq!(names(&d), vec!["Foo-20261103-100000", "Foo-20261104-100000", "Foo-20261105-090000", "Foo-recovered-20261101-100000", "Foo-x", "Foo_Bar-20261101-100000"]);
        prune_backups(&d.join("missing"), "Foo", 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_zip_slip() {
        assert!(safe_parts("../evil.lua").is_none());
        assert!(safe_parts("Addon/../../evil.lua").is_none());
        assert!(safe_parts("/etc/passwd").is_none());
        assert!(safe_parts("C:/Windows/evil.dll").is_none());
        assert!(safe_parts("..\\evil.lua").is_none());
        assert_eq!(safe_parts("Addon/./x.lua").unwrap(), vec!["Addon", "x.lua"]);

        let d = tmp("slip");
        let z = d.join("bad.zip");
        make_zip(&z, &[("Good/Good.toc", "## Title: Good"), ("../escaped.txt", "nope")]);
        let err = unzip(&z, &d.join("out")).unwrap_err();
        assert_eq!(err, UNSAFE);
        assert!(!d.join("escaped.txt").exists());
        assert!(!d.join("out").join("Good").exists(), "nothing is written when any entry is unsafe");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unwraps_a_wrapper_folder() {
        let d = tmp("wrap");
        let z = d.join("w.zip");
        make_zip(&z, &[("Thing-1.0/Thing/Thing.toc", "## Version: 1.0"), ("Thing-1.0/README.md", "hi")]);
        let tops = unzip(&z, &d.join("out")).unwrap();
        let (root, folders) = addon_root(&d.join("out"), &tops).unwrap();
        assert_eq!(folders, vec!["Thing"]);
        assert!(root.ends_with("Thing-1.0"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// install, update, rollback and remove on a temp folder, never touching a folder we didn't install
    #[test]
    fn install_update_rollback_remove() {
        let d = tmp("flow");
        let addons_dir = d.join("World of Warcraft").join("_forever_").join("Interface").join("AddOns");
        std::fs::create_dir_all(addons_dir.join("Someone_Else")).unwrap();
        std::fs::write(addons_dir.join("Someone_Else").join("Someone_Else.toc"), "## Title: Not ours").unwrap();
        let v1 = d.join("v1.zip");
        let v2 = d.join("v2.zip");
        make_zip(&v1, &[("Foo/Foo.toc", "## Version: 1.0"), ("Foo_Options/Foo_Options.toc", "## Version: 1.0")]);
        make_zip(&v2, &[("Foo/Foo.toc", "## Version: 2.0")]);
        let all = HashSet::new();

        let f1 = install_zip(&addons_dir, &v1, &all, None).unwrap();
        assert_eq!(f1, vec!["Foo", "Foo_Options"]);
        let read = |f: &str| std::fs::read_to_string(addons_dir.join(f).join(format!("{f}.toc"))).unwrap();
        assert_eq!(read("Foo"), "## Version: 1.0");

        // update: replaces our folders, drops the one the new version no longer has
        let ours: HashSet<String> = f1.iter().cloned().collect();
        let f2 = install_zip(&addons_dir, &v2, &ours, None).unwrap();
        assert_eq!(f2, vec!["Foo"]);
        assert_eq!(read("Foo"), "## Version: 2.0");
        assert!(!addons_dir.join("Foo_Options").exists());

        // rollback = installing the kept previous zip
        let ours2: HashSet<String> = f2.iter().cloned().collect();
        install_zip(&addons_dir, &v1, &ours2, None).unwrap();
        assert_eq!(read("Foo"), "## Version: 1.0");
        assert!(addons_dir.join("Foo_Options").exists());

        // a zip that would overwrite a folder we didn't install is refused, and nothing changes
        let clash = d.join("clash.zip");
        make_zip(&clash, &[("Someone_Else/Someone_Else.toc", "## Title: Mine now")]);
        let err = install_zip(&addons_dir, &clash, &HashSet::new(), None).unwrap_err();
        assert_eq!(err.conflict, vec!["Someone_Else"]);
        assert!(err.message.starts_with("Someone_Else is already in your AddOns folder"), "{err}");
        assert_eq!(read("Someone_Else"), "## Title: Not ours");

        // taking over (the player asked) backs the old folder up first
        let backup = d.join("backup");
        let take: HashSet<String> = ["Someone_Else".to_string()].into();
        install_zip(&addons_dir, &clash, &take, Some(&backup)).unwrap();
        assert_eq!(read("Someone_Else"), "## Title: Mine now");
        assert_eq!(std::fs::read_to_string(backup.join("Someone_Else").join("Someone_Else.toc")).unwrap(), "## Title: Not ours");

        // no staging folders left behind
        let left: Vec<_> = std::fs::read_dir(addons_dir.parent().unwrap()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".everlookdb")).collect();
        assert!(left.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn stagings(addons_dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(addons_dir.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .map(|e| e.path())
            .collect()
    }

    /// a failed swap puts the old version back; when even that fails, the old copy stays in staging and the next
    /// recover puts it back; it is never deleted
    #[test]
    fn a_failed_install_never_loses_the_old_addon() {
        let d = tmp("atomic");
        let addons_dir = d.join("World of Warcraft").join("_forever_").join("Interface").join("AddOns");
        let v1 = d.join("v1.zip");
        let v2 = d.join("v2.zip");
        make_zip(&v1, &[("Foo/Foo.toc", "## Version: 1.0"), ("Foo_Options/Foo_Options.toc", "## Version: 1.0")]);
        make_zip(&v2, &[("Foo/Foo.toc", "## Version: 2.0"), ("Foo_Zed/Foo_Zed.toc", "## Version: 2.0")]);
        let ours: HashSet<String> = install_zip(&addons_dir, &v1, &HashSet::new(), None).unwrap().into_iter().collect();
        let read = |f: &str| std::fs::read_to_string(addons_dir.join(f).join(format!("{f}.toc"))).ok();

        // placing the second new folder fails: everything goes back as it was
        let mut fail_place = |a: &Path, b: &Path| {
            if b.ends_with("Foo_Zed") && a.to_string_lossy().contains(STAGING_PREFIX) {
                return Err(std::io::Error::other("denied"));
            }
            std::fs::rename(a, b)
        };
        let err = install_zip_with(&addons_dir, &v2, &ours, None, &mut fail_place).unwrap_err();
        assert!(err.message.contains("Foo_Zed") && err.message.contains("Close WoW"), "{err}");
        assert_eq!(read("Foo").as_deref(), Some("## Version: 1.0"));
        assert_eq!(read("Foo_Options").as_deref(), Some("## Version: 1.0"));
        assert!(read("Foo_Zed").is_none());
        assert!(stagings(&addons_dir).is_empty());

        // and putting the old Foo back fails too: it stays in staging, with a journal
        let mut tries = 0;
        let mut fail_more = |a: &Path, b: &Path| {
            if b.ends_with("Foo_Zed") && a.to_string_lossy().contains(STAGING_PREFIX) {
                return Err(std::io::Error::other("denied"));
            }
            if b == addons_dir.join("Foo") && a.parent().is_some_and(|p| p.ends_with("old")) {
                tries += 1;
                return Err(std::io::Error::other("in use"));
            }
            std::fs::rename(a, b)
        };
        let err = install_zip_with(&addons_dir, &v2, &ours, None, &mut fail_more).unwrap_err();
        assert_eq!(tries, 1);
        assert!(err.message.contains("kept safe"), "{err}");
        assert!(read("Foo").is_none(), "Foo couldn't go back yet");
        let left = stagings(&addons_dir);
        assert_eq!(left.len(), 1);
        assert_eq!(std::fs::read_to_string(left[0].join("old").join("Foo").join("Foo.toc")).unwrap(), "## Version: 1.0");

        // the next start (or install) puts it back
        recover_in(&addons_dir, &d.join("backups"));
        assert_eq!(read("Foo").as_deref(), Some("## Version: 1.0"));
        assert_eq!(read("Foo_Options").as_deref(), Some("## Version: 1.0"));
        assert!(stagings(&addons_dir).is_empty());

        // then a normal update works
        install_zip(&addons_dir, &v2, &ours, None).unwrap();
        assert_eq!(read("Foo").as_deref(), Some("## Version: 2.0"));
        assert!(read("Foo_Options").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// staging folders a crash left behind
    #[test]
    fn recovers_leftover_staging() {
        let d = tmp("leftover");
        let addons_dir = d.join("World of Warcraft").join("_forever_").join("Interface").join("AddOns");
        let backups = d.join("backups");
        let put = |dir: &Path, f: &str, text: &str| {
            std::fs::create_dir_all(dir.join(f)).unwrap();
            std::fs::write(dir.join(f).join(format!("{f}.toc")), text).unwrap();
        };
        let parent = addons_dir.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&addons_dir).unwrap();

        // from before journals: a missing folder goes back; one whose place is taken goes to backups, not away
        let legacy = parent.join(format!("{STAGING_PREFIX}1-legacy"));
        put(&legacy.join("old"), "Gone", "## Version: old");
        put(&legacy.join("old"), "Taken", "## Version: old");
        put(&addons_dir, "Taken", "## Version: new");
        recover_in(&addons_dir, &backups);
        assert_eq!(std::fs::read_to_string(addons_dir.join("Gone").join("Gone.toc")).unwrap(), "## Version: old");
        assert_eq!(std::fs::read_to_string(addons_dir.join("Taken").join("Taken.toc")).unwrap(), "## Version: new");
        let kept: Vec<_> = std::fs::read_dir(&backups).unwrap().flatten().collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(kept[0].path().join("Taken").join("Taken.toc")).unwrap(), "## Version: old");
        assert!(!legacy.exists());

        // cut short mid-swap: the new half goes, the old comes back
        let mid = parent.join(format!("{STAGING_PREFIX}2-mid"));
        std::fs::remove_dir_all(addons_dir.join("Gone")).unwrap();
        put(&mid.join("old"), "Gone", "## Version: old");
        put(&addons_dir, "Gone", "## Version: half new");
        put(&addons_dir, "Gone_Extra", "## Version: half new");
        let j = Journal { moved: vec!["Gone".into()], placed: vec!["Gone".into(), "Gone_Extra".into()], done: false, backup: None };
        write_journal(&mid, &j).unwrap();
        recover_in(&addons_dir, &backups);
        assert_eq!(std::fs::read_to_string(addons_dir.join("Gone").join("Gone.toc")).unwrap(), "## Version: old");
        assert!(!addons_dir.join("Gone_Extra").exists());
        assert!(!mid.exists());

        // finished but not cleaned up: the old copies go only after their backup
        let done = parent.join(format!("{STAGING_PREFIX}3-done"));
        put(&done.join("old"), "Theirs", "## Version: theirs");
        let j = Journal { moved: vec!["Theirs".into()], placed: vec!["Theirs".into()], done: true, backup: Some(d.join("taken-over")) };
        write_journal(&done, &j).unwrap();
        recover_in(&addons_dir, &backups);
        assert_eq!(std::fs::read_to_string(d.join("taken-over").join("Theirs").join("Theirs.toc")).unwrap(), "## Version: theirs");
        assert!(!done.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn progress_is_throttled() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut t = Throttle::new(PROGRESS_EVERY);
        // the first report always goes through
        assert!(t.ready(ms(0), false));
        // then at most one per 100 ms
        assert!(!t.ready(ms(1), false));
        assert!(!t.ready(ms(99), false));
        assert!(t.ready(ms(100), false));
        assert!(!t.ready(ms(150), false));
        // the last one always goes through, and restarts the wait
        assert!(t.ready(ms(151), true));
        assert!(!t.ready(ms(200), false));
        assert!(t.ready(ms(251), false));
        // a steady stream of reads every 5 ms for 2 s gives about 10 a second
        let mut t = Throttle::new(PROGRESS_EVERY);
        let sent = (0..400).filter(|i| t.ready(ms(i * 5), false)).count();
        assert_eq!(sent, 20);
        // a clock that goes backwards (it can't with Instant, but saturating anyway) doesn't let a flood through
        let mut t = Throttle::new(PROGRESS_EVERY);
        assert!(t.ready(ms(500), false));
        assert!(!t.ready(ms(400), false));
    }

    #[test]
    fn records_flow_with_local_files() {
        // the full install()/rollback()/remove() path needs a download; check the pieces around it instead
        let a = Addon {
            id: "github:x/Foo".into(),
            name: "Foo".into(),
            version: "v2".into(),
            files: vec![CFile { flavors: vec!["forever".into()], url: "http://insecure".into(), ..Default::default() }],
            ..Default::default()
        };
        assert!(a.file_for(&["forever".into()]).is_some());
        assert!(download("http://insecure", 0, &std::env::temp_dir().join("x.zip"), &mut |_, _| {}).is_err());
        assert!(differs(Some("v1.0"), "1.1"));
        assert!(!differs(Some("v1.0"), "1.0"));
        assert!(!differs(None, "1.0"));
        assert!(!differs(Some("12.9.0"), "v12.9.0+v1.1.1"));
        assert!(!differs(Some("4.18.5"), "4.18.4.1"), "an installed newer version isn't an update");
        assert!(differs(Some("4.18.4"), "4.18.4.1"));
        assert!(differs(Some("v1.0-beta"), "v1.0"));
    }

    #[test]
    fn icons_links_and_the_addon_list() {
        let d = tmp("export");
        let root = d.join("World of Warcraft");
        let addons_dir = root.join("_forever_").join("Interface").join("AddOns");
        let toc = |folder: &str, text: &str| {
            std::fs::create_dir_all(addons_dir.join(folder)).unwrap();
            std::fs::write(addons_dir.join(folder).join(format!("{folder}.toc")), text).unwrap();
        };
        toc("Titan", "## Title: Titan Panel\n## Version: 9.3.6\n## Author: Titan Dev Team\n## IconTexture: Interface\\AddOns\\Titan\\Artwork\\Titan\n## X-Website: https://titanpanel.example\n");
        toc("TitanBag", "## Title: Titan Bag\n## Dependencies: Titan\n## X-Curse-Project-ID: 489\n## X-Wago-ID: abc\n");
        std::fs::create_dir_all(addons_dir.join("Titan").join("Artwork")).unwrap();
        std::fs::write(addons_dir.join("Titan").join("Artwork").join("Titan.tga"), b"x").unwrap();
        toc("Plain", "## Title: Plain\n## IconTexture: Interface\\Icons\\INV_Misc_Bag_08\n");
        let install = crate::wow::installs_in(&root).pop().unwrap();
        let items = list(&install, None);
        let titan = items.iter().find(|i| i.name == "Titan Panel").unwrap();
        assert_eq!(titan.icon_file.as_deref(), Some(addons_dir.join("Titan").join("Artwork").join("Titan.tga").as_path()));
        assert_eq!(titan.links, vec![("website", "https://titanpanel.example".to_string()), ("CurseForge", "489".into()), ("Wago", "abc".into())]);
        let plain = items.iter().find(|i| i.name == "Plain").unwrap();
        assert_eq!(plain.icon_file, None);

        let text = export_text(&install, &items, "2026-11-16");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{}: 2 addons, listed by EverlookDB on 2026-11-16", install.name));
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Plain | folder: Plain");
        assert_eq!(lines[3], "Titan Panel 9.3.6 by Titan Dev Team | folders: Titan, TitanBag | website: https://titanpanel.example | CurseForge: 489 | Wago: abc");
        assert_eq!(lines.len(), 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The whole flow against real GitHub downloads, into a temp folder: `cargo test -- --ignored`.
    #[test]
    #[ignore = "downloads from GitHub"]
    fn real_download_install_update_rollback_remove() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let d = tmp("real");
        config::init_dir(d.join("appdata"));
        let root = d.join("World of Warcraft");
        let dir = root.join("_forever_");
        std::fs::create_dir_all(dir.join("Interface").join("AddOns")).unwrap();
        let mut install = crate::wow::installs_in(&root).pop().unwrap();
        // a temp folder has no game in it; pretend it has, or nothing would be updated
        assert!(pending(&install, &Catalog::default()).is_empty());
        install.game = true;
        let file = |name: &str, size: u64| CFile {
            flavors: vec!["forever".into()],
            url: format!("https://github.com/quikmn/EverlookDB/releases/download/v0.1.3/{name}"),
            name: name.into(),
            size,
            ..Default::default()
        };
        let v1 = Addon { id: "github:quikmn/EverlookDB".into(), name: "EverlookDB Collector".into(), version: "v0.1.3".into(), files: vec![file("EverlookDB-v0.1.3-forever.zip", 69995)], ..Default::default() };
        // the same release's other asset stands in for a newer version
        let v2 = Addon { version: "v0.1.4".into(), files: vec![file("EverlookDB.zip", 69995)], ..v1.clone() };

        // someone else's copy is there first: refused without consent, backed up with it
        let pre = install.addons_dir.join("EverlookDB");
        std::fs::create_dir_all(&pre).unwrap();
        std::fs::write(pre.join("EverlookDB.toc"), "## Version: old").unwrap();
        assert!(super::install(&install, &v1, &[], &mut |_, _| {}).is_err());
        let mut reports = Vec::new();
        let c = super::install(&install, &v1, &["EverlookDB".to_string()], &mut |d, t| reports.push((d, t))).unwrap();
        // starts at 0 of the size, ends at size of size, never goes back
        assert_eq!(reports.first(), Some(&(0, 69995)));
        assert_eq!(reports.last(), Some(&(69995, 69995)));
        assert!(reports.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(c.to, "v0.1.3");
        assert!(std::fs::read_to_string(pre.join("EverlookDB.toc")).unwrap().contains("v0.1.3"));
        assert!(std::fs::read_dir(config::dir().join("backups")).unwrap().count() == 1);

        let c = super::install(&install, &v2, &[], &mut |_, _| {}).unwrap();
        assert_eq!((c.from.as_deref(), c.to.as_str()), (Some("v0.1.3"), "v0.1.4"));
        let item = list(&install, None).into_iter().find(|i| i.key == "github:quikmn/EverlookDB").unwrap();
        assert!(item.managed);
        assert_eq!(item.previous.as_deref(), Some("v0.1.3"));

        let c = rollback(&install, "github:quikmn/EverlookDB").unwrap();
        assert_eq!(c.to, "v0.1.3");
        let cat = Catalog { version: 1, addons: vec![v2.clone()], ..Default::default() };
        assert_eq!(pending(&install, &cat).len(), 1);
        let mut ids = HashSet::new();
        let (changes, failed) = update_all(&install, &cat, &mut |id, _, _| {
            ids.insert(id.to_string());
        });
        assert_eq!((changes.len(), failed.len()), (1, 0));
        assert_eq!(ids, HashSet::from(["github:quikmn/EverlookDB".to_string()]));
        assert!(pending(&install, &cat).is_empty());

        // size check
        let bad = Addon { files: vec![file("EverlookDB.zip", 12)], version: "v9".into(), ..v1.clone() };
        assert_eq!(super::install(&install, &bad, &[], &mut |_, _| {}).unwrap_err().message, DAMAGED);

        let key = "github:quikmn/EverlookDB";
        let p = plan(&install, None, key).unwrap();
        assert!(p.managed);
        let done = uninstall(&install, None, key, &p.folders, &mut |path: &Path| std::fs::remove_dir_all(path).map_err(|e| e.to_string())).unwrap();
        assert_eq!(done.name, "EverlookDB Collector");
        assert!(!pre.exists());
        assert!(Records::load().of(&install.id).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// a test install in a temp folder, with addon folders made from (folder, toc text)
    fn test_install(name: &str, tocs: &[(&str, &str)]) -> (PathBuf, Install) {
        let d = tmp(name);
        let root = d.join("World of Warcraft");
        let addons_dir = root.join("_forever_").join("Interface").join("AddOns");
        for (folder, text) in tocs {
            std::fs::create_dir_all(addons_dir.join(folder)).unwrap();
            std::fs::write(addons_dir.join(folder).join(format!("{folder}.toc")), text).unwrap();
        }
        std::fs::create_dir_all(&addons_dir).unwrap();
        (d, crate::wow::installs_in(&root).pop().unwrap())
    }

    /// stands in for the Recycle Bin: moves the folder into `bin`, and remembers what it was given
    fn bin_trash<'a>(bin: &'a Path, given: &'a mut Vec<String>) -> impl FnMut(&Path) -> Result<(), String> + 'a {
        move |p: &Path| {
            std::fs::create_dir_all(bin).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            given.push(name.clone());
            std::fs::rename(p, bin.join(name)).map_err(|e| e.to_string())
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn uninstalls_an_addon_from_elsewhere_exactly() {
        let (d, install) = test_install(
            "unmanaged",
            &[
                ("Titan", "## Title: Titan Panel\n## Version: 9.3.6\n"),
                ("TitanBag", "## Title: Titan Bag\n## Dependencies: Titan\n"),
                ("TitanXP", "## Title: Titan XP\n## Dependencies: Titan\n"),
                ("TomTom", "## Title: TomTom\n"),
                ("Titanic", "## Title: Not Titan\n"),
            ],
        );
        let addons = install.addons_dir.clone();
        // a loose file in AddOns is never touched
        std::fs::write(addons.join("readme.txt"), "x").unwrap();
        let items = list(&install, None);
        let titan = items.iter().find(|i| i.name == "Titan Panel").unwrap();
        assert!(!titan.managed && !titan.catalog);
        let p = plan(&install, None, &titan.key).unwrap();
        assert_eq!(p.folders, vec!["Titan", "TitanBag", "TitanXP"]);
        assert!(p.kept.is_empty() && !p.managed);

        // what the player confirmed must be what goes
        let bin = d.join("bin");
        let mut given = Vec::new();
        let err = uninstall(&install, None, &titan.key, &["Titan".into()], &mut bin_trash(&bin, &mut given)).unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert!(given.is_empty());

        let done = uninstall(&install, None, &titan.key, &p.folders, &mut bin_trash(&bin, &mut given)).unwrap();
        assert_eq!(done.name, "Titan Panel");
        assert_eq!(given, vec!["Titan", "TitanBag", "TitanXP"]);
        assert_eq!(names(&addons), vec!["Titanic", "TomTom", "readme.txt"]);
        assert_eq!(names(&bin), vec!["Titan", "TitanBag", "TitanXP"]);
        // nothing recorded
        assert!(Records::load().of(&install.id).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_anything_but_a_real_folder_in_addons() {
        let (d, install) = test_install("refuse", &[("Real", "## Title: Real\n")]);
        let addons = install.addons_dir.clone();
        std::fs::create_dir_all(d.join("Outside")).unwrap();
        std::fs::write(addons.join("loose.lua"), "x").unwrap();
        for bad in ["", ".", "..", "../Outside", "..\\Outside", "Real/..", "Real\\Sub", "C:\\Windows", "/etc", "C:Outside", "Real.", "Real "] {
            assert!(removable(&addons, bad).is_err(), "{bad:?} must be refused");
        }
        assert!(removable(&addons, "Missing").is_err());
        assert!(removable(&addons, "loose.lua").is_err());
        assert_eq!(removable(&addons, "Real").unwrap(), addons.join("Real"));

        // a junction to a folder outside AddOns: listed like any addon, but never removed or followed
        let target = d.join("Elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("Linked.toc"), "## Title: Linked\n").unwrap();
        let link = addons.join("Linked");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&target).output().unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let err = removable(&addons, "Linked").unwrap_err();
        assert!(err.contains("link"), "{err}");
        let item = list(&install, None).into_iter().find(|i| i.folders == ["Linked"]).unwrap();
        assert!(plan(&install, None, &item.key).unwrap_err().contains("link"));
        let mut given = Vec::new();
        let bin = d.join("bin");
        assert!(uninstall(&install, None, &item.key, &["Linked".into()], &mut bin_trash(&bin, &mut given)).is_err());
        assert!(given.is_empty());
        assert!(target.join("Linked.toc").exists() && link.exists());
        std::fs::remove_dir(&link).unwrap(); // unlinks the junction only
        assert!(target.join("Linked.toc").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keeps_folders_another_addon_lists() {
        let (d, install) = test_install("shared", &[("Alpha", "## Title: Alpha\n"), ("Alpha_Shared", "## Title: Shared lib\n"), ("Beta", "## Title: Beta\n")]);
        // Beta, installed by EverlookDB, lists Alpha_Shared too
        let mut records = Records::load();
        records.installs.entry(install.id.clone()).or_default().insert(
            "github:x/Beta".into(),
            Record { id: "github:x/Beta".into(), name: "Beta".into(), version: "1".into(), folders: vec!["Beta".into(), "Alpha_Shared".into()], ..Default::default() },
        );
        records.save().unwrap();
        let items = list(&install, None);
        let alpha = items.iter().find(|i| i.name == "Alpha").unwrap();
        assert_eq!(alpha.folders, vec!["Alpha"], "the record claimed Alpha_Shared first");

        // pure split: the shared folder stays
        let others: HashSet<String> = ["alpha_shared".to_string()].into();
        let (go, kept) = split_shared(&["Alpha".into(), "Alpha_Shared".into()], &others);
        assert_eq!((go, kept), (vec!["Alpha".to_string()], vec!["Alpha_Shared".to_string()]));

        let p = plan(&install, None, &alpha.key).unwrap();
        assert_eq!(p.folders, vec!["Alpha"]);
        let bin = d.join("bin");
        let mut given = Vec::new();
        uninstall(&install, None, &alpha.key, &p.folders, &mut bin_trash(&bin, &mut given)).unwrap();
        assert_eq!(names(&install.addons_dir), vec!["Alpha_Shared", "Beta"]);
        // Beta's record is untouched
        assert!(Records::load().of(&install.id).contains_key("github:x/Beta"));

        // removing Beta (ours) takes its folders and its record
        let p = plan(&install, None, "github:x/Beta").unwrap();
        assert!(p.managed);
        assert_eq!(p.folders, vec!["Beta", "Alpha_Shared"]);
        uninstall(&install, None, "github:x/Beta", &p.folders, &mut bin_trash(&bin, &mut given)).unwrap();
        assert!(names(&install.addons_dir).is_empty());
        assert!(Records::load().of(&install.id).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The real Recycle Bin, on a temp folder: `cargo test recycles -- --ignored`. Leaves one small folder named
    /// edb-mgr-recycle-... in the Recycle Bin.
    #[test]
    #[ignore = "puts a test folder in the Recycle Bin"]
    fn recycles_for_real() {
        let d = tmp("recycle");
        let f = d.join("EdbRecycleTest");
        std::fs::create_dir_all(f.join("Sub")).unwrap();
        std::fs::write(f.join("Sub").join("x.lua"), "x").unwrap();
        recycle(&f).unwrap();
        assert!(!f.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
