# Changelog

## Unreleased

- Update all with nothing to update no longer shows a second "up to date" box: the bar says it, with when it checked. Update all is only gold when there is something to update.
- Slim scrollbars in the app's colours.
- The Price Feed no longer reports a problem when the site is briefly unavailable; it tries again quietly and only says so if it keeps failing.
- The update switches now read "Update the app automatically" and "Update my addons automatically".

## v0.7.7 (2026-11-22)

- New: Freeze an addon to keep the version you have. EverlookDB never updates it until you unfreeze it, and still tells you when a newer one is out.

## v0.7.6 (2026-11-22)

- New: disenchant odds measured by players. The disenchants EverlookDB Auctions logs are shared, and the Price Feed brings everyone’s results back into the game.

## v0.7.5 (2026-11-21)

- The app never pops up on its own any more. It updates itself and your addons quietly; you can switch either off in Settings.

## v0.7.4 (2026-11-21)

- The Price Feed brings new auction scans within about a minute instead of 15 minutes.
- The Price Feed finds your auction houses again after the latest WoW Forever update.
- An auctions file the site turned down is no longer sent again every hour; it goes again when the game saves something new.

## v0.7.3 (2026-11-21)

- New: "Send error logs to the developer", next to every error and as "Report a problem" in the sidebar. You get a report id to quote. Your user name, account folders, emails and keys are taken out first, and "What gets sent" shows the exact text.

## v0.7.2 (2026-11-21)

- The shared auction prices are now called the Price Feed, and show in game as "EverlookDB Auctions: Price Feed". It is also on CurseForge; the app keeps it filled. Nothing needs reinstalling.
- New: get all of our addons in one click. A new install shows the EverlookDB addons with one Install all button, and Get addons has an EverlookDB addons filter.

## v0.7.1 (2026-11-21)

- Fixed: 0.7.0 froze at start. If your app froze after updating, download 0.7.1 from everlookdb.com once; it updates itself again from then on.

## v0.7.0 (2026-11-21)

- New: shared auction prices. The app brings the newest auction scans other players uploaded for your auction houses into WoW Forever, so EverlookDB Auctions has prices at your next /reload or login. One switch turns it off.

## v0.6.0 (2026-11-20)

- Opens faster: your addon list shows at once.
- Installs never lose the old addon, even when cut short by a crash or reboot.
- Automatic updates wait while World of Warcraft is running, and never downgrade an addon.
- WoW folders are found once and remembered.
- Clearer error messages, with a Replace button when a folder is in the way.
- Open folder, a What's new link next to updates, and fewer old backups kept.
- The app's own updates wait until nothing else is running and never take the focus.

## v0.5.1 (2026-11-19)

- "Update addons by itself" now updates every addon EverlookDB knows, also ones you or another app installed, not only the ones EverlookDB installed. The old copy is kept in the backups folder first. Developer checkouts (a folder with .git) and addons without a real version are left alone.
- Update all is always at the top of My addons: one click checks every addon for a new version and updates all that have one. It also says how many updates are waiting, or that everything is up to date.
- Each Update button says when that version was released ("released today 22:23"; hover for the exact time).
- The bottom line says when the addon list was checked, when the automatic check last ran and what it did ("nothing to update", "updated 2 addons"), and when it runs next. The "Updated by itself" note says when it happened.

## v0.5.0 (2026-11-18)

- Right-click an addon for its own menu: in My addons Update, Open folder, its project page, Copy name and
  Uninstall; in Get addons Install (or Update), its project page and Copy name. Uninstall now works for every addon,
  also ones you or another app installed: it shows exactly which folders go, keeps any that
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
