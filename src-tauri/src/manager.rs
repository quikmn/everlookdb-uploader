// Installing, updating, rolling back and removing addons.
//
// Download to a temp file, check its size, unzip into a staging folder (every entry must stay inside it: no `..`,
// no absolute paths), then swap the folders in. The previous version's zip is kept for one-click rollback. Folders
// we didn't install are never deleted: when the player updates an addon another app installed, its old folders are
// copied to the app's backups folder first. What we installed is recorded in the app's data folder (addons.json),
// never in the game folder.

use crate::addons::{self, Folder};
use crate::icons;
use crate::catalog::{Addon, Catalog};
use crate::config;
use crate::wow::Install;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Largest zip we download, and the most an archive may unpack to.
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;

/// One write at a time, across every install.
static LOCK: Mutex<()> = Mutex::new(());

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
        std::fs::create_dir_all(config::dir()).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
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
    pub latest: Option<String>,
    pub update: bool,
    pub previous: Option<String>,
    pub url: Option<String>,
    /// the texture its .toc names as its icon, when that is a file in AddOns (the window asks for it separately)
    #[serde(skip)]
    pub icon_file: Option<PathBuf>,
    /// where it comes from, for the copied addon list: (label, value)
    #[serde(skip)]
    pub links: Vec<(&'static str, String)>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Change {
    pub install: String,
    pub name: String,
    pub from: Option<String>,
    pub to: String,
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

/// The addons in an install, grouped, matched to records and the catalog.
pub fn list(install: &Install, catalog: Option<&Catalog>) -> Vec<Item> {
    let folders = addons::scan(&install.addons_dir, &install.folder);
    let records = Records::load().of(&install.id);
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
                name,
                author: entry.map(|a| a.author.clone()).filter(|a| !a.is_empty()).or(toc.author.clone()),
                notes: entry.map(|a| a.summary.clone()).filter(|s| !s.is_empty()).or(toc.notes.clone()),
                version,
                folders: g.folders.clone(),
                catalog: entry.is_some(),
                managed: record.is_some(),
                ours: entry.is_some_and(|a| a.ours),
                latest,
                previous: record.and_then(|r| r.previous.as_ref().filter(|p| p.zip.is_file()).map(|p| p.version.clone())),
                url: entry.map(|a| a.url.clone()).or(toc.website.clone()),
                icon_file,
                links,
            }
        })
        .collect();
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
        return Err("refusing a download that isn't https".into());
    }
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = http.get(url).send().map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: the server answered {}", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_DOWNLOAD) {
        return Err("the download is too large".into());
    }
    let size = resp.content_length().filter(|n| *n > 0).unwrap_or(expect);
    let mut throttle = Throttle::new(PROGRESS_EVERY);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    if throttle.ready(Instant::now(), false) {
        progress(0, size);
    }
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_DOWNLOAD {
            drop(out);
            let _ = std::fs::remove_file(dest);
            return Err("the download is too large".into());
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        if throttle.ready(Instant::now(), false) {
            progress(total, size.max(total));
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    if expect > 0 && total != expect {
        drop(out);
        let _ = std::fs::remove_file(dest);
        return Err(format!("the download is {total} bytes, the catalog says {expect}; not installed"));
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

/// Unpack a zip into `into`; returns the top-level folders. Any unsafe entry fails the whole thing.
pub fn unzip(zip: &Path, into: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("not a valid zip: {e}"))?;
    // check every name before writing anything
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        if safe_parts(&name).is_none() || entry.enclosed_name().is_none() {
            return Err(format!("the zip has an unsafe path ({name}); not installed"));
        }
    }
    std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
    let mut tops: Vec<String> = Vec::new();
    let mut unpacked: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let parts = safe_parts(&entry.name().map_err(|e| e.to_string())?).ok_or("unsafe path")?;
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
            return Err("unsafe path".into());
        }
        if is_dir {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| e.to_string())?;
        let mut limited = (&mut entry).take(MAX_UNPACKED - unpacked + 1);
        let n = std::io::copy(&mut limited, &mut out).map_err(|e| e.to_string())?;
        unpacked += n;
        if unpacked > MAX_UNPACKED {
            return Err("the zip unpacks to more than 2 GB; not installed".into());
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
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|e| e.path().is_dir() && has_toc(&e.path()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        subs.sort();
        if !subs.is_empty() {
            return Ok((inner, subs));
        }
    }
    Err("the zip holds no addon (no .toc file)".into())
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
    let dir = parent.join(format!(".everlookdb-staging-{}-{}", std::process::id(), stamp()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't write to {}: {e}", parent.display()))?;
    Ok(dir)
}

/// Unpack `zip` and swap its folders into `addons_dir`. `allowed` are the existing folders we may replace (ours, or
/// the ones the player asked us to take over). Returns the new folders and the replaced ones (moved to `old`).
fn install_zip(addons_dir: &Path, zip: &Path, allowed: &HashSet<String>, backup_to: Option<&Path>) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(addons_dir).map_err(|e| format!("can't create {}: {e}", addons_dir.display()))?;
    let staging = staging_for(addons_dir)?;
    let result = (|| {
        let unpack = staging.join("new");
        let tops = unzip(zip, &unpack)?;
        let (root, folders) = addon_root(&unpack, &tops)?;
        for f in &folders {
            if addons_dir.join(f).exists() && !allowed.contains(f) {
                return Err(format!("{f} is already in AddOns and EverlookDB didn't install it; not touched"));
            }
        }
        // move the old folders aside: the ones being replaced and the ones the old version had that the new one drops
        let old = staging.join("old");
        std::fs::create_dir_all(&old).map_err(|e| e.to_string())?;
        let mut moved: Vec<String> = Vec::new();
        for f in allowed {
            let p = addons_dir.join(f);
            if p.exists() {
                if let Err(e) = std::fs::rename(&p, old.join(f)) {
                    restore(addons_dir, &old, &moved);
                    return Err(format!("can't move {f} (is the game running?): {e}"));
                }
                moved.push(f.clone());
            }
        }
        let mut placed: Vec<String> = Vec::new();
        for f in &folders {
            if let Err(e) = std::fs::rename(root.join(f), addons_dir.join(f)) {
                for p in &placed {
                    let _ = std::fs::remove_dir_all(addons_dir.join(p));
                }
                restore(addons_dir, &old, &moved);
                return Err(format!("can't place {f}: {e}"));
            }
            placed.push(f.clone());
        }
        if let Some(dest) = backup_to {
            for f in &moved {
                let _ = copy_dir(&old.join(f), &dest.join(f));
            }
        }
        Ok(folders)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn restore(addons_dir: &Path, old: &Path, moved: &[String]) {
    for f in moved {
        let _ = std::fs::rename(old.join(f), addons_dir.join(f));
    }
}

fn zip_dir(install: &Install, id: &str) -> PathBuf {
    config::dir().join("zips").join(short_hash(&format!("{}|{id}", install.id)))
}

/// Install or update `addon` in `install`. `take_over`: folders of this addon another app installed may be replaced
/// (they are copied to the backups folder first).
pub fn install(install: &Install, addon: &Addon, take_over: &[String], progress: OnProgress) -> Result<Change, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = addon.file_for(&install.accepts).ok_or_else(|| format!("{} has no download for {}", addon.name, install.name))?;
    let mut records = Records::load();
    let old = records.of(&install.id).get(&addon.id).cloned();
    let dir = zip_dir(install, &addon.id);
    let name = if file.name.is_empty() { format!("{}.zip", short_hash(&file.url)) } else { file.name.replace(['/', '\\', ':'], "_") };
    let zip = dir.join(&name);
    let tmp = dir.join(format!("{name}.part"));
    download(&file.url, file.size, &tmp, progress)?;
    std::fs::rename(&tmp, &zip).map_err(|e| e.to_string())?;

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
    config::log(&format!("installed {} {} into {}", addon.id, addon.version, install.id));
    Ok(Change { install: install.name.clone(), name: addon.name.clone(), from: old.map(|r| r.version), to: addon.version.clone() })
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
pub fn rollback(install: &Install, id: &str) -> Result<Change, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = Records::load();
    let rec = records.of(&install.id).get(id).cloned().ok_or("EverlookDB didn't install this addon")?;
    let prev = rec.previous.clone().filter(|p| p.zip.is_file()).ok_or("no earlier version kept")?;
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
    Ok(Change { install: install.name.clone(), name: rec.name, from: Some(rec.version), to: prev.version })
}

/// Remove an addon we installed: its folders, its kept zips, its record. Nothing else.
pub fn remove(install: &Install, id: &str) -> Result<String, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = Records::load();
    let rec = records.of(&install.id).get(id).cloned().ok_or("EverlookDB didn't install this addon, so it leaves it alone")?;
    if install.addons_dir.is_dir() {
        let staging = staging_for(&install.addons_dir)?;
        let mut err = None;
        for f in &rec.folders {
            let p = install.addons_dir.join(f);
            if p.exists() {
                if let Err(e) = std::fs::rename(&p, staging.join(f)) {
                    err = Some(format!("can't remove {f} (is the game running?): {e}"));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&staging);
        if let Some(e) = err {
            return Err(e);
        }
    }
    let _ = std::fs::remove_dir_all(zip_dir(install, id));
    if let Some(m) = records.installs.get_mut(&install.id) {
        m.remove(id);
    }
    records.save()?;
    config::log(&format!("removed {id} from {}", install.id));
    Ok(rec.name)
}

/// The addons we installed in `install` that have a newer version in the catalog.
pub fn pending<'a>(install: &Install, catalog: &'a Catalog) -> Vec<&'a Addon> {
    let records = Records::load().of(&install.id);
    records
        .iter()
        .filter_map(|(id, rec)| catalog.get(id).filter(|a| a.file_for(&install.accepts).is_some() && differs(Some(&rec.version), &a.version)))
        .collect()
}

/// Update every addon we installed that has a newer version in the catalog. `progress` gets the addon's id with
/// each download report.
pub fn update_all(install: &Install, catalog: &Catalog, progress: &mut dyn FnMut(&str, u64, u64)) -> (Vec<Change>, Vec<String>) {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for addon in pending(install, catalog) {
        match self::install(install, addon, &[], &mut |d, t| progress(&addon.id, d, t)) {
            Ok(c) => done.push(c),
            Err(e) => failed.push(format!("{}: {e}", addon.name)),
        }
    }
    (done, failed)
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
        assert!(err.contains("unsafe"), "{err}");
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
        assert!(install_zip(&addons_dir, &clash, &HashSet::new(), None).is_err());
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
        let install = crate::wow::installs_in(&root).pop().unwrap();
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
        assert!(super::install(&install, &bad, &[], &mut |_, _| {}).unwrap_err().contains("bytes"));

        assert_eq!(remove(&install, "github:quikmn/EverlookDB").unwrap(), "EverlookDB Collector");
        assert!(!pre.exists());
        assert!(Records::load().of(&install.id).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
