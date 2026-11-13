// Watches every WTF/Account/*/SavedVariables/EverlookDB.lua and uploads it once the game has finished
// writing it. A cheap poll (every few seconds, file size and time only) instead of a file watcher: the
// game rewrites the file whole on logout or /reload, so this catches every save with no extra crates.
//
// Privacy: the request body is the file's bytes and nothing else. The file path (which holds the
// account folder name) is only used locally.

use crate::config::{self, Config, Done};
use crate::wow;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub const MAX_UPLOAD: u64 = 5 * 1024 * 1024;
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
}

pub struct Shared {
    pub cfg: Mutex<Config>,
    pub server: String,
}

pub fn upload_url(server: &str) -> String {
    format!("{}/api/v1/upload", server.trim_end_matches('/'))
}

pub enum Outcome {
    /// the server took it; the text sums up what was added
    Sent(String),
    /// refused for good (413/422/400): not retried until the file changes
    Refused(String),
    /// 429: wait this long
    Busy(Duration),
    /// network or server error: try again later
    Failed(String),
}

pub fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB-Uploader/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(90))
        .build()
        .expect("http client")
}

/// POST the file's bytes as the raw body. That is all that is sent.
pub fn upload(http: &reqwest::blocking::Client, server: &str, body: Vec<u8>) -> Outcome {
    let resp = match http.post(upload_url(server)).header("content-type", "text/plain").body(body).send() {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(if e.is_timeout() { "timed out".into() } else { "can't reach the server".into() }),
    };
    let code = resp.status().as_u16();
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
    let json: serde_json::Value = resp.bytes().ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let message = json["error"]["message"].as_str().map(str::to_string);
    match code {
        200 => Outcome::Sent(summary(&json)),
        429 => Outcome::Busy(Duration::from_secs(retry.unwrap_or(30).clamp(5, 3600))),
        413 => Outcome::Refused("the file is larger than 5 MB".into()),
        400 | 422 => Outcome::Refused(message.unwrap_or_else(|| "the server could not read the file".into())),
        _ => Outcome::Failed(format!("server error {code}")),
    }
}

/// "Uploaded: 9 NPCs, 22 loot sources" from the server's answer.
fn summary(v: &serde_json::Value) -> String {
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

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn key(server: &str, path: &Path) -> String {
    format!("{}|{}", server.trim_end_matches('/'), path.display())
}

#[derive(Default)]
struct Track {
    sig: Option<(u64, Option<SystemTime>)>,
    next_try: Option<Instant>,
    backoff: Duration,
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
            return Status { kind: Kind::Error, line: "WoW Forever folder not set".into(), files: 0 };
        };
        let files = wow::data_files(&dir);
        let mut result: Option<Status> = None;
        let mut set = |kind: Kind, line: String| {
            config::log(&line);
            result = Some(Status { kind, line, files: files.len() });
        };
        if paused && !force {
            return Status { kind: Kind::Paused, line: "Paused".into(), files: files.len() };
        }
        for f in &files {
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
            if !force && t.next_try.is_some_and(|n| n > Instant::now()) {
                continue;
            }
            let k = key(&self.shared.server, f);
            if meta.len() > MAX_UPLOAD {
                let done = self.shared.cfg.lock().unwrap().done.get(&k).is_some_and(|d| !d.ok && d.hash == "too-big");
                if !done || force {
                    remember(&self.shared, &k, "too-big".into(), false, None);
                    set(Kind::Error, "Not sent: EverlookDB.lua is larger than 5 MB".into());
                }
                continue;
            }
            let Ok(bytes) = std::fs::read(f) else { continue }; // locked mid-write: next poll
            if bytes.is_empty() {
                continue;
            }
            let hash = sha256(&bytes);
            let same = self.shared.cfg.lock().unwrap().done.get(&k).is_some_and(|d| d.hash == hash);
            if same && !force {
                continue;
            }
            match upload(&self.http, &self.shared.server, bytes) {
                Outcome::Sent(text) => {
                    t.backoff = Duration::ZERO;
                    t.next_try = None;
                    let line = format!("{} · {text}", chrono::Local::now().format("%H:%M"));
                    remember(&self.shared, &k, hash, true, Some(line.clone()));
                    set(Kind::Ok, line);
                }
                Outcome::Refused(why) => {
                    remember(&self.shared, &k, hash, false, None);
                    set(Kind::Error, format!("Not accepted: {why}"));
                }
                Outcome::Busy(wait) => {
                    self.blocked_until = Some(Instant::now() + wait);
                    set(Kind::Busy, format!("Server busy, trying again in {} s", wait.as_secs()));
                }
                Outcome::Failed(why) => {
                    t.backoff = (t.backoff * 2).clamp(Duration::from_secs(30), Duration::from_secs(15 * 60));
                    t.next_try = Some(Instant::now() + t.backoff);
                    set(Kind::Busy, format!("Upload failed ({why}), trying again in {} s", t.backoff.as_secs()));
                }
            }
        }
        result.unwrap_or_else(|| {
            let last = self.shared.cfg.lock().unwrap().last_status.clone();
            match (files.is_empty(), last) {
                (true, _) => Status { kind: Kind::Idle, line: "Waiting for EverlookDB.lua (log out or /reload in game)".into(), files: 0 },
                (false, Some(l)) => Status { kind: Kind::Ok, line: l, files: files.len() },
                (false, None) => Status { kind: Kind::Idle, line: "Watching for changes".into(), files: files.len() },
            }
        })
    }
}

fn remember(shared: &Shared, key: &str, hash: String, ok: bool, line: Option<String>) {
    let mut c = shared.cfg.lock().unwrap();
    c.done.insert(key.to_string(), Done { hash, ok });
    if line.is_some() {
        c.last_status = line;
    }
    c.save();
}
