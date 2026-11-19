// Finding World of Warcraft: every install (`World of Warcraft/_retail_`, `_classic_era_`, `_forever_` ...) with its
// client version from `.build.info`, plus the WoW Forever folder and the EverlookDB addons' data files for uploads.
// Everything here only reads.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FLAVOR: &str = "_forever_";

/// How long one folder may take to answer before it is skipped (a sleeping network drive, an empty card reader).
pub const FOLDER_WAIT: Duration = Duration::from_secs(3);

/// Run `f` on each item at once, each on its own thread; results in order, None for the ones that didn't answer
/// within `wait` (their threads are left to finish on their own).
pub fn each_within<T: Send + 'static, R: Send + 'static>(items: Vec<T>, wait: Duration, f: fn(T) -> R) -> Vec<Option<R>> {
    let n = items.len();
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, item) in items.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((i, f(item)));
        });
    }
    drop(tx);
    let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
    let deadline = Instant::now() + wait;
    let mut got = 0;
    while got < n {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, r)) => {
                out[i] = Some(r);
                got += 1;
            }
            Err(_) => break,
        }
    }
    out
}

/// The local hard drives (no network, CD or card-reader drives, which can hang or ask for a disc), as "C:\\".
#[cfg(windows)]
fn local_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|d| {
            let wide: Vec<u16> = d.encode_utf16().chain(Some(0)).collect();
            unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED }
        })
        .map(PathBuf::from)
        .collect()
}

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
pub fn candidates() -> Vec<PathBuf> {
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
        // local hard drives only, and only the ones that answer quickly
        let drives = local_drives();
        let ready = each_within(drives.clone(), FOLDER_WAIT, |d: PathBuf| d.is_dir());
        for (root, ok) in drives.iter().zip(ready) {
            if ok == Some(true) {
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
    match std::env::var_os("EVERLOOKDB_WOW_ROOTS").filter(|s| !s.is_empty()) {
        Some(list) => std::env::split_paths(&list).find_map(|p| resolve(&p)),
        None => candidates().iter().find_map(|p| resolve(p)),
    }
}

/// The SavedVariables files of our addons: EverlookDB (loot, NPCs, vendors, quests) and EverlookDB Auctions
/// (auction house scans). Nothing else is ever read or sent.
pub const DATA_FILES: [&str; 2] = ["EverlookDB.lua", "EverlookDBAuctions.lua"];

/// Every `WTF/Account/*/SavedVariables/<one of DATA_FILES>` under the `_forever_` folder.
pub fn data_files(forever: &Path) -> Vec<PathBuf> {
    let Ok(accounts) = std::fs::read_dir(forever.join("WTF").join("Account")) else { return Vec::new() };
    let mut out: Vec<PathBuf> = accounts
        .flatten()
        .flat_map(|a| DATA_FILES.map(|name| a.path().join("SavedVariables").join(name)))
        .filter(|f| f.is_file())
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Every install

/// One game client: a flavor folder such as `World of Warcraft/_retail_`.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct Install {
    /// the flavor folder's path, as text; stable, used as the install's id
    pub id: String,
    pub dir: PathBuf,
    /// the `World of Warcraft` folder
    pub root: PathBuf,
    /// `_retail_`, `_forever_` ...
    pub folder: String,
    /// "WoW Forever", "Retail", "Classic Era" ...
    pub name: String,
    /// "Midnight", "Mists of Pandaria Classic" ... when the version says so
    pub edition: Option<String>,
    /// the catalog flavor: retail, classic, classic_era, forever, anniversary, ptr, beta
    pub flavor: String,
    /// catalog flavors whose files run here, best first
    pub accepts: Vec<String>,
    /// "1.60.1" and "70938" from .build.info's "1.60.1.70938"
    pub version: Option<String>,
    pub build: Option<String>,
    pub addons_dir: PathBuf,
    /// the game itself is there (an executable or a .build.info row), not just a leftover folder
    pub game: bool,
}

/// Whether a flavor folder holds the game's executable (`Wow.exe`, `WowClassic.exe`, `World of Warcraft.app` ...).
fn has_executable(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                (n.starts_with("wow") && n.ends_with(".exe")) || (n.starts_with("world of warcraft") && n.ends_with(".app"))
            })
        })
        .unwrap_or(false)
}

