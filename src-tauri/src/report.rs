// "Send error logs to the developer": one click builds a report of what went wrong, scrubs it, and sends it to
// everlookdb.com (POST /api/v1/reports), which answers with a short id the player can quote.
//
// What is in it: the app's version, the OS, every game found (name, flavour, version) and which one is selected and
// which one uploads come from, the last errors the app noted (errors.json, with times), the end of uploader.log
// (the last MAX_LOG_LINES lines, at most MAX_LOG_BYTES), config.json without its upload hashes, receipts or any
// secret-looking field, the installed addons with their versions, and for each SavedVariables file the app uploads
// only its size, date and first and last line (quoted text in them blanked: no character names). The player's
// "What happened?" line and the error the button sat next to go with it.
//
// Everything passes through Scrub first: the Windows (or macOS) user name in paths becomes <user>, WTF account
// folders <account 1> (realm and character folders <realm>/<character>), emails <email>, receipts, giveaway codes,
// API keys and anything token-like <token>.
//
// When the server can't be reached (or is busy or failing) the report waits in reports/outbox/ and is sent by itself
// once everlookdb.com is back (flush_outbox); the window says so. Any other refusal keeps it as a zip in the settings
// folder (reports/), for "Show file".

use crate::catalog::Catalog;
use crate::config::{self, Config};
use crate::{manager, shared, wow};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const MAX_LOG_LINES: usize = 2000;
pub const MAX_LOG_BYTES: usize = 1024 * 1024;
/// What one report may be on the wire (gzip); the server takes 2 MB.
pub const MAX_SEND: usize = 1_900_000;
/// How many noted errors are kept.
const KEEP_ERRORS: usize = 40;
const LINE: usize = 160;

// ---------------------------------------------------------------------------------------------------------------
// The errors the app noted

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Noted {
    /// local time, "2026-11-21 14:05:09"
    pub at: String,
    /// "upload", "install", "update", "price feed", "window" ...
    pub source: String,
    pub message: String,
}

static ERRORS: Mutex<()> = Mutex::new(());

fn errors_path() -> PathBuf {
    config::dir().join("errors.json")
}

/// The errors noted so far, oldest first.
pub fn noted() -> Vec<Noted> {
    std::fs::read(errors_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Note an error the player saw (kept across restarts, the newest KEEP_ERRORS). The same message again within a
/// minute is noted once.
pub fn note_error(source: &str, message: &str) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    let _one = ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = noted();
    let now = chrono::Local::now();
    let recent = |n: &Noted| {
        chrono::NaiveDateTime::parse_from_str(&n.at, "%Y-%m-%d %H:%M:%S")
            .is_ok_and(|t| (now.naive_local() - t).num_seconds().abs() < 60)
    };
    if list.iter().rev().take(5).any(|n| n.message == message && recent(n)) {
        return;
    }
    let message: String = message.chars().take(2000).collect();
    list.push(Noted { at: now.format("%Y-%m-%d %H:%M:%S").to_string(), source: source.chars().take(40).collect(), message });
    let cut = list.len().saturating_sub(KEEP_ERRORS);
    list.drain(..cut);
    let _ = std::fs::create_dir_all(config::dir());
    let tmp = errors_path().with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, errors_path());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Scrubbing

/// Takes personal data out of text before it leaves the PC.
pub struct Scrub {
    /// the user's own name, as the OS knows it (USERNAME / USER, and the home folder's last part)
    users: Vec<String>,
    /// WTF account folder names in the order seen: <account 1>, <account 2> ...
    accounts: RefCell<Vec<String>>,
}

fn is_sep(c: u8) -> bool {
    c == b'\\' || c == b'/'
}

/// Where a path segment that starts at `from` ends: at the next separator, quote, line end or `|`.
fn seg_end(s: &str, from: usize) -> usize {
    s.as_bytes()[from..].iter().position(|&c| is_sep(c) || matches!(c, b'"' | b'\'' | b'\n' | b'\r' | b'\t' | b'|' | b'<' | b'>' | b',' | b';' | b')' | b']')).map_or(s.len(), |p| from + p)
}

/// Every place `needle` (lowercase ASCII) is found in `s`, ignoring ASCII case.
fn find_all_ci(s: &str, needle: &str) -> Vec<usize> {
    let hay = s.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(p) = hay[at..].find(needle) {
        out.push(at + p);
        at += p + needle.len().max(1);
    }
    out
}

fn tokenish(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '=')
}

