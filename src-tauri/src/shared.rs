// Shared auction prices: the freshest scan anyone uploaded to everlookdb.com for the auction houses this player uses,
// written into the game as a small data addon (Interface/AddOns/EverlookDBAuctions_Shared), so EverlookDB Auctions
// has it at the next login or /reload, even far from an auction house.
//
// Which houses: the realms in WTF/Account/*/ (a folder per realm the player has characters on) and the houses the
// Auctions addon saved prices for (EverlookDBAuctions.lua, "<realm>-<side>"), matched by realm name and side to
// GET /api/v1/auctions. Each house's GET /api/v1/auctions/house/{key}/compact is asked every 15 minutes and right
// after each auctions upload, with its ETag, so an unchanged house costs a 304. Errors back off (1, 5, 15, 30,
// 60 minutes) and never touch what is already written; the last good copy of each house is kept in the cache folder.
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

use crate::config;
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
const EVERY: Duration = Duration::from_secs(15 * 60);
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
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct View {
    pub on: bool,
    pub houses: Vec<HouseView>,
    /// why there is nothing, or the last error ("everlookdb.com can't be reached; trying again at 14:05")
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------------
// Which houses

/// Realm name -> the sides seen for it (empty: not known, so every side).
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
            for (realm, side) in saved_houses(&String::from_utf8_lossy(&text)) {
                out.entry(realm).or_default().insert(side);
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

/// The server's houses (GET /api/v1/auctions) the player uses: their keys, newest scan first. A realm with known
/// sides gets those and its neutral house.
pub fn pick(list: &Value, wanted: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut out = vec![];
    for h in list["houses"].as_array().into_iter().flatten() {
        let (Some(key), Some(realm), Some(side)) = (h["key"].as_str(), h["realm"].as_str(), h["side"].as_str()) else { continue };
        if !key_ok(key) {
            continue;
        }
        if let Some(sides) = wanted.get(realm) {
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

pub fn shared_lua(written: i64) -> String {
    format!(
        "{MARK}: other players' newest auction scans from everlookdb.com.\n\
         -- EverlookDB Auctions reads it at login and /reload, and keeps it apart from your own scans.\n\
         EverlookDBAuctionsShared = {{ version = 1, written = {written}, houses = {{}} }}\n"
    )
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
    let dir = addons.join(FOLDER);
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    let names: Vec<String> = houses.iter().map(|h| file_of(&h.key)).collect();
    for h in houses {
        changed |= put(&dir, &file_of(&h.key), &house_lua(h))?;
    }
    // Shared.lua's time only moves when a house file did, so an unchanged poll writes nothing at all; the empty copy's
    // Shared.lua (after a CurseForge update, or the feed was off) is replaced
    let shared = dir.join("Shared.lua");
    if changed || !ours(&shared) || std::fs::read_to_string(&shared).is_ok_and(|t| t.contains("written = 0,")) {
        changed |= put(&dir, "Shared.lua", &shared_lua(now))?;
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

fn get(http: &reqwest::blocking::Client, url: &str, etag: Option<&str>) -> Result<(u16, Option<String>, Vec<u8>), String> {
    let mut req = http.get(url).header("Accept-Encoding", "gzip");
    if let Some(t) = etag {
        req = req.header("If-None-Match", t);
    }
    let resp = req.send().map_err(|e| if e.is_timeout() { "no answer".to_string() } else { "can't reach everlookdb.com".to_string() })?;
    let code = resp.status().as_u16();
    let tag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
    let gz = resp.headers().get("content-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let mut raw = vec![];
    resp.take(MAX_BODY + 1).read_to_end(&mut raw).map_err(|_| "the answer was cut off".to_string())?;
    if raw.len() as u64 > MAX_BODY {
        return Err("the answer is too large".into());
    }
    let body = if gz {
        let mut out = vec![];
        flate2::read::GzDecoder::new(&raw[..]).take(MAX_BODY * 4).read_to_end(&mut out).map_err(|_| "a bad answer".to_string())?;
        out
    } else {
        raw
    };
    Ok((code, tag, body))
}

fn fetch_house(http: &reqwest::blocking::Client, server: &str, key: &str, etag: Option<&str>, now: i64) -> Result<Got, String> {
    let (code, tag, body) = get(http, &format!("{server}/api/v1/auctions/house/{key}/compact"), etag)?;
    match code {
        304 => Ok(Got::Same),
        404 => Ok(Got::Gone),
        200 => {
            let v: Value = serde_json::from_slice(&body).map_err(|_| "a bad answer".to_string())?;
            Ok(Got::New(parse(&v, key, now)?, tag))
        }
        429 => Err("the server is busy".into()),
        c => Err(format!("the server answered {c}")),
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

/// The houses in memory and what the last refresh did.
pub struct Poller {
    http: reqwest::blocking::Client,
    server: String,
    houses: BTreeMap<String, (House, Option<String>)>,
    failures: u32,
    /// what the last write left in the folder, to notice it was replaced
    last: Option<Written>,
}

struct Written {
    toc: String,
    keys: Vec<String>,
}

/// Writes the houses again when the folder no longer holds what was `last` written (its .toc changed, or a house
/// file is gone); what it wrote then, or None when nothing needed doing.
fn restore(addons: &Path, last: &Written, houses: &BTreeMap<String, (House, Option<String>)>) -> std::io::Result<Option<Written>> {
    let dir = addons.join(FOLDER);
    if read_toc(addons) == last.toc && last.keys.iter().all(|k| dir.join(file_of(k)).is_file()) {
        return Ok(None);
    }
    let list: Vec<&House> = last.keys.iter().filter_map(|k| houses.get(k).map(|h| &h.0)).collect();
    if list.is_empty() {
        return Ok(None);
    }
    write_addon(addons, &list, unix_now())?;
    Ok(Some(Written { toc: read_toc(addons), keys: list.iter().map(|h| h.key.clone()).collect() }))
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
        Poller { http, server: server.trim_end_matches('/').to_string(), houses: BTreeMap::new(), failures: 0, last: None }
    }

    /// The cheap check, about once a minute: when the folder no longer holds what the app last wrote (its .toc
    /// changed, or a house file is gone: CurseForge updated the addon and brought back its empty copy), writes it again
    /// from the houses in memory, without asking the server. Returns whether it wrote.
    pub fn check(&mut self, forever: &Path) -> bool {
        let Some(last) = &self.last else { return false };
        let addons = forever.join("Interface").join("AddOns");
        match restore(&addons, last, &self.houses) {
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

    /// One refresh of every wanted house for the `_forever_` folder; writes the data addon. Returns the view and
    /// when to look again.
    pub fn refresh(&mut self, forever: &Path, on: bool) -> (View, Duration) {
        let addons = forever.join("Interface").join("AddOns");
        if !on {
            self.last = None;
            if write_stub(&addons).unwrap_or(false) {
                config::log("price feed: off, the data addon is empty again");
            }
            return (View { on: false, houses: vec![], note: None }, EVERY);
        }
        if !addons.join(READER).is_dir() {
            self.last = None;
            let _ = write_stub(&addons);
            return (View { on, houses: vec![], note: Some("Install EverlookDB Auctions to use them".into()) }, EVERY);
        }
        let now = unix_now();
        let want = wanted(forever);
        let mut error: Option<String> = None;
        let keys: Vec<String> = match get(&self.http, &format!("{}/api/v1/auctions", self.server), None) {
            Ok((200, _, body)) => match serde_json::from_slice::<Value>(&body) {
                Ok(v) => pick(&v, &want),
                Err(_) => {
                    error = Some("a bad answer".into());
                    self.houses.keys().cloned().collect()
                }
            },
            Ok((c, _, _)) => {
                error = Some(format!("the server answered {c}"));
                self.houses.keys().cloned().collect()
            }
            Err(e) => {
                error = Some(e);
                // offline: keep writing what we have
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
        };
        self.houses.retain(|k, _| keys.contains(k));
        for key in &keys {
            if !self.houses.contains_key(key) {
                if let Some(c) = load_cached(key, now) {
                    self.houses.insert(key.clone(), c);
                }
            }
            if error.is_some() && self.houses.contains_key(key) {
                continue;
            }
            let etag = self.houses.get(key).and_then(|h| h.1.clone());
            match fetch_house(&self.http, &self.server, key, etag.as_deref(), now) {
                Ok(Got::New(h, tag)) => {
                    config::log(&format!("price feed: {} scan of {} ({} rows)", h.key, chrono::DateTime::from_timestamp(h.at, 0).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default(), h.rows.len()));
                    // the checked house, for the next start (a 304 needs it)
                    save_cached(key, &body_of(&h), tag.as_deref());
                    self.houses.insert(key.clone(), (h, tag));
                }
                Ok(Got::Same) => {}
                Ok(Got::Gone) => {
                    self.houses.remove(key);
                }
                Err(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
            }
        }
        let list: Vec<&House> = keys.iter().filter_map(|k| self.houses.get(k).map(|h| &h.0)).collect();
        let write = if list.is_empty() {
            self.last = None;
            write_stub(&addons)
        } else {
            let w = write_addon(&addons, &list, now);
            self.last = w.is_ok().then(|| Written { toc: read_toc(&addons), keys: list.iter().map(|h| h.key.clone()).collect() });
            w
        };
        if let Err(e) = write {
            config::log(&format!("price feed: can't write {FOLDER}: {e}"));
            error.get_or_insert_with(|| format!("can't write the data addon ({e})"));
        } else if write.is_ok_and(|c| c) {
            config::log(&format!("price feed: wrote {FOLDER} ({} houses)", list.len()));
        }
        let houses = list.iter().map(|h| HouseView { key: h.key.clone(), realm: h.realm.clone(), side: h.side.clone(), at: h.at }).collect();
        let (wait, note) = match error {
            Some(e) => {
                self.failures += 1;
                let wait = crate::worker::backoff(self.failures).min(EVERY);
                config::log(&format!("price feed: {e}; trying again in {} min", wait.as_secs() / 60));
                crate::report::note_error("price feed", &format!("Price Feed: {e}"));
                (wait, Some(format!("Last check failed: {e}")))
            }
            None => {
                self.failures = 0;
                let note = list.is_empty().then(|| "No one has uploaded a scan of your auction houses yet".to_string());
                (EVERY, note)
            }
        };
        (View { on, houses, note }, wait)
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

/// The shared-prices thread: refreshes at start, every 15 minutes (sooner after an error), and when poked.
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
                None => (View { on, houses: vec![], note: Some("No WoW Forever folder".into()) }, EVERY),
            };
            report(&view);
            next = Instant::now() + wait;
        }
        match rx.recv_timeout(next.saturating_duration_since(Instant::now()).min(Duration::from_secs(60))) {
            Ok(()) => {
                // an upload may be followed by more within seconds: take them as one
                std::thread::sleep(Duration::from_secs(2));
                while rx.try_recv().is_ok() {}
                next = Instant::now();
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
        let mut last = Written { toc: read_toc(&addons), keys: vec![h.key.clone()] };
        let mut check = || match restore(&addons, &last, &houses).unwrap() {
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
}
