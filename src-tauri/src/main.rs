// EverlookDB: an ad-free addon manager for World of Warcraft that also uploads the EverlookDB addons' data files
// whenever the game saves them, so players never have to upload them by hand on everlookdb.com/contribute.html.
// It lives in the tray; the window opens from there and is closed (not hidden) when the player closes it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod addons;
mod blp;
mod catalog;
mod config;
mod giveaway;
mod github;
mod icons;
mod manager;
mod worker;
mod wow;

use catalog::{Catalog, Origin};
use config::Config;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder, Wry};
#[cfg(target_os = "macos")]
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use worker::{Cmd, Kind, Shared, Status, Watcher};

const DEFAULT_SERVER: &str = "https://everlookdb.com";
const PRIVACY: &str = "Sends only the game facts in EverlookDB.lua and EverlookDBAuctions.lua";
const AUTOSTART: &str = if cfg!(target_os = "macos") { "Open at login" } else { "Start with Windows" };
/// how often the app checks for a new version of itself (the addon list follows the player's update_hours)
const HOURLY: Duration = Duration::from_secs(3600);
const HELP: &str = "EverlookDB: addon manager for World of Warcraft, and uploader of the EverlookDB addons' data to everlookdb.com.

Usage: everlookdb-uploader [options]
  --server <url>   server to upload to (default https://everlookdb.com; or EVERLOOKDB_SERVER)
  --wow <folder>   use (and remember) this WoW Forever folder for uploads
  --check          print the installs, addons and data files found, then exit
  --once           upload every changed data file once, print the results, then exit
  --background     start quietly in the tray (used at login)
Settings live in %APPDATA%\\com.everlookdb.uploader (macOS: ~/Library/Application Support/com.everlookdb.uploader),
or EVERLOOKDB_UPLOADER_DIR. EVERLOOKDB_CATALOG points the addon catalog elsewhere (a URL or a local file).";

struct Args {
    server: String,
    wow: Option<PathBuf>,
    check: bool,
    once: bool,
    background: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        server: std::env::var("EVERLOOKDB_SERVER").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_SERVER.into()),
        wow: None,
        check: false,
        once: false,
        background: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--server" => a.server = it.next().ok_or("--server needs a URL")?,
            "--wow" => a.wow = Some(it.next().ok_or("--wow needs a folder")?.into()),
            "--check" => a.check = true,
            "--once" => a.once = true,
            "--background" => a.background = true,
            "--help" | "-h" => return Err(HELP.into()),
            "--version" | "-V" => return Err(format!("everlookdb-uploader {}", env!("CARGO_PKG_VERSION"))),
            _ => {} // the OS or the single-instance plugin may add its own
        }
    }
    a.server = a.server.trim_end_matches('/').to_string();
    Ok(a)
}

fn default_config_dir() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("APPDATA").map(PathBuf::from)
    };
    base.unwrap_or_else(std::env::temp_dir).join("com.everlookdb.uploader")
}

/// %LOCALAPPDATA%\com.everlookdb.uploader (Windows), ~/Library/Caches/com.everlookdb.uploader (macOS).
fn default_cache_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"))
    } else {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    };
    base.map(|b| b.join("com.everlookdb.uploader"))
}

/// A windowed exe has no console; borrow the one it was started from for the command-line modes.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    // reqwest (ours and the updater's) uses rustls with ring
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = match args() {
        Ok(a) => a,
        Err(text) => {
            attach_console();
            println!("{text}");
            return;
        }
    };
    config::init_dir(default_config_dir());
    config::init_cache_dir(default_cache_dir());
    if args.check || args.once {
        attach_console();
        std::process::exit(command_line(&args));
    }
    run(args);
}

