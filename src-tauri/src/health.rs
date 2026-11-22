// One policy for every background job (uploads, disenchants, the Price Feed, the addon list, automatic addon updates,
// the app's own update check): heal quietly, and tell the player only when something is really broken.
//
// - Transient (no connection, DNS, timeouts, 5xx, 429 and 408): each job retries by itself with backoff and jitter
//   (and a server's Retry-After) and logs it. Nothing is shown while it has been failing for less than QUIET_FOR, and
//   once it works again nothing is shown at all.
// - Still failing after QUIET_FOR: one calm line. When jobs in two or more places fail together (everlookdb.com is
//   down) they make ONE line, not one per job. These lines offer "Send report".
// - Local problems the player can fix (no permission, a file in use, the disk full ...): shown at once, in plain
//   words that say what to do.
// - Refusals that are information ("no full scans yet", "already had", a scan refused for a reason) are not troubles:
//   the jobs keep them as quiet state, never here.
//
// The window has one place for messages (app.js, the inbox); `lines()` is what this module puts there.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// How long a job may fail quietly before the player is told: half an hour.
pub const QUIET_FOR: i64 = 30 * 60;

/// The one line for "the site is down" (several jobs failing together).
pub const SITE_DOWN: &str = "Can't reach everlookdb.com right now. Your data is kept and sent when it's back.";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Job {
    Uploads,
    Disenchants,
    PriceFeed,
    Catalog,
    AddonUpdates,
    AppUpdate,
}

impl Job {
    /// Talks to everlookdb.com (addon updates download from GitHub, Tukui and others).
    pub fn site(self) -> bool {
        !matches!(self, Job::AddonUpdates)
    }

