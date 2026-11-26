// Shared auction prices: the freshest scan anyone uploaded to everlookdb.com for the auction houses this player uses,
// written into the game as a small data addon (Interface/AddOns/EverlookDBAuctions_Shared), so EverlookDB Auctions
// has it at the next login or /reload, even far from an auction house.
//
// Which houses: the realms in WTF/Account/*/ (a folder per realm the player has characters on) and the houses the
// Auctions addon saved prices for (EverlookDBAuctions.lua, "<realm>-<side>"), matched by realm name and side to
// GET /api/v1/auctions (asked at start, every 15 minutes and right after each auctions upload). Once a minute (up to
// 5 s early at random, so players' checks spread out) one GET /api/v1/auctions/versions?houses=... asks every house's
// version in a few hundred bytes; only a house whose version moved is fetched (GET /api/v1/auctions/house/{key}/compact,
// with its ETag as a second guard, so an unchanged one costs a 304). An older server without /versions: each house's
// compact is asked with its ETag instead. Nothing is written unless a house changed. Errors (no connection, 429, 5xx)
// back off 2, 4, 8, then 15 minutes, or what the server's Retry-After asks, then it is once a minute again; they never
// touch what is already written; the last good copy of each house is kept in the cache folder.
//
// Nothing here may stall without saying so (2026-11-26: one house served a 7 h old scan for a day and nobody knew):
// - a house whose scan is hours behind (the site feeds it hourly, or the realm's other houses are much newer) is
//   marked late in the view (`late_houses`): the window shows every house's age, says which one is behind and offers
//   to send a report;
// - Shared.lua carries `checked`, the last time the site answered a check (rewritten at most every BEAT_EVERY), so
//   the addon can tell "the app isn't running" from "the site has no newer scan" and say which;
// - a house that drops out of the list or answers 404 keeps its copy for GONE_GRACE instead of being deleted from the
//   game at once (a list that comes back short or empty for a moment took every price away);
// - characters with the Price Feed on but EverlookDB Auctions turned off in the game's addon list are counted
//   (`reader_off`): there /eah does nothing and the game doesn't say why.
//
// What is written: only the folder EverlookDBAuctions_Shared, nothing else in the game folder. That folder is also a
// CurseForge addon ("EverlookDB Auctions: Price Feed", D:\Dev\EverlookDBAuctions_Shared) that ships empty, like
// TradeSkillMaster's AppHelper: the app writes its .toc (the shipped one's Title, Dependencies and IconTexture, and
// its Version), Shared.lua (the global table) and one Lua file per house (a file per house keeps each under Lua's
// constant limit) over that empty copy, each to a temp file renamed over the old one, and only when its content
// changed. It removes only house files it wrote itself (they start with MARK); README, LICENSE, Media/ and anything
// else stay. With the setting off (or the Auctions addon not installed) the folder goes back to the empty copy; it
// is never deleted. When a CurseForge update brings the empty copy back, the check once a minute (Poller::check)
// notices the .toc changed or a house file is gone and writes the houses again from memory.
//
// Every number and string from the server is checked before it goes into a Lua file: a bad row is skipped, never
// the house. There are no player names in the data (the server has none to send).

use crate::health::{self, Job, Trouble};
use crate::config;
use crate::delog::ODDS_FILE;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const FOLDER: &str = "EverlookDBAuctions_Shared";
/// The addon that reads it; nothing is written without it.
const READER: &str = "EverlookDBAuctions";
/// The Interface version when the Auctions addon's .toc doesn't say one.
const INTERFACE: u32 = 16001;
const SIDES: [&str; 3] = ["Alliance", "Horde", "Neutral"];
const MAX_ROWS: usize = 50_000;
const MAX_SERIES: usize = 40_000;
const MAX_DAYS: usize = 30;
const MAX_PRICE: i64 = 99_999_999_999;
const MAX_COUNT: i64 = 100_000_000;
/// Largest compact body read (a house of 40,000 rows is about 4 MB).
const MAX_BODY: u64 = 32 * 1024 * 1024;
/// Most houses written (a player on several realms).
const MAX_HOUSES: usize = 12;

static POKE: OnceLock<Mutex<Sender<()>>> = OnceLock::new();

/// Ask for a refresh now (after an auctions upload, or when the setting changed). Does nothing before run() starts.
pub fn poke() {
    if let Some(tx) = POKE.get() {
        let _ = tx.lock().unwrap().send(());
    }
}