/// --check and --once.
fn command_line(args: &Args) -> i32 {
    let mut cfg = Config::load();
    // development: what the window would get, as JSON (for designing the UI outside the app)
    if std::env::var_os("EVERLOOKDB_DUMP").is_some() {
        let (cat, _) = catalog::fetch(&args.server);
        let installs = wow::all_installs(&cfg.extra_roots);
        let items: std::collections::BTreeMap<String, Vec<manager::Item>> = installs.iter().map(|i| (i.id.clone(), manager::list(i, cat.as_ref()))).collect();
        println!("{}", serde_json::json!({ "installs": installs, "items": items, "catalog": cat }));
        return 0;
    }
    println!("Server:      {}", worker::upload_url(&args.server));
    println!("Settings:    {}", config::dir().display());
    for i in wow::all_installs(&cfg.extra_roots) {
        let n = manager::list(&i, None).len();
        println!(
            "Install:     {} {}{} in {} ({n} addons)",
            i.name,
            i.version.as_deref().unwrap_or("(version unknown)"),
            i.build.as_deref().map(|b| format!(" build {b}")).unwrap_or_default(),
            i.dir.display()
        );
    }
    let dir = args.wow.as_deref().and_then(wow::resolve).or(cfg.wow_dir.clone().filter(|d| d.is_dir())).or_else(wow::find_install);
    let Some(dir) = dir else {
        println!("WoW Forever folder: not found (pass --wow <folder>)");
        return 1;
    };
    println!("WoW Forever: {}", dir.display());
    let files = wow::data_files(&dir);
    for f in &files {
        println!("Data file:   {} ({} bytes)", f.display(), std::fs::metadata(f).map(|m| m.len()).unwrap_or(0));
    }
    if files.is_empty() {
        println!("No WTF/Account/*/SavedVariables/EverlookDB.lua or EverlookDBAuctions.lua yet.");
    }
    if !args.once {
        return 0;
    }
    cfg.wow_dir = Some(dir);
    cfg.paused = false;
    let shared = Arc::new(Shared { cfg: Mutex::new(cfg), server: args.server.clone() });
    let status = Watcher::new(shared).pass(false, true);
    println!("Result:      {}", status.line);
    for v in &status.views {
        println!("  {}: {}{}", v.label, v.headline, v.next.as_deref().map(|n| format!(" ({n})")).unwrap_or_default());
        for d in &v.details {
            println!("    {d}");
        }
    }
    if status.kind == Kind::Ok || status.kind == Kind::Idle { 0 } else { 2 }
}

/// What the tray and the window share.
struct Ui {
    status: MenuItem<Wry>,
    folder: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    pause: CheckMenuItem<Wry>,
    tx: Mutex<Sender<Cmd>>,
    shared: Arc<Shared>,
    uploads: Mutex<UploadState>,
    catalog: Mutex<CatalogState>,
    /// (install id, item key) -> the texture its .toc names as its icon (from the last list of that install)
    icon_files: Mutex<HashMap<(String, String), PathBuf>>,
    /// addons looked up by their GitHub link and shown to the player, by id: what "Install" installs
    github_seen: Mutex<HashMap<String, catalog::Addon>>,
}

#[derive(Serialize, Clone, Default)]
struct UploadState {
    line: String,
    kind: &'static str,
    accounts: usize,
    folder: Option<String>,
    paused: bool,
    /// each watched file's last answer (no paths, no account names)
    files: Vec<worker::FileView>,
    /// the giveaway code and checklist; None while the giveaway is off (giveaway::ENABLED)
    giveaway: Option<giveaway::View>,
}

#[derive(Default)]
struct CatalogState {
    catalog: Option<Arc<Catalog>>,
    origin: Option<Origin>,
    checked: Option<String>,
}

impl Ui {
    fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }
    fn catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.lock().unwrap().catalog.clone()
    }
}