    /// Where it belongs: two jobs of one place make one line.
    pub fn place(self) -> &'static str {
        match self {
            Job::Uploads | Job::Disenchants => "uploads",
            Job::PriceFeed => "price-feed",
            Job::Catalog | Job::AddonUpdates => "addons",
            Job::AppUpdate => "app",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Job::Uploads => "Uploads",
            Job::Disenchants => "Sending disenchants",
            Job::PriceFeed => "The Price Feed",
            Job::Catalog => "The addon list",
            Job::AddonUpdates => "Automatic addon updates",
            Job::AppUpdate => "The app's update check",
        }
    }

    /// The line when only this job can't reach its server.
    fn down_text(self) -> &'static str {
        match self {
            Job::Uploads | Job::Disenchants => "Can't reach everlookdb.com right now. Your files are kept and sent when it's back.",
            Job::PriceFeed => "Can't reach everlookdb.com right now. The prices you have stay in the game; new ones come when it's back.",
            Job::Catalog => "Can't reach everlookdb.com right now, so this is the addon list from last time.",
            Job::AddonUpdates => "Addon updates can't be downloaded right now. EverlookDB keeps trying by itself.",
            Job::AppUpdate => "Can't check for a new version of the app right now. EverlookDB keeps trying by itself.",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Trouble {
    /// no connection, DNS, a timeout, 5xx, 429, 408: retried quietly
    Unreachable(String),
    /// the server answered, but not usably (an unexpected 4xx, a bad answer): retried quietly too
    Server(String),
    /// a problem on this PC the player can fix: shown at once, in these words
    Local(String),
}

impl Trouble {
    pub fn is_local(&self) -> bool {
        matches!(self, Trouble::Local(_))
    }
    fn text(&self) -> &str {
        match self {
            Trouble::Unreachable(m) | Trouble::Server(m) | Trouble::Local(m) => m,
        }
    }
    /// An HTTP answer that isn't a success: transient statuses are Unreachable, the rest Server.
    pub fn of_status(code: u16, msg: impl Into<String>) -> Trouble {
        if transient_status(code) { Trouble::Unreachable(msg.into()) } else { Trouble::Server(msg.into()) }
    }
}

/// A status that goes away by itself: 408, 429 and every 5xx.
pub fn transient_status(code: u16) -> bool {
    code == 408 || code == 429 || code >= 500
}

/// A file or folder problem on this PC as a sentence that says what to do. `what`: "the Price Feed addon".
pub fn local_words(e: &std::io::Error, what: &str) -> String {
    use std::io::ErrorKind as K;
    let raw = e.raw_os_error();
    // Windows: 32 sharing violation, 33 lock violation; 112 / 39 disk full; Unix ENOSPC 28
    if matches!(raw, Some(32) | Some(33)) {
        return format!("WoW or another program is using {what}. Close it, and EverlookDB carries on by itself.");
    }
    if matches!(raw, Some(112) | Some(39)) && cfg!(windows) || e.kind() == K::StorageFull || (cfg!(unix) && raw == Some(28)) {
        return format!("Your disk is full, so EverlookDB can't save {what}. Free some space, and it carries on by itself.");
    }
    match e.kind() {
        K::PermissionDenied => format!("EverlookDB isn't allowed to change {what}. Check that the WoW folder isn't read-only (or run WoW and EverlookDB as the same user)."),
        K::ReadOnlyFilesystem => format!("{} is on a read-only drive. Move WoW to a drive you can write to.", capital(what)),
        K::NotFound => format!("{} isn't there any more. Check that the WoW folder is where it was.", capital(what)),
        _ => format!("EverlookDB can't save {what} ({e})."),
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// A random 0..=max, to spread retries out.
pub fn jitter(max: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    Duration::from_millis(h.finish() % (max.as_millis() as u64 + 1))
}

/// One message for the window's inbox.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Line {
    /// what it is about: lines of one kind replace each other ("site-down", "uploads-down", "uploads-local" ...)
    pub kind: String,
    pub text: String,
    /// "down" (still failing after QUIET_FOR) or "local" (the player can fix it)
    pub tone: &'static str,
    /// offer "Send report"
    pub report: bool,
    /// since when (unix seconds)
    pub since: i64,
}

struct Entry {
    trouble: Trouble,
    since: i64,
    /// already noted for problem reports
    noted: bool,
}

#[derive(Default)]
pub struct Health {
    jobs: BTreeMap<(Job, String), Entry>,
}

impl Health {
    pub const fn new() -> Health {
        Health { jobs: BTreeMap::new() }
    }

    /// `job` (one of its files, addons ...: `key`) failed. `since`: when it started failing (the first failure; a
    /// job that keeps its own record passes that, so a restart doesn't start the clock again).
    pub fn fail(&mut self, job: Job, key: &str, trouble: Trouble, since: i64) {
        match self.jobs.get_mut(&(job, key.to_string())) {
            Some(e) => {
                if e.trouble.is_local() != trouble.is_local() {
                    // another kind of problem: its own clock, and noted again when it shows
                    e.since = since;
                    e.noted = false;
                } else {
                    e.since = e.since.min(since);
                }
                if e.trouble.text() != trouble.text() && trouble.is_local() {
                    e.noted = false;
                }
                e.trouble = trouble;
            }
            None => {
                self.jobs.insert((job, key.to_string()), Entry { trouble, since, noted: false });
            }
        }
    }

    /// It worked. Returns whether it had been failing (not counting local problems).
    pub fn ok(&mut self, job: Job, key: &str) -> bool {
        self.jobs.remove(&(job, key.to_string())).is_some_and(|e| !e.trouble.is_local())
    }

    /// Only these keys of `job` are failing now (an addon updated by hand, or gone, is no trouble any more).
    pub fn keep(&mut self, job: Job, keys: &[String]) {
        self.jobs.retain(|(j, k), _| *j != job || keys.contains(k));
    }

    #[cfg(test)]
    pub fn failing(&self, job: Job, key: &str) -> Option<i64> {
        self.jobs.get(&(job, key.to_string())).map(|e| e.since)
    }

    /// The lines to show at `now`, and the messages shown for the first time (for problem reports).
    pub fn view(&mut self, now: i64) -> (Vec<Line>, Vec<String>) {
        let mut lines = Vec::new();
        let mut new = Vec::new();
        // local problems: at once
        for ((job, _), e) in self.jobs.iter_mut() {
            if let Trouble::Local(m) = &e.trouble {
                if !lines.iter().any(|l: &Line| l.text == *m) {
                    lines.push(Line { kind: format!("{}-local", job.place()), text: m.clone(), tone: "local", report: false, since: e.since });
                }
                if !e.noted {
                    e.noted = true;
                    new.push(format!("{}: {m}", job.name()));
                }
            }
        }
        let long = |e: &Entry| !e.trouble.is_local() && now - e.since >= QUIET_FOR;
        // everlookdb.com unreachable from two places or more: one line for all of them
        let down: BTreeSet<&'static str> =
            self.jobs.iter().filter(|((j, _), e)| j.site() && long(e) && matches!(e.trouble, Trouble::Unreachable(_))).map(|((j, _), _)| j.place()).collect();
        let together = down.len() >= 2;
        if together {
            let since = self.jobs.iter().filter(|((j, _), e)| j.site() && long(e)).map(|(_, e)| e.since).min().unwrap_or(now);
            lines.push(Line { kind: "site-down".into(), text: SITE_DOWN.into(), tone: "down", report: true, since });
        }
        let mut places: BTreeSet<&'static str> = BTreeSet::new();
        let mut site_noted = false;
        for ((job, _), e) in self.jobs.iter_mut() {
            if !long(e) {
                continue;
            }
            let in_one = together && job.site() && matches!(e.trouble, Trouble::Unreachable(_));
            if !in_one && places.insert(job.place()) {
                let text = match &e.trouble {
                    Trouble::Unreachable(_) => job.down_text().to_string(),
                    Trouble::Server(m) if *job == Job::AddonUpdates => format!("{m} EverlookDB tries again by itself."),
                    Trouble::Server(m) => format!("{} isn't working right now ({m}). EverlookDB keeps trying by itself.", job.name()),
                    Trouble::Local(_) => continue,
                };
                lines.push(Line { kind: format!("{}-down", job.place()), text, tone: "down", report: true, since: e.since });
            }
            if !e.noted {
                e.noted = true;
                if in_one {
                    if !site_noted {
                        site_noted = true;
                        new.push(SITE_DOWN.to_string());
                    }
                } else {
                    new.push(format!("{}: {} (failing for {} min)", job.name(), e.trouble.text(), (now - e.since) / 60));
                }
            }
        }
        (lines, new)
    }
}

static HEALTH: Mutex<Health> = Mutex::new(Health::new());
/// Counts the times an everlookdb.com job worked again after failing: the other jobs (uploads, queued reports) try
/// again at once instead of waiting out their backoff.
static BACK: AtomicU64 = AtomicU64::new(0);

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn health() -> std::sync::MutexGuard<'static, Health> {
    HEALTH.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn fail(job: Job, key: &str, trouble: Trouble) {
    health().fail(job, key, trouble, now());
}

pub fn fail_since(job: Job, key: &str, trouble: Trouble, since: i64) {
    health().fail(job, key, trouble, since);
}

pub fn ok(job: Job, key: &str) {
    if health().ok(job, key) && job.site() {
        BACK.fetch_add(1, Ordering::SeqCst);
        crate::config::log(&format!("{}: working again", job.name().to_lowercase()));
    }
}

pub fn keep(job: Job, keys: &[String]) {
    health().keep(job, keys);
}

/// How many times everlookdb.com came back (see BACK).
pub fn back() -> u64 {
    BACK.load(Ordering::SeqCst)
}

/// What the window's inbox shows now; what shows for the first time is noted for problem reports.
pub fn lines() -> Vec<Line> {
    let (lines, new) = health().view(now());
    for m in new {
        crate::report::note_error("health", &m);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_800_000_000;
    fn down(m: &str) -> Trouble {
        Trouble::Unreachable(m.into())
    }

    #[test]
    fn classification() {
        for c in [408, 429, 500, 502, 503, 504, 599] {
            assert!(transient_status(c), "{c}");
            assert!(matches!(Trouble::of_status(c, "x"), Trouble::Unreachable(_)));
        }
        for c in [400, 401, 403, 404, 413, 415, 422] {
            assert!(!transient_status(c), "{c}");
            assert!(matches!(Trouble::of_status(c, "x"), Trouble::Server(_)));
        }
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(local_words(&denied, "the Price Feed addon").contains("isn't allowed"));
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(local_words(&gone, "the Price Feed addon").starts_with("The Price Feed addon isn't there"));
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert!(local_words(&full, "the download").contains("disk is full"));
        if cfg!(windows) {
            let locked = std::io::Error::from_raw_os_error(32);
            assert!(local_words(&locked, "the Auctions file").starts_with("WoW or another program is using the Auctions file"));
            assert!(local_words(&std::io::Error::from_raw_os_error(112), "x").contains("disk is full"));
        }
    }

    #[test]
    fn transient_failures_are_quiet_until_the_threshold() {
        let mut h = Health::new();
        h.fail(Job::PriceFeed, "", down("can't reach everlookdb.com"), T);
        h.fail(Job::PriceFeed, "", down("the server answered 502"), T + 60);
        assert_eq!(h.failing(Job::PriceFeed, ""), Some(T), "the clock runs from the first failure");
        for at in [T, T + 60, T + QUIET_FOR - 1] {
            let (lines, new) = h.view(at);
            assert!(lines.is_empty() && new.is_empty(), "nothing shown or noted at {at}");
        }
        let (lines, new) = h.view(T + QUIET_FOR);
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].kind.as_str(), lines[0].tone, lines[0].report), ("price-feed-down", "down", true));
        assert!(lines[0].text.starts_with("Can't reach everlookdb.com"));
        assert_eq!(new.len(), 1, "noted once");
        let (lines, new) = h.view(T + QUIET_FOR + 600);
        assert_eq!((lines.len(), new.len()), (1, 0), "still shown, not noted again");
    }

    #[test]
    fn recovery_hides_the_line() {
        let mut h = Health::new();
        h.fail(Job::Uploads, "Auctions", down("timed out"), T);
        assert_eq!(h.view(T + QUIET_FOR + 1).0.len(), 1);
        assert!(h.ok(Job::Uploads, "Auctions"), "it had been failing");
        assert!(h.view(T + QUIET_FOR + 2).0.is_empty(), "nothing at all once it works");
        assert!(!h.ok(Job::Uploads, "Auctions"));
        // a short failure that recovered never shows
        h.fail(Job::Catalog, "", down("x"), T);
        h.ok(Job::Catalog, "");
        assert!(h.view(T + 10 * QUIET_FOR).0.is_empty());
    }

    #[test]
    fn local_problems_show_at_once() {
        let mut h = Health::new();
        h.fail(Job::PriceFeed, "", Trouble::Local("EverlookDB isn't allowed to change the Price Feed addon.".into()), T);
        let (lines, new) = h.view(T);
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].tone, lines[0].report, lines[0].kind.as_str()), ("local", false, "price-feed-local"));
        assert_eq!(new.len(), 1);
        assert!(h.view(T + 1).1.is_empty(), "noted once");
        // fixed: gone, and ok() doesn't count it as the site coming back
        assert!(!h.ok(Job::PriceFeed, ""));
        assert!(h.view(T + 2).0.is_empty());
    }

    #[test]
    fn a_site_outage_is_one_line() {
        let mut h = Health::new();
        h.fail(Job::Uploads, "Auctions", down("can't reach the server"), T);
        h.fail(Job::Uploads, "Game data", down("can't reach the server"), T + 5);
        h.fail(Job::Disenchants, "Auctions", down("can't reach the server"), T + 9);
        h.fail(Job::PriceFeed, "", down("can't reach everlookdb.com"), T + 30);
        h.fail(Job::Catalog, "", down("catalog"), T + 60);
        h.fail(Job::AppUpdate, "", down("app"), T + 90);
        let (lines, new) = h.view(T + QUIET_FOR + 100);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!((lines[0].kind.as_str(), lines[0].text.as_str(), lines[0].since), ("site-down", SITE_DOWN, T));
        assert_eq!(new, vec![SITE_DOWN.to_string()], "one note for the outage");
        // only the uploads' place is long: its own line, one for both files and the disenchants
        let mut h = Health::new();
        h.fail(Job::Uploads, "Auctions", down("x"), T);
        h.fail(Job::Uploads, "Game data", down("x"), T);
        h.fail(Job::Disenchants, "Auctions", down("x"), T);
        h.fail(Job::PriceFeed, "", down("x"), T + QUIET_FOR);
        let (lines, _) = h.view(T + QUIET_FOR + 1);
        assert_eq!(lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), vec!["uploads-down"]);
        // download servers (addon updates) aren't everlookdb.com: their own line
        let mut h = Health::new();
        h.fail(Job::PriceFeed, "", down("x"), T);
        h.fail(Job::Catalog, "", down("x"), T);
        h.fail(Job::AddonUpdates, "pawn", down("x"), T);
        h.fail(Job::AddonUpdates, "dbm", down("x"), T);
        let (lines, _) = h.view(T + QUIET_FOR);
        assert_eq!(lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), vec!["site-down", "addons-down"]);
    }

    #[test]
    fn server_answers_and_keep() {
        let mut h = Health::new();
        h.fail(Job::AddonUpdates, "a", Trouble::Server("Couldn't update Pawn: the download holds no addon.".into()), T);
        h.fail(Job::AddonUpdates, "b", down("x"), T);
        h.keep(Job::AddonUpdates, &["b".to_string()]);
        assert_eq!(h.failing(Job::AddonUpdates, "a"), None);
        assert_eq!(h.failing(Job::AddonUpdates, "b"), Some(T));
        let mut h = Health::new();
        h.fail(Job::PriceFeed, "", Trouble::Server("the server answered 404".into()), T);
        let (lines, _) = h.view(T + QUIET_FOR);
        assert_eq!(lines[0].text, "The Price Feed isn't working right now (the server answered 404). EverlookDB keeps trying by itself.");
        // a local problem after a transient one starts its own clock and shows at once
        h.fail(Job::PriceFeed, "", Trouble::Local("full".into()), T + QUIET_FOR + 5);
        let (lines, new) = h.view(T + QUIET_FOR + 5);
        assert_eq!((lines.len(), lines[0].tone, new.len()), (1, "local", 1));
    }
}
