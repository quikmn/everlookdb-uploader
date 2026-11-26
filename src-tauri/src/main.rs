// EverlookDB: an ad-free addon manager for World of Warcraft that also uploads the EverlookDB addons' data files
// whenever the game saves them, so players never have to upload them by hand on everlookdb.com/contribute.html.
// It lives in the tray; the window opens from there and is closed (not hidden) when the player closes it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod addons;
mod blp;
mod catalog;
mod config;
mod delog;
mod giveaway;
mod github;
mod health;
mod icons;
mod manager;
mod ours;
mod report;
mod shared;
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
const HELP: &str = "EverlookDB: addon manager for World of Warcraft, and uploader of the EverlookDB addons' data to everlookdb.com.

Usage: everlookdb-uploader [options]
  --server <url>   server to upload to (default https://everlookdb.com; or EVERLOOKDB_SERVER)
  --wow <folder>   use (and remember) this WoW Forever folder for uploads
  --check          print the installs, addons and data files found, then exit
  --once           upload every changed data file once, print the results, then exit
  --shared         fetch the shared auction prices once, write the data addon, print what it did, then exit
  --background     start quietly in the tray (used at login)
  --send-report    send a problem report to the developer (as the window's button does), print its id, then exit;
                   --what <text> adds a note, --test marks it as a test report
Settings live in %APPDATA%\\com.everlookdb.uploader (macOS: ~/Library/Application Support/com.everlookdb.uploader),
or EVERLOOKDB_UPLOADER_DIR. EVERLOOKDB_CATALOG points the addon catalog elsewhere (a URL or a local file).";

struct Args {
    server: String,
    wow: Option<PathBuf>,
    check: bool,
    once: bool,
    shared: bool,
    background: bool,
    send_report: bool,
    test: bool,
    what: String,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        server: std::env::var("EVERLOOKDB_SERVER").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_SERVER.into()),
        wow: None,
        check: false,
        once: false,
        shared: false,
        background: false,
        send_report: false,
        test: false,
        what: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--server" => a.server = it.next().ok_or("--server needs a URL")?,
            "--wow" => a.wow = Some(it.next().ok_or("--wow needs a folder")?.into()),
            "--check" => a.check = true,
            "--once" => a.once = true,
            "--shared" => a.shared = true,
            "--background" => a.background = true,
            "--send-report" => a.send_report = true,
            "--test" => a.test = true,
            "--what" => a.what = it.next().ok_or("--what needs a text")?,
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
    if args.send_report {
        attach_console();
        std::process::exit(send_report_once(&args));
    }
    if args.check || args.once || args.shared {
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
    if args.shared {
        let mut poller = shared::Poller::new(&args.server);
        let (view, _) = poller.refresh(&dir, cfg.shared_prices);
        println!("Price Feed: {}", if view.on { "on" } else { "off" });
        let now = chrono::Utc::now().timestamp();
        for h in &view.houses {
            println!("  {} ({} {}): scan {} min old{}", h.key, h.realm, h.side, (now - h.at).max(0) / 60, if h.late { " (late: the site should have a newer one)" } else { "" });
        }
        if let Some(n) = &view.note {
            println!("  {n}");
        }
        if view.reader_off > 0 {
            println!("  EverlookDB Auctions is turned off in the game's addon list on {} character(s) with the Price Feed on", view.reader_off);
        }
        if poller.failing() {
            println!("  The check failed (see uploader.log); the app tries again by itself");
        }
        return if poller.failing() { 2 } else { 0 };
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
        println!("  {}: {} ({})", v.label, v.headline, v.short);
        for d in &v.details {
            println!("    {d}");
        }
    }
    if status.kind == Kind::Ok || status.kind == Kind::Idle { 0 } else { 2 }
}

/// --send-report: the report the window's button sends, built from the settings folder, sent once.
fn send_report_once(args: &Args) -> i32 {
    let cfg = Config::load();
    let installs = wow::all_installs(&cfg.extra_roots);
    let cat = catalog::cached_labelled();
    let what = if args.what.is_empty() && args.test { "TEST report from the command line; delete it" } else { args.what.as_str() };
    let inp = report::Inputs { cfg: &cfg, installs: &installs, catalog: cat.as_ref(), price_feed: None, what, shown: "", test: args.test };
    let out = make_report(&args.server, &inp, false);
    match (&out.id, &out.error) {
        (Some(id), _) => {
            println!("Sent: report {id}");
            println!("What was sent: {}", out.text.as_deref().unwrap_or(""));
            0
        }
        (None, e) => {
            println!("Not sent: {}", e.as_deref().unwrap_or("?"));
            if let Some(f) = &out.file {
                println!("Kept: {f}");
            }
            2
        }
    }
}

/// What one press of "Send error logs to the developer" did.
#[derive(Serialize, Clone, Default)]
struct ReportOutcome {
    /// the report's id from the server ("K7Q2M9")
    id: Option<String>,
    /// why it wasn't sent
    error: Option<String>,
    /// the zip kept instead, when it wasn't sent
    file: Option<String>,
    /// the exact text of what was (or would have been) sent: reports/last-report.txt
    text: Option<String>,
    /// the server couldn't be reached: it waits in reports/outbox/ and goes by itself when it's back
    queued: bool,
}

/// Build, scrub and send a report. When the server can't be reached and `queue`: it waits in the outbox and goes by
/// itself when the server is back (spawn_health); otherwise a zip is kept.
fn make_report(server: &str, inp: &report::Inputs, queue: bool) -> ReportOutcome {
    let log = report::log_tail(&config::dir(), report::MAX_LOG_LINES, report::MAX_LOG_BYTES);
    let r = report::build(inp, &log);
    let text = report::write_text("last-report.txt", &report::readable(&r)).ok().map(|p| p.display().to_string());
    let gz = report::encode(&r).map_err(|e| report::NotSent::now(format!("couldn't pack the report ({e})")));
    let sent = gz.clone().and_then(|gz| report::send(server, gz));
    match sent {
        Ok(id) => {
            config::log(&format!("problem report sent: {id}"));
            ReportOutcome { id: Some(id), text, ..Default::default() }
        }
        Err(e) if e.later && queue && gz.as_ref().is_ok_and(|gz| report::queue(&report::outbox_dir(), gz, inp.shown).is_ok()) => {
            config::log(&format!("problem report waits for the server ({}); it goes by itself when it's back", e.msg));
            ReportOutcome { error: Some(e.msg), queued: true, text, ..Default::default() }
        }
        Err(e) => {
            let e = e.msg;
            config::log(&format!("problem report not sent: {e}"));
            let file = report::keep_copy(&r).map(|p| p.display().to_string()).map_err(|k| config::log(&format!("problem report not kept: {k}"))).ok();
            ReportOutcome { error: Some(e), file, text, ..Default::default() }
        }
    }
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
    /// what the shared-prices thread last did (shared.rs)
    shared_view: Mutex<shared::View>,
    catalog: Mutex<CatalogState>,
    /// (install id, item key) -> the texture its .toc names as its icon (from the last list of that install)
    icon_files: Mutex<HashMap<(String, String), PathBuf>>,
    /// addons looked up by their GitHub link and shown to the player, by id: what "Install" installs
    github_seen: Mutex<HashMap<String, catalog::Addon>>,
    /// where the window is now (saved to config.json when it closes)
    window_rect: Mutex<Option<config::WindowRect>>,
    /// the `World of Warcraft` folders found (the drive scan runs at start and on demand, not on every click)
    roots: Mutex<Option<Vec<PathBuf>>>,
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
    /// when the addon list was last fetched (local time, RFC 3339)
    checked: Option<String>,
    /// the last automatic check: when, and what it did
    auto: Option<AutoRun>,
    /// when the next automatic check is due (local time, RFC 3339)
    next: Option<String>,
    /// a fetch is running (the list shown may be the copy from last time)
    loading: bool,
    /// when the last fetch finished, so two asks close together fetch once
    fetched_at: Option<Instant>,
}

/// One run of the automatic check, for the window's footer.
#[derive(Serialize, Clone)]
struct AutoRun {
    at: String,
    /// "Update addons by itself" was off: the list was checked, nothing installed
    off: bool,
    updated: usize,
    failed: usize,
    /// updates waiting because the game is running (they go in once it is closed)
    #[serde(default)]
    waiting: usize,
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
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
/// This start is the app's first (no settings file yet): the welcome card offers our addons (ours.rs).
static NEW_INSTALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run(args: Args) {
    // no settings file yet: this is a new install
    let fresh = !Config::path().exists();
    NEW_INSTALL.store(fresh, std::sync::atomic::Ordering::Relaxed);
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
    // the window never opens by itself: at start only when the player started the app (see show_at_start), after
    // that only when they click the tray icon or start the app again
    let restart = config::Restart::take();
    let show_at_start = show_at_start(args.background, restart.is_some(), test_instance, cfg!(debug_assertions));
    if let Some(r) = &restart {
        config::log(&format!("started after an update{}; the window stays closed", if r.to.is_empty() { String::new() } else { format!(" to {}", r.to) }));
        let mut c = shared.cfg.lock().unwrap();
        c.note_recent(config::RecentUpdate { at: now_rfc3339(), install: String::new(), name: "EverlookDB app".into(), from: None, to: env!("CARGO_PKG_VERSION").into() });
        c.save();
    }
    if !test_instance {
        // a second start is the player asking for the window
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
            uninstall_plan,
            remove_addon,
            rollback_addon,
            set_frozen,
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
            our_addons,
            install_ours,
            send_report,
            report_text,
            show_report_file,
            copy_report_id,
            note_error,
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
        RunEvent::WindowEvent { event: tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_), .. } => remember_window(app),
        RunEvent::WindowEvent { event: tauri::WindowEvent::Destroyed, .. } => {
            save_window(app);
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

/// Whether the window opens at start: only when the player started the app themselves. Never at login
/// (`--background`), never after the app updated itself (`after_update`: it was started by the installer), never
/// in a development or test run.
fn show_at_start(background: bool, after_update: bool, test_instance: bool, debug: bool) -> bool {
    !background && !after_update && !test_instance && !debug
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
    // read the settings once, before app.manage: a lock taken inside that one statement is held until it ends, so a
    // second lock in it waits on the first forever (0.7.0 hung at every start that way)
    let (giveaway_view, shared_on) = {
        let c = shared.cfg.lock().unwrap();
        (giveaway::view_if_shown(&c.receipts), c.shared_prices)
    };
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
            giveaway: giveaway_view,
            ..Default::default()
        }),
        shared_view: Mutex::new(shared::View { on: shared_on, ..Default::default() }),
        catalog: Mutex::new(CatalogState::default()),
        icon_files: Mutex::new(HashMap::new()),
        github_seen: Mutex::new(HashMap::new()),
        window_rect: Mutex::new(None),
        roots: Mutex::new(None),
    });
    std::thread::spawn(icons::prune);
    // an install cut short last time (a crash, a power cut) is put right: the old addon back, or the swap finished
    let ui_handle = app.clone();
    std::thread::spawn(move || {
        rescan(&ui_handle);
        for i in installs(&ui_handle.state::<Ui>()) {
            manager::recover_leftovers(&i.addons_dir);
        }
    });

    let for_shared = shared.clone();
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

    let (handle, cfg_of) = (app.clone(), for_shared);
    let server = cfg_of.server.clone();
    std::thread::spawn(move || {
        shared::run(
            move || {
                let c = cfg_of.cfg.lock().unwrap();
                (c.wow_dir.clone(), c.shared_prices)
            },
            server,
            |v: &shared::View| {
                *handle.state::<Ui>().shared_view.lock().unwrap() = v.clone();
                let _ = handle.emit("shared", v.clone());
            },
        )
    });

    if show {
        show_window(app);
    }
    spawn_addon_checks(app.clone());
    spawn_updater(app.clone());
    spawn_health(app.clone());
    Ok(())
}

/// Show the window, creating it if it was closed, where it was last (centred the first time). Only for the
/// player's own clicks: the tray icon or menu, a second start, starting the app by hand. Nothing in the background
/// (updates, checks, errors) ever calls this; their state waits in the window and the log (see the test
/// no_background_path_shows_the_window).
fn show_window(app: &AppHandle) {
    config::log("window: shown (the player asked for it)");
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    let screens: Vec<(f64, f64, f64, f64)> = app
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let s = m.scale_factor();
            (m.position().x as f64 / s, m.position().y as f64 / s, m.size().width as f64 / s, m.size().height as f64 / s)
        })
        .collect();
    let last = app.state::<Ui>().shared.cfg.lock().unwrap().window.filter(|r| r.on_screen(&screens));
    let mut b = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("EverlookDB")
        .min_inner_size(760.0, 520.0)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(12, 20, 34, 255))
        .focused(true);
    b = match last {
        Some(r) => b.inner_size(r.w, r.h).position(r.x, r.y),
        None => b.inner_size(1100.0, 740.0).center(),
    };
    let built = b.build();
    // a game installed or moved since: looked for again in the background (the window gets "installs" if so)
    let again = app.clone();
    std::thread::spawn(move || rescan(&again));
    match built {
        Ok(w) => app_keys_only(&w),
        Err(e) => config::log(&format!("window: {e}")),
    }
}