/// A new install starts with Windows / at login (see Config::autostart_decided); set in run, used in setup.
static AUTOSTART_NEW_INSTALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run(args: Args) {
    // no settings file yet: this is a new install
    let fresh = !Config::path().exists();
    let mut cfg = Config::load();
    if !cfg.autostart_decided {
        cfg.autostart_decided = true;
        AUTOSTART_NEW_INSTALL.store(fresh, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(over) = args.wow.as_deref().and_then(wow::resolve) {
        cfg.wow_dir = Some(over);
    }
    if cfg.wow_dir.as_ref().is_none_or(|d| !d.is_dir()) {
        cfg.wow_dir = cfg.extra_roots.iter().find_map(|r| wow::resolve(r)).or_else(wow::find_install);
    }
    cfg.save();
    let shared = Arc::new(Shared { cfg: Mutex::new(cfg), server: args.server.clone() });
    config::log(&format!("start {} (server {})", env!("CARGO_PKG_VERSION"), args.server));

    let mut builder = tauri::Builder::default();
    // a test instance (its own settings folder) may run next to the real one, and never opens its window by itself
    let test_instance = std::env::var_os("EVERLOOKDB_UPLOADER_DIR").is_some_and(|v| !v.is_empty());
    // the window opens at start only when the player started the app (not at login, not a development or test run);
    // after that only when they click the tray icon
    let show_at_start = !args.background && !test_instance && !cfg!(debug_assertions);
    if !test_instance {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)));
    }
    let app = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(autostart().build())
        .invoke_handler(tauri::generate_handler![
            get_state,
            list_installed,
            get_catalog,
            install_addon,
            remove_addon,
            rollback_addon,
            update_all,
            add_folder,
            forget_folder,
            set_setting,
            upload_now,
            open_url,
            open_addons_folder,
            addon_icons,
            copy_addon_list,
            save_addon_list,
            copy_giveaway_code,
            github_lookup,
            install_from_github,
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            setup(app.handle(), shared, show_at_start)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("start EverlookDB");
    app.run(|app, event| match event {
        // closing the window leaves the app in the tray until Quit
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        RunEvent::WindowEvent { event: tauri::WindowEvent::Destroyed, .. } => {
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        }
        RunEvent::Exit => {
            if let Some(ui) = app.try_state::<Ui>() {
                ui.send(Cmd::Quit);
            }
        }
        _ => {}
    });
}

/// The login item keeps the Uploader's name, so existing installs keep starting with Windows / at login.
fn autostart() -> tauri_plugin_autostart::Builder {
    let b = tauri_plugin_autostart::Builder::new().app_name("EverlookDB Uploader").arg("--background");
    #[cfg(target_os = "macos")]
    let b = b.macos_launcher(MacosLauncher::LaunchAgent);
    b
}

fn setup(app: &AppHandle, shared: Arc<Shared>, show: bool) -> tauri::Result<()> {
    let paused = shared.cfg.lock().unwrap().paused;
    // point the login item at this copy of the app (an update may have installed it somewhere new); only for the
    // real app, never a development or test run, which would take over the player's login item
    if !cfg!(debug_assertions) && std::env::var_os("EVERLOOKDB_UPLOADER_DIR").is_none_or(|v| v.is_empty()) {
        let al = app.autolaunch();
        // a new install starts with Windows / at login by default (the player can turn it off in the window or the
        // tray menu, and it then stays off); otherwise only re-point an existing login item at this copy
        if AUTOSTART_NEW_INSTALL.load(std::sync::atomic::Ordering::Relaxed) || al.is_enabled().unwrap_or(false) {
            let _ = al.enable();
            if AUTOSTART_NEW_INSTALL.load(std::sync::atomic::Ordering::Relaxed) {
                config::log("new install: start with Windows / at login turned on (the player can turn it off)");
            }
        }
    }
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "", false, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", AUTOSTART, true, app.autolaunch().is_enabled().unwrap_or(false), None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", "Pause uploads", true, paused, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "window", "Open EverlookDB", true, None::<&str>)?,
            &sep()?,
            &status,
            &folder,
            &sep()?,
            &MenuItem::with_id(app, "upload", "Upload now", true, None::<&str>)?,
            &MenuItem::with_id(app, "site", "Open everlookdb.com", true, None::<&str>)?,
            &MenuItem::with_id(app, "change", "Change WoW folder…", true, None::<&str>)?,
            &sep()?,
            &autostart,
            &pause,
            &sep()?,
            &MenuItem::with_id(app, "privacy", PRIVACY, false, None::<&str>)?,
            &MenuItem::with_id(app, "quit", "Quit EverlookDB", true, None::<&str>)?,
        ],
    )?;
    let tray = TrayIconBuilder::with_id("main").menu(&menu).show_menu_on_left_click(false).tooltip("EverlookDB");
    #[cfg(target_os = "macos")]
    let tray = tray.icon(tauri::include_image!("icons/tray-template.png")).icon_as_template(true);
    #[cfg(not(target_os = "macos"))]
    let tray = tray.icon(app.default_window_icon().cloned().expect("app icon"));
    tray.on_menu_event(|app, e| on_menu(app, e.id.as_ref()))
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;

    let (tx, rx) = channel();
    app.manage(Ui {
        status,
        folder,
        autostart,
        pause,
        tx: Mutex::new(tx),
        shared: shared.clone(),
        uploads: Mutex::new(UploadState {
            line: "Starting…".into(),
            kind: "idle",
            paused,
            giveaway: giveaway::view_if_shown(&shared.cfg.lock().unwrap().receipts),
            ..Default::default()
        }),
        catalog: Mutex::new(CatalogState::default()),
        icon_files: Mutex::new(HashMap::new()),
        github_seen: Mutex::new(HashMap::new()),
    });
    std::thread::spawn(icons::prune);

    let handle = app.clone();
    let mut last: Option<(String, usize, Option<PathBuf>, Vec<worker::FileView>, config::Receipts)> = None;
    std::thread::spawn(move || {
        Watcher::new(shared.clone()).run(rx, |s: &Status| {
            let (dir, receipts) = {
                let c = shared.cfg.lock().unwrap();
                (c.wow_dir.clone(), c.receipts.clone())
            };
            let now = (s.line.clone(), s.files, dir.clone(), s.views.clone(), receipts);
            if last.as_ref() != Some(&now) {
                show_status(&handle, s, dir);
                last = Some(now);
            }
        })
    });

    if show {
        show_window(app);
    }
    spawn_hourly(app.clone());
    spawn_updater(app.clone());
    Ok(())
}

/// Show the window, creating it if it was closed.
fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("EverlookDB")
        .inner_size(1100.0, 740.0)
        .min_inner_size(760.0, 520.0)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(12, 20, 34, 255))
        .center()
        .build();
    if let Err(e) = built {
        config::log(&format!("window: {e}"));
    }
}

