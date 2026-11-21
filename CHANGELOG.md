# Changelog

## Unreleased

- Shared auction prices: the app now also brings prices back. Every 15 minutes, and right after each auctions upload, it fetches the newest scan anyone uploaded to everlookdb.com for the auction houses you play on (your realms and factions, and the neutral one) and writes it into WoW Forever as a small data addon, EverlookDBAuctions_Shared, so EverlookDB Auctions has it at your next /reload or login, even out in the world. Unchanged houses cost almost nothing (the server answers "not modified"), errors are retried later without touching what is already written, and the window shows "Shared prices: EU-6946-Alliance, 12 min old". The Shared prices switch turns it off (and removes that folder). It only ever writes its own EverlookDBAuctions_Shared folder, which isn't listed under My addons.

## v0.6.0 (2026-11-20)

- Opens faster: your addon list shows at once from the last check, and the fresh one loads in the background.
- Installs never lose the old addon: the old copy stays until the new one is fully in place, and an install cut short (a crash, a reboot) is put right the next time the app starts.
- Automatic updates wait while World of Warcraft is running and tell you to /reload afterwards. The update timer follows the clock, so a sleeping PC doesn't push it back.
- Automatic updates never downgrade: an addon EverlookDB didn't install is only replaced when the new version is clearly higher.
- WoW folders are found once and remembered: the drive scan runs at start or when you ask, only on local hard drives, and skips drives that don't answer within 3 seconds.
- Errors are plain sentences (details go to the log). A folder in the way of an install is reported once per version, with a Replace button that keeps a backup.
- Open folder opens the addon's own folder. A What's new link sits next to updates, Update all says which game it covers, a download with no data for a minute gives up instead of hanging, and only the newest 3 backups per addon are kept.
- The app's own updates wait until no addon change or upload is running; afterwards the window reopens only if it was open, where it was, without taking the focus.

## v0.5.1 (2026-11-19)

- "Update addons by itself" now updates every addon EverlookDB knows, also ones you or another app installed (Bartender4, Questie...), not only the ones EverlookDB installed. The old copy is kept in the backups folder first. Developer checkouts (a folder with .git) and addons without a real version are left alone.
- Update all is always at the top of My addons: one click checks every addon for a new version and updates all that have one. It also says how many updates are waiting, or that everything is up to date.
- Each Update button says when that version was released ("released today 22:23"; hover for the exact time).
- The bottom line says when the addon list was checked, when the automatic check last ran and what it did ("nothing to update", "updated 2 addons"), and when it runs next. The "Updated by itself" note says when it happened.

## v0.5.0 (2026-11-18)

- Right-click an addon for its own menu: in My addons Update, Open folder, its project page, Copy name and
  Uninstall; in Get addons Install (or Update), its project page and Copy name. Uninstall now works for every addon,
  also ones you or another app installed (Titan Panel, TomTom...): it shows exactly which folders go, keeps any that
  another addon uses too, and moves them to the Recycle Bin, so you can get them back. The browser's own menu (Back,
  Refresh, Print) and its reload and print keys are gone; text fields still have copy and paste.
- Install from GitHub: on Get addons, paste the link of an addon's GitHub page (or type it in the search box) and
  EverlookDB shows what it is (name, author, version, release date, the folders it adds, and whether it is made for
  your game) before installing it. It is then updated with your other addons and marked "from GitHub" in My addons;
  Remove takes it away and stops the checks. Addons already in EverlookDB's list install from there.
- A new install now starts with Windows (opens at login on a Mac) by default, so your uploads and addon updates keep going without you opening the app. Turn it off in the window or the tray menu and it stays off. Installs from before this keep the setting they had.
- Ready for the upcoming EverlookDB giveaway: the app keeps the receipt the site gives for each accepted upload,
  and will show your giveaway code under Uploads with a Copy button and a checklist of what is still missing (a full
  auction house scan, collector data). It stays hidden until the giveaway starts.
- Uploads are no longer lost when the server refuses or can't be reached. A failed upload is sent again at once
  when the file changes, otherwise after 1, 5, 15 and 30 minutes and then every hour for as long as it keeps
  failing (a refused file every hour, a busy server after the wait it asks for), and right away when the app starts.
- The window shows each watched file's last upload under Uploads: green when it was all taken, amber when part of
  it was refused or skipped (with the server's reasons), red when it was refused or keeps failing (with the
  server's message and when it is tried next). A red one is also shown at the top of the window, with Try now.
- Auction scan files (EverlookDBAuctions.lua) are sent gzip-compressed, so 10-30 MB scans upload quickly; an older
  server that doesn't understand that gets the file uncompressed. Auction files up to 64 MB are sent (was 12 MB).
- The log (uploader.log) now lists each auction scan's answer: accepted, already had or refused, rows and why
  rows were skipped.