impl Scrub {
    /// For this PC's user.
    pub fn here() -> Scrub {
        let mut users: Vec<String> = Vec::new();
        for v in ["USERNAME", "USER", "LOGNAME"] {
            if let Some(u) = std::env::var_os(v) {
                users.push(u.to_string_lossy().into_owned());
            }
        }
        for v in ["USERPROFILE", "HOME"] {
            if let Some(h) = std::env::var_os(v) {
                if let Some(n) = Path::new(&h).file_name() {
                    users.push(n.to_string_lossy().into_owned());
                }
            }
        }
        Scrub::for_users(&users.iter().map(String::as_str).collect::<Vec<_>>())
    }

    pub fn for_users(users: &[&str]) -> Scrub {
        let mut users: Vec<String> = users.iter().map(|u| u.trim().to_string()).filter(|u| u.chars().count() >= 2).collect();
        users.sort_by_key(|u| std::cmp::Reverse(u.len()));
        users.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        Scrub { users, accounts: RefCell::new(Vec::new()) }
    }

    fn account(&self, name: &str) -> String {
        let mut a = self.accounts.borrow_mut();
        let i = match a.iter().position(|n| n.eq_ignore_ascii_case(name)) {
            Some(i) => i,
            None => {
                a.push(name.to_string());
                a.len() - 1
            }
        };
        format!("<account {}>", i + 1)
    }

    pub fn text(&self, s: &str) -> String {
        let s = self.paths(s);
        let s = self.wtf(&s);
        let s = emails(&s);
        tokens(&s)
    }

    /// `\Users\<name>\`, `/Users/<name>/`, `/home/<name>/`, and the user's own name as any path segment.
    fn paths(&self, s: &str) -> String {
        let mut cuts: Vec<(usize, usize)> = Vec::new();
        for marker in ["\\users\\", "/users/", "/home/", "\\home\\"] {
            for p in find_all_ci(s, marker) {
                let from = p + marker.len();
                let end = seg_end(s, from);
                if end > from {
                    cuts.push((from, end));
                }
            }
        }
        for u in &self.users {
            let lower = u.to_ascii_lowercase();
            for p in find_all_ci(s, &lower) {
                let end = p + lower.len();
                let before = p == 0 || is_sep(s.as_bytes()[p - 1]);
                let after = end == s.len() || is_sep(s.as_bytes()[end]) || matches!(s.as_bytes()[end], b'"' | b'\n' | b' ' | b')');
                // a path segment of its own (C:\Games\<user>\WoW, D:\<user>), never a part of a word
                let word_start = s[..p].rfind(char::is_whitespace).map_or(0, |i| i + 1);
                let in_url = s[word_start..p].contains("://");
                if before && after && p > 0 && !in_url {
                    cuts.push((p, end));
                }
            }
        }
        replace_cuts(s, cuts, |_| "<user>".to_string())
    }

    /// `WTF\Account\<account>\<realm>\<character>`: the account folder numbered, realm and character blanked.
    fn wtf(&self, s: &str) -> String {
        let mut cuts: Vec<(usize, usize, String)> = Vec::new();
        for marker in ["wtf\\account\\", "wtf/account/"] {
            for p in find_all_ci(s, marker) {
                let from = p + marker.len();
                let end = seg_end(s, from);
                if end == from {
                    continue;
                }
                cuts.push((from, end, self.account(&s[from..end])));
                // a realm folder and a character folder may follow (not SavedVariables, not a file)
                let mut at = end;
                for what in ["<realm>", "<character>"] {
                    if at >= s.len() || !is_sep(s.as_bytes()[at]) {
                        break;
                    }
                    let f = at + 1;
                    let e = seg_end(s, f);
                    let seg = &s[f..e];
                    if e == f || seg.eq_ignore_ascii_case("SavedVariables") || seg.contains('.') {
                        break;
                    }
                    cuts.push((f, e, what.to_string()));
                    at = e;
                }
            }
        }
        cuts.sort_by_key(|c| c.0);
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        for (a, b, with) in cuts {
            if a < last {
                continue;
            }
            out += &s[last..a];
            out += &with;
            last = b;
        }
        out += &s[last..];
        out
    }

    /// A JSON value with every string (and every object key) scrubbed.
    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (self.text(k), self.value(x))).collect()),
            other => other.clone(),
        }
    }
}

/// Replace the byte ranges (overlaps: the first wins) with what `with` gives.
fn replace_cuts(s: &str, mut cuts: Vec<(usize, usize)>, with: impl Fn(&str) -> String) -> String {
    cuts.sort();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, b) in cuts {
        if a < last || !s.is_char_boundary(a) || !s.is_char_boundary(b) {
            continue;
        }
        out += &s[last..a];
        out += &with(&s[a..b]);
        last = b;
    }
    out += &s[last..];
    out
}