fn show_status(app: &AppHandle, s: &Status, dir: Option<PathBuf>) {
    let ui = app.state::<Ui>();
    let _ = ui.status.set_text(&s.line);
    let folder = match (&dir, s.files) {
        (None, _) => "No WoW Forever folder".to_string(),
        (Some(d), 1) => format!("Watching 1 account in {}", d.display()),
        (Some(d), n) => format!("Watching {n} accounts in {}", d.display()),
    };
    let _ = ui.folder.set_text(folder);
    if let Some(tray) = app.tray_by_id("main") {
        let tip: String = format!("EverlookDB\n{}", s.line).chars().take(120).collect();
        let _ = tray.set_tooltip(Some(tip));
    }
    let (paused, receipts) = {
        let c = ui.shared.cfg.lock().unwrap();
        (c.paused, c.receipts.clone())
    };
    let state = UploadState {
        line: s.line.clone(),
        kind: match s.kind {
            Kind::Idle => "idle",
            Kind::Ok => "ok",
            Kind::Busy => "busy",
            Kind::Error => "error",
            Kind::Paused => "paused",
        },
        accounts: s.files,
        folder: dir.map(|d| d.display().to_string()),
        paused,
        files: s.views.clone(),
        giveaway: giveaway::view_if_shown(&receipts),
    };
    *ui.uploads.lock().unwrap() = state.clone();
    let _ = app.emit("uploads", state);
}

fn on_menu(app: &AppHandle, id: &str) {
    let ui = app.state::<Ui>();
    match id {
        "window" => show_window(app),
        "upload" => ui.send(Cmd::UploadNow),
        "site" => {
            let _ = app.opener().open_url(format!("{}/contribute.html", ui.shared.server), None::<&str>);
        }
        "change" => pick_folder(app),
        "autostart" => {
            let al = app.autolaunch();
            let _ = if ui.autostart.is_checked().unwrap_or(false) { al.enable() } else { al.disable() };
            let _ = ui.autostart.set_checked(al.is_enabled().unwrap_or(false));
            let _ = app.emit("settings", ());
        }
        "pause" => set_paused(app, ui.pause.is_checked().unwrap_or(false)),
        "quit" => app.exit(0),
        _ => {}
    }
}

fn set_paused(app: &AppHandle, paused: bool) {
    let ui = app.state::<Ui>();
    let mut c = ui.shared.cfg.lock().unwrap();
    c.paused = paused;
    c.save();
    drop(c);
    let _ = ui.pause.set_checked(paused);
    ui.uploads.lock().unwrap().paused = paused;
    ui.send(Cmd::Wake);
    let _ = app.emit("settings", ());
}

/// The tray's "Change WoW folder…": which WoW Forever folder the uploader watches.
fn pick_folder(app: &AppHandle) {
    let handle = app.clone();
    app.dialog().file().set_title("Choose your World of Warcraft folder").pick_folder(move |picked| {
        let Some(path) = picked.and_then(|p| p.into_path().ok()) else { return };
        let Some(dir) = wow::resolve(&path) else {
            message(&handle, &format!("No WoW Forever (_forever_) folder in\n{}\n\nChoose the World of Warcraft folder.", path.display()), MessageDialogKind::Error);
            return;
        };
        let ui = handle.state::<Ui>();
        let mut c = ui.shared.cfg.lock().unwrap();
        c.wow_dir = Some(dir);
        if let Some(root) = wow::root_of(&path) {
            if !c.extra_roots.contains(&root) {
                c.extra_roots.push(root);
            }
        }
        c.save();
        drop(c);
        ui.send(Cmd::Wake);
        let _ = handle.emit("installs", ());
    });
}

fn message(app: &AppHandle, text: &str, kind: MessageDialogKind) {
    app.dialog().message(text).title("EverlookDB").kind(kind).show(|_| {});
}

// ---------------------------------------------------------------------------------------------------------------
// Commands the window calls

#[derive(Serialize)]
struct AppState {
    version: &'static str,
    installs: Vec<wow::Install>,
    selected: Option<String>,
    auto_update: bool,
    update_hours: u32,
    min_hours: u32,
    max_hours: u32,
    autostart: bool,
    autostart_label: &'static str,
    uploads: UploadState,
    catalog_origin: Option<Origin>,
    checked: Option<String>,
}

fn installs(ui: &Ui) -> Vec<wow::Install> {
    let extra = ui.shared.cfg.lock().unwrap().extra_roots.clone();
    wow::all_installs(&extra)
}

fn find_install(ui: &Ui, id: &str) -> Result<wow::Install, String> {
    installs(ui).into_iter().find(|i| i.id == id).ok_or_else(|| "That install isn't there any more.".to_string())
}

