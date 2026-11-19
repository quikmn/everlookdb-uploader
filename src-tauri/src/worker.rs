// Watches every WTF/Account/*/SavedVariables/EverlookDB.lua and EverlookDBAuctions.lua and uploads each once the game has finished
// writing it. A cheap poll (every few seconds, file size and time only) instead of a file watcher: the
// game rewrites the file whole on logout or /reload, so this catches every save with no extra crates.
//
// A failed upload is never dropped: it stays on screen (amber or red, with the server's words) and is sent again,
// at once when the file changes, otherwise after 1, 5, 15 and 30 minutes and then every hour for as long as it
// keeps failing (a refused file: every hour), and right away when the app starts.
//
// Privacy: the request body is the file's bytes (gzip-compressed for auction files) and nothing else. The file
// path (which holds the account folder name) is only used locally; the window and the log name a file by what it
// is ("Auctions", "Game data") and, with several accounts, by number, never by the account's name.

use crate::config::{self, Config, Done, FileState, Receipt};
use crate::{giveaway, wow};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// An upload is being sent right now (the app's self-update waits for it).
pub static UPLOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn uploading() -> bool {
    UPLOADING.load(std::sync::atomic::Ordering::SeqCst)
}

/// EverlookDB.lua is capped at 5 MB by the server. Auction files are sent gzip-compressed (a 30 MB scan file is a
/// few MB on the wire), so their cap is on the file as it is on disk.
pub const MAX_UPLOAD: u64 = 5 * 1024 * 1024;
pub const MAX_AUCTIONS_UPLOAD: u64 = 64 * 1024 * 1024;
const AUCTIONS: &str = "EverlookDBAuctions.lua";

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn is_auctions(path: &Path) -> bool {
    file_name(path) == AUCTIONS
}

fn max_for(path: &Path) -> u64 {
    if is_auctions(path) { MAX_AUCTIONS_UPLOAD } else { MAX_UPLOAD }
}
const POLL: Duration = Duration::from_secs(4);
/// the file must be this old and unchanged since the last poll before it is read
const SETTLE: Duration = Duration::from_secs(3);

pub enum Cmd {
    /// look now (settings changed)
    Wake,
    /// send every file now, even unchanged ones
    UploadNow,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Idle,
    Ok,
    Busy,
    Error,
    Paused,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub kind: Kind,
    pub line: String,
    pub files: usize,
    /// one row per watched file, for the window
    pub views: Vec<FileView>,
}

/// One watched file as the window shows it. No paths, no account names.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct FileView {
    /// "Auctions", "Game data"; "Auctions, account 2" when there are several accounts
    pub label: String,
    /// "ok", "partial", "retry", "failed" or "idle"
    pub level: String,
    pub headline: String,
    pub details: Vec<String>,
    /// when the last answer came ("14:05", or "16 Nov 14:05" on another day)
    pub when: Option<String>,
    /// "Trying again at 15:05" while it keeps failing
    pub next: Option<String>,
}

pub struct Shared {
    pub cfg: Mutex<Config>,
    pub server: String,
}

pub fn upload_url(server: &str) -> String {
    format!("{}/api/v1/upload", server.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------------------------------------------
// The server's answer

/// One auction scan in the server's answer (new servers only).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Scan {
    pub house: String,
    pub realm: Option<String>,
    pub side: Option<String>,
    pub at: Option<i64>,
    /// "accepted", "known" or "rejected"
    pub result: String,
    pub reason: Option<String>,
    pub rows: u64,
    /// (why, how many rows), e.g. ("unknown_item", 12)
    pub skipped: Vec<(String, u64)>,
}

/// A 200 answer. Old servers send only kind/status/accepted/known/houses; everything else is optional.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Answer {
    /// "auctions", or empty for EverlookDB.lua (the crowd data)
    pub kind: String,
    pub status: String,
    pub accepted: u64,
    pub known: u64,
    pub rejected: u64,
    pub skipped_rows: u64,
    pub message: Option<String>,
    pub scans: Vec<Scan>,
    /// the server's receipt for this upload (8 Crockford base32 characters), for the giveaway code
    pub receipt: Option<String>,
    /// auctions: the upload held a full auction house scan. The server's `full_scan` when it says; otherwise any
    /// scan taken or already had (the server refuses a file without a full scan).
    pub full_scan: bool,
    /// the one-sentence answer for the window and the tray
    pub text: String,
}