/// name@host.tld -> <email>
fn emails(s: &str) -> String {
    let b = s.as_bytes();
    let local = |c: u8| c.is_ascii_alphanumeric() || b"._%+-".contains(&c);
    let host = |c: u8| c.is_ascii_alphanumeric() || b".-".contains(&c);
    let mut cuts = Vec::new();
    for (i, _) in s.match_indices('@') {
        let mut a = i;
        while a > 0 && local(b[a - 1]) {
            a -= 1;
        }
        let mut e = i + 1;
        while e < b.len() && host(b[e]) {
            e += 1;
        }
        let dom = s[i + 1..e].trim_end_matches('.');
        let e = i + 1 + dom.len();
        if a < i && dom.contains('.') && dom.rsplit('.').next().is_some_and(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphabetic())) {
            cuts.push((a, e));
        }
    }
    replace_cuts(s, cuts, |_| "<email>".to_string())
}

/// Receipts, giveaway codes, API keys, bearer tokens, `token=`/`key=` values and long token-like runs.
fn tokens(s: &str) -> String {
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let b = s.as_bytes();
    let run_end = |from: usize| s[from..].char_indices().find(|(_, c)| !tokenish(*c)).map_or(s.len(), |(i, _)| from + i);
    // a value after one of these words: receipts, bearer tokens (to the next space), query values
    for (word, whole) in [("receipt ", false), ("receipt: ", false), ("bearer ", true), ("token=", false), ("key=", false), ("password=", false), ("secret=", false), ("sig=", false), ("signature=", false)] {
        for p in find_all_ci(s, word) {
            // "receipt" and "bearer" as words of their own; "access_token=", "api_key=" are fine
            if !word.ends_with('=') && p > 0 && (b[p - 1].is_ascii_alphanumeric() || b[p - 1] == b'_') {
                continue;
            }
            let from = p + word.len();
            let end = if whole { s[from..].find(char::is_whitespace).map_or(s.len(), |i| from + i) } else { run_end(from) };
            if end > from {
                cuts.push((from, end));
            }
        }
    }
    // EVL-<receipt>-<receipt> giveaway codes, qk_ API keys, GitHub tokens
    for prefix in ["evl-", "qk_", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_"] {
        for p in find_all_ci(s, prefix) {
            if p > 0 && b[p - 1].is_ascii_alphanumeric() {
                continue;
            }
            let end = run_end(p + prefix.len());
            if end > p + prefix.len() {
                cuts.push((p, end));
            }
        }
    }
    // anything 32+ characters long made of letters and digits both (keys, hashes, signatures)
    let mut i = 0;
    while i < s.len() {
        let end = run_end(i);
        if end > i {
            let run = &s[i..end];
            if run.len() >= 32 && run.chars().any(|c| c.is_ascii_digit()) && run.chars().any(|c| c.is_ascii_alphabetic()) {
                cuts.push((i, end));
            }
            i = end;
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    replace_cuts(s, cuts, |_| "<token>".to_string())
}

/// config.json for the report: no upload hashes (they name the files by path), no receipts, nothing whose name
/// sounds secret; then scrubbed like the rest.
pub fn config_json(cfg: &Config, scrub: &Scrub) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("done");
        o.remove("receipts");
        o.remove("de_sent");
    }
    drop_secrets(&mut v);
    scrub.value(&v)
}

fn secret_name(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "api_key", "apikey", "receipt", "cookie", "auth", "credential", "private"].iter().any(|w| k.contains(w))
}

fn drop_secrets(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.retain(|k, _| !secret_name(k));
            o.values_mut().for_each(drop_secrets);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_secrets),
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Building

/// The end of uploader.log (and the rotated uploader.log.old before it): the last `lines` lines, at most `bytes`.
pub fn log_tail(dir: &Path, lines: usize, bytes: usize) -> String {
    let read = |p: PathBuf| std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let text = read(dir.join("uploader.log.old")) + &read(dir.join("uploader.log"));
    tail(&text, lines, bytes)
}

pub fn tail(text: &str, lines: usize, bytes: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut size = 0;
    for l in all.iter().rev().take(lines) {
        if size + l.len() + 1 > bytes {
            break;
        }
        size += l.len() + 1;
        out.push(l);
    }
    out.reverse();
    out.join("\n")
}

/// A line of a SavedVariables file with its quoted text blanked (character, realm and guild names are quoted
/// keys and values there), at most LINE characters.
pub fn blank_line(l: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in l.trim().chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    out.push('…');
                    out.push(c);
                    quote = None;
                }
            }
            None => {
                out.push(c);
                if c == '"' || c == '\'' {
                    quote = Some(c);
                }
            }
        }
    }
    if quote.is_some() {
        out.push('…');
    }
    if out.chars().count() > LINE {
        out = out.chars().take(LINE).collect::<String>() + "…";
    }
    out
}

