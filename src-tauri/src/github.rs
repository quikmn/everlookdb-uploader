// Install from GitHub: the player pastes an addon's GitHub link and the app installs the newest release, then keeps
// it updated like a catalog addon. The repo is read by everlookdb.com (GET /api/v1/addons/github/{owner}/{repo}),
// which answers with the catalog's Addon shape, so the app never asks GitHub's API itself (its limit for apps without
// a token is 60 calls an hour, shared by everyone behind one address). The zip still comes straight from GitHub.
//
// The repos the player added are kept in config.json (`github_repos`); their newest answers in github-addons.json, so
// they are there offline and at start. On every catalog refresh they are asked for again and added to the catalog
// (marked `custom`); a repo the catalog itself lists is left to the catalog.

use crate::catalog::{version_label, Addon, Catalog};
use crate::config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// `owner/repo` from what the player typed: a github.com link (with or without https://, www., a trailing `.git`,
/// `/releases`, `/tree/main/...`, `?query` or `#anchor`), `git@github.com:owner/repo.git`, or plain `owner/repo`.
/// Names must follow GitHub's rules; links to any other site are refused.
pub fn parse_repo(input: &str) -> Result<String, String> {
    const HOW: &str = "Paste the addon's GitHub link, like https://github.com/owner/repo.";
    let s = input.trim();
    if s.is_empty() {
        return Err(HOW.into());
    }
    let s = s.split(['?', '#']).next().unwrap_or(s);
    // ASCII lowercasing keeps every byte where it is, so `s` can be cut where `lower` matched
    let lower = s.to_ascii_lowercase();
    let cut = |rest: &str| &s[s.len() - rest.len()..];
    let path = if let Some(rest) = lower.strip_prefix("git@github.com:") {
        cut(rest)
    } else {
        let scheme = ["https://", "http://"].iter().find_map(|p| lower.strip_prefix(p));
        let rest = scheme.unwrap_or(&lower);
        if let Some(path) = rest.strip_prefix("www.github.com/").or_else(|| rest.strip_prefix("github.com/")) {
            cut(path)
        } else if scheme.is_some() || rest.contains("://") || rest.split('/').next().is_some_and(|host| host.contains('.')) {
            // another site (gitlab.com/..., curseforge.com/..., github.com.example/...)
            return Err(format!("Only GitHub links work here. {HOW}"));
        } else {
            s
        }
    };
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else { return Err(HOW.into()) };
    let repo = if repo.len() > 4 && repo.to_ascii_lowercase().ends_with(".git") { &repo[..repo.len() - 4] } else { repo };
    // what may follow the repo name: a page of the repo (releases, tree/main ...), never another repo
    let owner_ok = (1..=39).contains(&owner.len()) && !owner.starts_with('-') && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = (1..=100).contains(&repo.len())
        && repo != "."
        && repo != ".."
        && repo.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    if !owner_ok || !repo_ok {
        return Err(format!("That isn't a GitHub repository. {HOW}"));
    }
    // github.com/settings/..., /orgs/..., /marketplace/...: pages of GitHub, not repos
    const NOT_OWNERS: [&str; 12] =
        ["settings", "orgs", "marketplace", "explore", "topics", "sponsors", "notifications", "login", "about", "features", "collections", "trending"];
    if NOT_OWNERS.contains(&owner.to_ascii_lowercase().as_str()) {
        return Err(format!("That isn't a GitHub repository. {HOW}"));
    }
    Ok(format!("{owner}/{repo}"))
}

/// The addon id a repo has in the catalog.
pub fn id_of(repo: &str) -> String {
    format!("github:{repo}")
}