fn text_of(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn parse_scans(v: &Value) -> Vec<Scan> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|s| Scan {
                    house: s["house"].as_str().unwrap_or("?").to_string(),
                    realm: text_of(&s["realm"]),
                    side: text_of(&s["side"]),
                    at: s["at"].as_i64(),
                    result: s["result"].as_str().unwrap_or("").to_string(),
                    reason: text_of(&s["reason"]),
                    rows: s["rows"].as_u64().unwrap_or(0),
                    skipped: s["skipped"]
                        .as_object()
                        .map(|o| o.iter().filter_map(|(k, n)| n.as_u64().filter(|n| *n > 0).map(|n| (k.clone(), n))).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_answer(v: &Value) -> Answer {
    let num = |k: &str| v[k].as_u64().unwrap_or(0);
    let mut a = Answer {
        kind: v["kind"].as_str().unwrap_or("").to_string(),
        status: v["status"].as_str().unwrap_or("").to_string(),
        accepted: num("accepted"),
        known: num("known"),
        rejected: num("rejected"),
        skipped_rows: num("skipped_rows"),
        message: text_of(&v["message"]),
        scans: parse_scans(&v["scans"]),
        receipt: v["receipt"].as_str().and_then(giveaway::receipt),
        full_scan: false,
        text: String::new(),
    };
    if a.is_auctions() {
        a.full_scan = v["full_scan"].as_bool().or_else(|| v["full"].as_bool()).unwrap_or(a.accepted + a.known > 0);
    }
    a.text = if a.is_auctions() { a.sentence() } else { summary(v) };
    a
}

fn scans(n: u64) -> String {
    format!("{n} {}", if n == 1 { "scan" } else { "scans" })
}

impl Answer {
    pub fn is_auctions(&self) -> bool {
        self.kind == "auctions"
    }

    /// Some of it was refused or skipped.
    pub fn partial(&self) -> bool {
        self.status == "partial"
            || self.rejected > 0
            || self.skipped_rows > 0
            || self.scans.iter().any(|s| s.result == "rejected" || !s.skipped.is_empty())
    }

    /// The server's own sentence, or one made from the counts: "3 scans accepted, 1 already had".
    pub fn sentence(&self) -> String {
        if let Some(m) = &self.message {
            return m.clone();
        }
        let mut parts = Vec::new();
        if self.accepted > 0 {
            parts.push(format!("{} accepted", scans(self.accepted)));
        }
        if self.known > 0 {
            parts.push(if self.accepted > 0 { format!("{} already had", self.known) } else { format!("{} already had", scans(self.known)) });
        }
        if self.rejected > 0 {
            parts.push(format!("{} rejected", if parts.is_empty() { scans(self.rejected) } else { self.rejected.to_string() }));
        }
        if self.skipped_rows > 0 {
            parts.push(format!("{} {} skipped", self.skipped_rows, if self.skipped_rows == 1 { "row" } else { "rows" }));
        }
        if parts.is_empty() {
            return "No new scans".into();
        }
        let s = parts.join(", ");
        if self.accepted == 0 && self.rejected == 0 && self.known > 0 {
            return format!("No new scans: {s}");
        }
        s
    }

    /// Why scans were refused, one line each.
    pub fn reasons(&self) -> Vec<String> {
        rejected_lines(&self.scans)
    }
}

/// "Shadowstrike Alliance, 16 Nov 20:01: scan is older than 7 days" for each refused scan.
fn rejected_lines(scans: &[Scan]) -> Vec<String> {
    scans.iter().filter(|s| s.result == "rejected").map(|s| format!("{}: {}", scan_name(s), s.reason.as_deref().unwrap_or("refused"))).collect()
}

fn scan_name(s: &Scan) -> String {
    let house = match (&s.realm, &s.side) {
        (Some(r), Some(sd)) => format!("{r} {sd}"),
        (Some(r), None) => r.clone(),
        _ => s.house.clone(),
    };
    match s.at {
        Some(t) => format!("{house}, {}", clock(t)),
        None => house,
    }
}

/// A log line per scan: "scan EU-6946-Alliance 2026-11-16 20:01 accepted, 1200 rows, skipped unknown_item 12".
fn scan_log(s: &Scan) -> String {
    let at = s.at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.with_timezone(&chrono::Local).format(" %Y-%m-%d %H:%M").to_string());
    let mut line = format!("  scan {}{} {}, {} rows", s.house, at.unwrap_or_default(), if s.result.is_empty() { "?" } else { &s.result }, s.rows);
    if !s.skipped.is_empty() {
        let sk: Vec<String> = s.skipped.iter().map(|(k, n)| format!("{k} {n}")).collect();
        line += &format!(", skipped {}", sk.join(", "));
    }
    if let Some(r) = &s.reason {
        line += &format!(": {r}");
    }
    line
}

/// A non-200 answer's message and scans: `{ "error": { "message": "..." }, "scans": [...] }` (scans optional).
pub fn parse_refusal(v: &Value) -> (Option<String>, Vec<Scan>) {
    (text_of(&v["error"]["message"]).or_else(|| text_of(&v["message"])), parse_scans(&v["scans"]))
}

/// "Uploaded: 9 NPCs, 22 loot sources" from the server's answer to EverlookDB.lua.
fn summary(v: &Value) -> String {
    const PARTS: &[(&str, &str)] =
        &[("npcs", "NPCs"), ("loot_sources", "loot sources"), ("objects", "objects"), ("vendors", "vendors"), ("quests", "quests")];
    let a = &v["accepted"];
    let parts: Vec<String> = PARTS
        .iter()
        .filter_map(|(k, label)| a[*k].as_u64().filter(|n| *n > 0).map(|n| format!("{n} {label}")))
        .take(3)
        .collect();
    let verb = match v["status"].as_str() {
        Some("duplicate") => return "Up to date (nothing new)".into(),
        Some("updated") => "Updated",
        _ => "Uploaded",
    };
    if parts.is_empty() {
        verb.to_string()
    } else {
        format!("{verb}: {}", parts.join(", "))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sending

pub enum Outcome {
    /// the server took it (all or part of it)
    Sent(Answer),
    /// 4xx: the server refused it; tried again every hour, and at once when the file changes
    Refused { code: u16, message: String, scans: Vec<Scan> },
    /// 429: wait this long
    Busy(Duration),
    /// network or server error: try again after `backoff`
    Failed(String),
}

/// How long to wait after this many failed tries in a row: 1, 5, 15, 30 minutes, then every hour for good.
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 60,
        2 => 5 * 60,
        3 => 15 * 60,
        4 => 30 * 60,
        _ => 60 * 60,
    })
}

/// A refused file (4xx) is sent again hourly: the server may have been fixed.
pub const REFUSED_RETRY: Duration = Duration::from_secs(60 * 60);

/// The wait after a 429: what Retry-After says, kept between 5 seconds and an hour.
pub fn busy_wait(retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(30).clamp(5, 3600))
}