/// First and last non-empty line of a file, reading only its two ends.
fn ends(path: &Path) -> (String, String) {
    let Ok(mut f) = std::fs::File::open(path) else { return Default::default() };
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).unwrap_or(0);
    head.truncate(n);
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut back = Vec::new();
    if f.seek(SeekFrom::Start(len.saturating_sub(4096))).is_ok() {
        let _ = f.read_to_end(&mut back);
    }
    let first = String::from_utf8_lossy(&head).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    let last = String::from_utf8_lossy(&back).lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
    (blank_line(&first), blank_line(&last))
}

/// The files uploads come from: what they are, size, date, first and last line. Never their content.
pub fn saved_variables(forever: &Path) -> Vec<Value> {
    let files = wow::data_files(forever);
    let names = crate::worker::labels(&files);
    files
        .iter()
        .zip(names)
        .map(|(f, label)| {
            let meta = std::fs::metadata(f).ok();
            let modified = meta.as_ref().and_then(|m| m.modified().ok()).map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string());
            let (first, last) = ends(f);
            json!({
                "what": label,
                "file": f.file_name().map(|n| n.to_string_lossy().into_owned()),
                "bytes": meta.map(|m| m.len()),
                "modified": modified,
                "first_line": first,
                "last_line": last,
            })
        })
        .collect()
}

/// "Windows 11 Pro 24H2 (build 26300, x86_64)", "macOS 15.1 (aarch64)".
pub fn os() -> String {
    let arch = std::env::consts::ARCH;
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
        if let Ok(k) = key {
            let get = |n: &str| k.get_value::<String, _>(n).unwrap_or_default();
            let build = get("CurrentBuild");
            let mut name = get("ProductName");
            // Windows 11 still says "Windows 10" there
            if build.parse::<u32>().is_ok_and(|b| b >= 22000) {
                name = name.replace("Windows 10", "Windows 11");
            }
            return format!("{name} {} (build {build}, {arch})", get("DisplayVersion")).replace("  ", " ");
        }
        format!("Windows ({arch})")
    }
    #[cfg(target_os = "macos")]
    {
        let v = std::process::Command::new("sw_vers").arg("-productVersion").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        format!("macOS {} ({arch})", v.unwrap_or_default())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        format!("{} ({arch})", std::env::consts::OS)
    }
}

fn install_json(i: &wow::Install) -> Value {
    json!({ "name": i.name, "edition": i.edition, "flavor": i.flavor, "folder": i.folder, "version": i.version, "build": i.build, "game": i.game })
}

/// What the report is made from.
pub struct Inputs<'a> {
    pub cfg: &'a Config,
    pub installs: &'a [wow::Install],
    pub catalog: Option<&'a Catalog>,
    /// the Price Feed's last state, when the app is running
    pub price_feed: Option<&'a shared::View>,
    /// "What happened?" as typed
    pub what: &'a str,
    /// the error message the button sat next to
    pub shown: &'a str,
    /// marked as a test (deleted after)
    pub test: bool,
}

/// The report, scrubbed, as it is sent.
pub fn build(inp: &Inputs, log: &str) -> Value {
    let scrub = Scrub::here();
    build_with(inp, log, &scrub)
}

pub fn build_with(inp: &Inputs, log: &str, scrub: &Scrub) -> Value {
    let cfg = inp.cfg;
    // the one the window shows: the saved choice, else WoW Forever, else the first (as main.rs get_state)
    let selected = cfg
        .selected
        .as_deref()
        .and_then(|s| inp.installs.iter().find(|i| i.id == s))
        .or_else(|| inp.installs.iter().find(|i| i.flavor == "forever"))
        .or(inp.installs.first());
    let uploads_from = cfg.wow_dir.as_ref().and_then(|d| inp.installs.iter().find(|i| &i.dir == d));
    let addons: Map<String, Value> = inp
        .installs
        .iter()
        .filter(|i| i.addons_dir.is_dir())
        .map(|i| {
            let items = manager::list(i, inp.catalog);
            let text = manager::export_text(i, &items, &chrono::Local::now().format("%Y-%m-%d").to_string());
            (format!("{} ({})", i.name, i.flavor), Value::String(text))
        })
        .collect();
    let report = json!({
        "kind": "everlookdb-app-report",
        "format": 1,
        "test": inp.test,
        "app": env!("CARGO_PKG_VERSION"),
        "os": os(),
        "created": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "what": inp.what.trim(),
        "shown": inp.shown.trim(),
        "errors": noted(),
        "game": selected.map(install_json),
        "uploads_from": uploads_from.map(install_json),
        "games": inp.installs.iter().map(install_json).collect::<Vec<_>>(),
        "upload_files": cfg.wow_dir.as_deref().map(saved_variables).unwrap_or_default(),
        "price_feed": inp.price_feed.map(|v| json!({ "on": v.on, "note": v.note, "houses": v.houses.iter().map(|h| &h.key).collect::<Vec<_>>() })),
        "config": config_json(cfg, scrub),
        "addons": addons,
        "log": log,
    });
    scrub.value(&report)
}

