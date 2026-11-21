# EverlookDB

A small, ad-free addon manager for World of Warcraft (Windows and macOS), and the uploader for the
[EverlookDB addons](https://github.com/quikmn/EverlookDB)' data to [everlookdb.com](https://everlookdb.com/contribute.html).

- **Finds every game.** Every `World of Warcraft` folder and every client in it (WoW Forever, Retail, Classic,
  Classic Era, Anniversary, PTR, Beta), each with its game version read from `.build.info`, big and clear. Add a
  folder by hand if it's somewhere unusual.
- **Shows your addons.** Everything in `Interface/AddOns`, grouped (BigWigs and its 11 folders are one addon), with
  its version and whether an update is waiting.
- **Finds new ones.** Instant search over every addon in the EverlookDB catalog (GitHub releases and Tukui:
  BigWigs, DBM, WeakAuras, ElvUI, Questie, Bartender4 …), filtered to the game you have selected. Forgiving:
  "dbm", "big wigs", "questy" all find what you meant. An addon that isn't in the list installs from its GitHub
  link (Install from GitHub), shown before it installs and then updated like the others.
- **Our addons in one click.** A new install shows the EverlookDB addons, all ticked, with one Install all button
  ("Not now" hides it for good); Get addons has an EverlookDB addons filter with Install all missing.
- **Keeps them updated.** One click to install, update, go back to the previous version or remove. Every hour (or
  as often as you set, up to once a week) it updates what it installed by itself (one switch turns that off), and tells you what changed (v427.8 → v427.9).
- **Uploads EverlookDB data**, exactly as the EverlookDB Uploader did: see below.

No ads, no account, no tracking. It lives in the tray (Windows) or the menu bar (macOS); click the icon to open the
window.

## Install

Download the latest release from the [releases page](https://github.com/quikmn/everlookdb-uploader/releases):

- **Windows:** the `_x64-setup.exe`. It installs for your user only (no administrator prompt).
- **macOS:** the `_universal.dmg` (Apple silicon and Intel). Drag it to Applications.

If you have the EverlookDB Uploader, it updates itself into this app; your settings stay. On Windows the new installer
removes the old "EverlookDB Uploader" install (its folder, Start menu and desktop shortcuts and its entry in Installed
apps) and puts the same shortcuts and the Start with Windows setting back for EverlookDB. On macOS the update replaces
the app where it is, so it keeps its old name in Applications (`EverlookDB Uploader.app`) until you install it fresh.

## What it touches

- **Its own data addon, EverlookDBAuctions_Shared** ("EverlookDB Auctions: Price Feed" in the addon list, below): the only folder it writes in
  `Interface/AddOns` by itself. Only when EverlookDB Auctions is installed, only that folder, each file written whole
  (a temp file renamed over the old one) and only when it changed. It isn't listed under My addons. The folder is
  also a CurseForge addon that ships empty: the app writes its .toc, Shared.lua and a file per auction house over it,
  never deletes a file it didn't write (README, LICENSE, Media/ stay), and writes its data again within a minute when
  an addon update brings back the empty copy. The Price Feed switch off puts the empty copy back.

- **Addons it installed**, and only those. Each install downloads the release zip straight from GitHub or Tukui,
  checks its size against the catalog, unpacks it into a staging folder next to `AddOns` (refusing any zip whose
  paths would land outside it), then swaps the folders in. The previous version's zip is kept for one-click
  rollback. Which addons it installed is recorded in its own settings folder, never in the game folder.
- **Addons you or another app installed** (WowUp, CurseForge) are listed and left alone. If one is in the catalog
  you can ask EverlookDB to keep it updated; its old folders are copied to `backups` in the settings folder first.
  Addons it can't match show as "Not from our sources" and are never changed.

## Privacy

**The addon catalog request sends nothing about you.** The app asks
`https://everlookdb.com/api/v1/addons/catalog` for the list with only its name and version as the user agent and
the ETag of the copy it already has: no ids, no cookies, no list of your addons or games. An addon added by its GitHub
link is looked up the same way (`/api/v1/addons/github/{owner}/{repo}`), so the app never calls GitHub's API itself. Addon downloads go straight
to GitHub or Tukui, never through everlookdb.com.

**Uploads send only the game facts in EverlookDB.lua and EverlookDBAuctions.lua.** WoW addons can't use the network.
The EverlookDB addon writes what it recorded to
`World of Warcraft/_forever_/WTF/Account/<account>/SavedVariables/EverlookDB.lua` (and `EverlookDBAuctions.lua`,
the EverlookDB Auctions addon's auction scans) when you log out or `/reload`. The app notices that, waits until the
game has finished writing, and sends the file to `https://everlookdb.com/api/v1/upload`, the same as the upload box
on the site. Each request body is one of those files, as it is on disk (auction files gzip-compressed), and nothing else: no account or character
names, no file paths (the path holds your account folder name, so it is never sent), no other files, no machine
information. The server keeps only the checked game facts, not the file or your IP address; see
[everlookdb.com/privacy.html](https://everlookdb.com/privacy.html). A file is sent only when its content changed since
the last upload or when the last upload failed: a failed or refused upload is sent again (at once when the file
changes, otherwise after 1, 5, 15 and 30 minutes, then hourly, and when the app starts), and the window shows
each file's last answer, green, amber or red. Auction files go gzip-compressed. Several accounts are fine. The switch under Uploads (or **Pause uploads** in the tray menu) stops it.

**The Price Feed fetches other players' auction scans and sends nothing about you.** Every 15 minutes, and right after an
auctions upload, the app asks `https://everlookdb.com/api/v1/auctions` for the list of auction houses and
`/api/v1/auctions/house/{house}/compact` for the ones you play on (picked on your computer from the realm folders in
`WTF/Account` and the houses in EverlookDBAuctions.lua; the request names only the house, like `EU-6946-Alliance`),
with the ETag of the copy it has, so an unchanged house is a short "not modified". Each answer is checked (plain
numbers only, a bad row is skipped) before it goes into the data addon. The data has no player names. A failed
check is tried again after 1, 5 and 15 minutes; what was written stays. The window shows each house and how old its
scan is.

**Problem reports are sent only when you click.** "Send error logs to the developer" (next to an error) or "Report a
problem" (the sidebar) sends one report to `https://everlookdb.com/api/v1/reports`: the app's version, the OS, the
games it found and their versions, the selected game, the errors it noted (with times), the end of `uploader.log`,
`config.json` without its upload hashes and receipts, the installed addons with versions, and for each file it uploads
only its size, date and first and last line (quoted text blanked). Before it leaves your PC the user name in paths
becomes `<user>`, account folders `<account 1>`, emails `<email>`, and codes, keys and tokens `<token>`. "What gets
sent" opens the exact text. The server answers with a short id (report K7Q2M9) and keeps the report, never your IP.
If it can't be sent, the report is kept in `reports/` in the settings folder.

Its settings, the addon records, the cached catalog, kept zips and a small log stay on your computer, in
`%APPDATA%\com.everlookdb.uploader` (Windows) or `~/Library/Application Support/com.everlookdb.uploader` (macOS).
App updates are checked at `https://everlookdb.com/uploader/latest.json` and downloaded from this repository's
releases; they are signed, and the app refuses an update whose signature doesn't match.

## Tray menu

**Open EverlookDB** (the window), the last upload ("Uploaded: 9 NPCs, 22 loot sources"), **Upload now**,
**Open everlookdb.com**, **Change WoW folder…** (which WoW Forever folder uploads come from),
**Start with Windows** / **Open at login**, **Pause uploads** and **Quit**.

## Command line

```
everlookdb-uploader --check                 print the games, their addons and the data files it finds
everlookdb-uploader --once                  upload every changed data file once and print the result
everlookdb-uploader --shared                fetch the Price Feed once, write the data addon, print the houses
everlookdb-uploader --server <url>          upload somewhere else (or EVERLOOKDB_SERVER), e.g. a local test server
everlookdb-uploader --wow <folder>          use (and remember) this WoW Forever folder for uploads
everlookdb-uploader --background            start in the tray without opening the window (used at login)
everlookdb-uploader --send-report [--test] [--what <text>]
                                            send a problem report as the window's button does and print its id
```

For testing: `EVERLOOKDB_UPLOADER_DIR` moves the settings folder (a copy started with it may run next to the real
one and never opens its window by itself; click its tray icon), `EVERLOOKDB_WOW_ROOTS` replaces game detection with
a list of folders, and `EVERLOOKDB_CATALOG` points the catalog at another URL or a local JSON file. `EVERLOOKDB_GIVEAWAY=1` shows the giveaway code under
Uploads even while it is switched off (`=0` hides it). Development builds
fall back to `src-tauri/fixtures/catalog-sample.json` while the server catalog isn't reachable.

## Building

Needs Rust and the [Tauri CLI](https://v2.tauri.app/) (`cargo install tauri-cli --version "^2" --locked`).

```
cd src-tauri
cargo test                       # toc and .build.info parsing, grouping, zip safety, install/update/rollback
cargo test -- --ignored          # the same flow against a real GitHub download
cargo build                      # a development build: target/debug/everlookdb-uploader
cargo tauri build --bundles nsis -c "{\"bundle\":{\"createUpdaterArtifacts\":false}}"
```

The last one builds an unsigned installer into `src-tauri/target/release/bundle/nsis/` (with
`EVERLOOKDB_SKIP_SIGNING=1` it won't try to code-sign even when the certificate is on the machine).

Problem reports: `src-tauri/src/report.rs` builds, scrubs and sends them (its tests cover the scrubbing and the size
cap); the server side is Quarry's `crates/server/src/reports.rs`. We read them with Quarry's `deploy/reports.sh`
(`list`, `show <id>`, `delete <id>`), on everlookdb.com's /admin/stats, or `GET /api/v1/admin/app-reports/<id>`.

Layout: `src-tauri/src/wow.rs` finds the games and versions, `addons.rs` reads `.toc` files and groups folders,
`catalog.rs` fetches and caches the addon catalog, `manager.rs` installs, updates, rolls back and removes,
`worker.rs` watches and uploads, `config.rs` keeps settings, `main.rs` is the tray, the window and the commands the
window calls. `src-tauri/windows/hooks.nsh` is the Windows installer's hook that moves an old "EverlookDB Uploader"
install over (its `EDB_OLD_*` names can be overridden to test it against a fake old install). `ui/` is the window: plain HTML, CSS and JavaScript, no build step. Icons are generated from
`assets/app-icon.svg` with `cargo tauri icon assets/app-icon.svg -o src-tauri/icons`; the macOS menu bar icon is
`assets/tray-template.svg` rendered to `src-tauri/icons/tray-template.png`. Fonts are the system's own (Segoe UI on Windows, San Francisco on macOS).

## Releasing

1. Bump `version` in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`, commit, tag `vX.Y.Z` and push the tag.
   GitHub Actions (`.github/workflows/release.yml`) builds, signs and notarizes the universal macOS app and uploads it to
   a draft release.
2. On the Windows PC, log in to SimplySign Desktop and run `scripts\release-windows.ps1`. It builds the NSIS installer,
   signs it (Certum code-signing certificate, `scripts/sign.ps1`), signs it for the updater, uploads it and writes
   `dist\latest.json`. It stops if the certificate isn't available.
3. When the Mac job has finished, run `scripts\release-windows.ps1 -LatestOnly`: `latest.json` then lists both
   platforms and the release is published.
4. Put `dist\latest.json` at `https://everlookdb.com/uploader/latest.json`.

The giveaway code under Uploads (`EVL-<auctions receipt>-<collector receipt>` from the server's upload receipts, with
a checklist) ships switched off. To switch it on, set `ENABLED` to `true` in `src-tauri/src/giveaway.rs` (and `ENDS`
to the first day it should no longer show), then release as above. It hides itself again on `ENDS`. Receipts are kept
either way, so players who uploaded before it is switched on already have their code.

GitHub secrets for the macOS job: `MAC_CERT_P12_BASE64`, `MAC_CERT_PASSWORD`, `APPSTORE_API_KEY_BASE64`,
`APPSTORE_KEY_ID`, `APPSTORE_ISSUER_ID`, `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the
variable `MAC_IDENTITY`. Without the Apple ones it builds an ad-hoc signed app that Gatekeeper blocks.

## License

GPL-3.0-or-later (see LICENSE). Not affiliated with Blizzard Entertainment.