/// An app, not a browser: WebView2's browser keys (F5 / Ctrl+R reload, Ctrl+P print, Alt+Left back, F12 devtools,
/// zoom) are off in release builds; copy, paste and undo still work. Debug builds keep them, for the devtools. The
/// browser's right-click menu is replaced in app.js (it stays in text fields, for copy and paste).
#[cfg(all(windows, not(debug_assertions)))]
fn app_keys_only(w: &tauri::WebviewWindow) {
    let _ = w.with_webview(|wv| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
        use windows_core::Interface;
        let Ok(core) = wv.controller().CoreWebView2() else { return };
        let Ok(settings) = core.Settings() else { return };
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
        }
    });
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn app_keys_only(_w: &tauri::WebviewWindow) {}

/// Note where the window is now (not while minimized or maximized).
fn remember_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    if w.is_minimized().unwrap_or(false) || w.is_maximized().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(size), Ok(s)) = (w.outer_position(), w.inner_size(), w.scale_factor()) else { return };
    let r = config::WindowRect { x: pos.x as f64 / s, y: pos.y as f64 / s, w: size.width as f64 / s, h: size.height as f64 / s };
    if let Some(ui) = app.try_state::<Ui>() {
        *ui.window_rect.lock().unwrap() = Some(r);
    }
}

