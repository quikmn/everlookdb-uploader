// Reading what is installed: `Interface/AddOns/*/*.toc`, grouped into addons (BigWigs + BigWigs_Core + ...).
// Only reads.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// What a `.toc` says about an addon folder.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Toc {
    pub title: Option<String>,
    pub version: Option<String>,
    pub author: Option<String>,
    pub notes: Option<String>,
    pub interfaces: Vec<u32>,
    /// required dependencies (Dependencies, RequiredDeps, Dep*)
    pub deps: Vec<String>,
    pub curse_id: Option<String>,
    pub wago_id: Option<String>,
    pub wowi_id: Option<String>,
    pub website: Option<String>,
}

/// Remove WoW's escape codes: colours (|cAARRGGBB ... |r), textures (|T...|t), atlases (|A...|a), line breaks (|n).
pub fn strip_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '|' {
            out.push(c);
            continue;
        }
        match it.peek().copied() {
            Some('c') | Some('C') => {
                it.next();
                for _ in 0..8 {
                    if it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                        it.next();
                    }
                }
            }
            Some('r') | Some('R') => {
                it.next();
            }
            Some('n') => {
                it.next();
                out.push(' ');
            }
            Some(open @ ('T' | 'A')) => {
                it.next();
                let close = open.to_ascii_lowercase();
                // skip to the matching |t / |a
                let mut prev = ' ';
                for c in it.by_ref() {
                    if prev == '|' && c == close {
                        break;
                    }
                    prev = c;
                }
            }
            Some('|') => {
                it.next();
                out.push('|');
            }
            _ => out.push('|'),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean(v: &str) -> Option<String> {
    let v = strip_codes(v.trim());
    // packager placeholders mean "unknown"
    if v.is_empty() || (v.starts_with('@') && v.ends_with('@')) {
        None
    } else {
        Some(v)
    }
}

pub fn parse_toc(text: &str) -> Toc {
    let mut t = Toc::default();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        let Some(rest) = line.strip_prefix("##") else { continue };
        let Some((key, value)) = rest.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "title" => t.title = clean(value),
            "version" => t.version = clean(value),
            "author" => t.author = clean(value),
            "notes" => t.notes = clean(value),
            "interface" => t.interfaces = value.split(',').filter_map(|n| n.trim().parse().ok()).collect(),
            "x-curse-project-id" => t.curse_id = clean(value),
            "x-wago-id" => t.wago_id = clean(value),
            "x-wowi-id" => t.wowi_id = clean(value),
            "x-website" => t.website = clean(value),
            k if k == "dependencies" || k == "requireddeps" || (k.starts_with("dep") && !k.contains('-')) => {
                t.deps.extend(value.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()));
            }
            _ => {}
        }
    }
    t
}

/// Flavor-specific toc suffixes the client prefers (`Name_Forever.toc`, `Name-Mainline.toc` ...), best first.
pub fn toc_suffixes(flavor_folder: &str) -> &'static [&'static str] {
    match flavor_folder.trim_matches('_') {
        "forever" => &["Forever"],
        "retail" | "ptr" | "xptr" | "beta" => &["Mainline"],
        "classic" | "classic_ptr" | "classic_beta" => &["Mists", "Cata", "Classic"],
        "classic_era" | "classic_era_ptr" => &["Vanilla", "Classic"],
        "anniversary" => &["TBC", "BCC", "Classic"],
        _ => &[],
    }
}

/// The toc the game would read for this folder.
pub fn toc_for(dir: &Path, folder: &str, suffixes: &[&str]) -> Option<PathBuf> {
    let tocs: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_ascii_lowercase().ends_with(".toc"))
        .collect();
    let find = |want: &str| tocs.iter().find(|n| n.eq_ignore_ascii_case(want));
    for s in suffixes {
        for sep in ['_', '-'] {
            if let Some(n) = find(&format!("{folder}{sep}{s}.toc")) {
                return Some(dir.join(n));
            }
        }
    }
    if let Some(n) = find(&format!("{folder}.toc")) {
        return Some(dir.join(n));
    }
    let mut rest = tocs.clone();
    rest.sort();
    rest.first().map(|n| dir.join(n))
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub name: String,
    pub toc: Toc,
}

