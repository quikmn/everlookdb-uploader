// Our own addons, the EverlookDB family: what the welcome card and the "EverlookDB addons" filter offer to install
// in one click. Which ones is data (src-tauri/ours.json): every catalog entry marked "ours" that has a release for
// the game, minus the ones that file excludes. Installing them goes through manager::install like any other addon.

use crate::catalog::{Addon, Catalog};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Deserialize, Default)]
pub struct List {
    #[serde(default)]
    pub addons: Vec<Listed>,
    #[serde(default)]
    pub exclude: Vec<Excluded>,
}

#[derive(Deserialize)]
pub struct Listed {
    pub id: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Deserialize)]
pub struct Excluded {
    pub id: String,
}

/// The compiled-in list (src-tauri/ours.json).
pub fn list() -> List {
    serde_json::from_str(include_str!("../ours.json")).expect("src-tauri/ours.json is valid")
}

/// One of our addons as the window shows it.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Pick {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub version: String,
    pub version_label: String,
    /// already in this install (from EverlookDB, another app or by hand)
    pub installed: bool,
}

/// The catalog's summary without the addon's name in front or "(WoW Forever)" behind, for an addon ours.json has
/// no line for.
fn plain_summary(a: &Addon) -> String {
    let s = a.summary.trim();
    let s = s.strip_prefix(&format!("{}:", a.name)).unwrap_or(s).trim();
    let s = s.strip_suffix("(WoW Forever)").unwrap_or(s).trim();
    s.to_string()
}

/// Our addons for an install that accepts `accepts`, in ours.json's order (then by name). `installed`: the keys of
/// the addons in the install (manager::Item::key, the catalog id when it is matched).
pub fn pick(list: &List, catalog: &Catalog, accepts: &[String], installed: &HashSet<String>) -> Vec<Pick> {
    let excluded: HashSet<String> = list.exclude.iter().map(|e| e.id.to_ascii_lowercase()).collect();
    let rank = |id: &str| list.addons.iter().position(|l| l.id.eq_ignore_ascii_case(id)).unwrap_or(usize::MAX);
    let installed: HashSet<String> = installed.iter().map(|k| k.to_ascii_lowercase()).collect();
    let mut out: Vec<(usize, Pick)> = catalog
        .addons
        .iter()
        // ours, from the catalog (not a repo the player added), released for this game
        .filter(|a| a.ours && !a.custom && !a.version.is_empty() && a.file_for(accepts).is_some())
        .filter(|a| !excluded.contains(&a.id.to_ascii_lowercase()))
        // the Price Feed's data addon is the app's own output (shared.rs), never installed from a zip
        .filter(|a| !a.folders.iter().any(|f| f.eq_ignore_ascii_case(crate::shared::FOLDER)))
        .map(|a| {
            let r = rank(&a.id);
            let listed = list.addons.get(r).map(|l| l.summary.trim()).filter(|s| !s.is_empty());
            let pick = Pick {
                id: a.id.clone(),
                name: a.name.clone(),
                summary: listed.map(str::to_string).unwrap_or_else(|| plain_summary(a)),
                version: a.version.clone(),
                version_label: if a.version_label.is_empty() { a.version.clone() } else { a.version_label.clone() },
                installed: installed.contains(&a.id.to_ascii_lowercase()),
            };
            (r, pick)
        })
        .collect();
    out.sort_by(|(ra, a), (rb, b)| ra.cmp(rb).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
    out.into_iter().map(|(_, p)| p).collect()
}

/// The ones of `picks` to install: not installed yet, and (when the player unticked some) among `wanted`.
pub fn missing<'a>(picks: &'a [Pick], wanted: Option<&[String]>) -> Vec<&'a Pick> {
    picks
        .iter()
        .filter(|p| !p.installed)
        .filter(|p| wanted.is_none_or(|w| w.iter().any(|id| id.eq_ignore_ascii_case(&p.id))))
        .collect()
}