/// Keep where the window was, for the next time it opens.
fn save_window(app: &AppHandle) {
    let Some(ui) = app.try_state::<Ui>() else { return };
    let Some(r) = *ui.window_rect.lock().unwrap() else { return };
    let mut c = ui.shared.cfg.lock().unwrap();
    if c.window != Some(r) {
        c.window = Some(r);
        c.save();
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
        rescan(&handle);
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
    app_auto_update: bool,
    /// the app's own update, in a few words ("0.7.5 installs when this window is closed"), when there is one
    app_update: Option<String>,
    /// updates done by themselves, newest first
    recent_updates: Vec<config::RecentUpdate>,
    shared_prices: bool,
    shared: shared::View,
    autostart: bool,
    autostart_label: &'static str,
    uploads: UploadState,
    catalog_origin: Option<Origin>,
    checked: Option<String>,
    /// the background jobs' messages (health.rs) and reports waiting for the server
    health: HealthView,
}

/// What the window's one inbox gets from the background (the "health" event, and get_state).
#[derive(Serialize, Clone, Default, PartialEq)]
struct HealthView {
    lines: Vec<health::Line>,
    /// problem reports waiting for the server (they go by themselves)
    reports_waiting: usize,
}

static HEALTH_VIEW: Mutex<Option<HealthView>> = Mutex::new(None);

fn health_view() -> HealthView {
    HealthView { lines: health::lines(), reports_waiting: report::waiting_count(&report::outbox_dir()) }
}

/// A report that had waited went out: the window shows its id next to what it was about.
#[derive(Serialize, Clone)]
struct ReportSent {
    shown: String,
    id: String,
}

/// Every few seconds: what the inbox should show (health.rs: a failure becomes a line only after health::QUIET_FOR,
/// without any new event, so this looks by itself), and the reports waiting for the server, sent once it is back (at
/// once when another job reaches it, otherwise every two minutes).
fn spawn_health(app: AppHandle) {
    std::thread::spawn(move || {
        let mut back = health::back();
        let mut tried: Option<Instant> = None;
        loop {
            let dir = report::outbox_dir();
            if report::waiting_count(&dir) > 0 && (health::back() != back || tried.is_none_or(|t| t.elapsed() >= Duration::from_secs(120))) {
                back = health::back();
                tried = Some(Instant::now());
                let server = app.state::<Ui>().shared.server.clone();
                for (shown, id) in report::flush_outbox(&dir, |gz| report::send(&server, gz)) {
                    let _ = app.emit("report-sent", ReportSent { shown, id });
                }
            }
            let v = health_view();
            let mut last = HEALTH_VIEW.lock().unwrap();
            if last.as_ref() != Some(&v) {
                *last = Some(v.clone());
                drop(last);
                let _ = app.emit("health", v);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

/// The installs in the folders found (scanning for them only when that hasn't happened yet).
fn installs(ui: &Ui) -> Vec<wow::Install> {
    let known = ui.roots.lock().unwrap().clone();
    let roots = match known {
        Some(r) => r,
        None => {
            let extra = ui.shared.cfg.lock().unwrap().extra_roots.clone();
            let r = wow::roots(&extra);
            *ui.roots.lock().unwrap() = Some(r.clone());
            r
        }
    };
    wow::installs_of(&roots)
}

/// Look for the game again (the usual places on the local drives, plus the folders the player added); tells the
/// window when what was found changed.
fn rescan(app: &AppHandle) {
    let ui = app.state::<Ui>();
    let extra = ui.shared.cfg.lock().unwrap().extra_roots.clone();
    let found = wow::roots(&extra);
    let changed = {
        let mut r = ui.roots.lock().unwrap();
        let changed = r.as_ref() != Some(&found);
        *r = Some(found);
        changed
    };
    if changed {
        let _ = app.emit("installs", ());
    }
}

fn find_install(ui: &Ui, id: &str) -> Result<wow::Install, String> {
    installs(ui).into_iter().find(|i| i.id == id).ok_or_else(|| "That install isn't there any more.".to_string())
}

#[tauri::command(async)]
fn get_state(app: AppHandle, ui: State<Ui>) -> AppState {
    let list = installs(&ui);
    let cfg = ui.shared.cfg.lock().unwrap().clone();
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
        app_auto_update: cfg.app_auto_update,
        app_update: APP_UPDATE.lock().unwrap().clone(),
        recent_updates: cfg.recent_updates.clone(),
        shared_prices: cfg.shared_prices,
        shared: ui.shared_view.lock().unwrap().clone(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        autostart_label: AUTOSTART,
        uploads: ui.uploads.lock().unwrap().clone(),
        catalog_origin: cat.origin,
        checked: cat.checked.clone(),
        health: HEALTH_VIEW.lock().unwrap().clone().unwrap_or_else(health_view),
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
    auto: Option<AutoRun>,
    next: Option<String>,
    loading: bool,
}

/// The catalog; `refresh` fetches it again first. Without `refresh`, the first ask answers at once with the copy from
/// last time (if any) and fetches the fresh one in the background ("catalog" event when it is in).
#[tauri::command]
async fn get_catalog(app: AppHandle, refresh: bool) -> Result<CatalogReply, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        if refresh {
            refresh_catalog(&ui, None);
        } else {
            let start = {
                let mut c = ui.catalog.lock().unwrap();
                let start = c.catalog.is_none() && !c.loading && c.fetched_at.is_none();
                if start {
                    c.loading = true;
                    let cached = catalog::cached_labelled();
                    let repos = ui.shared.cfg.lock().unwrap().github_repos.clone();
                    let custom = github::saved(&repos, cached.as_ref());
                    let shown = match cached {
                        Some(c) => Some(c),
                        None if !custom.is_empty() => Some(Catalog { version: 1, ..Default::default() }),
                        None => None,
                    };
                    c.catalog = shown.map(|mut cat| {
                        github::merge(&mut cat, &custom);
                        Arc::new(cat)
                    });
                }
                start
            };
            if start {
                let app = app.clone();
                std::thread::spawn(move || {
                    refresh_catalog(&app.state::<Ui>(), Some(Duration::from_secs(60)));
                    let _ = app.emit("catalog", ());
                });
            }
        }
        let c = ui.catalog.lock().unwrap();
        Ok(CatalogReply { catalog: c.catalog.clone(), origin: c.origin, checked: c.checked.clone(), auto: c.auto.clone(), next: c.next.clone(), loading: c.loading })
    })
    .await
}

/// Run file and network work (blocking reqwest) off the async runtime.
async fn blocking<T: Send + 'static>(app: AppHandle, f: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || f(&app)).await.map_err(|e| e.to_string())?
}

/// One fetch at a time.
static REFRESH: Mutex<()> = Mutex::new(());

/// Fetch the catalog, and the newest release of each GitHub repo the player added (github.rs), merged into it.
/// `max_age`: skip it when a fetch finished that recently (the window and the background both ask at start).
fn refresh_catalog(ui: &Ui, max_age: Option<Duration>) {
    let _one = REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(age), Some(at)) = (max_age, ui.catalog.lock().unwrap().fetched_at) {
        if at.elapsed() < age {
            return;
        }
    }
    ui.catalog.lock().unwrap().loading = true;
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
    // one line per fetch, so the log shows the catalog came in (the check after a start)
    config::log(&format!("catalog: {} addons ({:?})", cat.as_ref().map_or(0, |c| c.addons.len()), origin).to_lowercase());
    // the server not answering: the list from last time stays, quietly (health.rs)
    match origin {
        Origin::Cache | Origin::None => health::fail(health::Job::Catalog, "", health::Trouble::Unreachable("the addon list didn't load".into())),
        _ => health::ok(health::Job::Catalog, ""),
    }
    let mut c = ui.catalog.lock().unwrap();
    if let Some(cat) = cat {
        c.catalog = Some(Arc::new(cat));
    }
    c.origin = Some(origin);
    // when the list last came from the server (a failed check keeps the time of the last good one)
    if origin == Origin::Live {
        c.checked = Some(now_rfc3339());
    }
    c.loading = false;
    c.fetched_at = Some(Instant::now());
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

/// Install or update an addon. A folder in the way comes back as `conflict` (the window offers "Replace (backup
/// kept)", which asks again with those folders in `take_over`).
#[tauri::command]
async fn install_addon(app: AppHandle, install: String, id: String, take_over: Vec<String>) -> Result<manager::Change, manager::Failure> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let app = app2;
        let ui = app.state::<Ui>();
        let i = find_install(&ui, &install)?;
        if !i.game {
            return Err(manager::NO_GAME.into());
        }
        let cat = ui.catalog().ok_or("The addon list hasn't loaded yet.")?;
        let addon = cat.get(&id).ok_or("That addon isn't in the list any more.")?;
        let r = manager::install(&i, addon, &take_over, &mut |done, total| {
            let _ = app.emit("progress", Progress { install: &i.id, id: &addon.id, done, total, index: 1, count: 1 });
        });
        if let Err(e) = &r {
            if !e.conflict.is_empty() {
                manager::Conflicts::remember(&i.id, &addon.id, &addon.version, &e.conflict);
            }
            report::note_error("install", &format!("{} into {}: {}", addon.name, i.name, e.message));
        }
        r
    })
    .await
    .map_err(|e| manager::Failure::from(e.to_string()))?
}

