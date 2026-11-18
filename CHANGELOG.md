# Changelog

## Unreleased

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