/// Ask everlookdb.com for the addon in a repo's newest release.
pub fn lookup(server: &str, repo: &str) -> Result<Addon, String> {
    let url = format!("{}/api/v1/addons/github/{repo}", server.trim_end_matches('/'));
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http.get(&url).header("accept", "application/json").send().map_err(|_| "Couldn't reach everlookdb.com. Check your connection and try again.".to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().map_err(|e| e.to_string())?;
    if !status.is_success() {
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("everlookdb.com answered {}. Try again later.", status.as_u16()));
        return Err(message);
    }
    let mut a: Addon = serde_json::from_slice(&bytes).map_err(|e| format!("everlookdb.com sent something this app can't read: {e}"))?;
    if !a.id.starts_with("github:") || a.source != "github" {
        return Err("everlookdb.com sent something that isn't a GitHub addon.".into());
    }
    a.version_label = version_label(&a.version);
    Ok(a)
}

/// Only zips attached to a GitHub release are installed from a repo the player added (GitHub sends the download on
/// from objects.githubusercontent.com, which is fine).
pub fn safe_download(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://github.com/") else { return false };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() >= 6 && parts[2] == "releases" && parts[3] == "download" && !parts.iter().any(|p| p.is_empty() || *p == "..")
}

// ---------------------------------------------------------------------------------------------- what we remember

/// The newest answer for each added repo (github-addons.json), by repo (as GitHub writes it).
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Saved {
    #[serde(default)]
    pub addons: BTreeMap<String, Addon>,
}

impl Saved {
    fn path() -> std::path::PathBuf {
        config::dir().join("github-addons.json")
    }
    pub fn load() -> Saved {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        let path = Self::path();
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::create_dir_all(config::dir());
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Remember a repo the player installed from (config.json and its answer); nothing changes if it is there already.
pub fn remember(cfg: &mut config::Config, repo: &str, addon: &Addon) {
    if !cfg.github_repos.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
        cfg.github_repos.push(repo.to_string());
    }
    let mut saved = Saved::load();
    saved.addons.retain(|k, _| !k.eq_ignore_ascii_case(repo));
    saved.addons.insert(repo.to_string(), Addon { custom: true, ..addon.clone() });
    saved.save();
}

/// Forget a repo (removed from every install). Returns whether it was remembered.
pub fn forget(cfg: &mut config::Config, repo: &str) -> bool {
    let before = cfg.github_repos.len();
    cfg.github_repos.retain(|r| !r.eq_ignore_ascii_case(repo));
    let mut saved = Saved::load();
    saved.addons.retain(|k, _| !k.eq_ignore_ascii_case(repo));
    saved.save();
    cfg.github_repos.len() != before
}

/// The added repo an addon id belongs to, if it is one.
pub fn repo_of<'a>(cfg: &'a config::Config, id: &str) -> Option<&'a String> {
    let repo = id.strip_prefix("github:")?;
    cfg.github_repos.iter().find(|r| r.eq_ignore_ascii_case(repo))
}

/// Whether the catalog (not counting added repos) has this addon id, in any letter case.
pub fn in_catalog(catalog: Option<&Catalog>, id: &str) -> Option<Addon> {
    catalog?.addons.iter().find(|a| !a.custom && a.id.eq_ignore_ascii_case(id)).cloned()
}

/// The newest answer for each added repo the catalog doesn't list: asked for again (`ask`), or the one from last time
/// when that fails. Saves what it learned.
pub fn refresh(repos: &[String], catalog: Option<&Catalog>, ask: &dyn Fn(&str) -> Result<Addon, String>) -> Vec<Addon> {
    let mut saved = Saved::load();
    let mut out = Vec::new();
    for repo in repos {
        if in_catalog(catalog, &id_of(repo)).is_some() {
            continue;
        }
        match ask(repo) {
            Ok(a) => {
                saved.addons.retain(|k, _| !k.eq_ignore_ascii_case(repo));
                saved.addons.insert(repo.clone(), Addon { custom: true, ..a });
            }
            Err(e) => config::log(&format!("github {repo}: {e}")),
        }
        if let Some((_, a)) = saved.addons.iter().find(|(k, _)| k.eq_ignore_ascii_case(repo)) {
            out.push(Addon { custom: true, version_label: version_label(&a.version), ..a.clone() });
        }
    }
    saved.addons.retain(|k, _| repos.iter().any(|r| r.eq_ignore_ascii_case(k)));
    saved.save();
    out
}

