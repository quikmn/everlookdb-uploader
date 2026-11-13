// Finding the WoW Forever folder (`World of Warcraft/_forever_`) and the addon's data files in it.

use std::path::{Path, PathBuf};

const FLAVOR: &str = "_forever_";

/// Whether a folder looks like a game client folder (has WTF, Interface or the game executable).
fn looks_like_client(dir: &Path) -> bool {
    dir.join("WTF").is_dir()
        || dir.join("Interface").is_dir()
        || std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten().any(|e| {
                    let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                    n.starts_with("wow") && (n.ends_with(".exe") || n.ends_with(".app"))
                })
            })
            .unwrap_or(false)
}

/// Turn whatever the user or the system points at into the `_forever_` folder, if there is one:
/// the folder itself, its parent `World of Warcraft`, a sibling like `_retail_`, or its WTF/Interface.
pub fn resolve(p: &Path) -> Option<PathBuf> {
    let p = PathBuf::from(p.to_string_lossy().trim().trim_end_matches(['\\', '/']));
    let name = p.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name == FLAVOR && p.is_dir() {
        return Some(p);
    }
    let mut candidates = vec![p.join(FLAVOR)];
    if let Some(parent) = p.parent() {
        if name.starts_with('_') && name.ends_with('_') {
            candidates.push(parent.join(FLAVOR));
        }
        if name == "wtf" || name == "interface" {
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.into_iter().find(|c| c.is_dir() && (c.ends_with(FLAVOR) || looks_like_client(c)))
}

/// Places the game may be, most likely first.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        use winreg::RegKey;
        let keys = [
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Blizzard Entertainment\\World of Warcraft", "InstallPath"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\Blizzard Entertainment\\World of Warcraft", "InstallPath"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\World of Warcraft", "InstallLocation"),
            (HKEY_CURRENT_USER, "SOFTWARE\\Blizzard Entertainment\\World of Warcraft", "InstallPath"),
        ];
        for (root, key, value) in keys {
            if let Ok(p) = RegKey::predef(root).open_subkey(key).and_then(|k| k.get_value::<String, _>(value)) {
                out.push(PathBuf::from(p));
            }
        }
        const COMMON: &[&str] = &[
            "World of Warcraft",
            "Program Files (x86)\\World of Warcraft",
            "Program Files\\World of Warcraft",
            "Games\\World of Warcraft",
            "Games\\Blizzard\\World of Warcraft",
            "Blizzard\\World of Warcraft",
            "Battle.net\\World of Warcraft",
        ];
        for drive in b'C'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", drive as char));
            if root.is_dir() {
                out.extend(COMMON.iter().map(|rel| root.join(rel)));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from("/Applications/World of Warcraft"));
        if let Some(home) = std::env::var_os("HOME") {
            out.push(Path::new(&home).join("Applications/World of Warcraft"));
        }
        out.push(PathBuf::from("/Users/Shared/World of Warcraft"));
    }
    out
}

pub fn find_install() -> Option<PathBuf> {
    candidates().iter().find_map(|p| resolve(p))
}

/// Every `WTF/Account/*/SavedVariables/EverlookDB.lua` under the `_forever_` folder.
pub fn data_files(forever: &Path) -> Vec<PathBuf> {
    let Ok(accounts) = std::fs::read_dir(forever.join("WTF").join("Account")) else { return Vec::new() };
    let mut out: Vec<PathBuf> = accounts
        .flatten()
        .map(|a| a.path().join("SavedVariables").join("EverlookDB.lua"))
        .filter(|f| f.is_file())
        .collect();
    out.sort();
    out
}
