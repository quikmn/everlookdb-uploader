// Disenchant odds measured by players: the disenchants EverlookDB Auctions logs (`deLog` in EverlookDBAuctions.lua,
// and the older DisenchantValue addon's log when it is there) are sent to everlookdb.com, which adds up everyone's;
// the Price Feed (shared.rs) brings the result back into the game as DEOdds.lua (`EverlookDB_DEOdds`).
//
// What is sent (POST /api/v1/crowd/disenchant, JSON, in batches of BATCH): an install token (random, made once on
// this PC; not the account, not a person), and per disenchant only the item, its item level, quality and class, the
// materials and their counts, the day it happened (rounded down to the day) and an entry id. The entry id is a hash
// of the entry with a secret that never leaves this PC, so the server can drop a copy it already has without
// learning the exact time. No names, realms, GUIDs, zones or account folders, ever.
//
// What was sent: per SavedVariables file, the time of the newest disenchant the server took (`de_sent` in
// config.json). Only newer ones are sent; a batch the server did not take is sent again later.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Disenchants per request.
pub const BATCH: usize = 1000;
/// Entries read from one log at most (the addons cap theirs at 10,000).
const MAX_ENTRIES: usize = 50_000;
/// The global the Price Feed writes, and its file in the data addon.
pub const ODDS_FILE: &str = "DEOdds.lua";
/// The fewest disenchants a kind or item needs to be written.
pub const MIN_N: u64 = 5;
const MAX_ODDS_ROWS: usize = 20_000;

/// One disenchant as the addon logged it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Entry {
    pub t: i64,
    pub item: u32,
    pub ilvl: u32,
    pub q: u8,
    pub cls: u8,
    pub mats: BTreeMap<u32, u32>,
}

// ---------------------------------------------------------------------------------------------------------------
// A small reader for the Lua that WoW writes SavedVariables in. Nothing is run: tables, strings, numbers, booleans
// and nil only. Parts we don't need are skipped without being kept, so a 30 MB auctions file costs little.

