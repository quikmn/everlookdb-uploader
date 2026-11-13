// EverlookDB Uploader: a tray app that uploads the EverlookDB addon's data file whenever the game
// saves it, so players never have to upload it by hand on everlookdb.com/contribute.html.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod worker;
mod wow;

use config::Config;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use worker::{Cmd, Shared, Status, Watcher};

const DEFAULT_SERVER: &str = "https://everlookdb.com";
const PRIVACY: &str = "Sends only EverlookDB.lua game facts";
const AUTOSTART: &str = if cfg!(target_os = "macos") { "Open at login" } else { "Start with Windows" };
const HELP: &str = "EverlookDB Uploader: uploads the EverlookDB addon's data file to everlookdb.com whenever the game saves it.

Usage: everlookdb-uploader [options]
  --server <url>   server to upload to (default https://everlookdb.com; or EVERLOOKDB_SERVER)
  --wow <folder>   use (and remember) this WoW Forever folder
  --check          print the folder and data files found, then exit
  --once           upload every changed data file once, print the results, then exit
  --background     start quietly (used at login)
Settings live in %APPDATA%\\com.everlookdb.uploader (macOS: ~/Library/Application Support/com.everlookdb.uploader),
or EVERLOOKDB_UPLOADER_DIR.";

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
    if args.check || args.once {
        attach_console();
        std::process::exit(command_line(&args));
    }
    run_tray(args);
}

/// --check and --once.
fn command_line(args: &Args) -> i32 {
    let mut cfg = Config::load();
    let dir = args.wow.as_deref().and_then(wow::resolve).or(cfg.wow_dir.clone().filter(|d| d.is_dir())).or_else(wow::find_install);
    println!("Server:      {}", worker::upload_url(&args.server));
    println!("Settings:    {}", config::dir().display());
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
        println!("No WTF/Account/*/SavedVariables/EverlookDB.lua yet.");
    }
    if !args.once {
        return 0;
    }
    cfg.wow_dir = Some(dir);
    cfg.paused = false;
    let shared = Arc::new(Shared { cfg: Mutex::new(cfg), server: args.server.clone() });
    let status = Watcher::new(shared).pass(false, true);
    println!("Result:      {}", status.line);
    if status.kind == worker::Kind::Ok || status.kind == worker::Kind::Idle { 0 } else { 2 }
}

/// Handles the tray menu needs after start-up.
struct Ui {
    status: MenuItem<Wry>,
    folder: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    pause: CheckMenuItem<Wry>,
    tx: Mutex<Sender<Cmd>>,
    shared: Arc<Shared>,
}

impl Ui {
    fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }
}

fn run_tray(args: Args) {
    let mut cfg = Config::load();
    let first_run = !Config::exists();
    if let Some(over) = args.wow.as_deref().and_then(wow::resolve) {
        cfg.wow_dir = Some(over);
    }
    if cfg.wow_dir.as_ref().is_none_or(|d| !d.is_dir()) {
        cfg.wow_dir = wow::find_install();
    }
    cfg.save();
    let shared = Arc::new(Shared { cfg: Mutex::new(cfg), server: args.server.clone() });
    config::log(&format!("start {} (server {})", env!("CARGO_PKG_VERSION"), args.server));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            message(app, "EverlookDB Uploader is already running. Find it in the tray, by the clock.", MessageDialogKind::Info);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--background"])))
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            setup(app.handle(), shared, first_run && !args.background)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("start EverlookDB Uploader");
    app.run(|app, event| match event {
        // no windows: stay in the tray until Quit
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        RunEvent::Exit => {
            if let Some(ui) = app.try_state::<Ui>() {
                ui.send(Cmd::Quit);
            }
        }
        _ => {}
    });
}