/// The report as it goes on the wire: gzip JSON under MAX_SEND, cutting the log shorter if it has to.
pub fn encode(report: &Value) -> std::io::Result<Vec<u8>> {
    let mut r = report.clone();
    loop {
        let gz = crate::worker::gzip(&serde_json::to_vec(&r).map_err(std::io::Error::other)?)?;
        if gz.len() <= MAX_SEND {
            return Ok(gz);
        }
        let log = r["log"].as_str().unwrap_or("").to_string();
        if log.is_empty() {
            // the rest is too large on its own: keep only the facts
            if let Some(o) = r.as_object_mut() {
                o.insert("addons".into(), json!({ "cut": "too large" }));
            }
            let gz = crate::worker::gzip(&serde_json::to_vec(&r).map_err(std::io::Error::other)?)?;
            return Ok(gz);
        }
        r["log"] = Value::String(tail(&log, usize::MAX, log.len() / 2));
    }
}

/// The text a person reads: the report as pretty JSON, the log as plain lines at the end.
pub fn readable(report: &Value) -> String {
    let mut r = report.clone();
    let log = r.as_object_mut().and_then(|o| o.remove("log")).and_then(|l| l.as_str().map(str::to_string)).unwrap_or_default();
    format!(
        "This is exactly what \"Send error logs to the developer\" sends to everlookdb.com (gzip-compressed JSON).\n\n{}\n\n---- log (uploader.log, the last lines) ----\n{log}\n",
        serde_json::to_string_pretty(&r).unwrap_or_default()
    )
}

pub fn reports_dir() -> PathBuf {
    config::dir().join("reports")
}

/// Write a text file in reports/ (the preview, the copy of what was sent). Returns it.
pub fn write_text(name: &str, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(reports_dir())?;
    let p = reports_dir().join(name);
    std::fs::write(&p, if cfg!(windows) { text.replace('\n', "\r\n") } else { text.to_string() })?;
    Ok(p)
}

/// Keep a report that couldn't be sent: reports/report-<time>.zip with report.json and report.txt in it.
pub fn keep_copy(report: &Value) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(reports_dir())?;
    let p = reports_dir().join(format!("report-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let mut z = zip::ZipWriter::new(std::fs::File::create(&p)?);
    let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    z.start_file("report.json", opt).map_err(std::io::Error::other)?;
    z.write_all(&serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?)?;
    z.start_file("report.txt", opt).map_err(std::io::Error::other)?;
    z.write_all(readable(report).as_bytes())?;
    z.finish().map_err(std::io::Error::other)?;
    Ok(p)
}

pub fn reports_url(server: &str) -> String {
    format!("{}/api/v1/reports", server.trim_end_matches('/'))
}

/// A short report id as the server gives it (6 Crockford base32 characters).
pub fn report_id(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_uppercase();
    (s.len() == 6 && s.bytes().all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))).then_some(s)
}

/// Why a report didn't go: a sentence for the window, and whether it goes by itself later (no connection, a busy or
/// failing server: health::transient_status).
#[derive(Debug, Clone, PartialEq)]
pub struct NotSent {
    pub msg: String,
    pub later: bool,
}

impl NotSent {
    pub fn now(msg: impl Into<String>) -> NotSent {
        NotSent { msg: msg.into(), later: false }
    }
    fn later(msg: impl Into<String>) -> NotSent {
        NotSent { msg: msg.into(), later: true }
    }
}

/// Send it. Ok(the report's id), or why not.
pub fn send(server: &str, gz: Vec<u8>) -> Result<String, NotSent> {
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB-Uploader/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| NotSent::now(e.to_string()))?;
    let resp = http
        .post(reports_url(server))
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .body(gz)
        .send()
        .map_err(|e| {
            config::log(&format!("report: {}", crate::worker::error_chain(&e)));
            NotSent::later(if e.is_timeout() { "the server didn't answer in time" } else { "everlookdb.com can't be reached" })
        })?;
    let code = resp.status().as_u16();
    let v: Value = resp.bytes().ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let said = v["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("the server answered {code}"));
    match code {
        200..=299 => v["id"].as_str().and_then(report_id).ok_or_else(|| NotSent::now("the server's answer had no report id")),
        c if crate::health::transient_status(c) => Err(NotSent::later(said)),
        _ => Err(NotSent::now(said)),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reports waiting for the server

/// At most this many reports wait; older ones go (their zip copy stays in reports/).
const OUTBOX_KEEP: usize = 5;

pub fn outbox_dir() -> PathBuf {
    reports_dir().join("outbox")
}

/// A report waiting to be sent: the packed report (`<stamp>.gz`) and what it sat next to (`<stamp>.json`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Waiting {
    /// the error the button sat next to ('' for "Report a problem")
    pub shown: String,
    pub at: String,
}

/// Put a packed report in the outbox. Keeps the newest OUTBOX_KEEP.
pub fn queue(dir: &Path, gz: &[u8], shown: &str) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    std::fs::create_dir_all(dir)?;
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stamp = format!("{}-{n:06}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f"));
    std::fs::write(dir.join(format!("{stamp}.json")), serde_json::to_vec(&Waiting { shown: shown.to_string(), at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string() }).map_err(std::io::Error::other)?)?;
    std::fs::write(dir.join(format!("{stamp}.gz")), gz)?;
    let all = waiting_stamps(dir);
    for old in all.iter().take(all.len().saturating_sub(OUTBOX_KEEP)) {
        let _ = std::fs::remove_file(dir.join(format!("{old}.gz")));
        let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
    }
    Ok(())
}