#[derive(Clone, Debug, PartialEq)]
pub enum Lua {
    Nil,
    Bool(bool),
    Num(f64),
    Str(String),
    Table(Vec<(Key, Lua)>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Int(i64),
    Str(String),
}

impl Lua {
    pub fn get(&self, key: &str) -> Option<&Lua> {
        match self {
            Lua::Table(t) => t.iter().rev().find(|(k, _)| matches!(k, Key::Str(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
    fn num(&self) -> Option<f64> {
        if let Lua::Num(n) = self { Some(*n) } else { None }
    }
    fn str(&self) -> Option<&str> {
        if let Lua::Str(s) = self { Some(s) } else { None }
    }
    fn entries(&self) -> &[(Key, Lua)] {
        if let Lua::Table(t) = self { t } else { &[] }
    }
}

const MAX_DEPTH: usize = 64;

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.b[self.i.min(self.b.len())..].starts_with(b"--") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.i += 1;
                }
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Option<&'a str> {
        self.ws();
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        (!s.is_empty() && !s.as_bytes()[0].is_ascii_digit()).then_some(s)
    }

    /// A quoted string; `keep` false skips it.
    fn string(&mut self, keep: bool) -> Option<String> {
        let q = self.peek()?;
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'a' => 7,
                        b'b' => 8,
                        b'f' => 12,
                        b'v' => 11,
                        b'0'..=b'9' => {
                            let mut n = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(d @ b'0'..=b'9') => {
                                        n = n * 10 + (d - b'0') as u32;
                                        self.i += 1;
                                    }
                                    _ => break,
                                }
                            }
                            n.min(255) as u8
                        }
                        other => other,
                    };
                    if keep {
                        out.push(ch);
                    }
                }
                c if c == q => break,
                c => {
                    if keep {
                        out.push(c);
                    }
                }
            }
        }
        Some(if keep { String::from_utf8_lossy(&out).into_owned() } else { String::new() })
    }

    fn number(&mut self) -> Option<f64> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'.' || ((c == b'-' || c == b'+') && matches!(self.b[self.i - 1], b'e' | b'E'))) {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            return i64::from_str_radix(h, 16).ok().map(|n| n as f64);
        }
        match s {
            "inf" => Some(f64::INFINITY),
            "-inf" => Some(f64::NEG_INFINITY),
            "nan" | "-nan" => Some(f64::NAN),
            _ => s.parse().ok(),
        }
    }

    /// A value; `keep` false skips it (and returns Nil).
    fn value(&mut self, keep: bool, depth: usize) -> Option<Lua> {
        self.ws();
        match self.peek()? {
            b'{' => self.table(keep, depth + 1),
            b'"' | b'\'' => self.string(keep).map(Lua::Str),
            c if c == b'-' || c == b'.' || c.is_ascii_digit() => self.number().map(Lua::Num),
            _ => match self.name()? {
                "true" => Some(Lua::Bool(true)),
                "false" => Some(Lua::Bool(false)),
                "nil" => Some(Lua::Nil),
                "inf" => Some(Lua::Num(f64::INFINITY)),
                "nan" => Some(Lua::Num(f64::NAN)),
                _ => None,
            },
        }
    }

    fn key(&mut self, keep: bool, depth: usize) -> Option<Key> {
        let k = self.value(keep, depth)?;
        if !self.eat(b']') || !self.eat(b'=') {
            return None;
        }
        Some(match k {
            Lua::Num(n) if n.fract() == 0.0 && n.abs() < 9e15 => Key::Int(n as i64),
            Lua::Num(n) => Key::Str(n.to_string()),
            Lua::Str(s) => Key::Str(s),
            Lua::Bool(b) => Key::Str(b.to_string()),
            _ => Key::Str(String::new()),
        })
    }

    fn table(&mut self, keep: bool, depth: usize) -> Option<Lua> {
        if depth > MAX_DEPTH || !self.eat(b'{') {
            return None;
        }
        let mut out = Vec::new();
        let mut next = 1i64;
        loop {
            if self.eat(b'}') {
                return Some(if keep { Lua::Table(out) } else { Lua::Nil });
            }
            self.ws();
            let key = if self.eat(b'[') {
                Some(self.key(keep, depth)?)
            } else {
                // `name = value`, else a positional value
                let save = self.i;
                match self.name() {
                    Some(n) if !matches!(n, "true" | "false" | "nil") && self.eat(b'=') => Some(Key::Str(n.to_string())),
                    _ => {
                        self.i = save;
                        None
                    }
                }
            };
            let v = self.value(keep, depth)?;
            if keep {
                let k = key.unwrap_or_else(|| {
                    next += 1;
                    Key::Int(next - 1)
                });
                out.push((k, v));
            }
            if !self.eat(b',') && !self.eat(b';') {
                if self.eat(b'}') {
                    return Some(if keep { Lua::Table(out) } else { Lua::Nil });
                }
                return None;
            }
        }
    }
}