/// What uninstalling an addon would remove (its folders, and the ones kept because another addon lists them too),
/// for the confirmation.
#[tauri::command(async)]
fn uninstall_plan(ui: State<Ui>, install: String, id: String) -> Result<manager::Plan, String> {
    let i = find_install(&ui, &install)?;
    let cat = ui.catalog();
    manager::plan(&i, cat.as_deref(), &id)
}

/// Uninstall any listed addon, whoever installed it: exactly the `folders` the player confirmed go to the Recycle
/// Bin. Returns its name.
#[tauri::command(async)]
fn remove_addon(ui: State<Ui>, install: String, id: String, folders: Vec<String>) -> Result<String, String> {
    let i = find_install(&ui, &install)?;
    let cat = ui.catalog();
    let done = manager::uninstall(&i, cat.as_deref(), &id, &folders, &mut manager::recycle)
        .inspect_err(|e| report::note_error("uninstall", &format!("{id} in {}: {e}", i.name)))?;
    let name = done.name;
    // uninstalling ends a freeze: a new install of it updates again
    {
        let mut c = ui.shared.cfg.lock().unwrap();
        if !c.frozen_in(&i.id).is_disjoint(&done.folders.iter().map(|f| f.to_ascii_lowercase()).collect()) {
            c.unfreeze(&i.id, &done.folders);
            c.save();
        }
    }
    if !done.managed {
        return Ok(name);
    }
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
        })
        .map_err(|e| {
            report::note_error("install", &format!("{} (GitHub) into {}: {}", addon.name, i.name, e.message));
            e.message
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

/// "Freeze" (never update this addon) or "Unfreeze" a listed addon. Kept in config.json by its main folder, which
/// stays the same from version to version.
#[tauri::command(async)]
fn set_frozen(ui: State<Ui>, install: String, id: String, frozen: bool) -> Result<(), String> {
    let i = find_install(&ui, &install)?;
    let items = manager::list(&i, ui.catalog().as_deref());
    let it = items.iter().find(|x| x.key == id).ok_or("That addon isn't in the AddOns folder any more.")?;
    let mut c = ui.shared.cfg.lock().unwrap();
    if frozen {
        c.freeze(&i.id, &it.main);
    } else {
        c.unfreeze(&i.id, &it.folders);
    }
    c.save();
    config::log(&format!("{} {} ({}) in {}", if frozen { "froze" } else { "unfroze" }, it.name, it.main, i.id));
    Ok(())
}

#[tauri::command(async)]
fn rollback_addon(ui: State<Ui>, install: String, id: String) -> Result<manager::Change, String> {
    let i = find_install(&ui, &install)?;
    if !i.game {
        return Err(manager::NO_GAME.into());
    }
    manager::rollback(&i, &id).map_err(|e| {
        report::note_error("rollback", &format!("{id} in {}: {}", i.name, e.message));
        e.message
    })
}

#[derive(Serialize, Clone)]
struct Updated {
    changes: Vec<manager::Change>,
    failed: Vec<manager::Failed>,
}

/// Update everything we installed in `targets`, telling the window how far each download is and which of how many.
/// `background`: the automatic check (its failures go to health.rs, which notes them for reports when it shows them);
/// otherwise the player asked, and each failure is noted at once.
fn update_installs(app: &AppHandle, targets: &[wow::Install], cat: &catalog::Catalog, background: bool) -> Updated {
    // one job from start to end, so the app's self-update doesn't slip in between two addons
    let _busy = manager::Busy::start();
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
        for x in &f {
            if !background {
                report::note_error("update", &format!("{} in {}: {}", x.name, i.name, x.message));
            }
        }
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
        Ok(update_installs(app, &targets, &cat, false))
    })
    .await
}