/// The waiting reports' stamps, oldest first.
fn waiting_stamps(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".gz").map(str::to_string))
        .collect();
    v.sort();
    v
}

/// How many reports wait.
pub fn waiting_count(dir: &Path) -> usize {
    waiting_stamps(dir).len()
}

/// Send what waits, oldest first, with `send`; stops at the first one that has to wait again. Returns (what it sat
/// next to, its id) for each sent. One the server refuses for good leaves the outbox (its zip copy stays).
pub fn flush_outbox(dir: &Path, send: impl Fn(Vec<u8>) -> Result<String, NotSent>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for stamp in waiting_stamps(dir) {
        let gz_path = dir.join(format!("{stamp}.gz"));
        let meta_path = dir.join(format!("{stamp}.json"));
        let Ok(gz) = std::fs::read(&gz_path) else { continue };
        let shown = std::fs::read(&meta_path).ok().and_then(|b| serde_json::from_slice::<Waiting>(&b).ok()).map(|w| w.shown).unwrap_or_default();
        match send(gz) {
            Ok(id) => {
                config::log(&format!("problem report sent (it had waited for the server): {id}"));
                out.push((shown, id));
            }
            Err(e) if e.later => break,
            Err(e) => config::log(&format!("waiting problem report refused, dropped: {}", e.msg)),
        }
        let _ = std::fs::remove_file(&gz_path);
        let _ = std::fs::remove_file(&meta_path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Scrub {
        Scrub::for_users(&["Viktor Lundgren", "quikmn"])
    }

    #[test]
    fn user_names_in_paths() {
        let x = s();
        assert_eq!(x.text(r"C:\Users\Viktor Lundgren\AppData\Roaming\com.everlookdb.uploader"), r"C:\Users\<user>\AppData\Roaming\com.everlookdb.uploader");
        assert_eq!(x.text("/Users/bob/Library/Application Support/x"), "/Users/<user>/Library/Application Support/x");
        assert_eq!(x.text("/home/alice/.config"), "/home/<user>/.config");
        assert_eq!(x.text(r"c:\users\SOMEONE"), r"c:\users\<user>");
        // the user's own name as a folder anywhere, but not inside a word or an addon id
        assert_eq!(x.text(r"D:\quikmn\Games\World of Warcraft"), r"D:\<user>\Games\World of Warcraft");
        assert_eq!(x.text("installed github:quikmn/EverlookBar v0.2.2"), "installed github:quikmn/EverlookBar v0.2.2");
        assert_eq!(x.text("download https://github.com/quikmn/EverlookBar/releases/x.zip: 404"), "download https://github.com/quikmn/EverlookBar/releases/x.zip: 404");
        assert_eq!(x.text("D:\\World of Warcraft\\_forever_"), "D:\\World of Warcraft\\_forever_");
    }

    #[test]
    fn account_realm_and_character_folders() {
        let x = s();
        assert_eq!(
            x.text(r"https://everlookdb.com|D:\World of Warcraft\_forever_\WTF\Account\DEKIS\SavedVariables\EverlookDB.lua"),
            r"https://everlookdb.com|D:\World of Warcraft\_forever_\WTF\Account\<account 1>\SavedVariables\EverlookDB.lua"
        );
        assert_eq!(x.text("/w/WTF/Account/12345678#1/Shadowstrike/Thrall/SavedVariables/X.lua"), "/w/WTF/Account/<account 2>/<realm>/<character>/SavedVariables/X.lua");
        assert_eq!(x.text(r"WTF\Account\dekis\config-cache.wtf"), r"WTF\Account\<account 1>\config-cache.wtf", "the same account, the same number");
    }

    #[test]
    fn emails_and_tokens() {
        let x = s();
        assert_eq!(x.text("mail bob.smith+wow@gmail.com now"), "mail <email> now");
        assert_eq!(x.text("a@b is no email, nor is x@y.1"), "a@b is no email, nor is x@y.1");
        assert_eq!(x.text("2026-11-20 16:37:56   receipt 9DXX1T49 (full scan)"), "2026-11-20 16:37:56   receipt <token> (full scan)");
        assert_eq!(x.text("code EVL-7KQ2M9XA-0 copied"), "code <token> copied");
        assert_eq!(x.text("key qk_0123456789abcdef0123456789abcdef0123456789abcdef"), "key <token>");
        assert_eq!(x.text("Authorization: Bearer abc.DEF-123 x"), "Authorization: Bearer <token> x");
        assert_eq!(x.text("GET /x?token=s3cr3t&page=2"), "GET /x?token=<token>&page=2");
        assert_eq!(x.text("ghp_ABCDEFGHIJ0123456789"), "<token>");
        assert_eq!(x.text("hash 3f5a9c0e1b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a"), "hash <token>");
        // ordinary log lines stay as they are
        let line = "2026-11-21 14:28:13 installed github:quikmn/LatticePartyRaidFrames v0.8.0 into D:\\World of Warcraft\\_forever_";
        assert_eq!(x.text(line), line);
        assert_eq!(x.text("Auctions (EverlookDBAuctions.lua): 2 scans accepted"), "Auctions (EverlookDBAuctions.lua): 2 scans accepted");
    }

    #[test]
    fn config_loses_secrets_and_paths() {
        let raw = r#"{"wow_dir":"C:\\Users\\Viktor Lundgren\\Games\\World of Warcraft\\_forever_","paused":false,
            "done":{"https://everlookdb.com|D:\\W\\WTF\\Account\\DEKIS\\SavedVariables\\EverlookDB.lua":{"hash":"ab","ok":true}},
            "uploads":{"https://everlookdb.com|D:\\W\\WTF\\Account\\DEKIS\\SavedVariables\\EverlookDB.lua":{"level":"failed","headline":"Upload failed: can't reach the server","details":[],"at":5,"next_retry":null,"failures":2}},
            "receipts":{"auctions":{"code":"7K3M9QXZ","at":5,"full_scan":true}},"auto_update":true}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let v = config_json(&cfg, &s());
        let text = v.to_string();
        assert!(v.get("done").is_none() && v.get("receipts").is_none(), "{text}");
        assert!(!text.contains("DEKIS") && !text.contains("Viktor") && !text.contains("7K3M9QXZ"), "{text}");
        assert!(text.contains(r"WTF\\Account\\<account 1>\\SavedVariables") && text.contains("Upload failed: can't reach the server"), "{text}");
        assert_eq!(v["wow_dir"], r"C:\Users\<user>\Games\World of Warcraft\_forever_");
        let mut nested = json!({"a": {"api_key": "x", "GitHubToken": "y", "keep": 1}, "list": [{"password": "p"}]});
        drop_secrets(&mut nested);
        assert_eq!(nested, json!({"a": {"keep": 1}, "list": [{}]}));
    }

    #[test]
    fn saved_variables_lines_lose_names() {
        assert_eq!(blank_line(r#"["Thrall - Shadowstrike"] = {"#), r#"["…"] = {"#);
        assert_eq!(blank_line(r#"EverlookDBDB = {"#), "EverlookDBDB = {");
        assert_eq!(blank_line(r#"  ["name"] = "Bob \"the\" Rogue","#), r#"["…"] = "…","#);
        assert_eq!(blank_line(r#"x = "unclosed"#), "x = \"…");
        assert!(blank_line(&"a".repeat(500)).chars().count() <= LINE + 1);
    }

    #[test]
    fn log_tail_is_capped() {
        let text: String = (0..5000).map(|i| format!("2026-11-21 12:00:00 line {i}\n")).collect();
        let t = tail(&text, MAX_LOG_LINES, MAX_LOG_BYTES);
        assert_eq!(t.lines().count(), MAX_LOG_LINES);
        assert!(t.ends_with("line 4999"));
        let big: String = (0..3000).map(|i| format!("{i} {}\n", "x".repeat(1000))).collect();
        let t = tail(&big, MAX_LOG_LINES, MAX_LOG_BYTES);
        assert!(t.len() <= MAX_LOG_BYTES && t.lines().count() < 1100, "{}", t.len());
        assert!(t.ends_with(&"x".repeat(1000)));
    }

    #[test]
    fn report_is_built_scrubbed_and_fits() {
        let dir = std::env::temp_dir().join(format!("edb-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sv = dir.join(r"WoW/_forever_/WTF/Account/SECRETACCT/SavedVariables");
        std::fs::create_dir_all(&sv).unwrap();
        std::fs::write(sv.join("EverlookDB.lua"), "\nEverlookDBDB = {\n[\"Thrall - Shadowstrike\"] = {},\n}\n").unwrap();
        let forever = dir.join("WoW/_forever_");
        let cfg = Config { wow_dir: Some(forever.clone()), ..Default::default() };
        let inp = Inputs { cfg: &cfg, installs: &[], catalog: None, price_feed: None, what: "it broke, mail me at x@example.com", shown: "Auctions: upload failed (can't reach the server)", test: true };
        let log: String = (0..3000).map(|i| format!("2026-11-21 12:00:{:02} line {i}\n", i % 60)).collect();
        let r = build_with(&inp, &tail(&log, MAX_LOG_LINES, MAX_LOG_BYTES), &s());
        let text = r.to_string();
        assert!(!text.contains("SECRETACCT") && !text.contains("Thrall") && !text.contains("x@example.com"), "{}", &text[..2000.min(text.len())]);
        assert_eq!(r["shown"], "Auctions: upload failed (can't reach the server)");
        assert_eq!(r["test"], true);
        assert_eq!(r["upload_files"][0]["first_line"], "EverlookDBDB = {");
        assert_eq!(r["upload_files"][0]["last_line"], "}");
        assert_eq!(r["upload_files"][0]["what"], "Game data");
        assert_eq!(r["app"], env!("CARGO_PKG_VERSION"));
        assert_eq!(r["log"].as_str().unwrap().lines().count(), MAX_LOG_LINES);
        let gz = encode(&r).unwrap();
        assert!(gz.len() <= MAX_SEND, "{}", gz.len());
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut back).unwrap();
        let sent: Value = serde_json::from_slice(&back).unwrap();
        assert_eq!(sent["shown"], r["shown"]);
        assert!(readable(&sent).contains("---- log"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encode_cuts_the_log_to_fit() {
        // 3 MB that doesn't compress (a simple generator's letters): more than one report may be
        let mut x: u64 = 88172645463325252;
        let log: String = (0..3_000_000)
            .map(|i| {
                if i % 80 == 79 {
                    return '\n';
                }
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (b'!' + (x % 90) as u8) as char
            })
            .collect();
        let r = json!({ "app": "0.7.2", "shown": "x", "log": log });
        let gz = encode(&r).unwrap();
        assert!(gz.len() <= MAX_SEND, "{}", gz.len());
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut back).unwrap();
        let sent: Value = serde_json::from_slice(&back).unwrap();
        let kept = sent["log"].as_str().unwrap();
        assert!(kept.len() > 500_000 && log.trim_end().ends_with(kept), "the newest lines are kept ({})", kept.len());
    }

    #[test]
    fn ids() {
        assert_eq!(report_id(" k7q2m9 "), Some("K7Q2M9".into()));
        assert_eq!(report_id("K7Q2MU"), None);
        assert_eq!(report_id("../x"), None);
    }

    #[test]
    fn reports_wait_for_the_server_and_go_when_it_is_back() {
        let dir = std::env::temp_dir().join(format!("edb-outbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        queue(&dir, b"one", "Price Feed: down").unwrap();
        queue(&dir, b"two", "").unwrap();
        assert_eq!(waiting_count(&dir), 2);
        // still down: nothing goes, nothing is lost
        let sent = flush_outbox(&dir, |_| Err(NotSent { msg: "everlookdb.com can't be reached".into(), later: true }));
        assert!(sent.is_empty());
        assert_eq!(waiting_count(&dir), 2);
        // back: both go, oldest first, with what they sat next to
        let order = RefCell::new(Vec::new());
        let sent = flush_outbox(&dir, |gz| {
            order.borrow_mut().push(gz.clone());
            Ok(if gz == b"one" { "AAAAAA".into() } else { "BBBBBB".into() })
        });
        assert_eq!(sent, vec![("Price Feed: down".to_string(), "AAAAAA".to_string()), (String::new(), "BBBBBB".to_string())]);
        assert_eq!(*order.borrow(), vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(waiting_count(&dir), 0);
        // refused for good: dropped, not tried forever
        queue(&dir, b"bad", "x").unwrap();
        assert!(flush_outbox(&dir, |_| Err(NotSent { msg: "too large".into(), later: false })).is_empty());
        assert_eq!(waiting_count(&dir), 0);
        // only the newest few wait
        for i in 0..(OUTBOX_KEEP + 3) {
            queue(&dir, format!("r{i}").as_bytes(), "").unwrap();
        }
        assert_eq!(waiting_count(&dir), OUTBOX_KEEP);
        std::fs::remove_dir_all(&dir).ok();
    }
}