/// One house as the window shows it: "Price Feed: EU-4-Alliance, 12 min old".
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct HouseView {
    pub key: String,
    pub realm: String,
    pub side: String,
    /// server time of the scan (unix seconds)
    pub at: i64,
    /// its scan is hours behind what the site should have by now (`late_houses`)
    pub late: bool,
    /// the newest scan among the realm's other houses (0: there is none)
    pub sibling_at: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct View {
    pub on: bool,
    pub houses: Vec<HouseView>,
    /// why there is nothing, or the last error ("everlookdb.com can't be reached; trying again at 14:05")
    pub note: Option<String>,
    /// when the site last answered a check (unix seconds; 0: not yet)
    pub checked: i64,
    /// characters played lately with the Price Feed on but EverlookDB Auctions turned off in the game's addon list
    pub reader_off: usize,
}

// ---------------------------------------------------------------------------------------------------------------
// Which houses

/// Realm name -> the sides seen for it (empty: not known, so every side). A realm known only by its ID (the scans'
/// `realmID`: since WoW Forever's 2026-11-21 client the game no longer tells addons the realm's name) is keyed
/// `#<id>` (see `id_key`).
pub fn wanted(forever: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let Ok(accounts) = std::fs::read_dir(forever.join("WTF").join("Account")) else { return out };
    for a in accounts.flatten() {
        if let Ok(realms) = std::fs::read_dir(a.path()) {
            for r in realms.flatten() {
                let name = r.file_name().to_string_lossy().into_owned();
                if r.path().is_dir() && name != "SavedVariables" {
                    out.entry(name).or_default();
                }
            }
        }
        if let Ok(text) = std::fs::read(a.path().join("SavedVariables").join("EverlookDBAuctions.lua")) {
            let text = String::from_utf8_lossy(&text);
            for (realm, side) in saved_houses(&text) {
                out.entry(realm).or_default().insert(side);
            }
            for id in saved_realm_ids(&text) {
                out.entry(id_key(id)).or_default();
            }
        }
    }
    out
}

/// The `["<realm>-<side>"] = {` keys of the Auctions addon's saved houses.
pub fn saved_houses(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in text.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("[\"") else { continue };
        let Some(key) = rest.strip_suffix("\"] = {") else { continue };
        if let Some((realm, side)) = key.rsplit_once('-') {
            if SIDES.contains(&side) && !realm.is_empty() && !realm.contains('"') && realm.len() <= 64 {
                out.push((realm.to_string(), side.to_string()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The `wanted` key of a realm known only by its ID.
fn id_key(id: i64) -> String {
    format!("#{id}")
}

/// The realm IDs of the Auctions addon's saved scans (`["realmID"] = 6946,` lines).
pub fn saved_realm_ids(text: &str) -> BTreeSet<i64> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("[\"realmID\"] = ")?.strip_suffix(',')?.trim().parse::<i64>().ok())
        .filter(|id| (1..10_000_000).contains(id))
        .collect()
}

/// The server's houses (GET /api/v1/auctions) the player uses: their keys, newest scan first. A realm with known
/// sides gets those and its neutral house.
pub fn pick(list: &Value, wanted: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut out = vec![];
    for h in list["houses"].as_array().into_iter().flatten() {
        let (Some(key), Some(realm), Some(side)) = (h["key"].as_str(), h["realm"].as_str(), h["side"].as_str()) else { continue };
        if !key_ok(key) {
            continue;
        }
        let by_id = h["realm_id"].as_i64().and_then(|id| wanted.get(&id_key(id)));
        if let Some(sides) = wanted.get(realm).or(by_id) {
            if sides.is_empty() || sides.contains(side) || side == "Neutral" {
                out.push(key.to_string());
            }
        }
    }
    out.truncate(MAX_HOUSES);
    out
}

fn key_ok(key: &str) -> bool {
    !key.is_empty() && key.len() <= 80 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The houses of the list the site fills by itself about once an hour: Blizzard's own (`source`) and the players'
/// houses linked to one (`link`). Their scan should never be hours old.
pub fn hourly(list: &Value) -> BTreeSet<String> {
    list["houses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["link"].as_str().is_some_and(|l| !l.is_empty()) || h["source"].as_str() == Some("blizzard"))
        .filter_map(|h| h["key"].as_str().filter(|k| key_ok(k)).map(str::to_string))
        .collect()
}

/// A scan this far behind is late.
pub const LATE_AFTER: i64 = 3 * 3600;

/// The houses whose scan is behind, each with the newest scan among its realm's other houses (0: none): over
/// LATE_AFTER old, and either the site feeds it hourly (`hourly`) or another house of the same realm is at least
/// LATE_AFTER newer. A lone house nobody scans is just old, not late.
pub fn late_houses(houses: &[&House], hourly: &BTreeSet<String>, now: i64) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for h in houses {
        if now - h.at <= LATE_AFTER {
            continue;
        }
        let sibling = houses.iter().filter(|o| o.key != h.key && o.region == h.region && o.realm == h.realm).map(|o| o.at).max().unwrap_or(0);
        if hourly.contains(&h.key) || sibling - h.at >= LATE_AFTER {
            out.insert(h.key.clone(), sibling);
        }
    }
    out
}

/// "enabled"/"disabled" of one addon in a character's AddOns.txt ("EverlookDBAuctions: enabled"); None: not listed.
pub fn addon_state(text: &str, name: &str) -> Option<bool> {
    text.lines().find_map(|l| {
        let (n, v) = l.split_once(':')?;
        (n.trim() == name).then(|| v.trim().eq_ignore_ascii_case("enabled"))
    })
}

/// Only characters played in the last 30 days count.
const PLAYED_WITHIN: Duration = Duration::from_secs(30 * 86400);

/// How many characters played lately have EverlookDB Auctions turned off in the game's addon list while the Price
/// Feed is left on (WTF/Account/*/<realm>/<character>/AddOns.txt). The game then loads neither, /eah does nothing and
/// nothing in the game says why; only the app can.
pub fn reader_off(forever: &Path, now: std::time::SystemTime) -> usize {
    let mut n = 0;
    let dirs = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
    for account in dirs(&forever.join("WTF").join("Account")) {
        for realm in dirs(&account) {
            for character in dirs(&realm) {
                let file = character.join("AddOns.txt");
                let fresh = std::fs::metadata(&file).and_then(|m| m.modified()).is_ok_and(|m| now.duration_since(m).map_or(true, |d| d <= PLAYED_WITHIN));
                if !fresh {
                    continue;
                }
                let Ok(text) = std::fs::read(&file) else { continue };
                let text = String::from_utf8_lossy(&text);
                if addon_state(&text, READER) == Some(false) && addon_state(&text, FOLDER) != Some(false) {
                    n += 1;
                }
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------------------------------------------
// One house, checked

#[derive(Clone, Debug, PartialEq)]
pub struct House {
    pub key: String,
    pub region: String,
    pub realm: String,
    pub realm_id: i64,
    pub side: String,
    pub at: i64,
    pub auctions: i64,
    pub first_day: i64,
    /// ("id:suffix", the addon's row text)
    pub rows: Vec<(String, String)>,
    /// (item, daily lows "28,,30": the first is first_day, empty = no price that day)
    pub lows: Vec<(i64, String)>,
}

/// Printable text for a Lua string and a row field: no control characters, WoW escape bars, quotes, backslashes
/// or commas.
fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() && !matches!(c, '|' | '"' | '\\' | ',' | '<' | '>')).take(max).collect::<String>().trim().to_string()
}

fn int_in(v: &Value, lo: i64, hi: i64) -> Option<i64> {
    v.as_i64().filter(|n| (lo..=hi).contains(n))
}

/// Price or nothing: Some(None) for null, None when it is neither.
fn price(v: &Value) -> Option<Option<i64>> {
    if v.is_null() {
        return Some(None);
    }
    int_in(v, 0, MAX_PRICE).map(Some)
}

fn row_of(r: &Value) -> Option<(String, String)> {
    let a = r.as_array()?;
    if a.len() != 14 && a.len() != 15 {
        return None;
    }
    let id = int_in(&a[0], 1, 50_000_000)?;
    let suffix = int_in(&a[1], -1_000_000, 1_000_000)?;
    let (low, p10, med) = (price(&a[2])?, price(&a[3])?, price(&a[4])?);
    let mut counts = [0i64; 8];
    for (i, c) in counts.iter_mut().enumerate() {
        *c = int_in(&a[5 + i], 0, MAX_COUNT)?;
    }
    let min_bid = price(&a[13])?;
    let (qty, n) = (counts[0], counts[1]);
    if n < 1 || qty < n || low.is_some() != med.is_some() {
        return None;
    }
    let f = |p: Option<i64>| p.map(|x| x.to_string()).unwrap_or_default();
    let mut text = format!("{},{},{}", f(low), f(p10), f(med));
    for c in counts {
        text.push_str(&format!(",{c}"));
    }
    text.push_str(&format!(",{}", f(min_bid)));
    if let Some(name) = a.get(14).and_then(Value::as_str).map(|s| clean(s, 100)).filter(|s| !s.is_empty() && suffix != 0) {
        text.push(',');
        text.push_str(&name);
    }
    Some((format!("{id}:{suffix}"), text))
}

fn lows_of(s: &Value) -> Option<(i64, String)> {
    let a = s.as_array()?;
    let id = int_in(a.first()?, 1, 50_000_000)?;
    if a.len() < 2 || a.len() > MAX_DAYS + 1 {
        return None;
    }
    let mut out = Vec::with_capacity(a.len() - 1);
    for v in &a[1..] {
        out.push(price(v)?.map(|x| x.to_string()).unwrap_or_default());
    }
    Some((id, out.join(",")))
}

/// Reads one compact body (GET /api/v1/auctions/house/{key}/compact). A bad row or series is skipped; the house is
/// refused only when its facts (key, realm, side, time) don't read.
pub fn parse(v: &Value, key: &str, now: i64) -> Result<House, String> {
    if v["v"].as_i64() != Some(1) {
        return Err("unknown format".into());
    }
    if v["key"].as_str() != Some(key) {
        return Err("another house".into());
    }
    let side = v["side"].as_str().filter(|s| SIDES.contains(s)).ok_or("bad side")?.to_string();
    let realm = v["realm"].as_str().map(|s| clean(s, 64)).filter(|s| !s.is_empty()).ok_or("bad realm")?;
    let region = v["region"].as_str().map(|s| clean(s, 8)).unwrap_or_default();
    let at = int_in(&v["at"], 1_500_000_000, now + 3600).ok_or("bad time")?;
    let first_day = int_in(&v["first_day"], 10_000, 100_000).ok_or("bad first day")?;
    let rows: Vec<(String, String)> = v["rows"].as_array().into_iter().flatten().take(MAX_ROWS).filter_map(row_of).collect();
    if rows.is_empty() {
        return Err("no rows".into());
    }
    let lows = v["lows"].as_array().into_iter().flatten().take(MAX_SERIES).filter_map(lows_of).collect();
    Ok(House {
        key: key.to_string(),
        region,
        realm,
        realm_id: int_in(&v["realm_id"], 0, 10_000_000).unwrap_or(0),
        side,
        at,
        auctions: int_in(&v["auctions"], 0, 10_000_000).unwrap_or(0),
        first_day,
        rows,
        lows,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The data addon

fn file_of(key: &str) -> String {
    format!("{key}.lua")
}

/// The first line of every file the app writes: only files that start with it are ever replaced by the stub or
/// removed (the folder may be a CurseForge copy, with its README, LICENSE and Media/ the app leaves alone).
const MARK: &str = "-- Written by the EverlookDB App (do not edit)";

/// The .toc, the same as the shipped addon's (D:\Dev\EverlookDBAuctions_Shared: Title, Dependencies, IconTexture),
/// with the house files the app wrote. `version`: the installed copy's ## Version, kept as it was.
pub fn toc(interface: u32, version: Option<&str>, files: &[String]) -> String {
    let mut t = format!(
        "## Interface: {interface}\n## Title: EverlookDB Auctions: Price Feed\n## Notes: Other players' newest auction scans from everlookdb.com, for EverlookDB Auctions. The EverlookDB App fills it; it refreshes at /reload or login.\n## Author: quikmn\n"
    );
    if let Some(v) = version {
        t.push_str(&format!("## Version: {v}\n"));
    }
    t.push_str("## Dependencies: EverlookDBAuctions\n## IconTexture: Interface\\AddOns\\EverlookDBAuctions_Shared\\Media\\icon\n## X-License: GPL-3.0\n## LoadOnDemand: 0\n\nShared.lua\n");
    for f in files {
        t.push_str(f);
        t.push('\n');
    }
    t
}

/// `written`: when the app last wrote a house; `checked`: when the site last answered a check (see `beat`).
pub fn shared_lua(written: i64, checked: i64) -> String {
    format!(
        "{MARK}: other players' newest auction scans from everlookdb.com.\n\
         -- EverlookDB Auctions reads it at login and /reload, and keeps it apart from your own scans.\n\
         EverlookDBAuctionsShared = {{ version = 1, written = {written}, checked = {checked}, houses = {{}} }}\n"
    )
}

/// The `written = N` of a Shared.lua.
fn written_of(text: &str) -> Option<i64> {
    text.split("written = ").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// How often Shared.lua's `checked` is brought up to date while nothing else changes.
pub const BEAT_EVERY: i64 = 30 * 60;

/// The app is running and the site answered: Shared.lua says so (`checked = now`, its `written` kept), so the addon
/// can tell old prices the site has nothing newer for from an app that isn't running. Only the app's own filled
/// Shared.lua is touched; returns whether it was written.
pub fn beat(addons: &Path, now: i64) -> std::io::Result<bool> {
    let dir = addons.join(FOLDER);
    let shared = dir.join("Shared.lua");
    if !ours(&shared) {
        return Ok(false);
    }
    let Ok(text) = std::fs::read_to_string(&shared) else { return Ok(false) };
    match written_of(&text).filter(|w| *w > 0) {
        Some(written) => put(&dir, "Shared.lua", &shared_lua(written, now)),
        None => Ok(false),
    }
}

/// The empty Shared.lua, as the shipped addon has it (written = 0: no data).
pub fn stub_lua() -> String {
    format!(
        "{MARK}: the Price Feed is off, so this holds no prices.\n\
         EverlookDBAuctionsShared = {{ version = 1, written = 0, houses = {{}} }}\n"
    )
}

pub fn house_lua(h: &House) -> String {
    let mut s = String::with_capacity(h.rows.len() * 60 + h.lows.len() * 120 + 512);
    s.push_str(&format!("{MARK}.\nlocal d = EverlookDBAuctionsShared\nif type(d) ~= \"table\" or type(d.houses) ~= \"table\" then return end\n"));
    s.push_str(&format!(
        "d.houses[\"{}\"] = {{\n  region = \"{}\", realm = \"{}\", realmID = {}, side = \"{}\", at = {}, auctions = {}, firstDay = {},\n  rows = {{\n",
        h.key, h.region, h.realm, h.realm_id, h.side, h.at, h.auctions, h.first_day
    ));
    for (k, r) in &h.rows {
        s.push_str(&format!("    [\"{k}\"] = \"{r}\",\n"));
    }
    s.push_str("  },\n  lows = {\n");
    for (id, l) in &h.lows {
        s.push_str(&format!("    [{id}] = \"{l}\",\n"));
    }
    s.push_str("  },\n}\n");
    s
}

/// The Interface version in the Auctions addon's .toc, else INTERFACE.
fn interface_of(addons: &Path) -> u32 {
    std::fs::read_to_string(addons.join(READER).join(format!("{READER}.toc")))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("## Interface:").map(|v| v.split(',').next().unwrap_or("").trim().to_string())))
        .and_then(|v| v.parse().ok())
        .unwrap_or(INTERFACE)
}

/// The ## Version of the copy in the folder (the CurseForge one, or what the app kept of it), when it is plain.
fn version_of(dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(dir.join(format!("{FOLDER}.toc"))).ok()?;
    let v = t.lines().find_map(|l| l.trim().strip_prefix("## Version:").map(|v| v.trim().to_string()))?;
    (!v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))).then_some(v)
}

/// Whether the app wrote this file (it starts with MARK).
fn ours(path: &Path) -> bool {
    let mut head = [0u8; MARK.len()];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == MARK.as_bytes()
}

/// Writes `name` in `dir` when its content differs: a temp file, then a rename over the old one.
fn put(dir: &Path, name: &str, content: &str) -> std::io::Result<bool> {
    let path = dir.join(name);
    if std::fs::read(&path).is_ok_and(|old| old == content.as_bytes()) {
        return Ok(false);
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(true)
}

/// Removes the house files the app wrote that aren't in `keep`, and its own leftover temp files. Nothing else: a
/// file that doesn't start with MARK, Shared.lua, the .toc and any folder (Media/) stay.
fn prune(dir: &Path, keep: &[String]) -> std::io::Result<bool> {
    let mut changed = false;
    for e in std::fs::read_dir(dir)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let temp = n.starts_with('.') && n.ends_with(".tmp");
        let house = n.strip_suffix(".lua").is_some_and(key_ok) && n != "Shared.lua" && !keep.contains(&n) && ours(&path);
        if temp || house {
            std::fs::remove_file(&path)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Makes `addons/EverlookDBAuctions_Shared` hold these houses: its .toc, Shared.lua and a file per house, over the
/// shipped empty copy when that is what is there. Writes only files whose content changed and removes only house
/// files the app wrote earlier. Returns whether anything changed.
pub fn write_addon(addons: &Path, houses: &[&House], now: i64) -> std::io::Result<bool> {
    write_feed(addons, houses, None, now)
}

/// `write_addon`, with the players' disenchant odds (DEOdds.lua, see delog.rs) when there are any; without them an
/// earlier DEOdds.lua the app wrote is removed.
pub fn write_feed(addons: &Path, houses: &[&House], odds: Option<&str>, now: i64) -> std::io::Result<bool> {
    let dir = addons.join(FOLDER);
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    let mut names: Vec<String> = houses.iter().map(|h| file_of(&h.key)).collect();
    for h in houses {
        changed |= put(&dir, &file_of(&h.key), &house_lua(h))?;
    }
    if let Some(o) = odds {
        changed |= put(&dir, ODDS_FILE, o)?;
        names.push(ODDS_FILE.to_string());
    }
    // Shared.lua's time only moves when a house file did, so an unchanged poll writes nothing at all; the empty copy's
    // Shared.lua (after a CurseForge update, or the feed was off) is replaced
    let shared = dir.join("Shared.lua");
    if changed || !ours(&shared) || std::fs::read_to_string(&shared).is_ok_and(|t| t.contains("written = 0,")) {
        changed |= put(&dir, "Shared.lua", &shared_lua(now, now))?;
    }
    changed |= put(&dir, &format!("{FOLDER}.toc"), &toc(interface_of(addons), version_of(&dir).as_deref(), &names))?;
    changed |= prune(&dir, &names)?;
    Ok(changed)
}

/// The feed is off (or EverlookDB Auctions isn't installed): the folder goes back to the empty copy, the same as the
/// shipped one. It is never deleted: it may be a CurseForge install. Nothing is written when there is no folder.
pub fn write_stub(addons: &Path) -> std::io::Result<bool> {
    let dir = addons.join(FOLDER);
    if !dir.is_dir() {
        return Ok(false);
    }
    let mut changed = prune(&dir, &[])?;
    let shared = dir.join("Shared.lua");
    if !shared.exists() || ours(&shared) {
        changed |= put(&dir, "Shared.lua", &stub_lua())?;
    }
    changed |= put(&dir, &format!("{FOLDER}.toc"), &toc(interface_of(addons), version_of(&dir).as_deref(), &[]))?;
    Ok(changed)
}
// ---------------------------------------------------------------------------------------------------------------
// Fetching

enum Got {
    New(House, Option<String>),
    Same,
    Gone,
}

/// A failed request: what the window says, and how long the server asked us to wait (its Retry-After).
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub msg: String,
    pub retry_after: Option<Duration>,
    /// the server is busy, failing or unreachable (429, 5xx, no connection): ask nothing more this round
    pub stop: bool,
}

impl Fail {
    fn new(msg: impl Into<String>) -> Fail {
        Fail { msg: msg.into(), retry_after: None, stop: false }
    }
}

struct Answer {
    code: u16,
    etag: Option<String>,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

/// A Retry-After header: seconds, or an HTTP date.
pub fn retry_after_of(v: &str, now: i64) -> Option<Duration> {
    let v = v.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let t = chrono::DateTime::parse_from_rfc2822(v).ok()?.timestamp();
    Some(Duration::from_secs((t - now).max(0) as u64))
}

fn get(http: &reqwest::blocking::Client, url: &str, etag: Option<&str>) -> Result<Answer, Fail> {
    let mut req = http.get(url).header("Accept-Encoding", "gzip");
    if let Some(t) = etag {
        req = req.header("If-None-Match", t);
    }
    let resp = req.send().map_err(|e| Fail {
        msg: if e.is_timeout() { "no answer".to_string() } else { "can't reach everlookdb.com".to_string() },
        retry_after: None,
        stop: true,
    })?;
    let code = resp.status().as_u16();
    let tag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
    let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| retry_after_of(v, unix_now()));
    if code == 304 {
        return Ok(Answer { code, etag: tag, retry_after, body: vec![] });
    }
    let gz = resp.headers().get("content-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let mut raw = vec![];
    resp.take(MAX_BODY + 1).read_to_end(&mut raw).map_err(|_| Fail::new("the answer was cut off"))?;
    if raw.len() as u64 > MAX_BODY {
        return Err(Fail::new("the answer is too large"));
    }
    let body = if gz {
        let mut out = vec![];
        flate2::read::GzDecoder::new(&raw[..]).take(MAX_BODY * 4).read_to_end(&mut out).map_err(|_| Fail::new("a bad answer"))?;
        out
    } else {
        raw
    };
    Ok(Answer { code, etag: tag, retry_after, body })
}

/// An answer that isn't one we can use: 429 and 5xx stop the round and carry the server's Retry-After.
fn fail_of(a: &Answer) -> Fail {
    let msg = if a.code == 429 { "the server is busy".to_string() } else { format!("the server answered {}", a.code) };
    Fail { msg, retry_after: a.retry_after, stop: a.code == 429 || a.code >= 500 }
}

fn fetch_house(http: &reqwest::blocking::Client, server: &str, key: &str, etag: Option<&str>, now: i64) -> Result<Got, Fail> {
    let a = get(http, &format!("{server}/api/v1/auctions/house/{key}/compact"), etag)?;
    match a.code {
        304 => Ok(Got::Same),
        404 => Ok(Got::Gone),
        200 => {
            let v: Value = serde_json::from_slice(&a.body).map_err(|_| Fail::new("a bad answer"))?;
            Ok(Got::New(parse(&v, key, now).map_err(Fail::new)?, a.etag))
        }
        _ => Err(fail_of(&a)),
    }
}

/// GET /api/v1/auctions/versions?houses=a,b: every house's version in one small answer (the name in its compact
/// ETag; None: no scans). Ok(None): the server doesn't have it yet (an older one), so each house is asked with its
/// ETag instead.
fn fetch_versions(http: &reqwest::blocking::Client, server: &str, keys: &[String]) -> Result<Option<BTreeMap<String, Option<String>>>, Fail> {
    let mut sorted = keys.to_vec();
    sorted.sort();
    let a = get(http, &format!("{server}/api/v1/auctions/versions?houses={}", sorted.join(",")), None)?;
    match a.code {
        200 => {
            let v: Value = serde_json::from_slice(&a.body).map_err(|_| Fail::new("a bad answer"))?;
            Ok(Some(versions_of(&v, keys)))
        }
        400 | 404 | 405 => Ok(None),
        _ => Err(fail_of(&a)),
    }
}

/// The versions answer for these keys (missing or not a plain string: None).
pub fn versions_of(v: &Value, keys: &[String]) -> BTreeMap<String, Option<String>> {
    keys.iter().map(|k| (k.clone(), v[k.as_str()].as_str().filter(|s| s.len() <= 100).map(str::to_string))).collect()
}

/// The version an ETag names: W/"c1-1795227761-34-20778" -> c1-1795227761-34-20778.
pub fn tag_version(etag: &str) -> &str {
    etag.trim().trim_start_matches("W/").trim_matches('"')
}

/// The houses to fetch: those whose version moved from the copy we have, that we have no copy (or no ETag) of, and
/// those the server no longer has scans of (their fetch says 404 and they go).
pub fn moved(versions: &BTreeMap<String, Option<String>>, have: &BTreeMap<String, (House, Option<String>)>) -> BTreeSet<String> {
    versions
        .iter()
        .filter(|(k, v)| match (v, have.get(*k)) {
            (None, None) => false,
            (None, Some(_)) | (Some(_), None) => true,
            (Some(v), Some((_, tag))) => tag.as_deref().map(tag_version) != Some(v.as_str()),
        })
        .map(|(k, _)| k.clone())
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// When to look

/// How often the Price Feed checks for new scans: once a minute, up to JITTER early at random, so the players'
/// checks spread out instead of all reaching the server in the same second.
pub const EVERY: Duration = Duration::from_secs(60);
const JITTER: Duration = Duration::from_secs(5);
/// The longest wait after errors: 2, 4, 8, then 15 minutes (a Retry-After from the server can ask for up to an hour).
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// How often the list of houses (GET /api/v1/auctions) is asked again; also right after each auctions upload.
const LIST_EVERY: Duration = Duration::from_secs(15 * 60);
/// How often unchanged checks are summed up in the log (they write nothing else).
const QUIET_LOG: Duration = Duration::from_secs(15 * 60);
/// How long a house the site stopped listing (or answers 404 for) keeps the copy we have, in seconds: a list that
/// comes back short for a moment must not take the prices out of the game.
pub const GONE_GRACE: i64 = 6 * 3600;

/// A failed check as the app's one policy sees it (health.rs): the data addon that can't be written (`local`, in plain
/// words) is shown at once; a server that is busy, failing or unreachable, or an answer that isn't usable, is retried
/// quietly and only shown after health::QUIET_FOR.
pub fn trouble_of(e: &Fail, local: Option<String>) -> Trouble {
    match local {
        Some(m) => Trouble::Local(m),
        None if e.stop => Trouble::Unreachable(e.msg.clone()),
        None => Trouble::Server(e.msg.clone()),
    }
}

/// The wait before the next check. `failures`: errors in a row (0 after a success); `retry_after`: what the server
/// asked for; `jitter`: a random part, at most JITTER (taken off the minute, added to a backoff).
pub fn next_wait(failures: u32, retry_after: Option<Duration>, jitter: Duration) -> Duration {
    let jitter = jitter.min(JITTER);
    if failures == 0 {
        return EVERY - jitter;
    }
    let back = Duration::from_secs(60u64 << failures.min(5)).min(MAX_BACKOFF);
    let wait = match retry_after {
        Some(r) => back.max(r.min(MAX_RETRY_AFTER)),
        None => back,
    };
    wait + jitter
}

/// A random 0..=JITTER.
fn jitter() -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    Duration::from_millis(h.finish() % (JITTER.as_millis() as u64 + 1))
}

fn words(d: Duration) -> String {
    if d.as_secs() < 120 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} min", d.as_secs() / 60)
    }
}

fn cache_path(key: &str) -> PathBuf {
    config::cache_dir().join("shared").join(format!("{key}.json"))
}

/// A house kept from an earlier run: its checked body and ETag.
fn load_cached(key: &str, now: i64) -> Option<(House, Option<String>)> {
    let v: Value = serde_json::from_slice(&std::fs::read(cache_path(key)).ok()?).ok()?;
    let h = parse(&v["body"], key, now).ok()?;
    Some((h, v["etag"].as_str().map(str::to_string)))
}

fn save_cached(key: &str, body: &Value, etag: Option<&str>) {
    let p = cache_path(key);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&serde_json::json!({ "etag": etag, "body": body })).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// GET /api/v1/crowd/disenchant with the ETag we have: Some(odds) when there is a new answer, None when it is the
/// same (304) or the server doesn't have it yet (404).
fn fetch_odds(http: &reqwest::blocking::Client, server: &str, etag: Option<&str>) -> Result<Option<Odds>, Fail> {
    let a = get(http, &format!("{server}/api/v1/crowd/disenchant"), etag)?;
    match a.code {
        304 | 404 | 405 => Ok(None),
        200 => {
            let v: Value = serde_json::from_slice(&a.body).map_err(|_| Fail::new("a bad answer"))?;
            let lua = crate::delog::odds_lua(&v, MARK).ok_or_else(|| Fail::new("a bad answer"))?;
            Ok(Some(Odds { lua, etag: a.etag }))
        }
        _ => Err(fail_of(&a)),
    }
}

fn odds_path() -> PathBuf {
    config::cache_dir().join("deodds.json")
}

/// The odds kept from an earlier run (checked again on the way in).
fn load_odds() -> Option<Odds> {
    let v: Value = serde_json::from_slice(&std::fs::read(odds_path()).ok()?).ok()?;
    let lua = v["lua"].as_str().filter(|l| l.starts_with(MARK) && l.contains("EverlookDB_DEOdds = "))?.to_string();
    Some(Odds { lua, etag: v["etag"].as_str().map(str::to_string) })
}

fn save_odds(o: &Odds) {
    let p = odds_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&serde_json::json!({ "etag": o.etag, "lua": o.lua })).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// The houses in memory and what the last refresh did.
pub struct Poller {
    http: reqwest::blocking::Client,
    server: String,
    houses: BTreeMap<String, (House, Option<String>)>,
    failures: u32,
    /// what the last write left in the folder, to notice it was replaced
    last: Option<Written>,
    /// the houses the player uses, from the last list, and when that was asked
    keys: Vec<String>,
    listed: Option<Instant>,
    /// ask the list again at the next check (after an upload, or the setting changed)
    relist: bool,
    /// an older server without /versions: each house is asked with its ETag until then
    no_versions_until: Option<Instant>,
    /// not before this (the server's Retry-After), even when poked
    hold_until: Option<Instant>,
    /// unchanged checks since the last summary line in the log, and when that was
    quiet: u32,
    quiet_logged: Option<Instant>,
    /// the players' disenchant odds (DEOdds.lua), and when they were last asked
    odds: Option<Odds>,
    odds_asked: Option<Instant>,
    /// the houses the site fills hourly by itself (`hourly`), from the last list
    hourly: BTreeSet<String>,
    /// houses we hold that the site stopped listing or answered 404 for, and since when (unix seconds)
    absent: BTreeMap<String, i64>,
    /// when Shared.lua's `checked` was last brought up to date (unix seconds)
    beat_at: Option<i64>,
    /// when the site last answered a check (unix seconds; 0: not yet)
    checked: i64,
    /// see `reader_off`, counted with each list
    reader_off: usize,
    /// the late houses already logged and noted for problem reports
    late_noted: BTreeSet<String>,
}

/// The disenchant odds as written (DEOdds.lua) and the ETag of the answer they came from.
#[derive(Clone, Debug, PartialEq)]
struct Odds {
    lua: String,
    etag: Option<String>,
}

struct Written {
    toc: String,
    keys: Vec<String>,
    /// DEOdds.lua was written too
    odds: bool,
}

/// Writes the houses again when the folder no longer holds what was `last` written (its .toc changed, or a house
/// file is gone); what it wrote then, or None when nothing needed doing.
fn restore(addons: &Path, last: &Written, houses: &BTreeMap<String, (House, Option<String>)>, odds: Option<&str>) -> std::io::Result<Option<Written>> {
    let dir = addons.join(FOLDER);
    if read_toc(addons) == last.toc && last.keys.iter().all(|k| dir.join(file_of(k)).is_file()) && (!last.odds || dir.join(ODDS_FILE).is_file()) {
        return Ok(None);
    }
    let list: Vec<&House> = last.keys.iter().filter_map(|k| houses.get(k).map(|h| &h.0)).collect();
    let odds = odds.filter(|_| last.odds);
    if list.is_empty() && odds.is_none() {
        return Ok(None);
    }
    write_feed(addons, &list, odds, unix_now())?;
    Ok(Some(Written { toc: read_toc(addons), keys: list.iter().map(|h| h.key.clone()).collect(), odds: odds.is_some() }))
}

fn read_toc(addons: &Path) -> String {
    std::fs::read_to_string(addons.join(FOLDER).join(format!("{FOLDER}.toc"))).unwrap_or_default()
}

impl Poller {
    pub fn new(server: &str) -> Poller {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("EverlookDB-Uploader/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("http client");
        Poller {
            http,
            server: server.trim_end_matches('/').to_string(),
            houses: BTreeMap::new(),
            failures: 0,
            last: None,
            keys: vec![],
            listed: None,
            relist: false,
            no_versions_until: None,
            hold_until: None,
            quiet: 0,
            quiet_logged: None,
            odds: None,
            odds_asked: None,
            hourly: BTreeSet::new(),
            absent: BTreeMap::new(),
            beat_at: None,
            checked: 0,
            reader_off: 0,
            late_noted: BTreeSet::new(),
        }
    }

    /// The last refresh failed (the command line's exit code).
    pub fn failing(&self) -> bool {
        self.failures > 0
    }

    /// The cheap check, about once a minute: when the folder no longer holds what the app last wrote (its .toc
    /// changed, or a house file is gone: CurseForge updated the addon and brought back its empty copy), writes it again
    /// from the houses in memory, without asking the server. Returns whether it wrote.
    pub fn check(&mut self, forever: &Path) -> bool {
        let Some(last) = &self.last else { return false };
        let addons = forever.join("Interface").join("AddOns");
        match restore(&addons, last, &self.houses, self.odds.as_ref().map(|o| o.lua.as_str())) {
            Ok(Some(w)) => {
                config::log(&format!("price feed: {FOLDER} was replaced (an addon update?); wrote its {} houses again", w.keys.len()));
                self.last = Some(w);
                true
            }
            Ok(None) => false,
            Err(e) => {
                config::log(&format!("price feed: can't write {FOLDER} again: {e}"));
                self.last = None;
                false
            }
        }
    }

    /// The houses we know of when the list can't be had: the last list, those in memory, else the cached ones.
    fn known_keys(&self) -> Vec<String> {
        if !self.keys.is_empty() {
            return self.keys.clone();
        }
        let mut k: Vec<String> = self.houses.keys().cloned().collect();
        if k.is_empty() {
            k = std::fs::read_dir(config::cache_dir().join("shared"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_string))
                .filter(|k| key_ok(k))
                .collect();
        }
        k
    }

    /// One check of every wanted house for the `_forever_` folder: one small GET /api/v1/auctions/versions for all of
    /// them, then the compact body (with its ETag) only of a house whose version moved; the data addon is written
    /// only when a house changed. Returns the view and when to look again.
    pub fn refresh(&mut self, forever: &Path, on: bool) -> (View, Duration) {
        let addons = forever.join("Interface").join("AddOns");
        if !on {
            self.last = None;
            if write_stub(&addons).unwrap_or(false) {
                config::log("price feed: off, the data addon is empty again");
            }
            health::ok(Job::PriceFeed, "");
            return (View { on: false, ..Default::default() }, EVERY);
        }
        if !addons.join(READER).is_dir() {
            health::ok(Job::PriceFeed, "");
            self.last = None;
            let _ = write_stub(&addons);
            return (View { on, note: Some("Needs EverlookDB Auctions".into()), ..Default::default() }, EVERY);
        }
        let now = unix_now();
        let mut error: Option<Fail> = None;
        // the list of houses: at the first check, every LIST_EVERY and after an upload
        let list_due = self.relist || self.listed.is_none_or(|t| t.elapsed() >= LIST_EVERY);
        let mut keys: Vec<String> = if !list_due {
            self.keys.clone()
        } else {
            self.reader_off = reader_off(forever, std::time::SystemTime::now());
            match get(&self.http, &format!("{}/api/v1/auctions", self.server), None) {
                Ok(a) if a.code == 200 => match serde_json::from_slice::<Value>(&a.body) {
                    // a list without its houses is a bad answer, not "no houses": that would empty the game's copy
                    Ok(v) if v["houses"].is_array() => {
                        self.keys = pick(&v, &wanted(forever));
                        self.hourly = hourly(&v);
                        self.listed = Some(Instant::now());
                        self.relist = false;
                        self.keys.clone()
                    }
                    _ => {
                        error = Some(Fail::new("a bad answer"));
                        self.known_keys()
                    }
                },
                Ok(a) => {
                    error = Some(fail_of(&a));
                    self.known_keys()
                }
                Err(e) => {
                    error = Some(e);
                    // offline: keep writing what we have
                    self.known_keys()
                }
            }
        };
        // the houses to ask the site about; one we hold that it no longer lists keeps its copy for GONE_GRACE, unasked
        let asked: Vec<String> = keys.clone();
        for k in self.houses.keys() {
            if !asked.contains(k) {
                let since = *self.absent.entry(k.clone()).or_insert_with(|| {
                    config::log(&format!("price feed: {k} is no longer in the site's list; keeping the copy we have for up to {} h", GONE_GRACE / 3600));
                    now
                });
                if now - since < GONE_GRACE {
                    keys.push(k.clone());
                }
            }
        }
        self.houses.retain(|k, _| keys.contains(k));
        self.absent.retain(|k, _| self.houses.contains_key(k));
        for key in &keys {
            if !self.houses.contains_key(key) {
                if let Some(c) = load_cached(key, now) {
                    self.houses.insert(key.clone(), c);
                }
            }
        }
        // which houses to fetch: those whose version moved (every house, each with its ETag, on an older server)
        let mut ask: BTreeSet<String> = match &error {
            None if asked.is_empty() => BTreeSet::new(),
            None => {
                let try_versions = self.no_versions_until.is_none_or(|t| Instant::now() >= t);
                match if try_versions { fetch_versions(&self.http, &self.server, &asked) } else { Ok(None) } {
                    Ok(Some(v)) => {
                        self.no_versions_until = None;
                        // a house the site has a version of again is back
                        for (k, ver) in &v {
                            if ver.is_some() {
                                self.absent.remove(k);
                            }
                        }
                        moved(&v, &self.houses)
                    }
                    Ok(None) => {
                        if try_versions {
                            config::log("price feed: the server has no versions check yet; asking each house with its ETag");
                            self.no_versions_until = Some(Instant::now() + LIST_EVERY);
                        }
                        asked.iter().cloned().collect()
                    }
                    Err(e) => {
                        error = Some(e);
                        BTreeSet::new()
                    }
                }
            }
            // the list failed: only houses we have nothing of, and only when the server isn't the problem
            Some(e) if !e.stop => asked.iter().filter(|k| !self.houses.contains_key(*k)).cloned().collect(),
            Some(_) => BTreeSet::new(),
        };
        // a house the site answered 404 for is asked again with each list (every LIST_EVERY), not every minute
        if !list_due {
            ask.retain(|k| !self.absent.contains_key(k));
        }
        let mut changed = false;
        for key in asked.iter().filter(|k| ask.contains(*k)) {
            let etag = self.houses.get(key).and_then(|h| h.1.clone());
            match fetch_house(&self.http, &self.server, key, etag.as_deref(), now) {
                Ok(Got::New(h, tag)) => {
                    config::log(&format!("price feed: {} scan of {} ({} rows)", h.key, chrono::DateTime::from_timestamp(h.at, 0).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default(), h.rows.len()));
                    // the checked house, for the next start (a 304 needs it)
                    save_cached(key, &body_of(&h), tag.as_deref());
                    self.houses.insert(key.clone(), (h, tag));
                    self.absent.remove(key);
                    changed = true;
                }
                Ok(Got::Same) => {
                    self.absent.remove(key);
                }
                // the site has no scans of it (any more): the copy we have stays for GONE_GRACE, then it goes
                Ok(Got::Gone) => {
                    if self.houses.contains_key(key) {
                        let since = *self.absent.entry(key.clone()).or_insert_with(|| {
                            config::log(&format!("price feed: the site has no scans of {key} right now; keeping the copy we have for up to {} h", GONE_GRACE / 3600));
                            now
                        });
                        if now - since >= GONE_GRACE {
                            config::log(&format!("price feed: {key} has been gone from the site for {} h; removed", GONE_GRACE / 3600));
                            self.houses.remove(key);
                            self.absent.remove(key);
                            changed = true;
                        }
                    }
                }
                Err(e) => {
                    let stop = e.stop;
                    // one house's own trouble names the house (a busy or unreachable server is everyone's)
                    let e = if stop { e } else { Fail { msg: format!("{key}: {}", e.msg), ..e } };
                    error.get_or_insert(e);
                    if stop {
                        break;
                    }
                }
            }
        }
        // the players' disenchant odds: every LIST_EVERY, with the ETag (a failure only waits for the next time)
        if self.odds.is_none() {
            self.odds = load_odds();
        }
        if error.as_ref().is_none_or(|e| !e.stop) && self.odds_asked.is_none_or(|t| t.elapsed() >= LIST_EVERY) {
            self.odds_asked = Some(Instant::now());
            match fetch_odds(&self.http, &self.server, self.odds.as_ref().and_then(|o| o.etag.as_deref())) {
                Ok(Some(o)) => {
                    if self.odds.as_ref().map(|x| &x.lua) != Some(&o.lua) {
                        config::log("price feed: new disenchant odds");
                        changed = true;
                    }
                    save_odds(&o);
                    self.odds = Some(o);
                }
                Ok(None) => {}
                Err(e) => config::log(&format!("price feed: disenchant odds not fetched ({}); trying again in 15 min", e.msg)),
            }
        }
        let odds = self.odds.as_ref().map(|o| o.lua.as_str());
        let list: Vec<&House> = keys.iter().filter_map(|k| self.houses.get(k).map(|h| &h.0)).collect();
        let names: Vec<String> = list.iter().map(|h| h.key.clone()).collect();
        // nothing moved since the last write: nothing is written, not even compared (check() puts back a folder an
        // addon update replaced)
        let unchanged = !changed && self.last.as_ref().is_some_and(|w| w.keys == names && w.odds == odds.is_some());
        let write = if unchanged {
            Ok(false)
        } else if list.is_empty() && odds.is_none() {
            self.last = None;
            write_stub(&addons)
        } else {
            let w = write_feed(&addons, &list, odds, now);
            self.last = w.is_ok().then(|| Written { toc: read_toc(&addons), keys: names.clone(), odds: odds.is_some() });
            w
        };
        let mut wrote = false;
        let mut local = None;
        match write {
            Err(e) => {
                config::log(&format!("price feed: can't write {FOLDER}: {e}"));
                local = Some(health::local_words(&e, "the Price Feed addon"));
                error.get_or_insert_with(|| Fail::new(format!("can't write the data addon ({e})")));
            }
            Ok(true) => {
                wrote = true;
                config::log(&format!("price feed: wrote {FOLDER} ({} houses)", list.len()));
            }
            Ok(false) => {}
        }
        // a house whose scan is hours behind while the site answers: said in the window, logged and noted for problem
        // reports once (while the site can't be reached, old prices need no second explanation)
        let late = if error.is_none() { late_houses(&list, &self.hourly, now) } else { BTreeMap::new() };
        for (key, sibling) in &late {
            if self.late_noted.insert(key.clone()) {
                let at = self.houses.get(key).map_or(0, |h| h.0.at);
                let m = match sibling {
                    0 => format!("price feed: {key} is late: its newest scan on the site is {} h old", (now - at) / 3600),
                    s => format!("price feed: {key} is late: its newest scan on the site is {} h old, the realm's other houses' {} min", (now - at) / 3600, (now - s).max(0) / 60),
                };
                config::log(&m);
                crate::report::note_error("price feed", &m);
            }
        }
        if error.is_none() {
            self.late_noted.retain(|k| late.contains_key(k));
        }
        let houses = list
            .iter()
            .map(|h| HouseView { key: h.key.clone(), realm: h.realm.clone(), side: h.side.clone(), at: h.at, late: late.contains_key(&h.key), sibling_at: late.get(&h.key).copied().unwrap_or(0) })
            .collect();
        let (wait, note) = match error {
            Some(e) => {
                self.failures += 1;
                let wait = next_wait(self.failures, e.retry_after, jitter());
                self.hold_until = e.retry_after.map(|r| Instant::now() + r.min(MAX_RETRY_AFTER));
                config::log(&format!("price feed: {}; trying again in {}", e.msg, words(wait)));
                // a short server hiccup (a restart while the site updates answers 502 for a minute) is retried
                // quietly; health.rs decides when (and whether) the player hears of it, in the window's one inbox
                health::fail(Job::PriceFeed, "", trouble_of(&e, local));
                (wait, None)
            }
            None => {
                health::ok(Job::PriceFeed, "");
                self.failures = 0;
                self.hold_until = None;
                self.checked = now;
                // the app is running and the site answered: Shared.lua says so, at most every BEAT_EVERY
                if self.last.is_some() && self.beat_at.is_none_or(|t| now - t >= BEAT_EVERY) {
                    match beat(&addons, now) {
                        Ok(_) => self.beat_at = Some(now),
                        Err(e) => config::log(&format!("price feed: can't update Shared.lua's check time: {e}")),
                    }
                }
                if !changed && !wrote {
                    // an unchanged check logs nothing but a summary: the first one, then one every QUIET_LOG
                    self.quiet += 1;
                    if self.quiet_logged.is_none_or(|t| t.elapsed() >= QUIET_LOG) {
                        config::log(&format!("price feed: no new scans ({} checks of {} houses since the last summary; the next in 15 min)", self.quiet, list.len()));
                        self.quiet = 0;
                        self.quiet_logged = Some(Instant::now());
                    }
                }
                let note = list.is_empty().then(|| "No scans of your auction houses yet".to_string());
                (next_wait(0, None, jitter()), note)
            }
        };
        (View { on, houses, note, checked: self.checked, reader_off: self.reader_off }, wait)
    }
}

/// A house back in the compact form, for the cache (parse() reads it again at the next start).
fn body_of(h: &House) -> Value {
    let rows: Vec<Value> = h
        .rows
        .iter()
        .filter_map(|(k, r)| {
            let (id, sfx) = k.split_once(':')?;
            let f: Vec<&str> = r.splitn(13, ',').collect();
            let num = |s: &str| -> Value { s.parse::<i64>().map(Value::from).unwrap_or(Value::Null) };
            let mut a: Vec<Value> = vec![num(id), num(sfx)];
            a.extend(f.iter().take(12).map(|s| num(s)));
            if let Some(n) = f.get(12) {
                a.push(Value::from(*n));
            }
            Some(Value::Array(a))
        })
        .collect();
    let lows: Vec<Value> = h
        .lows
        .iter()
        .map(|(id, l)| {
            let mut a = vec![Value::from(*id)];
            a.extend(l.split(',').map(|s| s.parse::<i64>().map(Value::from).unwrap_or(Value::Null)));
            Value::Array(a)
        })
        .collect();
    serde_json::json!({ "v": 1, "key": h.key, "region": h.region, "realm": h.realm, "realm_id": h.realm_id, "side": h.side,
        "at": h.at, "auctions": h.auctions, "first_day": h.first_day, "rows": rows, "lows": lows })
}

/// The shared-prices thread: checks at start, about once a minute (later after errors: 2, 4, 8, 15 minutes, or what
/// the server's Retry-After asks), and when poked (after an auctions upload).
pub fn run(cfg: impl Fn() -> (Option<PathBuf>, bool), server: String, mut report: impl FnMut(&View)) {
    let (tx, rx): (Sender<()>, Receiver<()>) = std::sync::mpsc::channel();
    let _ = POKE.set(Mutex::new(tx));
    let mut p = Poller::new(&server);
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let (dir, on) = cfg();
            let (view, wait) = match dir {
                Some(d) => p.refresh(&d, on),
                None => (View { on, note: Some("No WoW Forever folder".into()), ..Default::default() }, EVERY),
            };
            report(&view);
            next = Instant::now() + wait;
        }
        match rx.recv_timeout(next.saturating_duration_since(Instant::now()).min(Duration::from_secs(60))) {
            Ok(()) => {
                // an upload may be followed by more within seconds: take them as one
                std::thread::sleep(Duration::from_secs(2));
                while rx.try_recv().is_ok() {}
                // the upload may have brought a new house: ask the list too; never sooner than a Retry-After
                p.relist = true;
                next = p.hold_until.map_or_else(Instant::now, |t| t.max(Instant::now()));
            }
            Err(RecvTimeoutError::Timeout) => {
                // about once a minute: put the data back if an addon update replaced the folder
                if let (Some(d), true) = cfg() {
                    p.check(&d);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(at: i64) -> Value {
        json!({
            "v": 1, "key": "EU-4-Alliance", "region": "EU", "realm": "CAMWOW EU5 Realm 4 PvP", "realm_id": 4, "side": "Alliance",
            "at": at, "auctions": 427, "scans": 2, "first_day": 20770, "days": 30,
            "rows": [
                [2589, 0, 28, 30, 31, 15710, 424, 0, 3, 10, 20, 100, 294, 20],
                [15347, 1019, 3222, 3222, 3300, 3, 3, 0, 0, 0, 1, 1, 1, 3000, "Bracers of the \"Eagle\", |cffff0000red"],
                [1, 0, -5, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, null],
                [2, 0, 1, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0, null],
                "nonsense",
            ],
            "lows": [[2589, 28, null, 30], [3, "x"], [15347]],
        })
    }

    #[test]
    fn checks_every_row_and_skips_bad_ones() {
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        assert_eq!(h.rows.len(), 2, "negative price, qty below auctions and junk are skipped");
        assert_eq!(h.rows[0], ("2589:0".into(), "28,30,31,15710,424,0,3,10,20,100,294,20".into()));
        // quotes, commas and escape bars can't reach the Lua file or split the row
        assert_eq!(h.rows[1].1, "3222,3222,3300,3,3,0,0,0,1,1,1,3000,Bracers of the Eagle cffff0000red");
        assert_eq!(h.lows, vec![(2589, "28,,30".into())]);
        assert!(parse(&body(1_795_000_000), "EU-5-Horde", 1_795_000_100).is_err(), "another house");
        assert!(parse(&body(1_900_000_000), "EU-4-Alliance", 1_795_000_100).is_err(), "from the future");
        let mut b = body(1_795_000_000);
        b["side"] = json!("Goblin");
        assert!(parse(&b, "EU-4-Alliance", 1_795_000_100).is_err());
        // the cache form reads back the same
        assert_eq!(parse(&body_of(&h), "EU-4-Alliance", 1_795_000_100).unwrap(), h);
    }

    #[test]
    fn finds_the_houses_the_player_uses() {
        let saved = "EverlookAuctionsDB = {\n[\"houses\"] = {\n[\"CAMWOW EU5 Realm 4 PvP-Alliance\"] = {\n[\"items\"] = {\n},\n},\n[\"Other-Neutral\"] = {\n},\n},\n[\"lists\"] = {\n[\"Bags-Horde\"] = \"x\",\n},\n}";
        assert_eq!(saved_houses(saved), vec![("CAMWOW EU5 Realm 4 PvP".into(), "Alliance".into()), ("Other".into(), "Neutral".into())]);
        let list = json!({ "houses": [
            { "key": "EU-6946-Alliance", "realm": "CAMWOW EU5 Realm 4 PvP", "side": "Alliance" },
            { "key": "EU-6946-Horde", "realm": "CAMWOW EU5 Realm 4 PvP", "side": "Horde" },
            { "key": "EU-6946-Neutral", "realm": "CAMWOW EU5 Realm 4 PvP", "side": "Neutral" },
            { "key": "EU-7-Horde", "realm": "Fresh", "side": "Horde" },
            { "key": "EU-8-Horde", "realm": "Elsewhere", "side": "Horde" },
            { "key": "../x", "realm": "Fresh", "side": "Alliance" },
        ]});
        let mut want: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        want.entry("CAMWOW EU5 Realm 4 PvP".into()).or_default().insert("Alliance".into());
        want.entry("Fresh".into()).or_default(); // a realm folder, side unknown
        assert_eq!(pick(&list, &want), vec!["EU-6946-Alliance", "EU-6946-Neutral", "EU-7-Horde"]);

        // scans that know only the realm ID (the house key "-Alliance"): the houses of that ID
        let saved = "[\"houses\"] = {
[\"-Alliance\"] = {
},
},
[\"scans\"] = {
{
[\"realmID\"] = 6946,
},
{
[\"realmID\"] = 0,
},
},";
        assert!(saved_houses(saved).is_empty());
        assert_eq!(saved_realm_ids(saved), BTreeSet::from([6946]));
        let list = json!({ "houses": [
            { "key": "EU-6946-Alliance", "realm": "CAMWOW EU5 Realm 4 PvP", "realm_id": 6946, "side": "Alliance" },
            { "key": "EU-7-Horde", "realm": "Fresh", "realm_id": 7, "side": "Horde" },
        ]});
        let want: BTreeMap<String, BTreeSet<String>> = BTreeMap::from([(id_key(6946), BTreeSet::new())]);
        assert_eq!(pick(&list, &want), vec!["EU-6946-Alliance"]);
    }

    fn game(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("everlookdb-shared-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let addons = root.join("Interface").join("AddOns");
        std::fs::create_dir_all(addons.join(READER)).unwrap();
        std::fs::write(addons.join(READER).join("EverlookDBAuctions.toc"), "## Interface: 16002\n## Title: x\n").unwrap();
        (root, addons)
    }

    /// The addon as CurseForge installs it: its .toc, the empty Shared.lua, README, LICENSE and Media/.
    fn install_stub(addons: &Path) {
        let dir = addons.join(FOLDER);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Media")).unwrap();
        std::fs::write(dir.join(format!("{FOLDER}.toc")), "## Interface: 16001\n## Title: EverlookDB Auctions: Price Feed\n## Version: v0.1.0\n## Dependencies: EverlookDBAuctions\n\nShared.lua\n").unwrap();
        std::fs::write(dir.join("Shared.lua"), "-- EverlookDB Auctions: Price Feed. This is the empty copy.\nEverlookDBAuctionsShared = { version = 1, written = 0, houses = {} }\n").unwrap();
        std::fs::write(dir.join("README.md"), "readme").unwrap();
        std::fs::write(dir.join("LICENSE"), "gpl").unwrap();
        std::fs::write(dir.join("Media").join("icon.tga"), "tga").unwrap();
    }

    fn names_in(dir: &Path) -> BTreeSet<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn writes_over_the_curseforge_copy_and_never_deletes_its_files() {
        let (root, addons) = game("cf");
        install_stub(&addons);
        let dir = addons.join(FOLDER);
        // a file the app didn't write, even one named like a house file, stays
        std::fs::write(dir.join("EU-9-Horde.lua"), "-- not the app's file").unwrap();
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        assert!(write_addon(&addons, &[&h], 100).unwrap());
        let toc = std::fs::read_to_string(dir.join(format!("{FOLDER}.toc"))).unwrap();
        assert!(toc.contains("## Version: v0.1.0\n"), "the installed copy's version is kept: {toc}");
        assert!(toc.contains("## IconTexture: Interface\\AddOns\\EverlookDBAuctions_Shared\\Media\\icon\n"));
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 100"), "the empty Shared.lua is replaced");
        assert_eq!(names_in(&dir), set(&["EU-4-Alliance.lua", "EU-9-Horde.lua", "EverlookDBAuctions_Shared.toc", "LICENSE", "Media", "README.md", "Shared.lua"]));
        // the feed turned off: the empty copy again, the CurseForge files untouched
        assert!(write_stub(&addons).unwrap());
        assert_eq!(names_in(&dir), set(&["EU-9-Horde.lua", "EverlookDBAuctions_Shared.toc", "LICENSE", "Media", "README.md", "Shared.lua"]));
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 0,"));
        let toc = std::fs::read_to_string(dir.join(format!("{FOLDER}.toc"))).unwrap();
        assert!(toc.ends_with("\nShared.lua\n") && toc.contains("## Version: v0.1.0\n") && toc.contains("## Dependencies: EverlookDBAuctions\n"));
        assert!(dir.join("Media").join("icon.tga").exists());
        assert_eq!(std::fs::read_to_string(dir.join("EU-9-Horde.lua")).unwrap(), "-- not the app's file");
        assert!(!write_stub(&addons).unwrap(), "nothing changes the second time");
        // no folder: the stub creates none
        let (root2, addons2) = game("none");
        assert!(!write_stub(&addons2).unwrap() && !addons2.join(FOLDER).exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn writes_the_data_again_when_an_addon_update_brings_back_the_empty_copy() {
        let (root, addons) = game("restore");
        install_stub(&addons);
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        let mut houses = BTreeMap::new();
        houses.insert(h.key.clone(), (h.clone(), None));
        write_addon(&addons, &[&h], 100).unwrap();
        let mut last = Written { toc: read_toc(&addons), keys: vec![h.key.clone()], odds: false };
        let mut check = || match restore(&addons, &last, &houses, None).unwrap() {
            Some(w) => {
                last = w;
                true
            }
            None => false,
        };
        assert!(!check(), "nothing to do while the folder holds what was written");
        // CurseForge updates the addon: the folder is replaced by the empty copy
        install_stub(&addons);
        assert!(check(), "noticed and written again");
        let dir = addons.join(FOLDER);
        assert!(dir.join("EU-4-Alliance.lua").is_file() && read_toc(&addons).ends_with("Shared.lua\nEU-4-Alliance.lua\n"));
        assert!(!std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 0,"));
        assert!(dir.join("README.md").exists() && dir.join("Media").join("icon.tga").exists());
        assert!(!check(), "and then quiet again");
        // only the house file went: also noticed
        std::fs::remove_file(dir.join("EU-4-Alliance.lua")).unwrap();
        assert!(check() && dir.join("EU-4-Alliance.lua").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_only_its_own_folder_and_only_on_change() {
        let root = std::env::temp_dir().join(format!("everlookdb-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let addons = root.join("Interface").join("AddOns");
        std::fs::create_dir_all(addons.join(READER)).unwrap();
        std::fs::write(addons.join(READER).join("EverlookDBAuctions.toc"), "## Interface: 16002\n## Title: x\n").unwrap();
        std::fs::create_dir_all(addons.join("Other")).unwrap();
        std::fs::write(addons.join("Other").join("Other.lua"), "x").unwrap();
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        assert!(write_addon(&addons, &[&h], 100).unwrap());
        let dir = addons.join(FOLDER);
        let toc = std::fs::read_to_string(dir.join(format!("{FOLDER}.toc"))).unwrap();
        assert!(toc.starts_with("## Interface: 16002\n"));
        assert!(toc.contains("## Title: EverlookDB Auctions: Price Feed\n"));
        // listed under EverlookDB Auctions in the addon list, and never loaded without it
        assert!(toc.contains("## Dependencies: EverlookDBAuctions\n"));
        assert!(toc.contains("## LoadOnDemand: 0") && toc.ends_with("Shared.lua\nEU-4-Alliance.lua\n"));
        let lua = std::fs::read_to_string(dir.join("EU-4-Alliance.lua")).unwrap();
        assert!(lua.contains("[\"2589:0\"] = \"28,30,31,15710,424,0,3,10,20,100,294,20\","));
        assert!(lua.contains("[2589] = \"28,,30\","));
        // nothing changed: nothing written, not even Shared.lua's time
        assert!(!write_addon(&addons, &[&h], 200).unwrap());
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 100"));
        // a house the app wrote that is no longer wanted goes; temp files never stay; other addons are untouched
        std::fs::write(dir.join("EU-9-Horde.lua"), format!("{MARK}.\nold")).unwrap();
        std::fs::write(dir.join(".EU-9-Horde.lua.tmp"), "half").unwrap();
        let mut h2 = h.clone();
        h2.at += 600;
        assert!(write_addon(&addons, &[&h2], 300).unwrap());
        let names: BTreeSet<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["EU-4-Alliance.lua", "EverlookDBAuctions_Shared.toc", "Shared.lua"].iter().map(|s| s.to_string()).collect());
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 300"));
        // off: the empty copy, never a deleted folder
        assert!(write_stub(&addons).unwrap());
        assert_eq!(names_in(&dir), set(&["EverlookDBAuctions_Shared.toc", "Shared.lua"]));
        assert!(addons.join("Other").join("Other.lua").exists() && addons.join(READER).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_the_disenchant_odds_next_to_the_houses() {
        let (_root, addons) = game("odds");
        install_stub(&addons);
        let dir = addons.join(FOLDER);
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        let odds = crate::delog::odds_lua(&json!({ "v": 1, "updated": 5, "kinds": { "4:2:25": { "n": 9, "mats": { "10940": { "p": 1.0, "c": 2.0 } } } }, "items": {} }), MARK).unwrap();
        assert!(write_feed(&addons, &[&h], Some(&odds), 100).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join(ODDS_FILE)).unwrap(), odds);
        assert!(read_toc(&addons).ends_with("Shared.lua\nEU-4-Alliance.lua\nDEOdds.lua\n"), "{}", read_toc(&addons));
        assert!(!write_feed(&addons, &[&h], Some(&odds), 200).unwrap(), "unchanged: nothing written");
        // odds only, no houses
        assert!(write_feed(&addons, &[], Some(&odds), 300).unwrap());
        assert!(!dir.join("EU-4-Alliance.lua").exists() && dir.join(ODDS_FILE).is_file());
        // an addon update brings back the empty copy: the odds are written again
        let last = Written { toc: read_toc(&addons), keys: vec![], odds: true };
        install_stub(&addons);
        std::fs::remove_file(dir.join(ODDS_FILE)).ok();
        assert!(restore(&addons, &last, &BTreeMap::new(), Some(&odds)).unwrap().is_some());
        assert!(dir.join(ODDS_FILE).is_file() && read_toc(&addons).ends_with("Shared.lua\nDEOdds.lua\n"));
        // without odds the app's DEOdds.lua goes; the stub removes it too
        assert!(write_feed(&addons, &[&h], None, 400).unwrap());
        assert!(!dir.join(ODDS_FILE).exists() && !read_toc(&addons).contains("DEOdds"));
        write_feed(&addons, &[&h], Some(&odds), 500).unwrap();
        assert!(write_stub(&addons).unwrap());
        assert!(!dir.join(ODDS_FILE).exists());
        assert!(dir.join("README.md").is_file(), "the CurseForge files stay");
    }

    #[test]
    fn house_file_is_plain_lua() {
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        let lua = house_lua(&h);
        // every string is quoted plainly: no quote, backslash or newline inside a value
        for line in lua.lines().filter(|l| l.trim_start().starts_with('[')) {
            assert_eq!(line.matches('"').count(), if line.trim_start().starts_with("[\"") { 4 } else { 2 }, "{line}");
            assert!(!line.contains('\\'));
        }
        assert!(lua.contains("realm = \"CAMWOW EU5 Realm 4 PvP\", realmID = 4, side = \"Alliance\", at = 1795000000"));
    }

    #[test]
    fn checks_once_a_minute_and_backs_off_on_errors() {
        // a minute, up to 5 s early at random
        assert_eq!(next_wait(0, None, Duration::ZERO), Duration::from_secs(60));
        assert_eq!(next_wait(0, None, Duration::from_secs(5)), Duration::from_secs(55));
        assert_eq!(next_wait(0, None, Duration::from_secs(60)), Duration::from_secs(55), "jitter is capped");
        for _ in 0..50 {
            let w = next_wait(0, None, jitter());
            assert!((Duration::from_secs(55)..=Duration::from_secs(60)).contains(&w), "{w:?}");
        }
        // errors: 2, 4, 8, then 15 minutes at most (plus the jitter)
        let mins = |n: u32| next_wait(n, None, Duration::ZERO).as_secs() / 60;
        assert_eq!((mins(1), mins(2), mins(3), mins(4), mins(5), mins(1000)), (2, 4, 8, 15, 15, 15));
        assert_eq!(next_wait(1, None, Duration::from_secs(3)), Duration::from_secs(123));
        // Retry-After is honoured: never sooner, up to an hour
        assert_eq!(next_wait(1, Some(Duration::from_secs(600)), Duration::ZERO), Duration::from_secs(600));
        assert_eq!(next_wait(1, Some(Duration::from_secs(10)), Duration::ZERO), Duration::from_secs(120), "the backoff is longer");
        assert_eq!(next_wait(4, Some(Duration::from_secs(7200)), Duration::ZERO), Duration::from_secs(3600));
        assert_eq!(retry_after_of("120", 0), Some(Duration::from_secs(120)));
        assert_eq!(retry_after_of("Thu, 01 Jan 1970 00:02:00 GMT", 60), Some(Duration::from_secs(60)));
        assert_eq!(retry_after_of("soon", 0), None);
    }

    #[test]
    fn fetches_only_houses_whose_version_moved() {
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        let mut have = BTreeMap::new();
        have.insert("EU-4-Alliance".to_string(), (h.clone(), Some("W/\"c1-1795000000-3-20777\"".to_string())));
        let mut h5 = h.clone();
        h5.key = "EU-5-Horde".into();
        have.insert("EU-5-Horde".to_string(), (h5, Some("W/\"c1-1-1-1\"".to_string())));
        let keys: Vec<String> = ["EU-4-Alliance", "EU-5-Horde", "EU-6-Neutral", "EU-7-Horde"].iter().map(|s| s.to_string()).collect();
        let v = versions_of(&json!({ "EU-4-Alliance": "c1-1795000000-3-20777", "EU-5-Horde": "c1-1795000600-2-20777", "EU-6-Neutral": "c1-9-1-1", "EU-7-Horde": null, "EU-8-Horde": "x" }), &keys);
        assert_eq!(v.len(), 4, "only the houses asked for");
        assert_eq!(moved(&v, &have), set(&["EU-5-Horde", "EU-6-Neutral"]), "moved, and new; not unchanged or never scanned");
        // a house we have that the server no longer has: fetched, so its 404 removes it
        let v = versions_of(&json!({ "EU-4-Alliance": null }), &keys[..1]);
        assert_eq!(moved(&v, &have), set(&["EU-4-Alliance"]));
        assert_eq!(tag_version("W/\"c1-1-2-3\""), "c1-1-2-3");
    }

    /// A small everlookdb.com: the house list, /versions (or none, like an older server), one house's compact with
    /// ETag and 304, or 429 with Retry-After. Every request is logged ("path", "+etag" when it sent If-None-Match).
    struct Fake {
        versions: bool,
        etag: String,
        body: Vec<u8>,
        busy: Option<u64>,
        log: Vec<String>,
        /// the list has no houses (a short list)
        unlisted: bool,
        /// the list answers 200 without its `houses`
        bad_list: bool,
        /// the site has no scans of the house: its version is null and its compact answers 404
        gone: bool,
    }

    fn serve(state: std::sync::Arc<Mutex<Fake>>) -> String {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = vec![];
                let mut b = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&b[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                let inm = req.lines().find(|l| l.to_ascii_lowercase().starts_with("if-none-match:")).map(|l| l[14..].trim().to_string());
                let mut f = state.lock().unwrap();
                f.log.push(format!("{path}{}", if inm.is_some() { "+etag" } else { "" }));
                let (code, extra, body): (u16, String, Vec<u8>) = if let Some(secs) = f.busy {
                    (429, format!("Retry-After: {secs}\r\n"), b"{}".to_vec())
                } else if path == "/api/v1/auctions" && f.bad_list {
                    (200, String::new(), b"{\"error\":\"busy\"}".to_vec())
                } else if path == "/api/v1/auctions" {
                    let list = if f.unlisted {
                        json!({ "houses": [] })
                    } else {
                        json!({ "houses": [{ "key": "EU-4-Alliance", "realm": "CAMWOW EU5 Realm 4 PvP", "realm_id": 4, "side": "Alliance", "link": "EU-B110-Alliance" }] })
                    };
                    (200, String::new(), list.to_string().into_bytes())
                } else if path == "/api/v1/auctions/versions?houses=EU-4-Alliance" && f.versions {
                    let v = if f.gone { Value::Null } else { json!(tag_version(&f.etag)) };
                    (200, String::new(), json!({ "EU-4-Alliance": v }).to_string().into_bytes())
                } else if path == "/api/v1/auctions/house/EU-4-Alliance/compact" && f.gone {
                    (404, String::new(), vec![])
                } else if path == "/api/v1/auctions/house/EU-4-Alliance/compact" {
                    if inm.as_deref() == Some(f.etag.as_str()) {
                        (304, format!("ETag: {}\r\n", f.etag), vec![])
                    } else {
                        (200, format!("ETag: {}\r\n", f.etag), f.body.clone())
                    }
                } else {
                    (404, String::new(), vec![])
                };
                let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    /// The tests that run a Poller share one house's cache file: one at a time.
    static FEED: Mutex<()> = Mutex::new(());

    /// The modified times of every file in the folder: the same after a check means nothing was written.
    fn stamps(dir: &Path) -> Vec<(String, std::time::SystemTime)> {
        let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.metadata().unwrap().modified().unwrap())).collect();
        v.sort();
        v
    }

    #[test]
    fn a_minute_check_asks_versions_and_writes_nothing_when_unchanged() {
        let _one = FEED.lock().unwrap_or_else(|e| e.into_inner());
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (root, addons) = game("feed");
        std::fs::create_dir_all(root.join("WTF").join("Account").join("ACC").join("CAMWOW EU5 Realm 4 PvP")).unwrap();
        let _ = std::fs::remove_file(cache_path("EU-4-Alliance"));
        let body1 = body(1_795_000_000).to_string().into_bytes();
        let fake = std::sync::Arc::new(Mutex::new(Fake { versions: true, etag: "W/\"c1-1795000000-1-20777\"".into(), body: body1, busy: None, log: vec![], unlisted: false, bad_list: false, gone: false }));
        let mut p = Poller::new(&serve(fake.clone()));
        let take_log = || std::mem::take(&mut fake.lock().unwrap().log);
        let dir = addons.join(FOLDER);
        let minute = Duration::from_secs(55)..=Duration::from_secs(60);

        // the first check: the list, the versions, the house, the disenchant odds (none on this server); written
        let (view, wait) = p.refresh(&root, true);
        assert_eq!(take_log(), ["/api/v1/auctions", "/api/v1/auctions/versions?houses=EU-4-Alliance", "/api/v1/auctions/house/EU-4-Alliance/compact", "/api/v1/crowd/disenchant"]);
        assert!(minute.contains(&wait), "{wait:?}");
        assert_eq!((view.houses.len(), view.note.clone()), (1, None));
        assert!(dir.join("EU-4-Alliance.lua").is_file());
        let before = stamps(&dir);
        std::thread::sleep(Duration::from_millis(30));

        // the next minute: one versions request, nothing fetched, nothing written
        let (view, wait) = p.refresh(&root, true);
        assert_eq!(take_log(), ["/api/v1/auctions/versions?houses=EU-4-Alliance"]);
        assert!(minute.contains(&wait) && view.houses.len() == 1);
        assert_eq!(stamps(&dir), before, "an unchanged check writes nothing");

        // an older server without /versions: the house with its ETag, a 304, and still nothing written
        fake.lock().unwrap().versions = false;
        p.refresh(&root, true);
        assert_eq!(take_log(), ["/api/v1/auctions/versions?houses=EU-4-Alliance", "/api/v1/auctions/house/EU-4-Alliance/compact+etag"]);
        let (_, wait) = p.refresh(&root, true);
        assert_eq!(take_log(), ["/api/v1/auctions/house/EU-4-Alliance/compact+etag"], "versions aren't asked again for a while");
        assert!(minute.contains(&wait));
        assert_eq!(stamps(&dir), before, "a 304 writes nothing");

        // busy: 429 with Retry-After; the backoff, nothing written, the data kept
        fake.lock().unwrap().busy = Some(300);
        let (view, wait) = p.refresh(&root, true);
        assert_eq!(take_log().len(), 1, "one request, then it stops");
        assert!(wait >= Duration::from_secs(300) && wait <= Duration::from_secs(305), "{wait:?}");
        assert!(view.note.is_none() && view.houses.len() == 1, "a failure is never the view's note (health.rs)");
        assert!(p.hold_until.is_some());
        fake.lock().unwrap().busy = Some(1);
        let (_, wait) = p.refresh(&root, true);
        take_log();
        assert_eq!(wait.as_secs() / 60, 4, "the second error in a row: 4 minutes");
        assert_eq!(stamps(&dir), before);

        // back: a new scan; the versions moved, the house is fetched and written, and it is once a minute again
        {
            let mut f = fake.lock().unwrap();
            f.busy = None;
            f.versions = true;
            f.etag = "W/\"c1-1795000600-2-20777\"".into();
            f.body = body(1_795_000_600).to_string().into_bytes();
        }
        p.no_versions_until = None;
        let (view, wait) = p.refresh(&root, true);
        assert_eq!(take_log(), ["/api/v1/auctions/versions?houses=EU-4-Alliance", "/api/v1/auctions/house/EU-4-Alliance/compact+etag"]);
        assert!(minute.contains(&wait), "{wait:?}");
        assert_eq!((view.houses[0].at, view.note), (1_795_000_600, None));
        assert!(p.hold_until.is_none() && p.failures == 0);
        assert!(std::fs::read_to_string(dir.join("EU-4-Alliance.lua")).unwrap().contains("at = 1795000600"));
        assert_ne!(stamps(&dir), before);
        let _ = std::fs::remove_file(cache_path("EU-4-Alliance"));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn failures_follow_the_one_policy() {
        let busy = Fail { msg: "the server answered 502".into(), retry_after: None, stop: true };
        assert_eq!(trouble_of(&busy, None), Trouble::Unreachable("the server answered 502".into()));
        assert_eq!(trouble_of(&Fail::new("a bad answer"), None), Trouble::Server("a bad answer".into()));
        // the data addon can't be written: shown at once, whatever the server did
        assert!(trouble_of(&busy, Some("EverlookDB isn't allowed to change the Price Feed addon.".into())).is_local());
    }

    fn house(key: &str, side: &str, at: i64) -> House {
        let mut h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        h.key = key.into();
        h.side = side.into();
        h.at = at;
        h
    }

    #[test]
    fn a_house_hours_behind_is_late() {
        let now = 1_795_100_000;
        let (min, hour) = (60, 3600);
        // 2026-11-26: Alliance 20 min old, Neutral 7 h old, and nothing said so
        let a = house("EU-6946-Alliance", "Alliance", now - 20 * min);
        let n = house("EU-6946-Neutral", "Neutral", now - 7 * hour);
        let none = BTreeSet::new();
        assert_eq!(late_houses(&[&a, &n], &none, now), BTreeMap::from([("EU-6946-Neutral".to_string(), a.at)]));
        // both old together (nobody scanned lately, no hourly feed): old, not late
        let a_old = house("EU-6946-Alliance", "Alliance", now - 6 * hour);
        assert!(late_houses(&[&a_old, &n], &none, now).is_empty());
        // a lone house nobody scans: not late either
        assert!(late_houses(&[&n], &none, now).is_empty());
        // one the site fills hourly must never be hours old, alone or not
        let linked = set(&["EU-6946-Neutral"]);
        assert_eq!(late_houses(&[&n], &linked, now), BTreeMap::from([("EU-6946-Neutral".to_string(), 0)]));
        assert!(late_houses(&[&house("EU-6946-Neutral", "Neutral", now - 2 * hour)], &linked, now).is_empty(), "two hours is still fine");
        // another realm's fresh house says nothing about this one
        let mut other = house("EU-7-Alliance", "Alliance", now - min);
        other.realm = "Elsewhere".into();
        assert!(late_houses(&[&other, &n], &none, now).is_empty());
        // which houses the site fills hourly: Blizzard's own and the ones linked to one
        let list = json!({ "houses": [
            { "key": "EU-6946-Alliance", "link": "EU-B110-Alliance" },
            { "key": "EU-B110-Alliance", "source": "blizzard" },
            { "key": "EU-6946-Neutral" },
            { "key": "EU-1-Horde", "link": "" },
            { "key": "../x", "link": "EU-B110-Alliance" },
        ]});
        assert_eq!(hourly(&list), set(&["EU-6946-Alliance", "EU-B110-Alliance"]));
    }

    #[test]
    fn shared_lua_says_when_the_site_last_answered() {
        let (root, addons) = game("beat");
        install_stub(&addons);
        let dir = addons.join(FOLDER);
        // the shipped empty copy is never touched
        assert!(!beat(&addons, 500).unwrap());
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 0,"));
        let h = parse(&body(1_795_000_000), "EU-4-Alliance", 1_795_000_100).unwrap();
        write_addon(&addons, &[&h], 100).unwrap();
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("written = 100, checked = 100,"));
        // later, nothing new on the site: only the check time moves, the house file isn't touched
        let before = std::fs::metadata(dir.join("EU-4-Alliance.lua")).unwrap().modified().unwrap();
        assert!(beat(&addons, 2000).unwrap());
        let text = std::fs::read_to_string(dir.join("Shared.lua")).unwrap();
        assert!(text.starts_with(MARK) && text.contains("written = 100, checked = 2000,"), "{text}");
        assert_eq!(std::fs::metadata(dir.join("EU-4-Alliance.lua")).unwrap().modified().unwrap(), before);
        assert!(!beat(&addons, 2000).unwrap(), "the same time again writes nothing");
        // an unchanged write keeps it; the off copy isn't touched
        assert!(!write_addon(&addons, &[&h], 3000).unwrap());
        assert!(std::fs::read_to_string(dir.join("Shared.lua")).unwrap().contains("checked = 2000,"));
        write_stub(&addons).unwrap();
        assert!(!beat(&addons, 4000).unwrap());
        // no folder at all: nothing is made
        let (root2, addons2) = game("beat-none");
        assert!(!beat(&addons2, 1).unwrap() && !addons2.join(FOLDER).exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn counts_characters_with_the_reader_turned_off() {
        let (root, _addons) = game("readeroff");
        let acc = root.join("WTF").join("Account").join("ACC");
        let put_char = |name: &str, text: &str| {
            let d = acc.join("Realm").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("AddOns.txt"), text).unwrap();
        };
        std::fs::create_dir_all(acc.join("SavedVariables")).unwrap();
        put_char("Fine", "Bagnon: enabled\nEverlookDBAuctions: enabled\nEverlookDBAuctions_Shared: enabled\n");
        // the friend's case: the addon off, the Price Feed left on (listed, or not listed yet)
        put_char("Off", "EverlookDB: enabled\r\nEverlookDBAuctions: disabled\r\nEverlookDBAuctions_Shared: enabled\r\n");
        put_char("OffUnlisted", "EverlookDBAuctions: disabled\n");
        // both off: the player's own choice
        put_char("BothOff", "EverlookDBAuctions: disabled\nEverlookDBAuctions_Shared: disabled\n");
        put_char("Neither", "Bagnon: enabled\n");
        let now = std::time::SystemTime::now();
        assert_eq!(reader_off(&root, now), 2);
        // a character not played for months doesn't count
        assert_eq!(reader_off(&root, now + Duration::from_secs(90 * 86400)), 0);
        assert_eq!(addon_state("EverlookDBAuctions_Shared: enabled\n", READER), None, "a longer name isn't the addon");
        assert_eq!(reader_off(&root.join("nowhere"), now), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_house_the_site_stops_listing_keeps_its_copy_for_a_while() {
        let _one = FEED.lock().unwrap_or_else(|e| e.into_inner());
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (root, addons) = game("grace");
        std::fs::create_dir_all(root.join("WTF").join("Account").join("ACC").join("CAMWOW EU5 Realm 4 PvP")).unwrap();
        let _ = std::fs::remove_file(cache_path("EU-4-Alliance"));
        let fake = std::sync::Arc::new(Mutex::new(Fake {
            versions: true,
            etag: "W/\"c1-1795000000-1-20777\"".into(),
            body: body(1_795_000_000).to_string().into_bytes(),
            busy: None,
            log: vec![],
            unlisted: false,
            bad_list: false,
            gone: false,
        }));
        let mut p = Poller::new(&serve(fake.clone()));
        let file = addons.join(FOLDER).join("EU-4-Alliance.lua");
        let (view, _) = p.refresh(&root, true);
        assert!(file.is_file() && view.houses.len() == 1 && view.checked > 0);
        // the scan is years old and the site fills this house hourly (its `link`): late
        assert!(view.houses[0].late && view.houses[0].sibling_at == 0);

        // the list answers without its houses: a bad answer, the copy stays and the check counts as failed
        fake.lock().unwrap().bad_list = true;
        p.relist = true;
        let (view, _) = p.refresh(&root, true);
        assert!(p.failing() && file.is_file() && view.houses.len() == 1);
        assert!(!view.houses[0].late, "nothing is called late while the site can't be asked");

        // the list comes back empty: the house keeps its copy, and isn't asked for
        {
            let mut f = fake.lock().unwrap();
            f.bad_list = false;
            f.unlisted = true;
            f.log.clear();
        }
        p.relist = true;
        let (view, _) = p.refresh(&root, true);
        assert_eq!(std::mem::take(&mut fake.lock().unwrap().log), ["/api/v1/auctions"]);
        assert!(!p.failing() && file.is_file() && view.houses.len() == 1 && view.note.is_none());
        assert!(p.absent.contains_key("EU-4-Alliance"));
        // listed again: asked again, no longer absent
        fake.lock().unwrap().unlisted = false;
        p.relist = true;
        p.refresh(&root, true);
        assert!(p.absent.is_empty() && file.is_file());
        // gone for longer than the grace: now it goes
        fake.lock().unwrap().unlisted = true;
        p.relist = true;
        p.refresh(&root, true);
        *p.absent.get_mut("EU-4-Alliance").unwrap() -= GONE_GRACE;
        let (view, _) = p.refresh(&root, true);
        assert!(view.houses.is_empty() && !file.exists() && p.absent.is_empty());
        assert_eq!(view.note.as_deref(), Some("No scans of your auction houses yet"));

        // listed, but the site has no scans of it (404): the same grace
        {
            let mut f = fake.lock().unwrap();
            f.unlisted = false;
            f.gone = false;
        }
        p.relist = true;
        p.refresh(&root, true);
        assert!(file.is_file());
        fake.lock().unwrap().gone = true;
        let (view, _) = p.refresh(&root, true);
        assert!(file.is_file() && view.houses.len() == 1 && p.absent.contains_key("EU-4-Alliance") && !p.failing());
        // not asked every minute while it is gone, only with each list
        fake.lock().unwrap().log.clear();
        p.refresh(&root, true);
        assert_eq!(std::mem::take(&mut fake.lock().unwrap().log), ["/api/v1/auctions/versions?houses=EU-4-Alliance"]);
        *p.absent.get_mut("EU-4-Alliance").unwrap() -= GONE_GRACE;
        p.relist = true;
        let (view, _) = p.refresh(&root, true);
        assert!(view.houses.is_empty() && !file.exists());
        let _ = std::fs::remove_file(cache_path("EU-4-Alliance"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