#[tauri::command(async)]
fn get_state(app: AppHandle, ui: State<Ui>) -> AppState {
    let list = installs(&ui);
    let cfg = ui.shared.cfg.lock().unwrap().clone();
    let hours = cfg.hours();
    let selected = cfg.selected.filter(|s| list.iter().any(|i| &i.id == s)).or_else(|| {
        // WoW Forever first: it is what EverlookDB is for
        list.iter().find(|i| i.flavor == "forever").or(list.first()).map(|i| i.id.clone())
    });
    let cat = ui.catalog.lock().unwrap();
    AppState {
        version: env!("CARGO_PKG_VERSION"),
        installs: list,
        selected,
        auto_update: cfg.auto_update,
        update_hours: hours,
        min_hours: config::MIN_HOURS,
        max_hours: config::MAX_HOURS,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        autostart_label: AUTOSTART,
        uploads: ui.uploads.lock().unwrap().clone(),
        catalog_origin: cat.origin,
        checked: cat.checked.clone(),
    }
}

#[tauri::command(async)]
fn list_installed(ui: State<Ui>, install: String) -> Result<Vec<manager::Item>, String> {
    let i = find_install(&ui, &install)?;
    let items = manager::list(&i, ui.catalog().as_deref());
    let mut files = ui.icon_files.lock().unwrap();
    files.retain(|(install, _), _| *install != i.id);
    files.extend(items.iter().filter_map(|it| it.icon_file.clone().map(|f| ((i.id.clone(), it.key.clone()), f))));
    Ok(items)
}

/// Icons for the rows the window shows (installed addons and catalog entries, by key), as data: URLs; null for
/// none (the window draws a letter tile). The addon's own icon first, then the one its source gives.
#[tauri::command]
async fn addon_icons(app: AppHandle, install: String, keys: Vec<String>) -> Result<HashMap<String, Option<String>>, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let cat = ui.catalog();
        let jobs: Vec<(String, Option<PathBuf>, Option<String>)> = {
            let files = ui.icon_files.lock().unwrap();
            keys.into_iter()
                .take(64)
                .map(|k| {
                    let file = files.get(&(install.clone(), k.clone())).cloned();
                    let seen = || ui.github_seen.lock().unwrap().get(&k).and_then(icons::source_url);
                    let url = cat.as_deref().and_then(|c| c.get(&k)).and_then(icons::source_url).or_else(seen);
                    (k, file, url)
                })
                .collect()
        };
        // downloads wait on the network: one thread each (a batch is a screenful)
        Ok(std::thread::scope(|s| {
            let running: Vec<_> = jobs
                .into_iter()
                .map(|(k, file, url)| s.spawn(move || (k, file.as_deref().and_then(icons::local).or_else(|| url.as_deref().and_then(icons::remote)))))
                .collect();
            running.into_iter().filter_map(|h| h.join().ok()).collect()
        }))
    })
    .await
}

/// The install, its addon list as text, and how many addons are in it.
fn addon_list_text(ui: &Ui, install: &str) -> Result<(wow::Install, String, usize), String> {
    let i = find_install(ui, install)?;
    let items = manager::list(&i, ui.catalog().as_deref());
    let text = manager::export_text(&i, &items, &chrono::Local::now().format("%Y-%m-%d").to_string());
    Ok((i, text, items.len()))
}

/// Copy the installed addons to the clipboard as plain text. Returns how many. (Done here rather than in the
/// window: the WebView only lets a page write the clipboard while it has the focus.)
#[tauri::command(async)]
fn copy_addon_list(ui: State<Ui>, install: String) -> Result<usize, String> {
    let (_, text, n) = addon_list_text(&ui, &install)?;
    set_clipboard(&text)?;
    Ok(n)
}

/// Copy the giveaway code to the clipboard (whatever parts it has). Returns the code.
#[tauri::command(async)]
fn copy_giveaway_code(ui: State<Ui>) -> Result<String, String> {
    let code = giveaway::view(&ui.shared.cfg.lock().unwrap().receipts).code;
    set_clipboard(&code)?;
    Ok(code)
}

#[cfg(windows)]
fn set_clipboard(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect();
    unsafe {
        // another app may have it open for a moment
        let mut open = false;
        for _ in 0..20 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                open = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !open {
            return Err("the clipboard is busy; try again".into());
        }
        let done = (|| {
            if EmptyClipboard() == 0 {
                return false;
            }
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                return false;
            }
            let p = GlobalLock(mem) as *mut u16;
            if p.is_null() {
                windows_sys::Win32::Foundation::GlobalFree(mem);
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            GlobalUnlock(mem);
            // the clipboard owns the memory once this succeeds
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                windows_sys::Win32::Foundation::GlobalFree(mem);
                return false;
            }
            true
        })();
        CloseClipboard();
        if done { Ok(()) } else { Err("the clipboard didn't take it".into()) }
    }
}

#[cfg(target_os = "macos")]
fn set_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    child.stdin.take().ok_or("no pbcopy")?.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    child.wait().map_err(|e| e.to_string()).map(|_| ())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn set_clipboard(_text: &str) -> Result<(), String> {
    Err("copying isn't available here; save the list as a file instead".into())
}