/// `<global>.<key>` for the first of `globals` the file assigns, reading only that part. None when it isn't there
/// or the file can't be read up to it.
pub fn find(bytes: &[u8], globals: &[&str], key: &str) -> Option<Lua> {
    let mut r = Reader { b: bytes, i: 0 };
    loop {
        r.ws();
        if r.peek().is_none() {
            return None;
        }
        let name = r.name()?;
        if !r.eat(b'=') {
            return None;
        }
        if !globals.contains(&name) {
            r.value(false, 0)?;
            continue;
        }
        r.ws();
        if r.peek() != Some(b'{') {
            r.value(false, 0)?;
            continue;
        }
        r.i += 1;
        loop {
            if r.eat(b'}') {
                break;
            }
            r.ws();
            let k = if r.eat(b'[') {
                Some(r.key(true, 1)?)
            } else {
                let save = r.i;
                match r.name() {
                    Some(n) if r.eat(b'=') => Some(Key::Str(n.to_string())),
                    _ => {
                        r.i = save;
                        None
                    }
                }
            };
            if matches!(&k, Some(Key::Str(s)) if s == key) {
                return r.value(true, 1);
            }
            r.value(false, 1)?;
            if !r.eat(b',') && !r.eat(b';') {
                r.eat(b'}');
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The logs

/// The Auctions addon's saved globals in EverlookDBAuctions.lua: the log is in EverlookDBAuctionsDB, next to the
/// main EverlookAuctionsDB (read too, in case).
pub const AUCTIONS_GLOBALS: [&str; 2] = ["EverlookAuctionsDB", "EverlookDBAuctionsDB"];
pub const LEGACY_FILE: &str = "DisenchantValue.lua";

fn int(v: Option<&Lua>, lo: i64, hi: i64) -> Option<i64> {
    let n = v?.num()?;
    (n.fract() == 0.0 && n >= lo as f64 && n <= hi as f64).then_some(n as i64)
}

/// Materials `{ [id] = count }`: every id and count must be plausible, else the whole entry is dropped.
fn mats_of(v: Option<&Lua>) -> Option<BTreeMap<u32, u32>> {
    let mut out = BTreeMap::new();
    for (k, n) in v?.entries() {
        let Key::Int(id) = k else { return None };
        if !(1..10_000_000).contains(id) {
            return None;
        }
        let n = int(Some(n), 1, 100)?;
        out.insert(*id as u32, n as u32);
    }
    (!out.is_empty() && out.len() <= 8).then_some(out)
}

/// A disenchantable entry: weapon or armor, uncommon to epic.
fn checked(e: Entry) -> Option<Entry> {
    (matches!(e.cls, 2 | 4) && (2..=4).contains(&e.q) && e.item > 0 && (1..=1000).contains(&e.ilvl) && e.t > 1_000_000_000).then_some(e)
}

/// EverlookDB Auctions' `deLog`: `{ t, item, ilvl, q, cls, mats = { [id] = n } }`. Bad entries are skipped.
pub fn parse_delog(bytes: &[u8]) -> Vec<Entry> {
    let Some(log) = find(bytes, &AUCTIONS_GLOBALS, "deLog") else { return vec![] };
    log.entries()
        .iter()
        .filter_map(|(_, e)| {
            checked(Entry {
                t: int(e.get("t"), 0, 4_000_000_000)?,
                item: int(e.get("item"), 1, 10_000_000)? as u32,
                ilvl: int(e.get("ilvl"), 1, 1000)? as u32,
                q: int(e.get("q"), 0, 7)? as u8,
                cls: int(e.get("cls"), 0, 20)? as u8,
                mats: mats_of(e.get("mats"))?,
            })
        })
        .take(MAX_ENTRIES)
        .collect()
}

/// The link's colour as a quality, for an item the old addon logged before the game had it cached.
fn quality_of_link(link: &str) -> Option<u8> {
    let i = link.find("|c")?;
    let hex = link.get(i + 4..i + 10)?.to_ascii_lowercase();
    Some(match hex.as_str() {
        "1eff00" => 2,
        "0070dd" => 3,
        "a335ee" => 4,
        "9d9d9d" => 0,
        "ffffff" => 1,
        "ff8000" => 5,
        _ => return None,
    })
}

/// The DisenchantValue addon's log: `log = { { t, char, itemID, link, ilvl, mats } }`, with the quality and class
/// from its `items` table. The character name, realm and link are never kept.
pub fn parse_legacy(bytes: &[u8]) -> Vec<Entry> {
    let (Some(log), items) = (find(bytes, &["DisenchantValueDB"], "log"), find(bytes, &["DisenchantValueDB"], "items")) else { return vec![] };
    let item_info = |id: i64| -> (Option<i64>, Option<i64>) {
        let it = items.as_ref().and_then(|t| t.entries().iter().find(|(k, _)| *k == Key::Int(id)).map(|(_, v)| v));
        (int(it.and_then(|i| i.get("quality")), 0, 7), int(it.and_then(|i| i.get("classID")), 0, 20))
    };
    log.entries()
        .iter()
        .filter_map(|(_, e)| {
            let item = int(e.get("itemID"), 1, 10_000_000)?;
            let (q, cls) = item_info(item);
            let q = q.map(|q| q as u8).or_else(|| quality_of_link(e.get("link")?.str()?))?;
            checked(Entry {
                t: int(e.get("t"), 0, 4_000_000_000)?,
                item: item as u32,
                ilvl: int(e.get("ilvl"), 1, 1000)? as u32,
                q,
                cls: cls? as u8,
                mats: mats_of(e.get("mats"))?,
            })
        })
        .take(MAX_ENTRIES)
        .collect()
}

/// Both logs as one, oldest first: an old-addon entry that EverlookDB Auctions logged too (same item and materials
/// within 5 seconds) counts once.
pub fn merge(mut ours: Vec<Entry>, legacy: Vec<Entry>) -> Vec<Entry> {
    let mut by_item: std::collections::HashMap<u32, Vec<usize>> = std::collections::HashMap::new();
    for (i, e) in ours.iter().enumerate() {
        by_item.entry(e.item).or_default().push(i);
    }
    let extra: Vec<Entry> = legacy
        .into_iter()
        .filter(|l| !by_item.get(&l.item).is_some_and(|is| is.iter().any(|&i| ours[i].mats == l.mats && (ours[i].t - l.t).abs() <= 5)))
        .collect();
    ours.extend(extra);
    ours.sort_by_key(|e| e.t);
    ours
}

// ---------------------------------------------------------------------------------------------------------------
// Sending

/// The disenchants newer than `mark` (the newest one the server took), oldest first.
pub fn pending(entries: &[Entry], mark: i64) -> Vec<Entry> {
    let mut out: Vec<Entry> = entries.iter().filter(|e| e.t > mark).cloned().collect();
    out.sort_by_key(|e| e.t);
    out
}

/// Batches of about `max`, never splitting entries of the same second (the mark moves by whole seconds).
pub fn batches(list: &[Entry], max: usize) -> Vec<&[Entry]> {
    let mut out = vec![];
    let mut start = 0;
    while start < list.len() {
        let mut end = (start + max.max(1)).min(list.len());
        while end < list.len() && list[end].t == list[end - 1].t {
            end += 1;
        }
        out.push(&list[start..end]);
        start = end;
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A new local secret: random from the OS (std's hasher keys) and the clock.
pub fn new_secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = Sha256::new();
    for i in 0..4u64 {
        let mut s = std::collections::hash_map::RandomState::new().build_hasher();
        s.write_u64(i);
        s.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.update(s.finish().to_le_bytes());
    }
    h.update(std::process::id().to_le_bytes());
    hex(&h.finalize())
}

/// The install token the server sees: derived from the secret, which can't be had back from it.
pub fn install_of(secret: &str) -> String {
    hex(&Sha256::digest(format!("install|{secret}").as_bytes()))[..32].to_string()
}

/// The entry's id: the same disenchant always gets the same id, and without the secret the exact time can't be found
/// from it.
pub fn entry_id(secret: &str, e: &Entry) -> String {
    let mats: Vec<String> = e.mats.iter().map(|(id, n)| format!("{id}:{n}")).collect();
    let s = format!("{secret}|{}|{}|{}|{}|{}|{}", e.t, e.item, e.ilvl, e.q, e.cls, mats.join(","));
    hex(&Sha256::digest(s.as_bytes()))[..32].to_string()
}

pub fn day(t: i64) -> i64 {
    t.div_euclid(86_400) * 86_400
}

/// One request's body. Everything in it is listed here.
pub fn body(secret: &str, entries: &[Entry]) -> Value {
    let list: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mats: serde_json::Map<String, Value> = e.mats.iter().map(|(id, n)| (id.to_string(), json!(n))).collect();
            json!({ "id": entry_id(secret, e), "t": day(e.t), "item": e.item, "ilvl": e.ilvl, "q": e.q, "cls": e.cls, "mats": mats })
        })
        .collect();
    json!({ "v": 1, "install": install_of(secret), "entries": list })
}

pub fn url(server: &str) -> String {
    format!("{}/api/v1/crowd/disenchant", server.trim_end_matches('/'))
}

#[derive(Debug, PartialEq)]
pub enum Sent {
    /// the server took the batch: (new, already had, refused)
    Ok(u64, u64, u64),
    /// try again later (no connection, busy, server error, or a server without the endpoint yet), and why
    Later(crate::health::Trouble),
}

pub fn send(http: &reqwest::blocking::Client, server: &str, body: &Value) -> Sent {
    let resp = match http.post(url(server)).header("content-type", "application/json").body(body.to_string()).send() {
        Ok(r) => r,
        Err(e) => return Sent::Later(crate::health::Trouble::Unreachable(if e.is_timeout() { "timed out".into() } else { "can't reach the server".into() })),
    };
    let code = resp.status().as_u16();
    let v: Value = resp.bytes().ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    match code {
        200..=299 => Sent::Ok(v["accepted"].as_u64().unwrap_or(0), v["known"].as_u64().unwrap_or(0), v["rejected"].as_u64().unwrap_or(0)),
        _ => Sent::Later(crate::health::Trouble::of_status(code, v["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("the server answered {code}")))),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The odds, for the Price Feed

fn odds_mats(v: &Value) -> Option<String> {
    let mut parts = vec![];
    for (id, m) in v.as_object()? {
        let Ok(id) = id.parse::<u32>() else { continue };
        if !(1..10_000_000).contains(&id) {
            continue;
        }
        let (Some(p), Some(c)) = (m["p"].as_f64(), m["c"].as_f64()) else { continue };
        if !(p.is_finite() && (0.0..=1.0).contains(&p) && c.is_finite() && (1.0..=100.0).contains(&c)) {
            continue;
        }
        parts.push(format!("[{id}] = {{ p = {}, c = {} }}", num(p, 4), num(c, 2)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// A number with at most `places` decimals, no trailing zeros ("0.25", "1").
fn num(x: f64, places: usize) -> String {
    let s = format!("{x:.places$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

/// A kind key "<cls>:<q>:<ilvl>" as the server sends it, checked.
fn kind_ok(k: &str) -> bool {
    let p: Vec<&str> = k.split(':').collect();
    p.len() == 3
        && matches!(p[0], "2" | "4")
        && matches!(p[1], "2" | "3" | "4")
        && p[2].parse::<u32>().is_ok_and(|n| (1..=1000).contains(&n))
        && !p[2].starts_with('0')
}

/// DEOdds.lua from the server's answer (GET /api/v1/crowd/disenchant): `EverlookDB_DEOdds = { v = 1, updated,
/// kinds = { ["cls:q:ilvl"] = { n, mats = { [id] = { p, c } } } }, items = { [id] = { n, mats } } }`. Kinds and
/// items with fewer than MIN_N disenchants are left out; every value is checked. None: not an answer we can use.
pub fn odds_lua(v: &Value, mark: &str) -> Option<String> {
    if v["v"].as_u64() != Some(1) {
        return None;
    }
    let updated = v["updated"].as_i64().filter(|t| (0..10_000_000_000).contains(t)).unwrap_or(0);
    // (n, "<table>[<key>] = { n = .., mats = { .. } }")
    let rows = |o: &Value, table: &str, key_ok: &dyn Fn(&str) -> Option<String>| -> Vec<(u64, String)> {
        let mut out: Vec<(u64, String)> = o
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, r)| {
                let key = key_ok(k)?;
                let n = r["n"].as_u64().filter(|n| *n >= MIN_N && *n < 1_000_000_000)?;
                Some((n, format!("{table}[{key}] = {{ n = {n}, mats = {{ {} }} }}\n", odds_mats(&r["mats"])?)))
            })
            .collect();
        // the best measured first when there are too many
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out.truncate(MAX_ODDS_ROWS);
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    };
    let kinds = rows(&v["kinds"], "k", &|k| kind_ok(k).then(|| format!("\"{k}\"")));
    let items = rows(&v["items"], "i", &|k| k.parse::<u32>().ok().filter(|i| (1..10_000_000).contains(i)).map(|i| i.to_string()));
    let mut s = format!("{mark}: disenchant odds measured by players, from everlookdb.com.\n");
    s.push_str(&format!("EverlookDB_DEOdds = {{ v = 1, updated = {updated}, kinds = {{}}, items = {{}} }}\nlocal k, i = EverlookDB_DEOdds.kinds, EverlookDB_DEOdds.items\n"));
    // one table per statement, so no single expression gets large
    for (_, r) in kinds.into_iter().chain(items) {
        s.push_str(&r);
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUCTIONS: &str = "\r\nEverlookAuctionsDB = {\r\n[\"houses\"] = {\r\n[\"Realm-Alliance\"] = {\r\n[\"rows\"] = {\r\n[\"1:0\"] = \"a,b,{c}\",\r\n},\r\n},\r\n},\r\n[\"deLog\"] = {\r\n{\r\n[\"t\"] = 1795280293,\r\n[\"item\"] = 15347,\r\n[\"ilvl\"] = 25,\r\n[\"q\"] = 2,\r\n[\"cls\"] = 4,\r\n[\"mats\"] = {\r\n[10940] = 3,\r\n},\r\n}, -- [1]\r\n{\r\n[\"t\"] = 1795280400,\r\n[\"item\"] = 15348,\r\n[\"ilvl\"] = 30,\r\n[\"q\"] = 3,\r\n[\"cls\"] = 2,\r\n[\"mats\"] = {\r\n[11084] = 1,\r\n},\r\n}, -- [2]\r\n{\r\n[\"t\"] = 1795280500,\r\n[\"item\"] = 2589,\r\n[\"ilvl\"] = 5,\r\n[\"q\"] = 1,\r\n[\"cls\"] = 7,\r\n[\"mats\"] = {\r\n[10940] = 1,\r\n},\r\n}, -- [3]\r\n},\r\n[\"fixedLiveUnit2\"] = true,\r\n}\r\nEverlookAuctionsCharDB = nil\r\n";

    #[test]
    fn reads_the_delog_and_skips_what_is_not_a_disenchant() {
        let e = parse_delog(AUCTIONS.as_bytes());
        assert_eq!(e.len(), 2, "cloth (class 7, common) is not a disenchant");
        assert_eq!(e[0], Entry { t: 1795280293, item: 15347, ilvl: 25, q: 2, cls: 4, mats: BTreeMap::from([(10940, 3)]) });
        assert_eq!(e[1].mats, BTreeMap::from([(11084, 1)]));
        // the other global name, and a file without a log
        let other = AUCTIONS.replace("EverlookAuctionsDB", "EverlookDBAuctionsDB");
        assert_eq!(parse_delog(other.as_bytes()).len(), 2);
        assert!(parse_delog(b"EverlookAuctionsDB = {\n[\"houses\"] = {},\n}\n").is_empty());
        assert!(parse_delog(b"").is_empty());
        // as the addon writes it: the log in a second global after the main one
        let two = "
EverlookAuctionsDB = {
[\"houses\"] = {
[\"R-Alliance\"] = { [\"rows\"] = { [\"1:0\"] = \"a,{b}\" } },
},
[\"fixedLiveUnit2\"] = true,
}
EverlookDBAuctionsDB = {
[\"deLog\"] = {
{
[\"t\"] = 1795280293,
[\"item\"] = 15347,
[\"ilvl\"] = 25,
[\"q\"] = 2,
[\"cls\"] = 4,
[\"mats\"] = {
[10940] = 3,
},
}, -- [1]
},
}
";
        assert_eq!(parse_delog(two.as_bytes()).len(), 1);
        assert!(parse_delog(b"EverlookAuctionsDB = { os.exit() }").is_empty());
    }

    /// A real EverlookDBAuctions.lua (EVERLOOK_AUCTIONS_SV=<path>) with a deLog put at its end: everything before it is
    /// read through.
    #[test]
    #[ignore]
    fn reads_through_a_real_auctions_file() {
        let Ok(path) = std::env::var("EVERLOOK_AUCTIONS_SV") else { return };
        let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        let end = text.rfind('}').unwrap();
        let log = "[\"deLog\"] = { { [\"t\"] = 1795280293, [\"item\"] = 15347, [\"ilvl\"] = 25, [\"q\"] = 2, [\"cls\"] = 4, [\"mats\"] = { [10940] = 3 } } },\n";
        let with = format!("{}{log}{}", &text[..end], &text[end..]);
        let t = std::time::Instant::now();
        assert_eq!(parse_delog(with.as_bytes()).len(), 1);
        eprintln!("read {} MB in {:?}", with.len() / 1_000_000, t.elapsed());
    }

    #[test]
    fn bad_values_drop_the_entry() {
        let bad = [
            "[\"mats\"] = {[10940] = 0,}",
            "[\"mats\"] = {[10940] = 500,}",
            "[\"mats\"] = {}",
            "[\"mats\"] = {[\"x\"] = 1,}",
        ];
        for m in bad {
            let f = format!("EverlookAuctionsDB = {{ [\"deLog\"] = {{ {{ [\"t\"] = 1795280293, [\"item\"] = 1, [\"ilvl\"] = 20, [\"q\"] = 2, [\"cls\"] = 4, {m} }}, }}, }}");
            assert!(parse_delog(f.as_bytes()).is_empty(), "{m}");
        }
        let f = "EverlookAuctionsDB = { deLog = { { t = 1795280293, item = 1, ilvl = 20.5, q = 2, cls = 4, mats = { [10940] = 1 } } } }";
        assert!(parse_delog(f.as_bytes()).is_empty(), "a fractional item level");
        let f = "EverlookAuctionsDB = { deLog = { { t = 1795280293, item = 1, ilvl = 20, q = 2, cls = 4, mats = { [10940] = 1 } } } }";
        assert_eq!(parse_delog(f.as_bytes()).len(), 1, "plain name keys read too");
    }

    #[test]
    fn reads_the_old_addon_log_without_names() {
        let f = "DisenchantValueDB = {\n[\"log\"] = {\n{\n[\"t\"] = 1795280000,\n[\"char\"] = \"Somebody\",\n[\"realmID\"] = 6946,\n[\"itemID\"] = 15347,\n[\"link\"] = \"|cff1eff00|Hitem:15347::::::::|h[Thing]|h|r\",\n[\"ilvl\"] = 25,\n[\"mats\"] = {\n[10940] = 2,\n},\n},\n{\n[\"t\"] = 1795280100,\n[\"itemID\"] = 999,\n[\"link\"] = \"|cff0070dd|Hitem:999|h[X]|h|r\",\n[\"ilvl\"] = 30,\n[\"mats\"] = {\n[11084] = 1,\n},\n},\n},\n[\"items\"] = {\n[15347] = {\n[\"quality\"] = 2,\n[\"classID\"] = 4,\n},\n},\n}\n";
        let e = parse_legacy(f.as_bytes());
        assert_eq!(e.len(), 1, "an item with no class known is skipped");
        assert_eq!(e[0], Entry { t: 1795280000, item: 15347, ilvl: 25, q: 2, cls: 4, mats: BTreeMap::from([(10940, 2)]) });
        // the same disenchant in both logs counts once
        let ours = vec![Entry { t: 1795280002, ..e[0].clone() }];
        assert_eq!(merge(ours.clone(), e.clone()).len(), 1);
        let other = vec![Entry { t: 1795290000, ..e[0].clone() }];
        assert_eq!(merge(other, e).len(), 2);
    }

    fn at(t: i64) -> Entry {
        Entry { t, item: 1, ilvl: 20, q: 2, cls: 4, mats: BTreeMap::from([(10940, 1)]) }
    }

    #[test]
    fn only_new_disenchants_are_sent_in_whole_seconds() {
        let list: Vec<Entry> = [5, 1, 2, 3, 3, 3, 4].iter().map(|t| at(*t)).collect();
        let p = pending(&list, 1);
        assert_eq!(p.iter().map(|e| e.t).collect::<Vec<_>>(), [2, 3, 3, 3, 4, 5]);
        let b = batches(&p, 2);
        assert_eq!(b.iter().map(|b| b.len()).collect::<Vec<_>>(), [4, 2], "the 3s stay together");
        assert!(pending(&list, 5).is_empty());
        assert!(batches(&[], 10).is_empty());
    }

    #[test]
    fn the_body_has_the_day_and_no_exact_time() {
        let secret = "s3cret";
        let e = Entry { t: 1795280293, item: 15347, ilvl: 25, q: 2, cls: 4, mats: BTreeMap::from([(10940, 3)]) };
        let b = body(secret, &[e.clone()]);
        let x = &b["entries"][0];
        assert_eq!(x["t"], 1795219200);
        assert_eq!(x["mats"]["10940"], 3);
        assert_eq!(b["install"].as_str().unwrap().len(), 32);
        assert!(!b.to_string().contains(secret) && !b.to_string().contains("1795280293"));
        let keys: Vec<&String> = x.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["cls", "id", "ilvl", "item", "mats", "q", "t"]);
        assert_eq!(entry_id(secret, &e), x["id"].as_str().unwrap(), "the same entry, the same id");
        assert_ne!(entry_id("other", &e), entry_id(secret, &e));
        assert_ne!(entry_id(secret, &Entry { t: e.t + 1, ..e.clone() }), entry_id(secret, &e));
        assert_ne!(new_secret(), new_secret());
    }

    #[test]
    fn writes_the_odds_as_plain_lua_and_leaves_out_small_and_bad_rows() {
        let v = json!({ "v": 1, "updated": 1795280000,
            "kinds": {
                "4:2:25": { "n": 40, "mats": { "10940": { "p": 0.75, "c": 2.4 }, "10938": { "p": 0.2, "c": 1.0 } } },
                "4:2:26": { "n": 4, "mats": { "10940": { "p": 1.0, "c": 2.0 } } },
                "9:2:26": { "n": 40, "mats": { "10940": { "p": 1.0, "c": 2.0 } } },
                "4:2:27\"]=os.exit()--": { "n": 40, "mats": { "10940": { "p": 1.0, "c": 2.0 } } },
                "2:3:30": { "n": 10, "mats": { "11084": { "p": 2.0, "c": 1.0 } } }
            },
            "items": {
                "15347": { "n": 12, "mats": { "10940": { "p": 0.833333, "c": 2.1 } } },
                "x": { "n": 12, "mats": { "10940": { "p": 0.5, "c": 2.0 } } }
            } });
        let lua = odds_lua(&v, "-- MARK").unwrap();
        assert!(lua.starts_with("-- MARK: "));
        assert!(lua.contains("EverlookDB_DEOdds = { v = 1, updated = 1795280000, kinds = {}, items = {} }\n"));
        assert!(lua.contains("k[\"4:2:25\"] = { n = 40, mats = { [10938] = { p = 0.2, c = 1 }, [10940] = { p = 0.75, c = 2.4 } } }\n"), "{lua}");
        assert!(lua.contains("i[15347] = { n = 12, mats = { [10940] = { p = 0.8333, c = 2.1 } } }\n"), "{lua}");
        for gone in ["4:2:26", "9:2:26", "os.exit", "2:3:30", "[x]"] {
            assert!(!lua.contains(gone), "{gone}");
        }
        assert_eq!(lua.lines().count(), 5);
        assert!(odds_lua(&json!({ "v": 2 }), "--").is_none());
        // what it wrote reads back as Lua
        let back = find(lua.as_bytes(), &["EverlookDB_DEOdds"], "v");
        assert_eq!(back, Some(Lua::Num(1.0)));
    }
}