/// Human name, catalog flavor and accepted flavors for a flavor folder.
pub fn flavor_info(folder: &str) -> (String, String, Vec<String>) {
    let f = folder.trim_matches('_').to_ascii_lowercase();
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (name, flavor, accepts): (&str, &str, Vec<String>) = match f.as_str() {
        "retail" => ("Retail", "retail", v(&["retail"])),
        "ptr" => ("Retail PTR", "ptr", v(&["ptr", "retail"])),
        "xptr" => ("Retail PTR 2", "ptr", v(&["ptr", "retail"])),
        "beta" => ("Beta", "beta", v(&["beta", "retail"])),
        "classic" => ("Classic", "classic", v(&["classic"])),
        "classic_ptr" => ("Classic PTR", "ptr", v(&["ptr", "classic"])),
        "classic_beta" => ("Classic Beta", "beta", v(&["beta", "classic"])),
        "classic_era" => ("Classic Era", "classic_era", v(&["classic_era"])),
        "classic_era_ptr" => ("Classic Era PTR", "ptr", v(&["ptr", "classic_era"])),
        "anniversary" => ("Anniversary", "anniversary", v(&["anniversary"])),
        "forever" => ("WoW Forever", "forever", v(&["forever"])),
        _ => ("", "", Vec::new()),
    };
    if name.is_empty() {
        // an unknown flavor: "_some_thing_" -> "Some thing"
        let n = f.replace('_', " ");
        let mut c = n.chars();
        let n = match c.next() {
            Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
            None => n,
        };
        return (n, f.clone(), vec![f]);
    }
    (name.to_string(), flavor.to_string(), accepts)
}

/// The expansion a version number belongs to, for the installs where that is worth saying.
pub fn edition(folder: &str, version: &str) -> Option<String> {
    let major: u32 = version.split('.').next()?.parse().ok()?;
    let f = folder.trim_matches('_');
    let classic = (f.starts_with("classic") && !f.starts_with("classic_era")) || f == "anniversary";
    let name = if classic {
        match major {
            2 => "The Burning Crusade Classic",
            3 => "Wrath of the Lich King Classic",
            4 => "Cataclysm Classic",
            5 => "Mists of Pandaria Classic",
            _ => return None,
        }
    } else if matches!(f, "retail" | "ptr" | "xptr" | "beta") {
        match major {
            11 => "The War Within",
            12 => "Midnight",
            _ => return None,
        }
    } else {
        return None;
    };
    Some(name.to_string())
}

/// A row of `.build.info`: which product (client) it describes and that client's version.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildRow {
    pub product: String,
    pub version: String,
    pub active: bool,
}

/// `.build.info` is a pipe-separated table; the first line names the columns ("Version!STRING:0").
pub fn parse_build_info(text: &str) -> Vec<BuildRow> {
    let mut lines = text.lines().map(|l| l.trim_start_matches('\u{feff}').trim_end_matches('\r')).filter(|l| !l.trim().is_empty());
    let Some(head) = lines.next() else { return Vec::new() };
    let cols: Vec<String> = head.split('|').map(|c| c.split('!').next().unwrap_or("").trim().to_ascii_lowercase()).collect();
    let col = |name: &str| cols.iter().position(|c| c == name);
    let (Some(vi), Some(pi)) = (col("version"), col("product")) else { return Vec::new() };
    let ai = col("active");
    lines
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('|').collect();
            let version = cells.get(vi)?.trim().to_string();
            let product = cells.get(pi)?.trim().to_string();
            if version.is_empty() || product.is_empty() {
                return None;
            }
            let active = ai.and_then(|i| cells.get(i)).is_none_or(|a| a.trim() != "0");
            Some(BuildRow { product, version, active })
        })
        .collect()
}