/// Save the installed addons as a text file where the player chooses. Returns the file, or None (cancelled).
#[tauri::command(async)]
fn save_addon_list(app: AppHandle, ui: State<Ui>, install: String) -> Result<Option<String>, String> {
    let (i, text, _) = addon_list_text(&ui, &install)?;
    let name = format!("{} addons.txt", i.name).replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "");
    let Some(picked) = app.dialog().file().set_title("Save the addon list").set_file_name(&name).add_filter("Text", &["txt"]).blocking_save_file() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let text = if cfg!(windows) { text.replace('\n', "\r\n") } else { text };
    std::fs::write(&path, text).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

#[derive(Serialize)]
struct CatalogReply {
    catalog: Option<Arc<Catalog>>,
    origin: Option<Origin>,
    checked: Option<String>,
}

/// The catalog; `refresh` fetches it again first.
#[tauri::command]
async fn get_catalog(app: AppHandle, refresh: bool) -> Result<CatalogReply, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        if refresh || ui.catalog.lock().unwrap().catalog.is_none() {
            refresh_catalog(&ui);
        }
        let c = ui.catalog.lock().unwrap();
        Ok(CatalogReply { catalog: c.catalog.clone(), origin: c.origin, checked: c.checked.clone() })
    })
    .await
}

/// Run file and network work (blocking reqwest) off the async runtime.
async fn blocking<T: Send + 'static>(app: AppHandle, f: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || f(&app)).await.map_err(|e| e.to_string())?
}

/// Fetch the catalog, and the newest release of each GitHub repo the player added (github.rs), merged into it.
fn refresh_catalog(ui: &Ui) {
    let (cat, origin) = catalog::fetch(&ui.shared.server);
    let repos = ui.shared.cfg.lock().unwrap().github_repos.clone();
    let server = ui.shared.server.clone();
    let custom = github::refresh(&repos, cat.as_ref(), &|repo| github::lookup(&server, repo));
    let cat = match cat {
        Some(c) => Some(c),
        None if !custom.is_empty() => Some(Catalog { version: 1, ..Default::default() }),
        None => None,
    }
    .map(|mut c| {
        github::merge(&mut c, &custom);
        c
    });
    let mut c = ui.catalog.lock().unwrap();
    if let Some(cat) = cat {
        c.catalog = Some(Arc::new(cat));
    }
    c.origin = Some(origin);
    c.checked = Some(chrono::Local::now().format("%H:%M").to_string());
}

/// A download's progress, for the window ("progress" event): addon `id` in `install`, `done` of `total` bytes
/// (total 0: unknown), the `index`th (from 1) of `count` addons in this run.
#[derive(Serialize, Clone)]
struct Progress<'a> {
    install: &'a str,
    id: &'a str,
    done: u64,
    total: u64,
    index: usize,
    count: usize,
}

#[tauri::command]
async fn install_addon(app: AppHandle, install: String, id: String, take_over: Vec<String>) -> Result<manager::Change, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let i = find_install(&ui, &install)?;
        if !i.game {
            return Err(manager::NO_GAME.into());
        }
        let cat = ui.catalog().ok_or("The addon list hasn't loaded yet.")?;
        let addon = cat.get(&id).ok_or("That addon isn't in the list any more.")?;
        manager::install(&i, addon, &take_over, &mut |done, total| {
            let _ = app.emit("progress", Progress { install: &i.id, id: &addon.id, done, total, index: 1, count: 1 });
        })
    })
    .await
}

#[tauri::command(async)]
fn remove_addon(ui: State<Ui>, install: String, id: String) -> Result<String, String> {
    let i = find_install(&ui, &install)?;
    let name = manager::remove(&i, &id)?;
    // an addon from a GitHub link that no install has any more: stop checking its repo
    let still = manager::Records::load().installs.values().any(|m| m.keys().any(|k| k.eq_ignore_ascii_case(&id)));
    if !still {
        let mut c = ui.shared.cfg.lock().unwrap();
        if let Some(repo) = github::repo_of(&c, &id).cloned() {
            github::forget(&mut c, &repo);
            c.save();
            drop(c);
            let mut cs = ui.catalog.lock().unwrap();
            if let Some(cat) = cs.catalog.as_deref() {
                let mut cat = cat.clone();
                cat.addons.retain(|a| !(a.custom && a.id.eq_ignore_ascii_case(&id)));
                cs.catalog = Some(Arc::new(cat));
            }
            config::log(&format!("forgot GitHub repo {repo}"));
        }
    }
    Ok(name)
}

/// What "Install from GitHub" shows before installing: the addon (the catalog's entry when the catalog lists the
/// repo), the file it would install here and whether that is known to be for this game, the version installed now,
/// and the folders already in AddOns that EverlookDB didn't install (installing replaces them, after a backup).
#[derive(Serialize)]
struct GithubPreview {
    addon: catalog::Addon,
    in_catalog: bool,
    /// "yes": made for this game; "unknown": nobody can tell (it may still work); "no": for other game versions
    fit: &'static str,
    file: Option<catalog::File>,
    installed: Option<String>,
    existing: Vec<String>,
}