fn setup(app: &AppHandle, shared: Arc<Shared>, welcome: bool) -> tauri::Result<()> {
    let paused = shared.cfg.lock().unwrap().paused;
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "", false, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", AUTOSTART, true, app.autolaunch().is_enabled().unwrap_or(false), None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", "Pause uploads", true, paused, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &folder,
            &sep()?,
            &MenuItem::with_id(app, "upload", "Upload now", true, None::<&str>)?,
            &MenuItem::with_id(app, "open", "Open EverlookDB", true, None::<&str>)?,
            &MenuItem::with_id(app, "change", "Change WoW folder…", true, None::<&str>)?,
            &sep()?,
            &autostart,
            &pause,
            &sep()?,
            &MenuItem::with_id(app, "privacy", PRIVACY, false, None::<&str>)?,
            &MenuItem::with_id(app, "quit", "Quit EverlookDB Uploader", true, None::<&str>)?,
        ],
    )?;
    let tray = TrayIconBuilder::with_id("main").menu(&menu).show_menu_on_left_click(true).tooltip("EverlookDB Uploader");
    #[cfg(target_os = "macos")]
    let tray = tray.icon(tauri::include_image!("icons/tray-template.png")).icon_as_template(true);
    #[cfg(not(target_os = "macos"))]
    let tray = tray.icon(app.default_window_icon().cloned().expect("app icon"));
    tray.on_menu_event(|app, e| on_menu(app, e.id.as_ref())).build(app)?;

    let (tx, rx) = channel();
    app.manage(Ui { status, folder, autostart, pause, tx: Mutex::new(tx), shared: shared.clone() });

    let handle = app.clone();
    let mut last: Option<(String, usize, Option<PathBuf>)> = None;
    std::thread::spawn(move || {
        Watcher::new(shared.clone()).run(rx, |s: &Status| {
            let dir = shared.cfg.lock().unwrap().wow_dir.clone();
            let now = (s.line.clone(), s.files, dir.clone());
            if last.as_ref() != Some(&now) {
                show_status(&handle, s, dir);
                last = Some(now);
            }
        })
    });

    if shared_dir(app).is_none() {
        message(app, "EverlookDB Uploader couldn't find WoW Forever. Choose your World of Warcraft folder (the one with _forever_ in it).", MessageDialogKind::Warning);
        pick_folder(app);
    } else if welcome {
        message(
            app,
            &format!(
                "EverlookDB Uploader is running in the tray.\n\nIt uploads EverlookDB.lua from\n{}\nwhenever the game saves it (log out or /reload).\n\n{PRIVACY}: no account or character names, no file paths.",
                shared_dir(app).unwrap().display()
            ),
            MessageDialogKind::Info,
        );
    }
    spawn_updater(app.clone());
    Ok(())
}

fn shared_dir(app: &AppHandle) -> Option<PathBuf> {
    app.state::<Ui>().shared.cfg.lock().unwrap().wow_dir.clone()
}

fn show_status(app: &AppHandle, s: &Status, dir: Option<PathBuf>) {
    let ui = app.state::<Ui>();
    let _ = ui.status.set_text(&s.line);
    let folder = match (dir, s.files) {
        (None, _) => "No WoW folder chosen".to_string(),
        (Some(d), 1) => format!("Watching 1 account in {}", d.display()),
        (Some(d), n) => format!("Watching {n} accounts in {}", d.display()),
    };
    let _ = ui.folder.set_text(folder);
    if let Some(tray) = app.tray_by_id("main") {
        let tip: String = format!("EverlookDB Uploader\n{}", s.line).chars().take(120).collect();
        let _ = tray.set_tooltip(Some(tip));
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let ui = app.state::<Ui>();
    match id {
        "upload" => ui.send(Cmd::UploadNow),
        "open" => {
            let _ = app.opener().open_url(format!("{}/contribute.html", ui.shared.server), None::<&str>);
        }
        "change" => pick_folder(app),
        "autostart" => {
            let al = app.autolaunch();
            let _ = if ui.autostart.is_checked().unwrap_or(false) { al.enable() } else { al.disable() };
            let _ = ui.autostart.set_checked(al.is_enabled().unwrap_or(false));
        }
        "pause" => {
            let mut c = ui.shared.cfg.lock().unwrap();
            c.paused = ui.pause.is_checked().unwrap_or(false);
            c.save();
            drop(c);
            ui.send(Cmd::Wake);
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

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
        c.save();
        drop(c);
        ui.send(Cmd::Wake);
    });
}

fn message(app: &AppHandle, text: &str, kind: MessageDialogKind) {
    app.dialog().message(text).title("EverlookDB Uploader").kind(kind).show(|_| {});
}

/// Check for a new version shortly after start and then every 6 hours; install it quietly and restart.
fn spawn_updater(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(60);
        loop {
            std::thread::sleep(wait);
            wait = Duration::from_secs(6 * 3600);
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