/// Our addons for an install (ours.rs), and whether the welcome card shows there.
#[derive(Serialize)]
struct OursReply {
    addons: Vec<ours::Pick>,
    card: bool,
    /// the game is running now: what gets installed loads after a /reload
    running: bool,
}

fn our_picks(ui: &Ui, i: &wow::Install) -> Vec<ours::Pick> {
    let Some(cat) = ui.catalog() else { return Vec::new() };
    let installed = manager::list(i, Some(&cat)).into_iter().map(|it| it.key).collect();
    ours::pick(&ours::list(), &cat, &i.accepts, &installed)
}

#[tauri::command(async)]
fn our_addons(ui: State<Ui>, install: String) -> Result<OursReply, String> {
    let i = find_install(&ui, &install)?;
    let addons = our_picks(&ui, &i);
    let dismissed = ui.shared.cfg.lock().unwrap().ours_card_dismissed;
    let card = ours::show_card(dismissed, i.game, NEW_INSTALL.load(std::sync::atomic::Ordering::Relaxed), &addons);
    Ok(OursReply { addons, card, running: i.game && wow::game_running(&i) })
}

/// How one addon of an "Install all" went, for the window ("ours" event) as soon as it is done.
#[derive(Serialize, Clone)]
struct OursStep<'a> {
    install: &'a str,
    id: &'a str,
    change: Option<&'a manager::Change>,
    failure: Option<&'a manager::Failure>,
}

/// Install our addons that aren't in the install yet (only `ids`, the ones the player left ticked), one after the
/// other, each the way every install goes (manager::install: download, check, staging folder, swap, the old copy
/// kept until the new one is in). One job from start to end, so the app's self-update waits for the whole run.
#[tauri::command]
async fn install_ours(app: AppHandle, install: String, ids: Vec<String>) -> Result<Updated, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        let i = find_install(&ui, &install)?;
        if !i.game {
            return Err(manager::NO_GAME.into());
        }
        let cat = ui.catalog().ok_or("The addon list hasn't loaded yet.")?;
        let _busy = manager::Busy::start();
        let picks = our_picks(&ui, &i);
        let todo: Vec<&catalog::Addon> = ours::missing(&picks, Some(&ids)).into_iter().filter_map(|p| cat.get(&p.id)).collect();
        config::log(&format!("install our addons into {}: {}", i.id, todo.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")));
        let count = todo.len();
        let mut out = Updated { changes: Vec::new(), failed: Vec::new() };
        for (n, addon) in todo.into_iter().enumerate() {
            let r = manager::install(&i, addon, &[], &mut |done, total| {
                let _ = app.emit("progress", Progress { install: &i.id, id: &addon.id, done, total, index: n + 1, count });
            });
            let _ = app.emit("ours", OursStep { install: &i.id, id: &addon.id, change: r.as_ref().ok(), failure: r.as_ref().err() });
            match r {
                Ok(c) => out.changes.push(c),
                Err(e) => {
                    if !e.conflict.is_empty() {
                        manager::Conflicts::remember(&i.id, &addon.id, &addon.version, &e.conflict);
                    }
                    report::note_error("install", &format!("{} into {}: {}", addon.name, i.name, e.message));
                    out.failed.push(manager::Failed { id: addon.id.clone(), name: addon.name.clone(), message: e.message, conflict: e.conflict });
                }
            }
        }
        Ok(out)
    })
    .await
}

// ---------------------------------------------------------------------------------------------------------------
// "Send error logs to the developer" (report.rs)

/// The report's inputs as the window sees them now.
fn with_inputs<T>(ui: &Ui, what: &str, shown: &str, f: impl FnOnce(&report::Inputs) -> T) -> T {
    let cfg = ui.shared.cfg.lock().unwrap().clone();
    let list = installs(ui);
    let cat = ui.catalog();
    let feed = ui.shared_view.lock().unwrap().clone();
    f(&report::Inputs { cfg: &cfg, installs: &list, catalog: cat.as_deref(), price_feed: Some(&feed), what, shown, test: false })
}

/// One click: build, scrub and send the report. `shown`: the error the button sat next to; `what`: the player's
/// optional line.
#[tauri::command]
async fn send_report(app: AppHandle, what: String, shown: String) -> Result<ReportOutcome, String> {
    blocking(app, move |app| {
        let ui = app.state::<Ui>();
        if !shown.trim().is_empty() {
            report::note_error("window", &shown);
        }
        Ok(with_inputs(&ui, &what, &shown, |inp| make_report(&ui.shared.server, inp, true)))
    })
    .await
}

/// "What gets sent": open the exact text in the player's text editor. `after`: the one last sent (or kept), when
/// there is one; otherwise what a press would send now.
#[tauri::command]
async fn report_text(app: AppHandle, what: String, shown: String, after: bool) -> Result<(), String> {
    blocking(app, move |app| {
        let last = report::reports_dir().join("last-report.txt");
        let path = if after && last.is_file() {
            last
        } else {
            let ui = app.state::<Ui>();
            let log = report::log_tail(&config::dir(), report::MAX_LOG_LINES, report::MAX_LOG_BYTES);
            let r = with_inputs(&ui, &what, &shown, |inp| report::build(inp, &log));
            report::write_text("preview.txt", &report::readable(&r)).map_err(|e| format!("Couldn't write the preview: {e}"))?
        };
        app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
    })
    .await
}