/// Every addon folder (a folder with a toc) in `Interface/AddOns`. Blizzard's own folders are skipped.
pub fn scan(addons_dir: &Path, flavor_folder: &str) -> Vec<Folder> {
    let Ok(rd) = std::fs::read_dir(addons_dir) else { return Vec::new() };
    let suffixes = toc_suffixes(flavor_folder);
    let mut out: Vec<Folder> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("Blizzard_") || name.starts_with('.') {
                return None;
            }
            let toc = toc_for(&e.path(), &name, suffixes)?;
            let text = std::fs::read(&toc).ok()?;
            Some(Folder { toc: parse_toc(&String::from_utf8_lossy(&text)), name })
        })
        .collect();
    out.sort_by_key(|f| f.name.to_ascii_lowercase());
    out
}

/// Folders that belong together, shown as one addon.
#[derive(Debug, Clone)]
pub struct Group {
    /// the folder whose toc speaks for the group
    pub main: String,
    pub folders: Vec<String>,
    /// the record or catalog entry that claimed these folders
    pub key: Option<String>,
}

/// The folder the others lean on: most dependents, then the shortest name.
fn pick_main(folders: &[String], by_name: &BTreeMap<String, &Folder>) -> String {
    folders
        .iter()
        .max_by_key(|f| {
            let dependents = folders.iter().filter(|o| by_name.get(*o).is_some_and(|x| x.toc.deps.iter().any(|d| d.eq_ignore_ascii_case(f)))).count();
            (dependents, std::cmp::Reverse(f.len()))
        })
        .cloned()
        .unwrap_or_default()
}