/// Whether the welcome card shows: the player hasn't said "Not now", the game is there, something is left to
/// install, and this is a new install of the app or none of our addons is in the game yet.
pub fn show_card(dismissed: bool, game: bool, new_install: bool, picks: &[Pick]) -> bool {
    !dismissed && game && picks.iter().any(|p| !p.installed) && (new_install || !picks.iter().any(|p| p.installed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::File;

    fn addon(id: &str, name: &str, ours: bool, flavors: &[&str]) -> Addon {
        Addon {
            id: id.into(),
            name: name.into(),
            ours,
            version: "v1.0.0".into(),
            summary: format!("{name}: does things (WoW Forever)"),
            folders: vec![id.rsplit('/').next().unwrap().into()],
            files: vec![File { flavors: flavors.iter().map(|f| f.to_string()).collect(), url: format!("https://github.com/{id}/x.zip"), ..Default::default() }],
            ..Default::default()
        }
    }

    fn catalog() -> Catalog {
        let mut shared = addon("github:quikmn/PriceFeed", "Price Feed", true, &["forever"]);
        shared.folders = vec!["EverlookDBAuctions_Shared".into()];
        let mut custom = addon("github:someone/Mine", "Mine", true, &["forever"]);
        custom.custom = true;
        let mut unreleased = addon("github:quikmn/Soon", "Soon", true, &["forever"]);
        unreleased.version = String::new();
        Catalog {
            version: 1,
            addons: vec![
                addon("github:BigWigsMods/BigWigs", "BigWigs", false, &["forever", "retail"]),
                addon("github:quikmn/Whack", "Whack", true, &["forever"]),
                addon("github:quikmn/CraftColors", "CraftColors", true, &["forever"]),
                addon("github:quikmn/EverlookDB", "EverlookDB Collector", true, &["forever"]),
                addon("github:quikmn/QuarryCollector", "Tooltip Collector", true, &["forever"]),
                addon("github:quikmn/RetailOnly", "Retail only", true, &["retail"]),
                addon("github:quikmn/Zeta", "Zeta", true, &["forever"]),
                shared,
                custom,
                unreleased,
            ],
            ..Default::default()
        }
    }

    fn forever() -> Vec<String> {
        vec!["forever".into()]
    }

    #[test]
    fn picks_ours_without_the_stand_downs_and_unreleased() {
        let picks = pick(&list(), &catalog(), &forever(), &HashSet::new());
        let ids: Vec<&str> = picks.iter().map(|p| p.id.as_str()).collect();
        // ours.json's order first (EverlookDB before Whack), then one it doesn't list (Zeta)
        assert_eq!(ids, ["github:quikmn/EverlookDB", "github:quikmn/Whack", "github:quikmn/Zeta"]);
        // the card's line from ours.json; the catalog's, cleaned, for one it doesn't list
        assert_eq!(picks[0].summary, "Records loot, NPCs and quests for everlookdb.com.");
        assert_eq!(picks[2].summary, "does things");
        assert!(picks.iter().all(|p| !p.installed));
    }

    #[test]
    fn nothing_for_a_game_our_addons_dont_run_in() {
        let picks = pick(&list(), &catalog(), &["retail".to_string()], &HashSet::new());
        assert_eq!(picks.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["github:quikmn/RetailOnly"]);
        assert!(pick(&list(), &catalog(), &["classic".to_string()], &HashSet::new()).is_empty());
    }

    #[test]
    fn skips_the_installed_ones() {
        let installed: HashSet<String> = ["github:quikmn/whack".to_string(), "folder:Something".to_string()].into();
        let picks = pick(&list(), &catalog(), &forever(), &installed);
        assert!(picks.iter().find(|p| p.id == "github:quikmn/Whack").unwrap().installed);
        let todo: Vec<&str> = missing(&picks, None).iter().map(|p| p.id.as_str()).collect();
        assert_eq!(todo, ["github:quikmn/EverlookDB", "github:quikmn/Zeta"]);
        // unticked ones stay out; an installed one asked for again is still skipped
        let wanted = vec!["github:quikmn/Zeta".to_string(), "github:quikmn/Whack".to_string()];
        let todo: Vec<&str> = missing(&picks, Some(&wanted)).iter().map(|p| p.id.as_str()).collect();
        assert_eq!(todo, ["github:quikmn/Zeta"]);
    }

    #[test]
    fn the_card_shows_until_dismissed_or_one_is_installed() {
        let none = pick(&list(), &catalog(), &forever(), &HashSet::new());
        let one = pick(&list(), &catalog(), &forever(), &["github:quikmn/Whack".to_string()].into());
        let all = pick(&list(), &catalog(), &forever(), &none.iter().map(|p| p.id.clone()).collect());
        assert!(show_card(false, true, false, &none));
        assert!(!show_card(true, true, false, &none), "Not now hides it for good");
        assert!(!show_card(false, false, false, &none), "no game, nothing to install into");
        assert!(!show_card(false, true, false, &one), "an existing player who has one of ours");
        assert!(show_card(false, true, true, &one), "a new install of the app still sees it");
        assert!(!show_card(false, true, true, &all), "nothing left to install");
        assert!(!show_card(false, true, true, &[]));
    }

    #[test]
    fn the_shipped_list_is_sound() {
        let l = list();
        assert!(l.exclude.iter().any(|e| e.id == "github:quikmn/CraftColors"));
        assert!(l.exclude.iter().any(|e| e.id == "github:quikmn/QuarryCollector"));
        assert!(l.addons.iter().all(|a| !a.summary.is_empty() && !l.exclude.iter().any(|e| e.id == a.id)));
    }
}