/// The flavor folder a Battle.net product code installs into.
pub fn product_folder(product: &str) -> Option<&'static str> {
    Some(match product.to_ascii_lowercase().as_str() {
        "wow" => "_retail_",
        "wowt" => "_ptr_",
        "wowxptr" => "_xptr_",
        "wow_beta" => "_beta_",
        "wow_classic" => "_classic_",
        "wow_classic_ptr" => "_classic_ptr_",
        "wow_classic_beta" => "_classic_beta_",
        "wow_classic_era" => "_classic_era_",
        "wow_classic_era_ptr" => "_classic_era_ptr_",
        "wow_anniversary" => "_anniversary_",
        "wowf" | "wow_forever" => "_forever_",
        _ => return None,
    })
}

/// "1.60.1.70938" -> ("1.60.1", Some("70938")).
pub fn split_version(v: &str) -> (String, Option<String>) {
    match v.rsplit_once('.') {
        Some((head, build)) if head.contains('.') && !build.is_empty() && build.chars().all(|c| c.is_ascii_digit()) => {
            (head.to_string(), Some(build.to_string()))
        }
        _ => (v.to_string(), None),
    }
}

/// Whether a name is a flavor folder (`_retail_`, `_classic_era_` ...).
fn is_flavor_folder(name: &str) -> bool {
    name.len() > 2 && name.starts_with('_') && name.ends_with('_')
}

fn has_flavors(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| is_flavor_folder(&e.file_name().to_string_lossy()) && e.path().is_dir() && looks_like_client(&e.path())))
        .unwrap_or(false)
}

/// The `World of Warcraft` folder a path points into, if it is one (or a flavor folder, WTF or Interface in one).
pub fn root_of(p: &Path) -> Option<PathBuf> {
    let p = PathBuf::from(p.to_string_lossy().trim().trim_end_matches(['\\', '/']));
    let mut cur: Option<&Path> = Some(p.as_path());
    for _ in 0..4 {
        let dir = cur?;
        if has_flavors(dir) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Every install under one `World of Warcraft` folder.
pub fn installs_in(root: &Path) -> Vec<Install> {
    let rows = std::fs::read_to_string(root.join(".build.info")).map(|t| parse_build_info(&t)).unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut folders: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_flavor_folder(n) && looks_like_client(&root.join(n)))
        .collect();
    folders.sort();
    let only = folders.len() == 1;
    folders
        .iter()
        .map(|folder| {
            let row = rows
                .iter()
                .filter(|r| product_folder(&r.product).is_some_and(|f| f.eq_ignore_ascii_case(folder)))
                .max_by_key(|r| r.active);
            // one client and one row whose product code we don't know: it is that client's
            let row = row.or_else(|| if only && rows.len() == 1 { rows.first() } else { None });
            let (version, build) = match row {
                Some(r) => {
                    let (v, b) = split_version(&r.version);
                    (Some(v), b)
                }
                None => (None, None),
            };
            let (name, flavor, accepts) = flavor_info(folder);
            let dir = root.join(folder);
            Install {
                game: row.is_some() || has_executable(&dir),
                id: dir.to_string_lossy().into_owned(),
                addons_dir: dir.join("Interface").join("AddOns"),
                edition: version.as_deref().and_then(|v| edition(folder, v)),
                dir,
                root: root.to_path_buf(),
                folder: folder.clone(),
                name,
                flavor,
                accepts,
                version,
                build,
            }
        })
        .collect()
}

/// `World of Warcraft` folders: the usual places (or EVERLOOKDB_WOW_ROOTS, for testing) plus `extra`.
pub fn roots(extra: &[PathBuf]) -> Vec<PathBuf> {
    let found: Vec<PathBuf> = match std::env::var_os("EVERLOOKDB_WOW_ROOTS").filter(|s| !s.is_empty()) {
        Some(list) => std::env::split_paths(&list).collect(),
        None => candidates(),
    };
    let key = |x: &Path| x.to_string_lossy().to_ascii_lowercase();
    let mut out: Vec<PathBuf> = Vec::new();
    // every place at once; one that doesn't answer in time (a network drive that is gone) is skipped
    let all: Vec<PathBuf> = found.iter().chain(extra).cloned().collect();
    for r in each_within(all, FOLDER_WAIT, |p: PathBuf| root_of(&p)).into_iter().flatten().flatten() {
        if !out.iter().any(|o| key(o) == key(&r)) {
            out.push(r);
        }
    }
    out
}

pub fn all_installs(extra: &[PathBuf]) -> Vec<Install> {
    installs_of(&roots(extra))
}

/// The installs in these `World of Warcraft` folders (found before: no drive scan), WoW Forever first. A folder that
/// doesn't answer in time is skipped this once.
pub fn installs_of(roots: &[PathBuf]) -> Vec<Install> {
    let mut all: Vec<Install> = each_within(roots.to_vec(), FOLDER_WAIT, |r: PathBuf| installs_in(&r)).into_iter().flatten().flatten().collect();
    // real clients first (WoW Forever, then Blizzard's live games, then test clients); leftover folders last
    let rank = |f: &str| ["forever", "retail", "classic", "anniversary", "classic_era"].iter().position(|x| *x == f).unwrap_or(9);
    all.sort_by_key(|i| (!i.game, rank(&i.flavor)));
    all
}

// ---------------------------------------------------------------------------------------------------------------
// Is the game running?

/// Whether `exe` is a game executable in the flavor folder `dir` (Wow.exe, WowClassic.exe ... or the macOS app).
pub fn runs_from(exe: &Path, dir: &Path) -> bool {
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    let (e, d) = (key(exe), key(dir));
    let Some(rest) = e.strip_prefix(&d).and_then(|r| r.strip_prefix('/')) else { return false };
    let first = rest.split('/').next().unwrap_or("");
    (first.starts_with("wow") && first.ends_with(".exe") && !rest.contains('/')) || (first.starts_with("world of warcraft") && first.ends_with(".app"))
}

/// The paths of the running game executables (only processes whose name starts with "wow" or "World of Warcraft").
#[cfg(windows)]
pub fn running_games() -> Vec<PathBuf> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
            if name.starts_with("wow") && name.ends_with(".exe") {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                if !h.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut n = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0 {
                        out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])));
                    }
                    CloseHandle(h);
                }
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