/// "Show file": the kept zip in its folder.
#[tauri::command(async)]
fn show_report_file(app: AppHandle, path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.starts_with(report::reports_dir()) || !p.is_file() {
        return Err("That file isn't there any more.".into());
    }
    app.opener().reveal_item_in_dir(&p).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn copy_report_id(id: String) -> Result<String, String> {
    let id = report::report_id(&id).ok_or("That isn't a report id.")?;
    set_clipboard(&id)?;
    Ok(id)
}

/// An error the window showed (install, update, a link that didn't open ...), for the next report.
#[tauri::command(async)]
fn note_error(source: String, message: String) {
    report::note_error(&source, &message);
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
    rescan(&app);
    Ok(Some(root.display().to_string()))
}

#[tauri::command(async)]
fn forget_folder(app: AppHandle, ui: State<Ui>, root: String) {
    let mut c = ui.shared.cfg.lock().unwrap();
    c.extra_roots.retain(|r| r.to_string_lossy() != root);
    c.save();
    drop(c);
    rescan(&app);
}

#[tauri::command(async)]
fn set_setting(app: AppHandle, ui: State<Ui>, name: String, value: serde_json::Value) -> Result<(), String> {
    match name.as_str() {
        "auto_update" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.set_auto_update(value.as_bool().unwrap_or(true));
            c.save();
        }
        "app_auto_update" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.set_app_auto_update(value.as_bool().unwrap_or(true));
            c.save();
        }
        "shared_prices" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.shared_prices = value.as_bool().unwrap_or(true);
            c.save();
            drop(c);
            shared::poke();
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
        "ours_card_dismissed" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.ours_card_dismissed = value.as_bool().unwrap_or(true);
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

/// Open the AddOns folder, or `folder` in it (an addon's own folder, when it is a real folder there).
#[tauri::command(async)]
fn open_addons_folder(app: AppHandle, ui: State<Ui>, install: String, folder: Option<String>) -> Result<(), String> {
    let i = find_install(&ui, &install)?;
    let own = folder.and_then(|f| manager::removable(&i.addons_dir, &f).ok());
    let dir = match own {
        Some(d) => d,
        None if i.addons_dir.is_dir() => i.addons_dir,
        None => i.dir,
    };
    app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Background

/// Refresh the catalog soon after start and then every CHECK_EVERY (10 minutes); when "Update my addons by itself"
/// is on, update what we installed, without asking. Updates for a game that is running wait until it is closed
/// (looked at every 30 seconds), then go in. Nothing here ever shows or focuses the window: what happened goes in
/// the log, the footer line and the "updated by itself" list for when the player opens it.
fn spawn_addon_checks(app: AppHandle) {
    std::thread::spawn(move || {
        // the wall clock, so a PC that slept through a check checks when it wakes
        let started = chrono::Local::now();
        let mut last: Option<chrono::DateTime<chrono::Local>> = None;
        let mut first = true;
        // installs whose updates wait for the game to close, and when to look again
        let mut waiting: Vec<String> = Vec::new();
        let mut look_again = started;
        loop {
            let every = config::check_every();
            let now = chrono::Local::now();
            let due = config::next_check(started, last, now, Duration::from_secs(5), every);
            if now < due {
                if !waiting.is_empty() && now >= look_again {
                    look_again = now + chrono::Duration::from_std(WAIT_FOR_GAME).unwrap_or_default();
                    let ui = app.state::<Ui>();
                    let auto = ui.shared.cfg.lock().unwrap().auto_update;
                    if let (true, Some(cat)) = (auto, ui.catalog()) {
                        let targets: Vec<wow::Install> = installs(&ui).into_iter().filter(|i| waiting.contains(&i.id)).collect();
                        let (out, still) = auto_update(&app, &targets, &cat);
                        waiting = still;
                        if !out.changes.is_empty() || !out.failed.is_empty() {
                            note_auto(&app, false, &out, &waiting, &cat);
                        }
                    } else {
                        waiting.clear();
                    }
                }
                let left = (due - now).to_std().unwrap_or_default();
                std::thread::sleep(left.min(Duration::from_secs(5)));
                continue;
            }
            last = Some(now);
            let ui = app.state::<Ui>();
            let next = chrono::Local::now() + chrono::Duration::from_std(every).unwrap_or_default();
            ui.catalog.lock().unwrap().next = Some(next.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
            // the window may have just loaded it
            refresh_catalog(&ui, first.then(|| Duration::from_secs(60)));
            let _ = app.emit("catalog", ());
            first = false;
            let Some(cat) = ui.catalog() else { continue };
            if !ui.shared.cfg.lock().unwrap().auto_update {
                waiting.clear();
                note_auto(&app, true, &Updated { changes: Vec::new(), failed: Vec::new() }, &waiting, &cat);
                continue;
            }
            let (out, still) = auto_update(&app, &installs(&ui), &cat);
            waiting = still;
            look_again = chrono::Local::now() + chrono::Duration::from_std(WAIT_FOR_GAME).unwrap_or_default();
            note_auto(&app, false, &out, &waiting, &cat);
        }
    });
}

/// How often an update waiting for the game to close looks whether it has.
const WAIT_FOR_GAME: Duration = Duration::from_secs(30);

/// The automatic update of `targets`, except installs whose game is running (replacing an addon's files under a
/// running game can mix two versions): those are returned, to be tried again once the game is closed.
fn auto_update(app: &AppHandle, targets: &[wow::Install], cat: &Catalog) -> (Updated, Vec<String>) {
    let (running, free): (Vec<wow::Install>, Vec<wow::Install>) = targets.iter().cloned().partition(|i| wow::game_running(i));
    let waiting: Vec<String> = running.iter().filter(|i| !manager::pending(i, cat).is_empty()).map(|i| i.id.clone()).collect();
    for i in &running {
        if waiting.contains(&i.id) {
            config::log(&format!("auto-update: {} is running; its updates wait until it is closed", i.name));
        }
    }
    (update_installs(app, &free, cat, true), waiting)
}

/// Remember what the automatic check did (the window's footer), and tell the window what changed.
fn note_auto(app: &AppHandle, off: bool, out: &Updated, waiting: &[String], cat: &Catalog) {
    let ui = app.state::<Ui>();
    let waiting_n: usize = installs(&ui).iter().filter(|i| waiting.contains(&i.id)).map(|i| manager::pending(i, cat).len()).sum();
    ui.catalog.lock().unwrap().auto = Some(AutoRun { at: now_rfc3339(), off, updated: out.changes.len(), failed: out.failed.len(), waiting: waiting_n });
    if off {
        config::log("auto-update: off");
    } else {
        config::log(&format!("auto-update: checked, {} updated, {} failed, {} waiting for the game to close", out.changes.len(), out.failed.len(), waiting_n));
    }
    for f in &out.failed {
        config::log(&format!("auto-update failed: {}: {}", f.id, f.message));
    }
    // the one policy (health.rs): a download that didn't work is tried again at the next check and only shown when it
    // keeps failing; a problem on this PC is shown at once; a folder in the way is the row's own Replace button
    let troubles: Vec<(String, health::Trouble)> = out.failed.iter().filter_map(|f| manager::failure_trouble(f).map(|t| (f.id.clone(), t))).collect();
    health::keep(health::Job::AddonUpdates, &troubles.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>());
    for (id, t) in troubles {
        health::fail(health::Job::AddonUpdates, &id, t);
    }
    if !out.changes.is_empty() {
        let mut c = ui.shared.cfg.lock().unwrap();
        for ch in &out.changes {
            config::log(&format!("auto-update: updated {} {} to {} ({})", ch.name, ch.from.as_deref().unwrap_or("?"), ch.to, ch.install));
            c.note_recent(config::RecentUpdate { at: now_rfc3339(), install: ch.install.clone(), name: ch.name.clone(), from: ch.from.clone(), to: ch.to.clone() });
        }
        c.save();
    }
    let _ = app.emit("catalog", ());
    if !out.changes.is_empty() {
        // only an open window hears these (a quiet notice in the page); a closed window stays closed. Failures are
        // not in it: health.rs shows them when (and if) they should be
        let _ = app.emit("settings", ());
        let _ = app.emit("updated", Updated { changes: out.changes.clone(), failed: Vec::new() });
    }
}

/// The app's own update, for the window (get_state): None when there is none.
static APP_UPDATE: Mutex<Option<String>> = Mutex::new(None);

fn set_app_update(app: &AppHandle, note: Option<String>) {
    let mut n = APP_UPDATE.lock().unwrap();
    if *n != note {
        *n = note;
        drop(n);
        let _ = app.emit("settings", ());
    }
}

/// The window as the app's own update sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WindowNow {
    /// not there (closed to the tray, or never opened)
    Closed,
    Hidden,
    Minimized,
    Shown,
}

fn window_now(app: &AppHandle) -> WindowNow {
    match app.get_webview_window("main") {
        None => WindowNow::Closed,
        Some(w) if !w.is_visible().unwrap_or(false) => WindowNow::Hidden,
        Some(w) if w.is_minimized().unwrap_or(false) => WindowNow::Minimized,
        Some(_) => WindowNow::Shown,
    }
}

/// Whether the app may install its update now. Never while the player has the window (shown, or minimized on the
/// taskbar): the restart would take it away from under them, and the new version never opens a window by itself.
/// Never while an addon change or an upload runs (`busy`). So it goes in once the window is closed, quietly.
fn may_install_app_update(window: WindowNow, busy: bool) -> bool {
    matches!(window, WindowNow::Closed | WindowNow::Hidden) && !busy
}

/// Check for a new version of the app 30 seconds after start and then every CHECK_EVERY (10 minutes). When "Update
/// the app by itself" is on: download it (the updater checks its signature against the public key in
/// tauri.conf.json), wait until the window is closed and nothing is running (may_install_app_update), then install
/// it silently; the installer starts the new version, which stays in the tray (config::Restart, show_at_start).
/// Nothing here asks the player anything or shows the window. A development build or a test instance only checks.
fn spawn_updater(app: AppHandle) {
    let test_instance = std::env::var_os("EVERLOOKDB_UPLOADER_DIR").is_some_and(|v| !v.is_empty());
    let may_install = !cfg!(debug_assertions) && !test_instance;
    std::thread::spawn(move || {
        let started = chrono::Local::now();
        let mut last: Option<chrono::DateTime<chrono::Local>> = None;
        // downloaded and checked, waiting for its moment
        let mut ready: Option<(tauri_plugin_updater::Update, Vec<u8>)> = None;
        // ticks in a row that may_install_app_update said yes (two, 5 s apart, before it goes in)
        let mut quiet = 0;
        let mut waiting_logged = false;
        loop {
            let on = app.state::<Ui>().shared.cfg.lock().unwrap().app_auto_update;
            let now = chrono::Local::now();
            if now >= config::next_check(started, last, now, Duration::from_secs(30), config::check_every()) {
                last = Some(now);
                let have = ready.as_ref().map(|(u, _)| u.version.clone());
                match check_app_update(&app, on && may_install, have.as_deref()) {
                    Ok(Some(found)) => {
                        health::ok(health::Job::AppUpdate, "");
                        ready = Some(found);
                        waiting_logged = false;
                    }
                    Ok(None) => health::ok(health::Job::AppUpdate, ""),
                    Err(e) => {
                        // retried at the next check (10 minutes); shown only when it keeps failing (health.rs)
                        config::log(&format!("app update: check failed: {e}"));
                        health::fail(health::Job::AppUpdate, "", health::Trouble::Unreachable(e.to_string()));
                    }
                }
            }
            if !on && ready.take().is_some() {
                config::log("app update: \"Update the app by itself\" was turned off; the downloaded update is dropped");
                set_app_update(&app, None);
            }
            if let Some((update, _)) = &ready {
                let window = window_now(&app);
                let busy = !manager::idle() || worker::uploading();
                quiet = if may_install_app_update(window, busy) { quiet + 1 } else { 0 };
                if quiet == 0 && !waiting_logged && matches!(window, WindowNow::Shown | WindowNow::Minimized) {
                    config::log(&format!("app update: {} is ready; it installs once the window is closed", update.version));
                    set_app_update(&app, Some(format!("Version {} installs by itself once this window is closed", update.version)));
                    waiting_logged = true;
                }
                if quiet >= 2 {
                    quiet = 0;
                    if let Some((update, bytes)) = ready.take() {
                        ready = install_app_update(&app, update, bytes);
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

/// One check. `download`: get (and verify) a new version, unless it is the one already `have`. Ok(Some) is a
/// downloaded update; every outcome has a log line.
fn check_app_update(
    app: &AppHandle,
    download: bool,
    have: Option<&str>,
) -> Result<Option<(tauri_plugin_updater::Update, Vec<u8>)>, tauri_plugin_updater::Error> {
    tauri::async_runtime::block_on(async {
        let Some(update) = app.updater()?.check().await? else {
            config::log(&format!("app update: checked, {} is the newest", env!("CARGO_PKG_VERSION")));
            set_app_update(app, None);
            return Ok(None);
        };
        if have == Some(update.version.as_str()) {
            config::log(&format!("app update: checked, {} already downloaded", update.version));
            return Ok(None);
        }
        if !download {
            let on = app.state::<Ui>().shared.cfg.lock().unwrap().app_auto_update;
            let why = if on { "a development build or test instance never installs" } else { "updating the app by itself is off" };
            config::log(&format!("app update: checked, {} is out ({why})", update.version));
            set_app_update(app, Some(format!("Version {} is out", update.version)));
            return Ok(None);
        }
        config::log(&format!("app update: downloading {}", update.version));
        let bytes = update.download(|_, _| {}, || {}).await?;
        config::log(&format!("app update: {} downloaded and its signature checked", update.version));
        Ok(Some((update, bytes)))
    })
}

/// Hold every addon change off and install. On Windows a working install ends this process (the installer starts
/// the new version); an update that couldn't start is handed back, to try again at the next quiet moment.
fn install_app_update(app: &AppHandle, update: tauri_plugin_updater::Update, bytes: Vec<u8>) -> Option<(tauri_plugin_updater::Update, Vec<u8>)> {
    let Some(hold) = manager::hold() else { return Some((update, bytes)) };
    if worker::uploading() || !may_install_app_update(window_now(app), false) {
        return Some((update, bytes));
    }
    save_window(app);
    config::Restart::write(&update.version);
    config::log(&format!("app update: installing {} quietly", update.version));
    match update.install(&bytes) {
        Ok(()) => {
            drop(hold);
            // macOS and Linux: the new version is in place; start it (it stays in the tray)
            app.restart();
        }
        Err(e) => {
            let _ = std::fs::remove_file(config::dir().join("restart.json"));
            config::log(&format!("app update: install failed: {e}"));
            set_app_update(app, Some(format!("Version {} couldn't be installed; it is tried again later", update.version)));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_opens_at_start_only_when_the_player_started_the_app() {
        // started by hand (Start menu, desktop shortcut)
        assert!(show_at_start(false, false, false, false));
        // at login
        assert!(!show_at_start(true, false, false, false));
        // by the installer after the app updated itself: never, whatever the window was before
        assert!(!show_at_start(false, true, false, false));
        assert!(!show_at_start(true, true, false, false));
        // a test instance or a development build
        assert!(!show_at_start(false, false, true, false));
        assert!(!show_at_start(false, false, false, true));
    }

    #[test]
    fn the_app_update_waits_while_the_window_is_open() {
        assert!(may_install_app_update(WindowNow::Closed, false));
        assert!(may_install_app_update(WindowNow::Hidden, false));
        assert!(!may_install_app_update(WindowNow::Shown, false));
        assert!(!may_install_app_update(WindowNow::Minimized, false));
        // an addon change or an upload is running
        assert!(!may_install_app_update(WindowNow::Closed, true));
    }

    /// The top-level function each line of main.rs is in (before the tests).
    fn functions_of_lines(src: &str) -> Vec<(String, String)> {
        let body = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        let mut current = String::new();
        let mut out = Vec::new();
        for line in body.lines() {
            let t = line.strip_prefix("async ").unwrap_or(line);
            if let Some(rest) = t.strip_prefix("fn ") {
                current = rest.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("").to_string();
            }
            out.push((current.clone(), line.to_string()));
        }
        out
    }

    /// Nothing that runs by itself (update checks, installs, errors, the tray's status) shows, focuses or creates
    /// the window or opens a dialog: only the player's own clicks and starts do.
    #[test]
    fn no_background_path_shows_the_window() {
        let src = &include_str!("main.rs").replace("\r\n", "\n");
        // what may show a window or a dialog, and the only places allowed to
        let tokens = ["show_window(", ".show()", ".show(|", "set_focus", "unminimize", "WebviewWindowBuilder::new", ".dialog()", "message(&", "request_user_attention", "always_on_top"];
        let allowed = [
            // the single-instance plugin: the player started the app again
            "run",
            // at start, only when show_at_start says so
            "setup",
            "show_window",
            // the tray menu's "Open EverlookDB" and "Change WoW folder…"
            "on_menu",
            "pick_folder",
            "message",
            // the window's own buttons
            "save_addon_list",
            "add_folder",
        ];
        for (f, line) in functions_of_lines(src) {
            if line.trim_start().starts_with("//") || line.trim_start().starts_with("use ") {
                continue;
            }
            for t in tokens {
                if line.contains(t) {
                    assert!(allowed.contains(&f.as_str()), "{f} may show a window: {line}");
                }
            }
        }
        // setup shows it only behind `if show`, and the tray only on a click
        assert!(src.contains("    if show {\n        show_window(app);\n    }"));
        assert!(src.contains("MouseButtonState::Up, .. } = e {\n                show_window(tray.app_handle());"));
        // pick_folder and message are reached only from the tray menu
        for (f, line) in functions_of_lines(src) {
            if line.contains(" pick_folder(app)") {
                assert_eq!(f, "on_menu", "{line}");
            }
            if line.contains("message(&") && !line.trim_start().starts_with("fn ") {
                assert_eq!(f, "pick_folder", "{line}");
            }
        }
        // the other modules never touch a window
        for (name, other) in [
            ("addons", include_str!("addons.rs")),
            ("catalog", include_str!("catalog.rs")),
            ("config", include_str!("config.rs")),
            ("github", include_str!("github.rs")),
            ("manager", include_str!("manager.rs")),
            ("ours", include_str!("ours.rs")),
            ("report", include_str!("report.rs")),
            ("shared", include_str!("shared.rs")),
            ("worker", include_str!("worker.rs")),
            ("wow", include_str!("wow.rs")),
            ("giveaway", include_str!("giveaway.rs")),
            ("icons", include_str!("icons.rs")),
        ] {
            for t in ["get_webview_window", "WebviewWindowBuilder", "set_focus", "unminimize", ".dialog()", "show_window"] {
                assert!(!other.contains(t), "{name}.rs uses {t}");
            }
        }
        // the window config: no window made by tauri.conf.json itself (it would open at every start)
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["app"]["windows"], serde_json::json!([]));
        // and the update installs with no installer window
        assert_eq!(conf["plugins"]["updater"]["windows"]["installMode"], "quiet");
    }
}