/// After a gzip upload was refused: was it the compression the server didn't understand (an older server)?
/// Then it is sent once more as it is on disk.
pub fn plain_fallback(code: u16, message: &str, scans: usize) -> bool {
    if code == 415 {
        return true;
    }
    if !(400..500).contains(&code) || code == 413 || code == 429 || scans > 0 {
        return false;
    }
    let m = message.to_lowercase().replace('\u{2019}', "'");
    ["doesn't look like", "does not look like", "not a savedvariables", "gzip", "encoding", "unsupported media"].iter().any(|p| m.contains(p))
}

pub fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4 + 64), flate2::Compression::default());
    e.write_all(bytes)?;
    e.finish()
}

pub fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB-Uploader/", env!("CARGO_PKG_VERSION")))
        // an uncompressed 30 MB auction file on a slow line
        .timeout(Duration::from_secs(300))
        .build()
        .expect("http client")
}

/// POST the body as it is. That is all that is sent.
fn post(http: &reqwest::blocking::Client, server: &str, body: Vec<u8>, gzipped: bool) -> Outcome {
    let mut req = http.post(upload_url(server)).header("content-type", "text/plain");
    if gzipped {
        req = req.header("content-encoding", "gzip");
    }
    let resp = match req.body(body).send() {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(if e.is_timeout() { "timed out".into() } else { "can't reach the server".into() }),
    };
    let code = resp.status().as_u16();
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
    let json: Value = resp.bytes().ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    match code {
        200..=299 => Outcome::Sent(parse_answer(&json)),
        429 => Outcome::Busy(busy_wait(retry)),
        400..=499 => {
            let (message, scans) = parse_refusal(&json);
            let message = message.unwrap_or_else(|| match code {
                413 => "the file is too large".into(),
                _ => format!("the server could not read the file ({code})"),
            });
            Outcome::Refused { code, message, scans }
        }
        _ => Outcome::Failed(format!("server error {code}")),
    }
}

