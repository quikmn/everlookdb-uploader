# EverlookDB Uploader

A small tray app for Windows and macOS that uploads the [EverlookDB addon](https://github.com/quikmn/EverlookDB)'s
data to [everlookdb.com](https://everlookdb.com/contribute.html) by itself, so you never have to upload the file by hand.

WoW addons can't use the network. The EverlookDB addon writes what it recorded to
`World of Warcraft/_forever_/WTF/Account/<account>/SavedVariables/EverlookDB.lua` when you log out or `/reload`.
The uploader notices that, waits until the game has finished writing, and sends the file to
`https://everlookdb.com/api/v1/upload`, the same as the upload box on the site.

## Install

Download the latest release from the [releases page](https://github.com/quikmn/everlookdb-uploader/releases):

- **Windows:** `EverlookDB-Uploader_<version>_x64-setup.exe`. It installs for your user only (no administrator prompt).
- **macOS:** `EverlookDB-Uploader_<version>_universal.dmg` (Apple silicon and Intel). Drag it to Applications.

Start it once. It finds WoW Forever by itself (the Battle.net install path, then common folders); if it can't, it asks
you to choose your World of Warcraft folder. After that it lives in the tray (Windows) or the menu bar (macOS), and it
updates itself.

The menu shows the last upload ("Uploaded: 9 NPCs, 22 loot sources") and has **Upload now**, **Open EverlookDB**,
**Change WoW folder…**, **Start with Windows** / **Open at login**, **Pause uploads** and **Quit**.

## Privacy

**It sends only EverlookDB.lua game facts.** The request body is that one file, as it is on disk, and nothing else:
no account or character names, no file paths (the path holds your account folder name, so it is never sent), no
other files, no machine information. The server keeps only the checked game facts, not the file or your IP address;
see [everlookdb.com/privacy.html](https://everlookdb.com/privacy.html).

It sends a file only when its content changed since the last upload. Several accounts are fine: every
`WTF/Account/*/SavedVariables/EverlookDB.lua` is watched.

Its settings and a small log stay on your computer, in `%APPDATA%\com.everlookdb.uploader` (Windows) or
`~/Library/Application Support/com.everlookdb.uploader` (macOS). Updates are checked at
`https://everlookdb.com/uploader/latest.json` and downloaded from this repository's releases; they are signed, and
the app refuses an update whose signature doesn't match.

## Command line

```
everlookdb-uploader --check                 print the WoW folder and data files it finds
everlookdb-uploader --once                  upload every changed data file once and print the result
everlookdb-uploader --server <url>          upload somewhere else (or EVERLOOKDB_SERVER), e.g. a local test server
everlookdb-uploader --wow <folder>          use (and remember) this WoW folder
```

`EVERLOOKDB_UPLOADER_DIR` moves the settings folder (handy for testing).

## Building

Needs Rust and the [Tauri CLI](https://v2.tauri.app/) (`cargo install tauri-cli --version "^2" --locked`, or use
`npx @tauri-apps/cli@2`).

```
cargo tauri build --bundles nsis -c "{\"bundle\":{\"createUpdaterArtifacts\":false}}"
```

builds an unsigned installer into `src-tauri/target/release/bundle/nsis/` (with `EVERLOOKDB_SKIP_SIGNING=1` it won't
try to code-sign even when the certificate is on the machine). `cargo build` in `src-tauri` is enough for the app alone.

Layout: `src-tauri/src/wow.rs` finds the game folder and data files, `worker.rs` watches and uploads, `config.rs`
keeps settings, `main.rs` is the tray. `ui/` is an empty page (the app has no window). Icons are generated from
`assets/app-icon.svg` with `cargo tauri icon assets/app-icon.svg -o src-tauri/icons`; the macOS menu bar icon is
`assets/tray-template.svg` rendered to `src-tauri/icons/tray-template.png`.

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

GitHub secrets for the macOS job: `MAC_CERT_P12_BASE64`, `MAC_CERT_PASSWORD`, `APPSTORE_API_KEY_BASE64`,
`APPSTORE_KEY_ID`, `APPSTORE_ISSUER_ID`, `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the
variable `MAC_IDENTITY`. Without the Apple ones it builds an ad-hoc signed app that Gatekeeper blocks.

## License

GPL-3.0-or-later (see LICENSE). Not affiliated with Blizzard Entertainment.