/// Look up a GitHub link (on everlookdb.com) for `install`.
#[tauri::command]
async fn github_lookup(app: AppHandle, install: String, link: String) -> Result<GithubPreview, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let i = find_install(&ui, &install)?;
        let repo = github::parse_repo(&link)?;
        let cat = ui.catalog();
        let (addon, in_catalog) = match github::in_catalog(cat.as_deref(), &github::id_of(&repo)) {
            Some(a) => (a, true),
            None => {
                let a = github::lookup(&ui.shared.server, &repo)?;
                // the server gives the catalog's own entry for a repo the catalog lists (renamed, other letter case)
                match github::in_catalog(cat.as_deref(), &a.id) {
                    Some(c) => (c, true),
                    None => (catalog::Addon { custom: true, ..a }, false),
                }
            }
        };
        let file = addon.file_for(&i.accepts).cloned();
        let fit = match &file {
            Some(f) if f.flavors.is_empty() => "unknown",
            Some(_) => "yes",
            None => "no",
        };
        let records = manager::Records::load().of(&i.id);
        let record = records.iter().find(|(k, _)| k.eq_ignore_ascii_case(&addon.id)).map(|(_, r)| r);
        let existing = addon
            .folders
            .iter()
            .filter(|f| i.addons_dir.join(f).exists() && !record.is_some_and(|r| r.folders.contains(f)))
            .cloned()
            .collect();
        ui.github_seen.lock().unwrap().insert(addon.id.clone(), addon.clone());
        Ok(GithubPreview { installed: record.map(|r| r.version.clone()), addon, in_catalog, fit, file, existing })
    })
    .await
}

/// Install an addon the player looked up by its GitHub link (github_lookup). `take_over`: the folders already in
/// AddOns it may replace (the player saw them; they are backed up first). A repo the catalog doesn't list is
/// remembered, so it is updated like the catalog's addons.
#[tauri::command]
async fn install_from_github(app: AppHandle, install: String, id: String, take_over: Vec<String>) -> Result<manager::Change, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let i = find_install(&ui, &install)?;
        if !i.game {
            return Err(manager::NO_GAME.into());
        }
        let addon = ui.github_seen.lock().unwrap().get(&id).cloned().ok_or("Look the link up again, then install.")?;
        let change = manager::install(&i, &addon, &take_over, &mut |done, total| {
            let _ = app.emit("progress", Progress { install: &i.id, id: &addon.id, done, total, index: 1, count: 1 });
        })?;
        if addon.custom {
            let repo = addon.id.strip_prefix("github:").unwrap_or(&addon.id).to_string();
            {
                let mut c = ui.shared.cfg.lock().unwrap();
                github::remember(&mut c, &repo, &addon);
                c.save();
            }
            let mut cs = ui.catalog.lock().unwrap();
            let mut cat = cs.catalog.as_deref().cloned().unwrap_or(Catalog { version: 1, ..Default::default() });
            let custom: Vec<catalog::Addon> =
                cat.addons.iter().filter(|a| a.custom && !a.id.eq_ignore_ascii_case(&addon.id)).cloned().chain(std::iter::once(addon.clone())).collect();
            github::merge(&mut cat, &custom);
            cs.catalog = Some(Arc::new(cat));
            config::log(&format!("added GitHub repo {repo}"));
        }
        Ok(change)
    })
    .await
}

#[tauri::command(async)]
fn rollback_addon(ui: State<Ui>, install: String, id: String) -> Result<manager::Change, String> {
    let i = find_install(&ui, &install)?;
    if !i.game {
        return Err(manager::NO_GAME.into());
    }
    manager::rollback(&i, &id)
}

#[derive(Serialize, Clone)]
struct Updated {
    changes: Vec<manager::Change>,
    failed: Vec<String>,
}

/// Update everything we installed in `targets`, telling the window how far each download is and which of how many.
fn update_installs(app: &AppHandle, targets: &[wow::Install], cat: &catalog::Catalog) -> Updated {
    let count: usize = targets.iter().map(|i| manager::pending(i, cat).len()).sum();
    let mut out = Updated { changes: Vec::new(), failed: Vec::new() };
    let mut index = 0;
    let mut current = String::new();
    for i in targets {
        let (c, f) = manager::update_all(i, cat, &mut |id, done, total| {
            if id != current {
                current = id.to_string();
                index += 1;
            }
            let _ = app.emit("progress", Progress { install: &i.id, id, done, total, index, count: count.max(index) });
        });
        out.changes.extend(c);
        out.failed.extend(f);
    }
    out
}

/// Update everything we installed, in one install or (None) every install.
#[tauri::command]
async fn update_all(app: AppHandle, install: Option<String>) -> Result<Updated, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let cat = ui.catalog().ok_or("The addon list hasn't loaded yet.")?;
        let targets: Vec<wow::Install> = match install {
            Some(id) => vec![find_install(&ui, &id)?],
            None => installs(&ui),
        };
        Ok(update_installs(app, &targets, &cat))
    })
    .await
}