/// Send one file's bytes; auction files go gzip-compressed, and once more uncompressed if the server didn't
/// understand the compressed body.
pub fn upload(http: &reqwest::blocking::Client, server: &str, bytes: &[u8], compress: bool) -> Outcome {
    if compress {
        if let Ok(gz) = gzip(bytes) {
            let out = post(http, server, gz, true);
            match &out {
                Outcome::Refused { code, message, scans } if plain_fallback(*code, message, scans.len()) => {
                    config::log(&format!("gzip upload refused ({code}: {message}); sending it uncompressed"));
                }
                _ => return out,
            }
        }
    }
    post(http, server, bytes.to_vec(), false)
}

// ---------------------------------------------------------------------------------------------------------------
// Watching

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn key(server: &str, path: &Path) -> String {
    format!("{}|{}", server.trim_end_matches('/'), path.display())
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// "14:05" today, "16 Nov 14:05" on another day (with the year in another year).
fn clock(t: i64) -> String {
    let Some(d) = chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(&chrono::Local)) else { return String::new() };
    let today = chrono::Local::now().date_naive();
    if d.date_naive() == today {
        d.format("%H:%M").to_string()
    } else if d.format("%Y").to_string() == today.format("%Y").to_string() {
        d.format("%-d %b %H:%M").to_string()
    } else {
        d.format("%-d %b %Y %H:%M").to_string()
    }
}

/// What each file is, by name and (with several accounts) by number: never the account folder's name.
fn labels(files: &[PathBuf]) -> Vec<String> {
    let account = |f: &PathBuf| f.parent().and_then(Path::parent).map(Path::to_path_buf);
    let mut accounts: Vec<PathBuf> = files.iter().filter_map(account).collect();
    accounts.sort();
    accounts.dedup();
    files
        .iter()
        .map(|f| {
            let what = if is_auctions(f) { "Auctions" } else { "Game data" };
            match accounts.iter().position(|a| Some(a) == account(f).as_ref()) {
                Some(i) if accounts.len() > 1 => format!("{what}, account {}", i + 1),
                _ => what.to_string(),
            }
        })
        .collect()
}

#[derive(Default)]
struct Track {
    sig: Option<(u64, Option<SystemTime>)>,
    /// the file as it was at the last look that read it (and sent it, or found nothing to send)
    seen: Option<(u64, Option<SystemTime>)>,
    /// while it keeps failing: when to try again (none after a start: a failure from before is tried at once)
    retry_at: Option<Instant>,
}

pub struct Watcher {
    shared: Arc<Shared>,
    http: reqwest::blocking::Client,
    tracks: HashMap<PathBuf, Track>,
    blocked_until: Option<Instant>,
}

impl Watcher {
    pub fn new(shared: Arc<Shared>) -> Watcher {
        Watcher { shared, http: client(), tracks: HashMap::new(), blocked_until: None }
    }