#[cfg(not(windows))]
pub fn running_games() -> Vec<PathBuf> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "comm="]).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("World of Warcraft"))
        .map(|l| {
            // ".../_retail_/World of Warcraft.app/Contents/MacOS/World of Warcraft" -> the .app
            let l = l.trim();
            match l.find(".app/") {
                Some(i) => PathBuf::from(&l[..i + 4]),
                None => PathBuf::from(l),
            }
        })
        .collect()
}

/// Whether the game of this install is running now.
pub fn game_running(install: &Install) -> bool {
    running_games().iter().any(|exe| runs_from(exe, &install.dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "Branch!STRING:0|Active!DEC:1|Build Key!HEX:16|CDN Key!HEX:16|Install Key!HEX:16|IM Size!DEC:4|CDN Path!STRING:0|CDN Hosts!STRING:0|CDN Servers!STRING:0|Tags!STRING:0|Armadillo!STRING:0|Last Activated!STRING:0|Version!STRING:0|KeyRing!HEX:16|Product!STRING:0\neu|1|3627b099867bfde3e37a17cf44f69cdc|3363ac656da5f261cdc675ab28b2744c|||tpr/wow|level3.blizzard.com|http://x|Windows x86_64 EU?|||1.60.1.70938||wowf\n";

    #[test]
    fn build_info_forever() {
        let rows = parse_build_info(REAL);
        assert_eq!(rows, vec![BuildRow { product: "wowf".into(), version: "1.60.1.70938".into(), active: true }]);
        assert_eq!(product_folder("wowf"), Some("_forever_"));
        assert_eq!(split_version("1.60.1.70938"), ("1.60.1".into(), Some("70938".into())));
        assert_eq!(split_version("1.60"), ("1.60".into(), None));
    }

    #[test]
    fn build_info_several_products() {
        let t = "Branch!STRING:0|Active!DEC:1|Version!STRING:0|Product!STRING:0\r\nus|1|12.0.5.64000|wow\r\nus|0|1.15.8.63000|wow_classic_era\r\nus|1|5.5.1.63100|wow_classic\r\n";
        let rows = parse_build_info(t);
        assert_eq!(rows.len(), 3);
        assert!(!rows[1].active);
        assert_eq!(product_folder(&rows[2].product), Some("_classic_"));
        assert_eq!(parse_build_info("nonsense"), vec![]);
        assert_eq!(parse_build_info(""), vec![]);
    }

    #[test]
    fn flavors() {
        assert_eq!(flavor_info("_forever_").0, "WoW Forever");
        assert_eq!(flavor_info("_forever_").1, "forever");
        assert_eq!(flavor_info("_classic_era_").1, "classic_era");
        assert_eq!(flavor_info("_classic_ptr_").2, vec!["ptr", "classic"]);
        assert_eq!(flavor_info("_retail_").0, "Retail");
        assert_eq!(flavor_info("_new_thing_").0, "New thing");
        assert_eq!(edition("_classic_", "5.5.1"), Some("Mists of Pandaria Classic".into()));
        assert_eq!(edition("_retail_", "12.0.5"), Some("Midnight".into()));
        assert_eq!(edition("_classic_era_", "1.15.8"), None);
        assert_eq!(edition("_forever_", "1.60.1"), None);
    }

    #[test]
    fn tells_which_game_runs() {
        let dir = Path::new(r"D:\Games\World of Warcraft\_forever_");
        assert!(runs_from(Path::new(r"D:\Games\World of Warcraft\_forever_\Wow.exe"), dir));
        assert!(runs_from(Path::new(r"d:\games\world of warcraft\_FOREVER_\WowClassic.exe"), dir));
        assert!(!runs_from(Path::new(r"D:\Games\World of Warcraft\_retail_\Wow.exe"), dir));
        assert!(!runs_from(Path::new(r"D:\Games\World of Warcraft\_forever_2\Wow.exe"), dir));
        assert!(!runs_from(Path::new(r"D:\Games\World of Warcraft\_forever_\Tools\Wow.exe"), dir));
        assert!(!runs_from(Path::new(r"D:\Games\World of Warcraft\_forever_\Battle.net.exe"), dir));
        assert!(runs_from(Path::new("/Applications/World of Warcraft/_retail_/World of Warcraft.app"), Path::new("/Applications/World of Warcraft/_retail_/")));
        // whatever runs here, asking never fails
        let _ = running_games();
    }

    #[test]
    fn slow_folders_are_skipped() {
        let got = each_within(vec![1u64, 0, 2], Duration::from_millis(300), |n: u64| {
            if n == 0 {
                std::thread::sleep(Duration::from_secs(5));
            }
            n * 10
        });
        assert_eq!(got, vec![Some(10), None, Some(20)]);
        assert!(each_within(Vec::<u8>::new(), Duration::from_millis(10), |n: u8| n).is_empty());
        #[cfg(windows)]
        assert!(local_drives().iter().all(|d| d.to_string_lossy().ends_with(":\\")));
    }

    #[test]
    fn finds_installs() {
        let tmp = std::env::temp_dir().join(format!("edb-wow-test-{}", std::process::id()));
        let root = tmp.join("World of Warcraft");
        for f in ["_forever_", "_retail_"] {
            std::fs::create_dir_all(root.join(f).join("Interface").join("AddOns")).unwrap();
        }
        std::fs::create_dir_all(root.join("_notaclient_")).unwrap();
        std::fs::write(root.join(".build.info"), "Active!DEC:1|Version!STRING:0|Product!STRING:0\n1|1.60.1.70938|wowf\n1|12.0.5.64000|wow\n").unwrap();
        assert_eq!(root_of(&root.join("_forever_").join("Interface")), Some(root.clone()));
        let all = installs_in(&root);
        assert_eq!(all.len(), 2);
        let fv = all.iter().find(|i| i.flavor == "forever").unwrap();
        assert_eq!(fv.version.as_deref(), Some("1.60.1"));
        assert_eq!(fv.build.as_deref(), Some("70938"));
        let rt = all.iter().find(|i| i.flavor == "retail").unwrap();
        assert_eq!(rt.edition.as_deref(), Some("Midnight"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