/// Add the added repos' addons to the catalog (those it doesn't list itself), replacing older copies of them.
pub fn merge(catalog: &mut Catalog, custom: &[Addon]) {
    catalog.addons.retain(|a| !a.custom);
    for a in custom {
        if !catalog.addons.iter().any(|c| c.id.eq_ignore_ascii_case(&a.id)) {
            catalog.addons.push(Addon { custom: true, ..a.clone() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_links() {
        for (input, repo) in [
            ("https://github.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/releases", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/releases/tag/v0.1.0", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/releases/latest", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/tree/main", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/tree/main/Locales", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets/blob/main/README.md#install", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets.git", "quikmn/EverlookPets"),
            ("https://github.com/quikmn/EverlookPets?tab=readme-ov-file", "quikmn/EverlookPets"),
            ("http://github.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("HTTPS://GitHub.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("https://www.github.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("github.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("www.github.com/quikmn/EverlookPets", "quikmn/EverlookPets"),
            ("  quikmn/EverlookPets  ", "quikmn/EverlookPets"),
            ("quikmn/EverlookPets.git", "quikmn/EverlookPets"),
            ("git@github.com:quikmn/EverlookPets.git", "quikmn/EverlookPets"),
            ("https://github.com/BigWigsMods/BigWigs", "BigWigsMods/BigWigs"),
            ("https://github.com/a-b/x.y_z-1", "a-b/x.y_z-1"),
            ("https://github.com/some/.github", "some/.github"),
        ] {
            assert_eq!(parse_repo(input).as_deref(), Ok(repo), "{input}");
        }
    }

    #[test]
    fn rejects_junk() {
        for input in [
            "",
            "   ",
            "EverlookPets",
            "https://github.com/",
            "https://github.com/quikmn",
            "https://gitlab.com/quikmn/EverlookPets",
            "https://evil.example/github.com/quikmn/EverlookPets",
            "https://github.com.evil.example/quikmn/EverlookPets",
            "https://raw.githubusercontent.com/quikmn/EverlookPets/main/x.toc",
            "https://www.curseforge.com/wow/addons/bigwigs",
            "curseforge.com/wow/addons",
            "ftp://github.com/quikmn/EverlookPets",
            "file:///C:/Windows/x",
            "https://github.com/-bad/repo",
            "https://github.com/bad_owner/repo",
            "https://github.com/owner/re po",
            "https://github.com/owner/..",
            "https://github.com/owner/.",
            "https://github.com/settings/profile",
            "https://github.com/orgs/x",
            "../../etc/passwd",
            "owner/repo%2F..",
            "C:\\Games\\World of Warcraft",
            "https://github.com/ownerownerownerownerownerownerownerowner1/x",
        ] {
            assert!(parse_repo(input).is_err(), "{input:?} -> {:?}", parse_repo(input));
        }
    }

    #[test]
    fn only_release_downloads() {
        assert!(safe_download("https://github.com/quikmn/EverlookPets/releases/download/v0.1.0/EverlookPets.zip"));
        assert!(!safe_download("http://github.com/quikmn/EverlookPets/releases/download/v0.1.0/EverlookPets.zip"));
        assert!(!safe_download("https://github.com.evil.example/quikmn/x/releases/download/v1/a.zip"));
        assert!(!safe_download("https://evil.example/quikmn/x/releases/download/v1/a.zip"));
        assert!(!safe_download("https://github.com/quikmn/x/archive/refs/heads/main.zip"));
        assert!(!safe_download("https://github.com/quikmn/x/releases/download/../../a.zip"));
        assert!(!safe_download("https://objects.githubusercontent.com/x.zip"));
    }

    fn addon(id: &str, version: &str) -> Addon {
        serde_json::from_value(serde_json::json!({ "id": id, "name": id.rsplit('/').next().unwrap(), "source": "github", "version": version,
            "files": [{ "flavors": [], "url": format!("https://github.com/{}/releases/download/{version}/a.zip", &id[7..]), "name": "a.zip", "size": 1 }] }))
        .unwrap()
    }

    #[test]
    fn added_repos_are_kept_and_merged() {
        // the settings folder is set once per run; no other test writes these files
        config::init_dir(std::env::temp_dir().join(format!("edb-github-{}", std::process::id())));
        let _ = std::fs::remove_file(Saved::path());

        let mut cfg = config::Config::default();
        let pets = addon("github:quikmn/EverlookPets", "v0.1.0");
        remember(&mut cfg, "quikmn/EverlookPets", &pets);
        remember(&mut cfg, "quikmn/everlookpets", &pets);
        assert_eq!(cfg.github_repos, vec!["quikmn/EverlookPets"], "one entry per repo, whatever the letter case");
        assert_eq!(repo_of(&cfg, "github:QUIKMN/everlookpets").map(String::as_str), Some("quikmn/EverlookPets"));
        assert_eq!(repo_of(&cfg, "github:BigWigsMods/BigWigs"), None);
        // config.json keeps the list across restarts, and an old one without it reads as none
        let back: config::Config = serde_json::from_slice(&serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.github_repos, cfg.github_repos);
        let old: config::Config = serde_json::from_str(r#"{"auto_update": true}"#).unwrap();
        assert!(old.github_repos.is_empty());

        // a refresh asks for each added repo; when that fails, last time's answer is used
        let newer = addon("github:quikmn/EverlookPets", "v0.2.0");
        let got = refresh(&cfg.github_repos, None, &|_| Ok(newer.clone()));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].version, "v0.2.0");
        assert!(got[0].custom);
        let offline = refresh(&cfg.github_repos, None, &|_| Err("offline".into()));
        assert_eq!(offline[0].version, "v0.2.0", "kept from last time");
        assert_eq!(Saved::load().addons["quikmn/EverlookPets"].version, "v0.2.0");

        // merged into the catalog, once, after its own addons; a repo the catalog lists is left to the catalog
        let mut cat = Catalog { version: 1, addons: vec![addon("github:BigWigsMods/BigWigs", "v1")], ..Default::default() };
        merge(&mut cat, &got);
        merge(&mut cat, &got);
        assert_eq!(cat.addons.len(), 2);
        assert!(cat.get("github:quikmn/EverlookPets").unwrap().custom);
        assert!(in_catalog(Some(&cat), "github:quikmn/everlookpets").is_none(), "added repos don't count as the catalog's");
        assert!(in_catalog(Some(&cat), "github:bigwigsmods/bigwigs").is_some());
        let listed = Catalog { version: 1, addons: vec![addon("github:quikmn/EverlookPets", "v0.3.0")], ..Default::default() };
        let asked = std::cell::Cell::new(0);
        assert!(refresh(&cfg.github_repos, Some(&listed), &|_| {
            asked.set(asked.get() + 1);
            Ok(newer.clone())
        })
        .is_empty());
        assert_eq!(asked.get(), 0, "a repo the catalog lists isn't asked for");
        let mut listed2 = Catalog { version: 1, addons: listed.addons.clone(), ..Default::default() };
        merge(&mut listed2, &got);
        assert_eq!(listed2.addons.len(), 1);
        assert!(!listed2.addons[0].custom);

        // an unknown-flavor file installs anyway, but only from an added repo
        let accepts = vec!["forever".to_string()];
        assert!(got[0].file_for(&accepts).is_some());
        assert!(Addon { custom: false, ..got[0].clone() }.file_for(&accepts).is_none());

        // forgetting removes it from both files
        assert!(forget(&mut cfg, "QUIKMN/EverlookPets"));
        assert!(cfg.github_repos.is_empty());
        assert!(Saved::load().addons.is_empty());
        assert!(!forget(&mut cfg, "quikmn/EverlookPets"));
        let _ = std::fs::remove_file(Saved::path());
    }
}
