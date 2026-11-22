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
        }
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
            return (View { on: false, houses: vec![], note: None }, EVERY);
        }
        if !addons.join(READER).is_dir() {
            self.last = None;
            let _ = write_stub(&addons);
            return (View { on, houses: vec![], note: Some("Install EverlookDB Auctions to use them".into()) }, EVERY);
        }
        let now = unix_now();
        let mut error: Option<Fail> = None;
        // the list of houses: at the first check, every LIST_EVERY and after an upload
        let list_due = self.relist || self.listed.is_none_or(|t| t.elapsed() >= LIST_EVERY);
        let keys: Vec<String> = if !list_due {
            self.keys.clone()
        } else {
            match get(&self.http, &format!("{}/api/v1/auctions", self.server), None) {
                Ok(a) if a.code == 200 => match serde_json::from_slice::<Value>(&a.body) {
                    Ok(v) => {
                        self.keys = pick(&v, &wanted(forever));
                        self.listed = Some(Instant::now());
                        self.relist = false;
                        self.keys.clone()
                    }
                    Err(_) => {
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
        self.houses.retain(|k, _| keys.contains(k));
        for key in &keys {
            if !self.houses.contains_key(key) {
                if let Some(c) = load_cached(key, now) {
                    self.houses.insert(key.clone(), c);
                }
            }
        }
        // which houses to fetch: those whose version moved (every house, each with its ETag, on an older server)
        let ask: BTreeSet<String> = match &error {
            None if keys.is_empty() => BTreeSet::new(),
            None => {
                let try_versions = self.no_versions_until.is_none_or(|t| Instant::now() >= t);
                match if try_versions { fetch_versions(&self.http, &self.server, &keys) } else { Ok(None) } {
                    Ok(Some(v)) => {
                        self.no_versions_until = None;
                        moved(&v, &self.houses)
                    }
                    Ok(None) => {
                        if try_versions {
                            config::log("price feed: the server has no versions check yet; asking each house with its ETag");
                            self.no_versions_until = Some(Instant::now() + LIST_EVERY);
                        }
                        keys.iter().cloned().collect()
                    }
                    Err(e) => {
                        error = Some(e);
                        BTreeSet::new()
                    }
                }
            }
            // the list failed: only houses we have nothing of, and only when the server isn't the problem
            Some(e) if !e.stop => keys.iter().filter(|k| !self.houses.contains_key(*k)).cloned().collect(),
            Some(_) => BTreeSet::new(),
        };
        let mut changed = false;
        for key in keys.iter().filter(|k| ask.contains(*k)) {
            let etag = self.houses.get(key).and_then(|h| h.1.clone());
            match fetch_house(&self.http, &self.server, key, etag.as_deref(), now) {
                Ok(Got::New(h, tag)) => {
                    config::log(&format!("price feed: {} scan of {} ({} rows)", h.key, chrono::DateTime::from_timestamp(h.at, 0).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default(), h.rows.len()));
                    // the checked house, for the next start (a 304 needs it)
                    save_cached(key, &body_of(&h), tag.as_deref());
                    self.houses.insert(key.clone(), (h, tag));
                    changed = true;
                }
                Ok(Got::Same) => {}
                Ok(Got::Gone) => {
                    changed |= self.houses.remove(key).is_some();
                }
                Err(e) => {
                    let stop = e.stop;
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
        match write {
            Err(e) => {
                config::log(&format!("price feed: can't write {FOLDER}: {e}"));
                error.get_or_insert_with(|| Fail::new(format!("can't write the data addon ({e})")));
            }
            Ok(true) => {
                wrote = true;
                config::log(&format!("price feed: wrote {FOLDER} ({} houses)", list.len()));
            }
            Ok(false) => {}
        }
        let houses = list.iter().map(|h| HouseView { key: h.key.clone(), realm: h.realm.clone(), side: h.side.clone(), at: h.at }).collect();
        let (wait, note) = match error {
            Some(e) => {
                self.failures += 1;
                let wait = next_wait(self.failures, e.retry_after, jitter());
                self.hold_until = e.retry_after.map(|r| Instant::now() + r.min(MAX_RETRY_AFTER));
                config::log(&format!("price feed: {}; trying again in {}", e.msg, words(wait)));
                crate::report::note_error("price feed", &format!("Price Feed: {}", e.msg));
                (wait, Some(format!("Last check failed: {}", e.msg)))
            }
            None => {
                self.failures = 0;
                self.hold_until = None;
                if !changed && !wrote {
                    // an unchanged check logs nothing but a summary: the first one, then one every QUIET_LOG
                    self.quiet += 1;
                    if self.quiet_logged.is_none_or(|t| t.elapsed() >= QUIET_LOG) {
                        config::log(&format!("price feed: no new scans ({} checks of {} houses since the last summary; the next in 15 min)", self.quiet, list.len()));
                        self.quiet = 0;
                        self.quiet_logged = Some(Instant::now());
                    }
                }
                let note = list.is_empty().then(|| "No one has uploaded a scan of your auction houses yet".to_string());
                (next_wait(0, None, jitter()), note)
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
                } else if path == "/api/v1/auctions" {
                    let list = json!({ "houses": [{ "key": "EU-4-Alliance", "realm": "CAMWOW EU5 Realm 4 PvP", "realm_id": 4, "side": "Alliance" }] });
                    (200, String::new(), list.to_string().into_bytes())
                } else if path == "/api/v1/auctions/versions?houses=EU-4-Alliance" && f.versions {
                    (200, String::new(), json!({ "EU-4-Alliance": tag_version(&f.etag) }).to_string().into_bytes())
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

    /// The modified times of every file in the folder: the same after a check means nothing was written.
    fn stamps(dir: &Path) -> Vec<(String, std::time::SystemTime)> {
        let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.metadata().unwrap().modified().unwrap())).collect();
        v.sort();
        v
    }

    #[test]
    fn a_minute_check_asks_versions_and_writes_nothing_when_unchanged() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (root, addons) = game("feed");
        std::fs::create_dir_all(root.join("WTF").join("Account").join("ACC").join("CAMWOW EU5 Realm 4 PvP")).unwrap();
        let _ = std::fs::remove_file(cache_path("EU-4-Alliance"));
        let body1 = body(1_795_000_000).to_string().into_bytes();
        let fake = std::sync::Arc::new(Mutex::new(Fake { versions: true, etag: "W/\"c1-1795000000-1-20777\"".into(), body: body1, busy: None, log: vec![] }));
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
        assert!(view.note.as_deref().is_some_and(|n| n.contains("busy")) && view.houses.len() == 1);
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
}
