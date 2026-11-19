// The addon catalog: everlookdb.com builds it from GitHub releases and Tukui (docs/addon-manager.md has the
// contract). The request carries nothing about the player: no cookies, no ids, just "EverlookDB/<version>" as the
// user agent and the ETag of the copy we already have. The answer is cached in the app's data folder.

use crate::config;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Catalog {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub addons: Vec<Addon>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Addon {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub released: String,
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub ours: bool,
    /// a square picture the source gives for the addon (https), when the catalog has one
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    /// from a GitHub repo the player added (github.rs), not from the catalog
    #[serde(default)]
    pub custom: bool,
    /// `version` as the window shows it (see `version_label`); set by `fetch`
    #[serde(default, skip_deserializing)]
    pub version_label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct File {
    #[serde(default)]
    pub flavors: Vec<String>,
    #[serde(default)]
    pub interfaces: Vec<u32>,
    pub url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
}

impl Addon {
    /// The file for an install that accepts these flavors (best first). When several files fit a flavor (Pawn ships
    /// `Pawn-…-Forever.zip` beside a main zip that also lists Forever), the one built for it wins: its name says the
    /// flavor, then the one made for the fewest flavors. For a repo the player added, a zip whose game versions
    /// nobody could tell (no interface numbers) is taken when none says it is for this one.
    pub fn file_for(&self, accepts: &[String]) -> Option<&File> {
        let known = self.known_file_for(accepts);
        if known.is_some() || !self.custom {
            return known;
        }
        self.files.iter().find(|f| f.flavors.is_empty())
    }

    fn known_file_for(&self, accepts: &[String]) -> Option<&File> {
        accepts.iter().find_map(|f| {
            let word = match f.as_str() {
                "classic_era" => "vanilla",
                "retail" => "mainline",
                other => other,
            };
            self.files
                .iter()
                .filter(|file| file.flavors.iter().any(|x| x == f))
                .min_by_key(|file| {
                    let name = file.name.to_ascii_lowercase();
                    (!(name.contains(f.as_str()) || name.contains(word)), file.flavors.len())
                })
        })
    }
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Addon> {
        self.addons.iter().find(|a| a.id == id)
    }
}

/// Where the catalog came from this time.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// fresh from the server (or the server said our copy is current)
    Live,
    /// the server didn't answer; the copy from last time
    Cache,
    /// a development build with no server catalog yet: the sample in fixtures/
    Sample,
    /// the server's catalog is in a newer format than this app reads: the app needs updating (the copy from last
    /// time is used meanwhile, if there is one)
    Outdated,
    None,
}

/// Why a fetch failed.
#[derive(Debug, PartialEq)]
pub enum FetchError {
    /// the catalog's format is newer than this app knows
    Newer(u32),
    Other(String),
}

pub fn url(server: &str) -> String {
    std::env::var("EVERLOOKDB_CATALOG").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{}/api/v1/addons/catalog", server.trim_end_matches('/')))
}

fn cache_path() -> std::path::PathBuf {
    config::dir().join("catalog.json")
}
fn etag_path() -> std::path::PathBuf {
    config::dir().join("catalog.etag")
}

pub fn cached() -> Option<Catalog> {
    std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice::<Catalog>(&b).ok()).filter(|c| c.version == 1)
}

/// The copy from last time, with its version labels, without asking the server: what the window shows at once while
/// the fresh list loads.
pub fn cached_labelled() -> Option<Catalog> {
    let mut c = cached()?;
    label(&mut c);
    Some(c)
}

fn label(c: &mut Catalog) {
    for a in c.addons.iter_mut() {
        a.version_label = version_label(&a.version);
    }
}

fn sample() -> Option<Catalog> {
    if cfg!(debug_assertions) {
        serde_json::from_str(include_str!("../fixtures/catalog-sample.json")).ok()
    } else {
        None
    }
}

