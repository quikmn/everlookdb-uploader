# Changelog

## Unreleased

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