    /// The worker thread: poll, upload, report. `report` is called after every pass.
    pub fn run(mut self, rx: Receiver<Cmd>, mut report: impl FnMut(&Status)) {
        let mut force = false;
        loop {
            let status = self.pass(force, false);
            force = force && self.blocked_until.is_some_and(|t| t > Instant::now());
            report(&status);
            match rx.recv_timeout(POLL) {
                Ok(Cmd::UploadNow) => force = true,
                Ok(Cmd::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// One look at the files. `force`: send even unchanged files. `now`: don't wait for files to settle.
    pub fn pass(&mut self, force: bool, now: bool) -> Status {
        let (dir, paused) = {
            let c = self.shared.cfg.lock().unwrap();
            (c.wow_dir.clone(), c.paused)
        };
        let Some(dir) = dir else {
            return Status { kind: Kind::Error, line: "WoW Forever folder not set".into(), files: 0, views: Vec::new() };
        };
        let files = wow::data_files(&dir);
        let names = labels(&files);
        if paused && !force {
            return Status { kind: Kind::Paused, line: "Paused".into(), files: files.len(), views: self.views(&files, &names) };
        }
        let mut event: Option<(Kind, String)> = None;
        for (f, label) in files.iter().zip(&names) {
            if self.blocked_until.is_some_and(|t| t > Instant::now()) {
                break;
            }
            let Ok(meta) = std::fs::metadata(f) else { continue };
            let sig = (meta.len(), meta.modified().ok());
            let t = self.tracks.entry(f.clone()).or_default();
            let first = t.sig.is_none();
            let changed = t.sig != Some(sig);
            t.sig = Some(sig);
            let age = sig.1.and_then(|m| SystemTime::now().duration_since(m).ok()).unwrap_or(SETTLE);
            if !now && ((changed && !first) || age < SETTLE) {
                continue; // the game may still be writing it
            }
            let due = t.retry_at.map_or(true, |r| r <= Instant::now());
            if !force && t.seen == Some(sig) && !(t.retry_at.is_some() && due) {
                continue; // unchanged since the last look, and no retry due: no need to read it
            }
            let k = key(&self.shared.server, f);
            if meta.len() > max_for(f) {
                let known = self.shared.cfg.lock().unwrap().done.get(&k).is_some_and(|d| !d.ok && d.hash == "too-big");
                t.seen = Some(sig);
                t.retry_at = None;
                if !known || force {
                    let why = format!("{} is larger than {} MB", file_name(f), max_for(f) / 1024 / 1024);
                    let state = FileState { level: "failed".into(), headline: format!("Not sent: {why}"), at: unix_now(), ..Default::default() };
                    config::log(&format!("{label}: not sent: {why}"));
                    remember(&self.shared, &k, "too-big".into(), false, state, None);
                    event = Some((Kind::Error, format!("{label}: not sent: {why}")));
                }
                continue;
            }
            let Ok(bytes) = std::fs::read(f) else { continue }; // locked mid-write: next poll
            if bytes.is_empty() {
                continue;
            }
            let hash = sha256(&bytes);
            let last = self.shared.cfg.lock().unwrap().done.get(&k).cloned();
            let same = last.as_ref().is_some_and(|d| d.hash == hash);
            if !force && same && (last.as_ref().is_some_and(|d| d.ok) || !due) {
                t.seen = Some(sig);
                continue;
            }
            let failures = self.shared.cfg.lock().unwrap().uploads.get(&k).map_or(0, |s| s.failures);
            let at = unix_now();
            t.seen = Some(sig);
            UPLOADING.store(true, std::sync::atomic::Ordering::SeqCst);
            let outcome = upload(&self.http, &self.shared.server, &bytes, is_auctions(f));
            UPLOADING.store(false, std::sync::atomic::Ordering::SeqCst);
            match outcome {
                Outcome::Sent(ans) => {
                    t.retry_at = None;
                    let level = if ans.partial() { "partial" } else { "ok" };
                    let state = FileState { level: level.into(), headline: ans.text.clone(), details: ans.reasons(), at, next_retry: None, failures: 0 };
                    let line = if ans.is_auctions() { format!("Auctions: {}", ans.text) } else { ans.text.clone() };
                    let line = format!("{} · {line}", chrono::Local::now().format("%H:%M"));
                    config::log(&format!("{label} ({}): {}", file_name(f), ans.text));
                    for s in &ans.scans {
                        config::log(&scan_log(s));
                    }
                    if let Some(code) = &ans.receipt {
                        let auctions = is_auctions(f);
                        config::log(&format!("  receipt {code}{}", if auctions && ans.full_scan { " (full scan)" } else { "" }));
                        let r = Receipt { code: code.clone(), at, full_scan: auctions && ans.full_scan };
                        let mut c = self.shared.cfg.lock().unwrap();
                        if auctions {
                            giveaway::keep_auctions(&mut c.receipts, r);
                        } else {
                            c.receipts.collector = Some(r);
                        }
                    }
                    remember(&self.shared, &k, hash, true, state, Some(line.clone()));
                    event = Some((if level == "ok" { Kind::Ok } else { Kind::Busy }, line));
                }
                Outcome::Refused { code, message, scans } => {
                    let failures = failures + 1;
                    t.retry_at = Some(Instant::now() + REFUSED_RETRY);
                    let next = at + REFUSED_RETRY.as_secs() as i64;
                    let state = FileState {
                        level: "failed".into(),
                        headline: format!("Not accepted: {message}"),
                        details: rejected_lines(&scans),
                        at,
                        next_retry: Some(next),
                        failures,
                    };
                    config::log(&format!("{label} ({}) not accepted ({code}): {message}; trying again at {}", file_name(f), clock(next)));
                    for s in &scans {
                        config::log(&scan_log(s));
                    }
                    remember(&self.shared, &k, hash, false, state, None);
                    event = Some((Kind::Error, format!("{label} not accepted: {message}")));
                }
                Outcome::Busy(wait) => {
                    let failures = failures + 1;
                    self.blocked_until = Some(Instant::now() + wait);
                    t.retry_at = Some(Instant::now() + wait);
                    let next = at + wait.as_secs() as i64;
                    let state = FileState {
                        level: if failures >= 2 { "failed" } else { "retry" }.into(),
                        headline: "The server is busy".into(),
                        details: Vec::new(),
                        at,
                        next_retry: Some(next),
                        failures,
                    };
                    config::log(&format!("{label} ({}): server busy, trying again at {}", file_name(f), clock(next)));
                    remember(&self.shared, &k, hash, false, state, None);
                    event = Some((Kind::Busy, format!("Server busy, trying again in {} s", wait.as_secs())));
                }
                Outcome::Failed(why) => {
                    let failures = failures + 1;
                    let wait = backoff(failures);
                    t.retry_at = Some(Instant::now() + wait);
                    let next = at + wait.as_secs() as i64;
                    let state = FileState {
                        level: if failures >= 2 { "failed" } else { "retry" }.into(),
                        headline: format!("Upload failed: {why}"),
                        details: Vec::new(),
                        at,
                        next_retry: Some(next),
                        failures,
                    };
                    config::log(&format!("{label} ({}): upload failed ({why}), try {failures}; trying again at {}", file_name(f), clock(next)));
                    remember(&self.shared, &k, hash, false, state, None);
                    let kind = if failures >= 2 { Kind::Error } else { Kind::Busy };
                    event = Some((kind, format!("{label}: upload failed ({why}), trying again at {}", clock(next))));
                }
            }
        }
        let views = self.views(&files, &names);
        if let Some((kind, line)) = event {
            return Status { kind, line, files: files.len(), views };
        }
        // nothing sent this time: the worst file decides the tray's line
        let worst = |lvl: &str| views.iter().find(|v| v.level == lvl);
        if let Some(v) = worst("failed").or_else(|| worst("retry")) {
            let kind = if v.level == "failed" { Kind::Error } else { Kind::Busy };
            let line = format!("{}: {}", v.label, v.headline);
            return Status { kind, line, files: files.len(), views };
        }
        let last = self.shared.cfg.lock().unwrap().last_status.clone();
        let partial = views.iter().any(|v| v.level == "partial");
        match (files.is_empty(), last) {
            (true, _) => Status { kind: Kind::Idle, line: "Waiting for the addons' files (log out or /reload in game)".into(), files: 0, views },
            (false, Some(l)) => Status { kind: if partial { Kind::Busy } else { Kind::Ok }, line: l, files: files.len(), views },
            (false, None) => Status { kind: Kind::Idle, line: "Watching for changes".into(), files: files.len(), views },
        }
    }

    /// The window's rows: each file's last answer.
    fn views(&self, files: &[PathBuf], names: &[String]) -> Vec<FileView> {
        let c = self.shared.cfg.lock().unwrap();
        let now = unix_now();
        files
            .iter()
            .zip(names)
            .map(|(f, label)| {
                let k = key(&self.shared.server, f);
                match c.uploads.get(&k) {
                    Some(s) => FileView {
                        label: label.clone(),
                        level: s.level.clone(),
                        headline: s.headline.clone(),
                        details: s.details.clone(),
                        when: (s.at > 0).then(|| clock(s.at)),
                        next: s.next_retry.map(|t| if t > now { format!("Trying again at {}", clock(t)) } else { "Trying again now".into() }),
                    },
                    None => FileView {
                        label: label.clone(),
                        level: "idle".into(),
                        headline: match c.done.get(&k) {
                            Some(d) if d.ok => "Sent".into(),
                            Some(_) => "Sending again".into(),
                            None => "Waiting for the next save".into(),
                        },
                        ..Default::default()
                    },
                }
            })
            .collect()
    }
}

fn remember(shared: &Shared, key: &str, hash: String, ok: bool, state: FileState, line: Option<String>) {
    let mut c = shared.cfg.lock().unwrap();
    c.done.insert(key.to_string(), Done { hash, ok });
    c.uploads.insert(key.to_string(), state);
    if line.is_some() {
        c.last_status = line;
    }
    c.save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_auction_answer() {
        let a = parse_answer(&json!({"kind":"auctions","status":"added","accepted":3,"known":1,"houses":[{"realm":"R","side":"Alliance","auctions":10}]}));
        assert!(a.is_auctions());
        assert!(!a.partial());
        assert_eq!(a.text, "3 scans accepted, 1 already had");
        assert!(a.reasons().is_empty());
        let dup = parse_answer(&json!({"kind":"auctions","status":"duplicate","accepted":0,"known":2,"houses":[]}));
        assert_eq!(dup.text, "No new scans: 2 scans already had");
        assert!(!dup.partial());
        assert_eq!(parse_answer(&json!({"kind":"auctions","status":"added","accepted":1,"known":0})).text, "1 scan accepted");
        assert_eq!(parse_answer(&json!({"kind":"auctions","status":"duplicate"})).text, "No new scans");
    }

    #[test]
    fn new_auction_answer() {
        let v = json!({
            "kind":"auctions","status":"partial","accepted":3,"known":0,"rejected":1,"skipped_rows":12,
            "message":"3 scans accepted, 12 rows skipped (unknown items)","houses":[],
            "scans":[
                {"house":"EU-6946-Alliance","realm":"Shadowstrike","side":"Alliance","at":1763000000,"result":"accepted","rows":900,
                 "skipped":{"unknown_item":12,"joke_price":0,"bad_row":0}},
                {"house":"EU-6946-Horde","realm":"Shadowstrike","side":"Horde","at":1763000100,"result":"rejected","reason":"scan is from the future","rows":0}
            ]
        });
        let a = parse_answer(&v);
        assert!(a.partial());
        assert_eq!(a.text, "3 scans accepted, 12 rows skipped (unknown items)");
        assert_eq!((a.rejected, a.skipped_rows, a.scans.len()), (1, 12, 2));
        assert_eq!(a.scans[0].skipped, vec![("unknown_item".to_string(), 12)]);
        let r = a.reasons();
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("Shadowstrike Horde, "), "{r:?}");
        assert!(r[0].ends_with(": scan is from the future"), "{r:?}");
        assert!(scan_log(&a.scans[0]).contains("accepted, 900 rows, skipped unknown_item 12"));

        // no message: one is made from the counts; skipped rows alone make it partial
        let b = parse_answer(&json!({"kind":"auctions","status":"added","accepted":2,"known":1,"rejected":0,"skipped_rows":4}));
        assert!(b.partial());
        assert_eq!(b.text, "2 scans accepted, 1 already had, 4 rows skipped");
        // a rejected scan under status "added" is still partial
        let c = parse_answer(&json!({"kind":"auctions","status":"added","accepted":1,"scans":[{"house":"H","result":"rejected"}]}));
        assert!(c.partial());
        assert_eq!(c.reasons(), vec!["H: refused".to_string()]);
    }

    #[test]
    fn crowd_answer_unchanged() {
        let a = parse_answer(&json!({"status":"added","accepted":{"npcs":9,"loot_sources":22}}));
        assert!(!a.is_auctions());
        assert!(!a.partial());
        assert_eq!(a.text, "Uploaded: 9 NPCs, 22 loot sources");
        assert_eq!(parse_answer(&json!({"status":"duplicate"})).text, "Up to date (nothing new)");
        assert_eq!(parse_answer(&json!({"status":"updated","accepted":{"quests":2}})).text, "Updated: 2 quests");
    }

    #[test]
    fn receipts_in_answers() {
        // auctions: the receipt, and a full scan when a scan was taken or already had
        let a = parse_answer(&json!({"kind":"auctions","status":"added","accepted":1,"known":0,"receipt":"7k3m9qxz"}));
        assert_eq!((a.receipt.as_deref(), a.full_scan), (Some("7K3M9QXZ"), true));
        let known = parse_answer(&json!({"kind":"auctions","status":"duplicate","accepted":0,"known":2,"receipt":"7K3M9QXZ"}));
        assert!(known.full_scan);
        // the server's own word wins
        let said = parse_answer(&json!({"kind":"auctions","status":"added","accepted":1,"receipt":"7K3M9QXZ","full_scan":false}));
        assert!(!said.full_scan);
        let none = parse_answer(&json!({"kind":"auctions","status":"partial","accepted":0,"known":0,"rejected":1}));
        assert_eq!((none.receipt, none.full_scan), (None, false));
        // collector data
        let c = parse_answer(&json!({"status":"added","accepted":{"npcs":9},"receipt":"ABCDEFGH"}));
        assert_eq!((c.receipt.as_deref(), c.full_scan), (Some("ABCDEFGH"), false));
        assert_eq!(c.text, "Uploaded: 9 NPCs");
        // an older server: none; a receipt that isn't one: ignored
        assert_eq!(parse_answer(&json!({"status":"added"})).receipt, None);
        assert_eq!(parse_answer(&json!({"status":"added","receipt":"not-a-receipt"})).receipt, None);
        assert_eq!(parse_answer(&json!({"status":"added","receipt":12345678})).receipt, None);
    }

    #[test]
    fn refusals() {
        let (m, s) = parse_refusal(&json!({"error":{"message":"no usable scans"}}));
        assert_eq!(m.as_deref(), Some("no usable scans"));
        assert!(s.is_empty());
        let (m, s) = parse_refusal(&json!({"error":{"message":"nothing usable"},"scans":[
            {"house":"EU-1-Horde","result":"rejected","reason":"all rows unknown","rows":0},
            {"house":"EU-1-Alliance","realm":"R","side":"Alliance","result":"rejected","reason":"too old"}]}));
        assert_eq!(m.as_deref(), Some("nothing usable"));
        assert_eq!(rejected_lines(&s), vec!["EU-1-Horde: all rows unknown".to_string(), "R Alliance: too old".to_string()]);
        let (m, s) = parse_refusal(&Value::Null);
        assert!(m.is_none() && s.is_empty());
    }

    #[test]
    fn backoff_schedule() {
        let mins: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs() / 60).collect();
        assert_eq!(mins, vec![1, 5, 15, 30, 60, 60, 60, 60]);
        assert_eq!(backoff(1000), Duration::from_secs(3600));
        assert_eq!(REFUSED_RETRY, Duration::from_secs(3600));
        assert_eq!(busy_wait(Some(120)), Duration::from_secs(120));
        assert_eq!(busy_wait(None), Duration::from_secs(30));
        assert_eq!(busy_wait(Some(0)), Duration::from_secs(5));
        assert_eq!(busy_wait(Some(86400)), Duration::from_secs(3600));
    }

    #[test]
    fn gzip_fallback() {
        assert!(plain_fallback(415, "", 0));
        assert!(plain_fallback(400, "That doesn't look like a SavedVariables file", 0));
        assert!(plain_fallback(422, "That doesn\u{2019}t look like a SavedVariables file", 0));
        assert!(plain_fallback(400, "unsupported Content-Encoding", 0));
        // understood, just nothing usable
        assert!(!plain_fallback(422, "no usable scans", 0));
        assert!(!plain_fallback(422, "doesn't look like a SavedVariables file", 2));
        // not about the body
        assert!(!plain_fallback(413, "gzip body too large", 0));
        assert!(!plain_fallback(429, "gzip", 0));
        assert!(!plain_fallback(500, "doesn't look like", 0));
        // and the body really is gzip
        let raw = b"EverlookDBAuctionsDB = { }\n".repeat(1000);
        let gz = gzip(&raw).unwrap();
        assert!(gz.len() < raw.len() / 10 && gz[..2] == [0x1f, 0x8b]);
        let mut back = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut back).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn labels_name_no_account() {
        let f = |a: &str, n: &str| PathBuf::from(format!("/w/WTF/Account/{a}/SavedVariables/{n}"));
        let one = labels(&[f("SECRETNAME", "EverlookDB.lua"), f("SECRETNAME", AUCTIONS)]);
        assert_eq!(one, vec!["Game data", "Auctions"]);
        let two = labels(&[f("A", AUCTIONS), f("B", "EverlookDB.lua"), f("B", AUCTIONS)]);
        assert_eq!(two, vec!["Auctions, account 1", "Game data, account 2", "Auctions, account 2"]);
    }
}