/// A release name as a version to show: "Pawn-2.14.3" -> "2.14.3", "Plater-v664" -> "v664", "Classic-0.1.1" ->
/// "0.1.1", "120100.151-Release" -> "120100.151", "bundle/v12.9.0+v1.1.1" -> "v12.9.0". A word in front (the
/// addon's name, a flavor) and a "release" word at the end go; the rest stays as the author wrote it.
pub fn version_label(raw: &str) -> String {
    let mut v = raw.trim();
    // a tag path ("bundle/v12.9.0") and build metadata ("+v1.1.1")
    v = v.rsplit('/').next().unwrap_or(v);
    v = v.split('+').next().unwrap_or(v).trim();
    // "-Release", "_release", " release" at the end
    for sep in ['-', '_', ' ', '.'] {
        if let Some((head, tail)) = v.rsplit_once(sep) {
            if tail.eq_ignore_ascii_case("release") && !head.is_empty() {
                v = head;
                break;
            }
        }
    }
    // words in front, up to the first part that starts with a number (or v and a number)
    let starts_number = |s: &str| {
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        s.chars().next().is_some_and(|c| c.is_ascii_digit())
    };
    let mut rest = v;
    while !starts_number(rest) {
        let Some((word, tail)) = rest.split_once(['-', '_', ' ']) else { break };
        if word.is_empty() || !word.chars().all(|c| c.is_alphanumeric() || c == '.') || word.chars().any(|c| c.is_ascii_digit()) {
            break;
        }
        rest = tail.trim_start();
    }
    if starts_number(rest) {
        v = rest;
    }
    if v.is_empty() { raw.trim().to_string() } else { v.to_string() }
}

/// Fetch the catalog (If-None-Match with the cached copy's ETag), falling back to the cache, then (dev builds) the
/// sample. Each addon's `version_label` is filled in.
pub fn fetch(server: &str) -> (Option<Catalog>, Origin) {
    let (mut c, origin) = fetch_any(server);
    if let Some(c) = c.as_mut() {
        label(c);
    }
    (c, origin)
}

fn fetch_any(server: &str) -> (Option<Catalog>, Origin) {
    let target = url(server);
    // a local file, for testing: EVERLOOKDB_CATALOG=C:\path\catalog.json
    if !target.starts_with("http") {
        let c = std::fs::read(&target).ok().and_then(|b| serde_json::from_slice::<Catalog>(&b).ok());
        return (c, Origin::Live);
    }
    let have = cached();
    match fetch_http(&target, have.is_some()) {
        Ok(Some(c)) => (Some(c), Origin::Live),
        Ok(None) => (have, Origin::Live),
        Err(e) => {
            config::log(&format!("catalog: {e:?}"));
            fallback(&e, have, sample)
        }
    }
}

/// What to use when the server's catalog couldn't be had: the copy from last time, then (dev builds) the sample.
/// A catalog newer than the app says so (the app needs an update), whatever copy is used meanwhile.
fn fallback(e: &FetchError, have: Option<Catalog>, sample: fn() -> Option<Catalog>) -> (Option<Catalog>, Origin) {
    let newer = matches!(e, FetchError::Newer(_));
    match have {
        Some(c) => (Some(c), if newer { Origin::Outdated } else { Origin::Cache }),
        None if newer => (None, Origin::Outdated),
        None => match sample() {
            Some(c) => (Some(c), Origin::Sample),
            None => (None, Origin::None),
        },
    }
}

/// Ok(None): 304, our copy is current.
fn fetch_http(target: &str, have: bool) -> Result<Option<Catalog>, FetchError> {
    let other = |e: String| FetchError::Other(e);
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| other(e.to_string()))?;
    let mut req = http.get(target).header("accept", "application/json");
    if have {
        if let Ok(tag) = std::fs::read_to_string(etag_path()) {
            req = req.header("if-none-match", tag.trim());
        }
    }
    let resp = req.send().map_err(|e| other(format!("can't reach {target}: {e}")))?;
    match resp.status().as_u16() {
        304 => return Ok(None),
        200 => {}
        code => return Err(other(format!("{target} answered {code}"))),
    }
    let tag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = resp.bytes().map_err(|e| other(e.to_string()))?;
    check_version(&bytes)?;
    let c: Catalog = serde_json::from_slice(&bytes).map_err(|e| other(format!("catalog is not valid: {e}")))?;
    let _ = std::fs::create_dir_all(config::dir());
    let _ = std::fs::write(cache_path(), &bytes);
    match tag {
        Some(t) => {
            let _ = std::fs::write(etag_path(), t);
        }
        None => {
            let _ = std::fs::remove_file(etag_path());
        }
    }
    Ok(Some(c))
}