/// Group folders: first by `known` (records, then catalog entries: key and their folders), then by name and
/// dependency (`BigWigs_Core` and `TitanXP`, which depends on `Titan`, go with `BigWigs` and `Titan`).
pub fn group(folders: &[Folder], known: &[(String, Vec<String>)]) -> Vec<Group> {
    let by_name: BTreeMap<String, &Folder> = folders.iter().map(|f| (f.name.clone(), f)).collect();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (key, list) in known {
        let present: Vec<String> = list.iter().filter(|f| by_name.contains_key(*f) && !claimed.contains(*f)).cloned().collect();
        if present.is_empty() {
            continue;
        }
        claimed.extend(present.iter().cloned());
        out.push(Group { main: pick_main(&present, &by_name), folders: present, key: Some(key.clone()) });
    }
    let rest: Vec<&Folder> = folders.iter().filter(|f| !claimed.contains(&f.name)).collect();
    let names: Vec<&str> = rest.iter().map(|f| f.name.as_str()).collect();
    let parent = |f: &Folder| -> Option<&str> {
        let lower = f.name.to_ascii_lowercase();
        names
            .iter()
            .filter(|d| d.len() >= 3 && d.len() < f.name.len() && lower.starts_with(&d.to_ascii_lowercase()))
            .filter(|d| {
                let next = f.name.as_bytes()[d.len()];
                next == b'_' || next == b'-' || f.toc.deps.iter().any(|x| x.eq_ignore_ascii_case(d))
            })
            .max_by_key(|d| d.len())
            .copied()
    };
    let parents: BTreeMap<&str, &str> = rest.iter().filter_map(|f| parent(f).map(|p| (f.name.as_str(), p))).collect();
    let root = |start: &str| -> String {
        let mut n = start.to_string();
        for _ in 0..16 {
            match parents.get(n.as_str()) {
                Some(p) => n = p.to_string(),
                None => break,
            }
        }
        n
    };
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &rest {
        groups.entry(root(&f.name)).or_default().push(f.name.clone());
    }
    for (main, mut list) in groups {
        list.sort_by_key(|n| (n != &main, n.to_ascii_lowercase()));
        out.push(Group { main, folders: list, key: None });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_fields() {
        let t = parse_toc("\u{feff}## Interface: 120105, 50504, 16001\n## Title: Titan Panel [|cffeda55f_Core_|r] |cff00aa009.3.6|r\n## Version: 9.3.6\n## Author: Titan Panel Dev Team\n## Notes: Bars.|n|TInterface/Addons/X/Art/Patreon:12:12|t |cff82c5ffpatreon|r\n## X-Curse-Project-ID: 489\n## X-Wago-ID: abc\n## RequiredDeps: Foo, Bar\n## Dependencies: Baz\n# comment: no\n");
        assert_eq!(t.title.as_deref(), Some("Titan Panel [_Core_] 9.3.6"));
        assert_eq!(t.version.as_deref(), Some("9.3.6"));
        assert_eq!(t.interfaces, vec![120105, 50504, 16001]);
        assert_eq!(t.notes.as_deref(), Some("Bars. patreon"));
        assert_eq!(t.curse_id.as_deref(), Some("489"));
        assert_eq!(t.wago_id.as_deref(), Some("abc"));
        assert_eq!(t.deps, vec!["Foo", "Bar", "Baz"]);
        assert_eq!(parse_toc("## Version: @project-version@").version, None);
        assert_eq!(strip_codes("a || b"), "a | b");
    }

    fn folder(name: &str, deps: &[&str]) -> Folder {
        Folder { name: name.into(), toc: Toc { deps: deps.iter().map(|s| s.to_string()).collect(), ..Default::default() } }
    }

    #[test]
    fn groups() {
        let folders = vec![
            folder("BigWigs", &[]),
            folder("BigWigs_Core", &[]),
            folder("Titan", &[]),
            folder("TitanXP", &["Titan"]),
            folder("EverlookDB", &[]),
            folder("EverlookDBAuctions", &[]),
            folder("ForeverAuras", &[]),
            folder("ForeverAurasOptions", &["ForeverAuras"]),
            folder("ForeverAurasTemplates", &["ForeverAuras", "ForeverAurasOptions"]),
            folder("DBM-Core", &[]),
            folder("DBM-GUI", &["DBM-Core"]),
        ];
        let known = vec![("github:DeadlyBossMods/DeadlyBossMods".to_string(), vec!["DBM-Brawlers".to_string(), "DBM-Core".into(), "DBM-GUI".into()])];
        let g = group(&folders, &known);
        let find = |m: &str| g.iter().find(|x| x.main == m).unwrap_or_else(|| panic!("no group {m}: {g:?}"));
        assert_eq!(find("DBM-Core").folders.len(), 2);
        assert_eq!(find("DBM-Core").key.as_deref(), Some("github:DeadlyBossMods/DeadlyBossMods"));
        assert_eq!(find("BigWigs").folders, vec!["BigWigs", "BigWigs_Core"]);
        assert_eq!(find("Titan").folders, vec!["Titan", "TitanXP"]);
        assert_eq!(find("ForeverAuras").folders.len(), 3);
        // separate addons that merely share a prefix stay separate
        assert_eq!(find("EverlookDB").folders, vec!["EverlookDB"]);
        assert_eq!(find("EverlookDBAuctions").folders, vec!["EverlookDBAuctions"]);
        assert_eq!(g.len(), 6);
    }

    #[test]
    fn picks_flavor_toc() {
        let dir = std::env::temp_dir().join(format!("edb-toc-test-{}", std::process::id()));
        let q = dir.join("Questie");
        std::fs::create_dir_all(&q).unwrap();
        for n in ["Questie.toc", "Questie_Forever.toc", "Questie-Mists.toc", "Questie_Vanilla.toc"] {
            std::fs::write(q.join(n), format!("## Title: {n}\n")).unwrap();
        }
        let name = |s: &str| toc_for(&q, "Questie", toc_suffixes(s)).unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name("_forever_"), "Questie_Forever.toc");
        assert_eq!(name("_classic_"), "Questie-Mists.toc");
        assert_eq!(name("_classic_era_"), "Questie_Vanilla.toc");
        assert_eq!(name("_retail_"), "Questie.toc");
        let found = scan(&dir, "_forever_");
        assert_eq!(found[0].toc.title.as_deref(), Some("Questie_Forever.toc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