/// Ask for a `World of Warcraft` folder and remember it. Returns the folder added, or None (cancelled).
#[tauri::command(async)]
fn add_folder(app: AppHandle, ui: State<Ui>) -> Result<Option<String>, String> {
    let Some(picked) = app.dialog().file().set_title("Choose a World of Warcraft folder").blocking_pick_folder() else { return Ok(None) };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let root = wow::root_of(&path)
        .ok_or_else(|| format!("No game in {}. Choose the World of Warcraft folder (the one with _retail_, _classic_ or _forever_ in it).", path.display()))?;
    let mut c = ui.shared.cfg.lock().unwrap();
    if !c.extra_roots.contains(&root) {
        c.extra_roots.push(root.clone());
    }
    if c.wow_dir.as_ref().is_none_or(|d| !d.is_dir()) {
        c.wow_dir = wow::resolve(&root);
    }
    c.save();
    drop(c);
    ui.send(Cmd::Wake);
    Ok(Some(root.display().to_string()))
}

#[tauri::command(async)]
fn forget_folder(ui: State<Ui>, root: String) {
    let mut c = ui.shared.cfg.lock().unwrap();
    c.extra_roots.retain(|r| r.to_string_lossy() != root);
    c.save();
}

#[tauri::command(async)]
fn set_setting(app: AppHandle, ui: State<Ui>, name: String, value: serde_json::Value) -> Result<(), String> {
    match name.as_str() {
        "update_hours" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.set_hours(&value);
            c.save();
            drop(c);
            let _ = app.emit("settings", ());
        }
        "auto_update" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.auto_update = value.as_bool().unwrap_or(true);
            c.save();
        }
        "paused" => set_paused(&app, value.as_bool().unwrap_or(false)),
        "autostart" => {
            let al = app.autolaunch();
            let _ = if value.as_bool().unwrap_or(false) { al.enable() } else { al.disable() };
            let _ = ui.autostart.set_checked(al.is_enabled().unwrap_or(false));
        }
        "selected" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.selected = value.as_str().map(str::to_string);
            c.save();
        }
        _ => return Err(format!("unknown setting {name}")),
    }
    Ok(())
}

#[tauri::command(async)]
fn upload_now(ui: State<Ui>) {
    ui.send(Cmd::UploadNow);
}

#[tauri::command(async)]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links open".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn open_addons_folder(app: AppHandle, ui: State<Ui>, install: String) -> Result<(), String> {
    let i = find_install(&ui, &install)?;
    let dir = if i.addons_dir.is_dir() { i.addons_dir } else { i.dir };
    app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Background

/// Refresh the catalog soon after start and then every `update_hours` (the player's setting, at least an hour); when
/// automatic updates are on, update what we installed. A changed interval counts from the last check, without a
/// restart.
fn spawn_hourly(app: AppHandle) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut last: Option<Instant> = None;
        let mut first = true;
        loop {
            let hours = app.state::<Ui>().shared.cfg.lock().unwrap().hours();
            let due = match last {
                None => started + Duration::from_secs(5),
                Some(t) => t + Duration::from_secs(u64::from(hours) * 3600),
            };
            let now = Instant::now();
            if now < due {
                // look at the setting again now and then, so a new interval takes effect
                std::thread::sleep((due - now).min(Duration::from_secs(15)));
                continue;
            }
            last = Some(now);
            let ui = app.state::<Ui>();
            // the window may have just loaded it
            if !(first && ui.catalog().is_some()) {
                refresh_catalog(&ui);
                let _ = app.emit("catalog", ());
            }
            first = false;
            if !ui.shared.cfg.lock().unwrap().auto_update {
                continue;
            }
            let Some(cat) = ui.catalog() else { continue };
            let out = update_installs(&app, &installs(&ui), &cat);
            for f in &out.failed {
                config::log(&format!("auto-update failed: {f}"));
            }
            if !out.changes.is_empty() || !out.failed.is_empty() {
                let _ = app.emit("updated", out);
            }
        }
    });
}

/// Check for a new version of the app a minute after start and then every hour; install it silently and restart.
fn spawn_updater(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(60);
        loop {
            std::thread::sleep(wait);
            wait = HOURLY;
            let result = tauri::async_runtime::block_on(async {
                let Some(update) = app.updater()?.check().await? else { return Ok(false) };
                config::log(&format!("updating to {}", update.version));
                update.download_and_install(|_, _| {}, || {}).await?;
                Ok::<bool, tauri_plugin_updater::Error>(true)
            });
            match result {
                Ok(true) => app.restart(),
                Ok(false) => {}
                Err(e) => config::log(&format!("update check failed: {e}")),
            }
        }
    });
}