/// The catalog's format version, read before the rest (a newer format may not parse as this one).
fn check_version(bytes: &[u8]) -> Result<(), FetchError> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        version: u32,
    }
    let head: Head = serde_json::from_slice(bytes).map_err(|e| FetchError::Other(format!("catalog is not valid: {e}")))?;
    match head.version {
        1 => Ok(()),
        v if v > 1 => Err(FetchError::Newer(v)),
        v => Err(FetchError::Other(format!("catalog version {v} is not valid"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_catalog_asks_for_an_app_update() {
        assert_eq!(check_version(br#"{"version":2,"addons":"a new shape"}"#), Err(FetchError::Newer(2)));
        assert_eq!(check_version(br#"{"version":1}"#), Ok(()));
        assert!(matches!(check_version(b"nope"), Err(FetchError::Other(_))));
        let have = || Some(Catalog { version: 1, ..Default::default() });
        let none = || None;
        // newer: say so, keep showing the copy from last time
        let (c, o) = fallback(&FetchError::Newer(2), have(), none);
        assert!(c.is_some());
        assert_eq!(o, Origin::Outdated);
        assert_eq!(fallback(&FetchError::Newer(2), None, have).1, Origin::Outdated, "never the sample when the app is too old");
        // offline: the copy from last time
        assert_eq!(fallback(&FetchError::Other("offline".into()), have(), none).1, Origin::Cache);
        assert_eq!(fallback(&FetchError::Other("offline".into()), None, none).1, Origin::None);
        assert_eq!(fallback(&FetchError::Other("offline".into()), None, have).1, Origin::Sample);
    }

    #[test]
    fn sample_parses_and_picks_files() {
        let c: Catalog = serde_json::from_str(include_str!("../fixtures/catalog-sample.json")).unwrap();
        assert_eq!(c.version, 1);
        let bw = c.get("github:BigWigsMods/BigWigs").unwrap();
        assert!(bw.file_for(&["forever".into()]).is_some());
        assert!(bw.folders.contains(&"BigWigs_Core".to_string()));
        let wa = c.get("github:WeakAuras/WeakAuras2").unwrap();
        assert!(wa.file_for(&["forever".into()]).is_none());
        assert!(wa.file_for(&["ptr".into(), "classic".into()]).is_some());
        assert!(c.addons.iter().any(|a| a.ours));
    }

    #[test]
    fn version_labels() {
        for (raw, shown) in [
            ("120100.151-Release", "120100.151"),
            ("120100.100-release", "120100.100"),
            ("r274-release", "r274"),
            ("Plater-v664", "v664"),
            ("Pawn-2.14.3", "2.14.3"),
            ("Classic-0.1.1", "0.1.1"),
            ("bundle/v12.9.0+v1.1.1", "v12.9.0"),
            ("v427.9", "v427.9"),
            ("4.18.4.1", "4.18.4.1"),
            ("v11.2.5-1.0.0", "v11.2.5-1.0.0"),
            ("v4.3.9-classic", "v4.3.9-classic"),
            ("2.9.0-RC4", "2.9.0-RC4"),
            ("v1.8.7-c", "v1.8.7-c"),
            ("12.1.5-04", "12.1.5-04"),
            ("r835", "r835"),
            ("v2026.42.1", "v2026.42.1"),
            ("Release", "Release"),
            ("beta", "beta"),
            ("", ""),
        ] {
            assert_eq!(version_label(raw), shown, "{raw}");
        }
    }

    #[test]
    fn prefers_the_file_built_for_the_flavor() {
        let file = |name: &str, flavors: &[&str]| File {
            flavors: flavors.iter().map(|s| s.to_string()).collect(),
            interfaces: vec![],
            url: format!("https://example.test/{name}"),
            name: name.into(),
            size: 1,
        };
        let pawn = Addon {
            files: vec![file("Pawn-2.14.3.zip", &["retail", "classic", "forever"]), file("Pawn-2.14.3-Forever.zip", &["forever"])],
            ..serde_json::from_str(r#"{"id":"github:VgerMods/Pawn","name":"Pawn"}"#).unwrap()
        };
        assert_eq!(pawn.file_for(&["forever".into()]).unwrap().name, "Pawn-2.14.3-Forever.zip");
        assert_eq!(pawn.file_for(&["retail".into()]).unwrap().name, "Pawn-2.14.3.zip");
    }
}
