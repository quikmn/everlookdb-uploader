# EverlookDB App: addon manager

The EverlookDB Uploader becomes the **EverlookDB App**: a small, gorgeous, ad-free desktop app (Windows, macOS) that

1. finds every World of Warcraft install and shows clearly which game version each one is,
2. lists the addons in each, with their versions and whether an update is waiting,
3. searches every addon we are allowed to offer, and installs, updates and removes them,
4. updates by itself about once an hour (one switch to turn it off),
5. keeps doing what the Uploader does: sends EverlookDB / EverlookDB Auctions data to everlookdb.com.

Principles: the player first. No ads, no accounts, no tracking, nothing in the background except the hourly
check and the uploads. Minimal footprint. Every action is one click and says what it did.

## Sources (only what we may use without stepping on anyone's toes)

- **GitHub releases.** Most big addons publish a ready-to-install zip on GitHub with every release
  (BigWigs packager). Many also attach `release.json`, which lists each zip's game flavors and interface numbers.
- **Tukui** (ElvUI, Tukui and their addons): tukui.org's public API, the one its own client and other
  managers use.
- **WoWInterface**: only if its terms allow a third-party client; otherwise later.
- **CurseForge, Wago**: we've applied for access (2026-11-16). Nothing from them until they say yes.

## Who does what

- **everlookdb.com builds the catalog** (`GET /api/v1/addons/catalog`). The server refreshes it from the
  sources about once an hour, with its own GitHub token, so the app never hits GitHub's rate limits and search
  is instant. Downloads never go through our server: the app downloads each zip straight from GitHub or Tukui.
- **The app** reads the catalog, the installs and their addons, and does installs, updates and removals locally.

## Catalog contract (v1)

`GET https://everlookdb.com/api/v1/addons/catalog` returns JSON (ETag + Cache-Control; the app sends
If-None-Match):

```json
{
  "version": 1,
  "updated": "2026-11-16T15:00:00Z",
  "addons": [
    {
      "id": "github:BigWigsMods/BigWigs",
      "name": "BigWigs Bossmods",
      "author": "BigWigsMods",
      "summary": "Boss timers and warnings.",
      "source": "github",
      "url": "https://github.com/BigWigsMods/BigWigs",
      "version": "v427.9",
      "released": "2026-11-10T12:00:00Z",
      "files": [
        { "flavors": ["retail", "classic", "forever"], "interfaces": [120001, 50503, 16001],
          "url": "https://github.com/BigWigsMods/BigWigs/releases/download/v427.9/BigWigs-v427.9.zip",
          "name": "BigWigs-v427.9.zip", "size": 1234567 }
      ],
      "folders": ["BigWigs", "BigWigs_Core", "BigWigs_Plugins"],
      "ours": false
    }
  ]
}
```

- `id`: `github:<owner>/<repo>`, `tukui:<slug>` or `wowi:<id>`. Stable; the app stores it per installed addon.
- `flavors`: `retail`, `classic` (Cataclysm/MoP Classic, whatever `_classic_` holds), `classic_era`
  (`_classic_era_`), `forever` (`_forever_`), `ptr`, `beta`. When a release has no `release.json`, the
  server reads the `## Interface:` lines of the zip's `.toc` files (it may download the zip once per release
  to learn this and its folder names; never re-host it).
- `interfaces`: the `## Interface` numbers the file declares (WoW Forever is `16001`).
- `folders`: the top-level folders the zip unpacks to (used to recognise addons already installed, e.g. by
  WowUp). May be empty if unknown.
- `ours`: true for EverlookDB's own addons.

## App: install detection

For each `World of Warcraft` folder found (Battle.net registry/config, common folders, folders the user adds),
every flavor folder (`_retail_`, `_classic_`, `_classic_era_`, `_forever_`, `_ptr_`, `_beta_`,
`_xptr_` …) is an install. Show its human name (WoW Forever, Retail, Classic, Classic Era, PTR …) and the
client version from `.build.info` (or the executable), big and clear.

## App: installed addons

Scan `Interface/AddOns/*/*.toc` (including flavor-specific tocs like `Name_Forever.toc` / `Name-Classic.toc`),
group folders that belong together, read Title (strip colour codes), Version, Author, Interface, and the
`X-Curse-Project-ID`, `X-Wago-ID`, `X-WoWI-ID` and `X-Website` fields. Match to the catalog by stored id, then by
folder names. Addons installed by WowUp/CurseForge app that we can't match stay listed as "Not from our sources"
and are never touched.

## App: install / update / remove

Download to a temp file, check size, unzip into a staging folder, check every entry stays inside the target
(no `..`, no absolute paths), then swap the folders in. Keep the previous version's zip for one-click rollback.
Never delete a folder we didn't install. Records live in the app's data folder, not in the game folder.
