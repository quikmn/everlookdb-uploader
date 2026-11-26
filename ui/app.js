// EverlookDB window. Rust does every file and network job; this file only asks it (Tauri commands) and draws.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
// what Rust sent back as an error: a sentence, or { message, conflict } from an install
const errText = (e) => (e && typeof e === 'object' && 'message' in e ? String(e.message) : String(e));

const S = {
  app: null,
  sel: null,
  items: [],
  catalog: null,
  origin: null,
  checked: null,
  tab: 'mine',
  q: '',
  open: new Set(),
  busy: new Map(),
  progress: new Map(),
  confirm: null,
  // what "Uninstall" would remove, from uninstall_plan, for the confirmation of S.confirm
  plan: null,
  showOther: false,
  catIndex: new Map(),
  // addon icons for the selected install, by row key: a data: URL, or null (no icon: a letter tile)
  icons: new Map(),
  iconWait: new Set(),
  // our addons (src-tauri/src/ours.rs) for the selected install: the welcome card and the "EverlookDB addons" filter
  ours: { install: null, addons: [], card: false, running: false, ticked: new Set(), run: null, hidden: false },
  oursOnly: false,
};

const install = () => S.app?.installs.find((i) => i.id === S.sel) || null;

// ------------------------------------------------------------------------------------------------ loading

async function loadState() {
  S.app = await invoke('get_state');
  S.sel = S.app.selected;
  renderRail();
  renderHero();
  renderUploads(S.app.uploads);
  renderSettings();
}

async function loadItems() {
  const i = install();
  if (!i) {
    S.items = [];
    renderList();
    return;
  }
  try {
    S.items = await invoke('list_installed', { install: i.id });
  } catch (e) {
    S.items = [];
    showError(errText(e));
  }
  await loadOurs();
  renderList();
}

// our addons and whether the welcome card shows (ticked: every one not installed yet, until the player unticks it)
async function loadOurs() {
  const i = install();
  const o = S.ours;
  if (!i) return;
  let r;
  try {
    r = await invoke('our_addons', { install: i.id });
  } catch {
    return;
  }
  if (o.install !== i.id) {
    o.install = i.id;
    o.ticked = new Set(r.addons.filter((a) => !a.installed).map((a) => a.id));
    o.run = null;
  } else {
    for (const a of r.addons) if (!o.addons.some((x) => x.id === a.id) && !a.installed) o.ticked.add(a.id);
  }
  o.addons = r.addons;
  o.running = r.running;
  // once shown, the card stays until it is dismissed or closed (it shows what the run did)
  o.card = !o.hidden && (r.card || (o.card && !!o.run));
}

async function loadCatalog(refresh) {
  if (refresh) renderStatus('Checking the addon list…');
  const r = await invoke('get_catalog', { refresh });
  S.catalog = r.catalog;
  S.origin = r.origin;
  S.checked = r.checked;
  S.auto = r.auto;
  S.next = r.next;
  S.loading = r.loading;
  S.catIndex = new Map();
  for (const a of S.catalog?.addons || []) {
    S.catIndex.set(a.id, Search.index(a.name, [a.author, a.summary, ...(a.folders || [])]));
  }
  renderStatus();
  await loadItems();
}

// ------------------------------------------------------------------------------------------------ rail

function renderRail() {
  const games = $('games');
  const list = S.app.installs;
  games.innerHTML = list
    .map((i) => {
      const sub = i.edition || (list.filter((x) => x.name === i.name).length > 1 ? i.root : '');
      return `<li><button class="game${i.game ? '' : ' leftover'}" type="button" data-install="${esc(i.id)}" aria-current="${i.id === S.sel}">
        <span class="game-name">${esc(i.name)}</span>
        <span class="game-version">${i.game ? esc(i.version || '') : 'No game'}</span>
        ${sub ? `<span class="game-sub">${esc(sub)}</span>` : ''}
      </button></li>`;
    })
    .join('');
}

function renderSettings() {
  $('sw-auto').checked = S.app.auto_update;
  $('sw-app-auto').checked = S.app.app_auto_update;
  // the app's own update ("Version 0.7.8 installs by itself once this window is closed"): a quiet inbox line
  if (S.app.app_update) notify({ kind: 'app', tone: 'info', title: S.app.app_update });
  else unnotify('app');
  renderRecent(S.app.recent_updates || []);
  renderShared(S.app.shared && { ...S.app.shared, on: S.app.shared_prices });
  $('sw-start').checked = S.app.autostart;
  $('start-label').textContent = S.app.autostart_label;
  $('app-version').textContent = `v${S.app.version}`;
  $('app-version').title = `EverlookDB ${S.app.version}`;
  if (S.app.health) renderHealth(S.app.health);
  paintReport($('report-foot'));
}

// what updated by itself (newest first, a few lines): "Details! updated to v12.1 · 10 min ago"
function renderRecent(list) {
  const shown = list.slice(0, 3);
  $('recent').hidden = !shown.length;
  $('recent-list').innerHTML = shown
    .map((u) => `<li title="${esc([u.install, u.from ? `${u.from} → ${u.to}` : u.to, exact(u.at)].filter(Boolean).join(' · '))}">Updated ${esc(u.name)} to ${esc(u.to)}<span class="recent-when"> · ${esc(when(u.at))}</span></li>`)
    .join('');
}

// Price Feed (src-tauri/src/shared.rs): "Price Feed: EU-4-Alliance, 12 min old", one line per house
function sharedAge(at) {
  const m = Math.max(0, Math.round((Date.now() / 1000 - at) / 60));
  if (m < 90) return `${m} min old`;
  const h = Math.round(m / 60);
  return h < 36 ? `${h} h old` : `${Math.round(h / 24)} days old`;
}

// One line per auction house, each with its own age ("Alliance  20 min old", "Neutral  7 h old · behind"): a summary
// of the newest one hid a house that had stood still for hours (2026-11-26). Amber: a scan the site should have a
// newer one of by now (shared.rs late_houses), or one over a day old.
function renderShared(v) {
  if (!v) return;
  S.shared = v;
  $('sw-shared').checked = v.on;
  const houses = v.on ? v.houses || [] : [];
  const now = Date.now() / 1000;
  // a problem that is still there after a while is in the inbox; the dot only turns amber
  const trouble = (S.health?.lines || []).some((l) => l.kind.startsWith('price-feed') || l.kind === 'site-down');
  $('shared-text').textContent = !v.on ? 'Off' : v.note || 'Looking for your auction houses…';
  $('shared-dot').className = 'dot ' + (!v.on ? '' : trouble ? 'busy' : 'idle');
  // the rows say it all; the one line is for when there are none
  $('shared-text').parentElement.hidden = houses.length > 0;
  const list = $('shared-houses');
  list.hidden = !houses.length;
  const realms = new Set(houses.map((h) => h.realm)).size;
  list.innerHTML = houses
    .map((h) => {
      const amber = trouble || h.late || now - h.at > 86400;
      const tip = [`${h.realm} ${h.side} (${h.key}): the newest scan is ${sharedAge(h.at)}`];
      if (h.late) tip.push(h.sibling_at ? `Behind: this realm's other prices are ${sharedAge(h.sibling_at)}.` : 'Behind: everlookdb.com usually has a new scan every hour.');
      if (v.checked) tip.push(`Checked ${when(v.checked * 1000)}`);
      return `<li class="ufile" title="${esc(tip.join('\n'))}">
        <span class="dot ${amber ? 'busy' : 'ok'}"></span>
        <span class="ufile-name">${esc(realms > 1 ? h.key : h.side)}</span>
        <span class="ufile-say">${esc(sharedAge(h.at))}${h.late ? ' · behind' : ''}</span>
      </li>`;
    })
    .join('');
  feedNotices(v, houses);
}

// What the Price Feed has to say goes to the inbox like everything else: a house whose prices are hours behind while
// the app itself is working (nothing for the player to fix; "Send report" tells us), and EverlookDB Auctions turned
// off in the game's addon list while the Price Feed is on (/eah does nothing there, and the game doesn't say why).
function feedNotices(v, houses) {
  const late = houses.filter((h) => h.late);
  if (late.length) {
    const say = (h) => `${h.realm} ${h.side}: ${sharedAge(h.at)}${h.sibling_at ? ` (this realm's other prices: ${sharedAge(h.sibling_at)})` : ''}`;
    const title = late.length === 1 ? `${late[0].side} auction prices are behind` : `${late.length} auction houses' prices are behind`;
    notify({
      kind: 'feed-late',
      tone: 'info',
      title,
      body: `${late.map((h) => `<p>${esc(say(h))}</p>`).join('')}<p>The app is working: everlookdb.com has no newer scan of ${late.length === 1 ? 'it' : 'them'} yet. There's nothing to fix on your side.</p>`,
      report: `${title}: ${late.map(say).join('; ')}`,
      since: Math.max(...late.map((h) => h.at)),
      at: Math.max(...late.map((h) => h.at)) * 1000,
      keep: true,
    });
  } else unnotify('feed-late');
  const n = v.on ? v.reader_off || 0 : 0;
  if (n) {
    notify({
      kind: 'feed-reader-off',
      tone: 'local',
      title: 'EverlookDB Auctions is turned off in the game',
      body: `<p>On ${n === 1 ? 'one of your characters' : `${n} of your characters`} the Price Feed is on but EverlookDB Auctions itself is off, so /eah and the prices don't work there. Turn it on in the game's addon list (the AddOns button on the character screen), then log in again.</p>`,
      since: 1,
      at: 0,
      keep: true,
    });
  } else unnotify('feed-reader-off');
}
setInterval(() => renderShared(S.shared), 30000);

// each file's last answer, one quiet line: green sent, amber partly taken or waiting for the server, grey refused
// (the server's reason is information, on hover), red a problem on this PC (the inbox says what to do)
const FILE_DOT = { ok: 'ok', partial: 'busy', retry: 'busy', info: '', failed: 'error' };

function renderUploads(u) {
  if (!u) return;
  renderGiveaway(u.giveaway);
  $('upload-text').textContent = u.paused ? 'Paused' : u.line;
  $('upload-text').title = u.paused ? 'Turn on “Send my game data” in Settings to upload again' : '';
  $('upload-dot').className = 'dot ' + (u.paused ? '' : u.kind === 'error' ? 'busy' : u.kind);
  $('sw-uploads').checked = !u.paused;

  const files = u.files || [];
  const list = $('upload-files');
  list.hidden = !files.length;
  // the rows say it all; the one summary line is for when there are none (or uploads are paused)
  $('upload-text').parentElement.hidden = files.length > 0 && !u.paused;
  list.innerHTML = files
    .map(
      (f) => `<li class="ufile ${esc(f.level)}" title="${esc([f.headline, ...f.details].join('\n'))}">
        <span class="dot ${FILE_DOT[f.level] || ''}"></span>
        <span class="ufile-name">${esc(f.label)}</span>
        <span class="ufile-say">${esc(f.short)}${f.when ? ` ${esc(f.when)}` : ''}</span>
      </li>`
    )
    .join('');
}

// ------------------------------------------------------------------------------------------------ giveaway

// The giveaway code: null while the giveaway is off (giveaway::ENABLED in src-tauri/src/giveaway.rs).
const GIVEAWAY_STEPS = [
  { key: null, done: 'EverlookDB app installed' },
  {
    key: 'auctions',
    done: 'Full auction house scan uploaded',
    how: 'Do a full scan in EverlookDB Auctions, then /reload or log out so the game saves it; the app uploads it.',
  },
  {
    key: 'collector',
    done: 'Collector data uploaded',
    how: 'Play a while with the EverlookDB addon on, then /reload or log out so the game saves it; the app uploads it.',
  },
];

function renderGiveaway(g) {
  const box = $('giveaway');
  box.hidden = !g;
  if (!g) return;
  $('giveaway-code').textContent = g.code;
  $('giveaway-steps').innerHTML = GIVEAWAY_STEPS.map((st) => {
    const ok = st.key === null || g[st.key];
    return `<li class="gstep ${ok ? 'ok' : 'todo'}">
      <span class="gmark" aria-hidden="true">${ok ? '✓' : '✗'}</span>
      <div><p>${esc(st.done)}<span class="sr-only">${ok ? ': done' : ': not yet'}</span></p>${ok ? '' : `<p class="ghow">${esc(st.how)}</p>`}</div>
    </li>`;
  }).join('');
  $('giveaway-note').textContent = g.complete
    ? 'All three done. Paste this code where the giveaway asks for it.'
    : 'The giveaway needs all three. You can copy the code now; it fills in as the uploads go through.';
}

let giveawayTimer = 0;
async function copyGiveaway() {
  try {
    const code = await invoke('copy_giveaway_code');
    const b = $('giveaway-copy');
    $('giveaway-copy-label').textContent = 'Copied';
    b.classList.add('done');
    $('announce').textContent = `Copied ${code} to the clipboard`;
    clearTimeout(giveawayTimer);
    giveawayTimer = setTimeout(() => {
      $('giveaway-copy-label').textContent = 'Copy';
      b.classList.remove('done');
    }, 2400);
  } catch (e) {
    showError(`Couldn’t copy the giveaway code: ${e}`);
  }
}

function uploadNow() {
  invoke('upload_now');
  $('upload-text').title = '';
  $('upload-text').textContent = 'Uploading…';
  $('upload-text').parentElement.hidden = false;
}

// ------------------------------------------------------------------------------------------------ hero

function renderHero() {
  const hero = $('hero');
  const i = install();
  if (!i) {
    hero.innerHTML = `<div class="hero-empty">
      <h1 class="hero-name">No World of Warcraft found</h1>
      <p>EverlookDB looked in the usual places. Point it at your World of Warcraft folder, the one with _retail_, _classic_ or _forever_ inside.</p>
      <p><button class="btn gold" type="button" data-act="add-folder">Choose a folder</button></p>
    </div>`;
    return;
  }
  const version = i.game
    ? i.version
      ? `<span class="num">${esc(i.version)}</span><span class="label">Game version${i.build ? ` ${' '}build ${esc(i.build)}` : ''}</span>`
      : `<span class="num unknown">Version unknown</span><span class="label">No .build.info for this game</span>`
    : `<span class="num unknown">Not installed</span><span class="label">Only leftover files in this folder</span>`;
  hero.innerHTML = `
    <h1 class="hero-name">${esc(i.name)}</h1>
    <div class="hero-version">${version}</div>
    <p class="hero-where">
      ${i.edition ? `<span>${esc(i.edition)}</span>` : ''}
      <span class="path" title="${esc(i.dir)}">${esc(i.dir)}</span>
      <button class="link-button" type="button" data-act="open-folder">Open AddOns folder</button>
    </p>`;
}

// ------------------------------------------------------------------------------------------------ list

// a file for one of these flavors; for a repo added by its GitHub link, also a zip nobody could tell the game of
const fileFor = (a, accepts) =>
  accepts.some((f) => (a.files || []).some((file) => (file.flavors || []).includes(f))) ||
  (!!a.custom && (a.files || []).some((file) => !(file.flavors || []).length));

function renderList() {
  const list = $('list');
  const i = install();
  $('count-mine').textContent = i ? S.items.length : '';
  const runs = (S.catalog?.addons || []).filter((a) => i && fileFor(a, i.accepts)).length;
  $('count-browse').textContent = S.catalog ? runs : '';
  list.setAttribute('aria-labelledby', S.tab === 'mine' ? 'tab-mine' : 'tab-browse');
  document.querySelector('.toolbar').hidden = !i;
  if (!i) {
    list.innerHTML = '';
    return;
  }
  list.innerHTML = S.tab === 'mine' ? mineHtml(i) : browseHtml(i);
  fitVersions(list);
  $('list-tools').hidden = S.tab !== 'mine' || !S.items.length;
  $('gh-open').hidden = S.tab !== 'browse';
  if ($('gh').open) renderGh();
  paintProgress();
  watchIcons();
}

// ------------------------------------------------------------------------------------------------ icons

// a colour of its own for every name, for the letter tile of addons without an icon
function hue(name) {
  let h = 7;
  for (const c of String(name)) h = (h * 31 + c.codePointAt(0)) >>> 0;
  return h % 360;
}

// the addon's icon, or its letter tile until the icon is in (or when it has none); fixed size, so nothing moves
function iconHtml(key, name) {
  const url = S.icons.get(key);
  if (url) return `<span class="ico img" aria-hidden="true"><img src="${url}" alt=""></span>`;
  const letter = (String(name).match(/[\p{L}\p{N}]/u)?.[0] || '?').toUpperCase();
  const want = S.icons.has(key) ? '' : ` data-icon="${esc(key)}"`;
  return `<span class="ico" aria-hidden="true"${want} style="--h:${hue(name)}">${esc(letter)}</span>`;
}

// icons load as their rows come near the screen, a few at a time
const iconSeen = new IntersectionObserver(
  (entries) => {
    const keys = [];
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      iconSeen.unobserve(e.target);
      const k = e.target.dataset.icon;
      if (k && !S.icons.has(k) && !S.iconWait.has(k)) {
        S.iconWait.add(k);
        keys.push(k);
      }
    }
    for (let n = 0; n < keys.length; n += 8) loadIcons(keys.slice(n, n + 8));
  },
  { root: $('list'), rootMargin: '400px 0px' }
);

function watchIcons() {
  iconSeen.disconnect();
  for (const el of $('list').querySelectorAll('.ico[data-icon]')) iconSeen.observe(el);
}

async function loadIcons(keys) {
  const sel = S.sel;
  let got = {};
  try {
    got = await invoke('addon_icons', { install: sel, keys });
  } catch {
    // no icons this time: the letter tiles stay
  }
  if (sel !== S.sel) return;
  for (const k of keys) {
    S.iconWait.delete(k);
    S.icons.set(k, got[k] || null);
    paintIcon(k);
  }
}

// swap the tile for the icon in place, once the picture is decoded (no flash, no layout change)
function paintIcon(key) {
  const url = S.icons.get(key);
  for (const el of document.querySelectorAll(`.ico[data-icon="${CSS.escape(key)}"]`)) {
    el.removeAttribute('data-icon');
    if (!url) continue;
    const img = new Image();
    img.alt = '';
    img.src = url;
    img.decode().then(
      () => {
        el.classList.add('img');
        el.replaceChildren(img);
      },
      () => {}
    );
  }
}

// after an install, update or rollback the addon's icon may be new
function forgetIcons(key) {
  if (key === '*') S.icons.clear();
  else S.icons.delete(key);
}

function mineHtml(i) {
  const q = S.q.trim();
  let items = S.items;
  if (q) {
    items = items
      .map((it) => ({ it, s: Search.score(Search.index(it.name, [it.author, it.notes, ...it.folders]), q) }))
      .filter((x) => x.s > 0)
      .sort((a, b) => b.s - a.s)
      .map((x) => x.it);
  }
  if (!S.items.length && !i.game) {
    return `<div class="empty"><h3>${esc(i.name)} isn’t installed here</h3>
      <p>Only leftover files are in this folder, so there are no addons to show or install.</p></div>`;
  }
  // the welcome card: our addons in one click (it has the screen's one gold button while it shows)
  const card = !q && S.ours.card && i.game ? oursCardHtml(i) : '';
  if (!S.items.length) {
    return `${card}<div class="empty${card ? ' quiet' : ''}"><h3>${card ? 'Or find others' : `No addons in ${esc(i.name)} yet`}</h3>
      <p>Find boss mods, bag addons, unit frames and more, and install them with one click.</p>
      <button class="btn${card ? '' : ' gold'}" type="button" data-act="tab" data-tab="browse">Get addons</button></div>`;
  }
  if (!items.length) {
    return `<div class="empty"><h3>Nothing installed matches “${esc(q)}”</h3>
      <p>It may be in the addon list.</p>
      <button class="btn gold" type="button" data-act="tab" data-tab="browse">Search all addons for “${esc(q)}”</button></div>`;
  }
  // always there: one click checks the addon list again and updates every addon that has a newer version
  const updates = i.game ? S.items.filter((it) => it.update && it.catalog) : [];
  const byHand = updates.filter((it) => !it.auto).length;
  const handNote = byHand
    ? `<span class="where" title="Their installed versions can’t be compared with the newest ones (like 1.2-forever and 1.2), so EverlookDB only replaces them when you click their own Update button."> · ${plural(byHand, 'needs', 'need')} its own click</span>`
    : '';
  const bar =
    i.game && !q
      ? `<div class="updates-bar"><p>${updates.length ? `${plural(updates.length, 'update', 'updates')} ready in ${esc(i.name)}${handNote}` : `Everything in ${esc(i.name)} is up to date${S.allChecked ? ` <span class="where">· checked ${esc(when(S.allChecked))}</span>` : ''}`}</p>
         <button class="btn${card || !updates.length ? '' : ' solid'}" type="button" data-act="update-all" ${busyAttrs('*')} title="Checks every addon in ${esc(i.name)} for a new version, then updates all that have one (other games are updated from their own page)">Update all</button></div>`
      : '';
  return card + bar + `<ul class="rows">${items.map((it) => mineRow(it, i)).join('')}</ul>`;
}

// ------------------------------------------------------------------------------------------------ our addons

// where one of our addons is in a run: waiting, downloading (with %), installed, or what went wrong
function oursState(a) {
  const run = S.ours.run;
  const r = run?.results.get(a.id);
  if (r?.change) return { cls: 'ok', html: `${CHECK}Installed ${esc(r.change.to)}` };
  if (r?.failure) return { cls: 'bad', html: esc(errText(r.failure)) };
  if (a.installed) return { cls: 'ok', html: `${CHECK}Installed` };
  if (run && run.ids.includes(a.id)) {
    const p = S.progress.get(a.id);
    if (!p) return { cls: 'wait', html: run.done ? 'Not installed' : 'Waiting…' };
    if (p.total && p.done >= p.total) return { cls: 'busy', html: 'Unpacking…' };
    return { cls: 'busy', html: p.total ? `Downloading ${Math.floor((p.done * 100) / p.total)}%` : `Downloading ${mb(p.done)}` };
  }
  return null;
}

function oursRowsHtml() {
  const run = S.ours.run;
  return S.ours.addons
    .map((a) => {
      const st = oursState(a);
      const tick = a.installed || run
        ? ''
        : `<input type="checkbox" data-ours-tick="${esc(a.id)}" ${S.ours.ticked.has(a.id) ? 'checked' : ''} aria-label="Install ${esc(a.name)}">`;
      return `<li class="wl-row${a.installed ? ' have' : ''}">
        <label class="wl-label">
          <span class="wl-tick">${tick || (st?.cls === 'ok' ? '' : '<span class="wl-dot"></span>')}</span>
          <span class="wl-text"><span class="wl-name">${esc(a.name)}</span><span class="wl-sum">${esc(a.summary)}</span></span>
        </label>
        <span class="wl-state ${st?.cls || ''}" data-ours-state="${esc(a.id)}">${st?.html || ''}</span>
      </li>`;
    })
    .join('');
}

// how installs go, in the card: the same safe path as every install
function oursHow() {
  return `They install like every EverlookDB install: downloaded from GitHub, checked, then swapped in whole. ${
    S.ours.running ? 'WoW is running: type /reload in the game once they are in.' : 'They load the next time you start WoW.'
  }`;
}

function oursCardHtml(i) {
  const o = S.ours;
  const run = o.run;
  const todo = o.addons.filter((a) => !a.installed && o.ticked.has(a.id));
  if (run?.done) {
    const ok = [...run.results.values()].filter((r) => r.change).length;
    const bad = [...run.results.values()].filter((r) => r.failure).length;
    const reload = run.changes.some((c) => c.running) ? 'WoW is running: type /reload in the game to load them.' : 'They load the next time you start WoW (or type /reload if it’s open).';
    return `<section class="welcome" aria-labelledby="welcome-title">
      <div class="wl-head">
        <h2 id="welcome-title">${bad ? (ok ? `${plural(ok, 'addon', 'addons')} installed, ${bad} didn’t` : 'Nothing was installed') : `Installed ${plural(ok, 'EverlookDB addon', 'EverlookDB addons')}`}</h2>
        <p>${ok ? `${esc(reload)} EverlookDB keeps them updated from now on.` : 'Check your connection, then try again.'}</p>
      </div>
      <ul class="wl-list">${oursRowsHtml()}</ul>
      <div class="wl-actions">${bad ? '<button class="btn" type="button" data-act="ours-install" data-where="card">Try again</button>' : ''}<button class="btn" type="button" data-act="ours-close">Close</button></div>
    </section>`;
  }
  const n = todo.length;
  const label = run ? 'Installing…' : n === o.addons.filter((a) => !a.installed).length ? 'Install all' : n ? `Install ${plural(n, 'addon', 'addons')}` : 'Install all';
  const busy = run ? `disabled aria-busy="true" data-busy="${esc(run.count > 1 ? `Installing ${Math.min(run.index, run.count)} of ${run.count}` : 'Installing…')}"` : n ? '' : 'disabled';
  return `<section class="welcome" aria-labelledby="welcome-title">
    <div class="wl-head">
      <h2 id="welcome-title">Get the EverlookDB addons</h2>
      <p>Our own addons for ${esc(i.name)}, made to work together and with everlookdb.com: tick what you want and install them in one go.</p>
    </div>
    <ul class="wl-list">${oursRowsHtml()}</ul>
    <p class="wl-how">${esc(oursHow())}</p>
    <div class="wl-actions">
      <button class="btn solid" type="button" data-act="ours-install" data-where="card" ${busy}>${esc(label)}</button>
      ${run ? '' : '<button class="btn" type="button" data-act="ours-later">Not now</button>'}
    </div>
  </section>`;
}

// the permanent way back, in Get addons: our addons only, and one plain button for the missing ones
function oursBarHtml(i) {
  const o = S.ours;
  const missing = o.addons.filter((a) => !a.installed);
  const run = o.run;
  const busy = run && !run.done ? `disabled aria-busy="true" data-busy="${esc(`Installing ${Math.min(run.index, run.count)} of ${run.count}`)}"` : '';
  const text = !o.addons.length
    ? `Our addons are made for WoW Forever; none of them runs in ${esc(i.name)}.`
    : missing.length
      ? `${o.addons.length - missing.length} of ${o.addons.length} installed in ${esc(i.name)}`
      : `All ${o.addons.length} are installed in ${esc(i.name)}`;
  const button = i.game && missing.length ? `<button class="btn" type="button" data-act="ours-install" data-where="bar" ${busy} title="${esc(oursHow())}">Install all missing (${missing.length})</button>` : '';
  return `<div class="ours-bar"><p><strong>EverlookDB addons</strong>&ensp;${text}</p>${button}</div>`;
}

async function installOurs(where) {
  const o = S.ours;
  if (o.run && !o.run.done) return;
  const ids = o.addons.filter((a) => !a.installed && (where === 'bar' || o.ticked.has(a.id))).map((a) => a.id);
  if (!ids.length) return;
  o.run = { where, ids, results: new Map(), changes: [], done: false, index: 1, count: ids.length };
  for (const id of ids) S.busy.set(id, 'Waiting…');
  renderList();
  try {
    const r = await invoke('install_ours', { install: S.sel, ids });
    o.run.changes = r.changes;
    if (where === 'bar' || !o.card) {
      showChanges(r.changes.length ? `Installed ${plural(r.changes.length, 'EverlookDB addon', 'EverlookDB addons')}` : 'Nothing was installed', r.changes, r.failed);
    }
  } catch (e) {
    showError(errText(e));
  } finally {
    o.run.done = true;
    for (const id of ids) {
      S.busy.delete(id);
      S.progress.delete(id);
      forgetIcons(id);
    }
    await loadItems();
  }
}

// a progress event during a run: the row's state and the button's "Installing 2 of 11" in place
function paintOurs() {
  const run = S.ours.run;
  if (!run || run.done) return;
  for (const a of S.ours.addons) {
    const el = document.querySelector(`[data-ours-state="${CSS.escape(a.id)}"]`);
    const st = oursState(a);
    if (el && st) {
      el.className = `wl-state ${st.cls}`;
      el.innerHTML = st.html;
    }
  }
  for (const b of document.querySelectorAll('[data-act="ours-install"][aria-busy]')) {
    b.dataset.busy = run.count > 1 ? `Installing ${Math.min(run.index, run.count)} of ${run.count}` : 'Installing…';
  }
}

// our own addons' pages on everlookdb.com, by GitHub repository name
const OUR_PAGES = {
  everlookdb: 'https://everlookdb.com/contribute.html',
  everlookdbauctions: 'https://everlookdb.com/auctions-addon.html',
  lattice: 'https://everlookdb.com/lattice.html',
  latticepartyraidframes: 'https://everlookdb.com/lattice.html',
  whack: 'https://everlookdb.com/whack.html',
  craftcolors: 'https://everlookdb.com/craftcolors.html',
  everlookdbcrafting: 'https://everlookdb.com/everlookdb-crafting.html',
};
const ourPage = (id) => OUR_PAGES[String(id || '').split('/').pop().toLowerCase()] || null;

// installed from a GitHub link the player pasted (not from the addon list)
function ghMark(on) {
  return on ? '<span class="ours gh" title="Added by its GitHub link. EverlookDB checks the repository for new releases.">from GitHub</span>' : '';
}

function oursMark(on) {
  return on ? '<span class="ours" title="Made by EverlookDB"><i></i>by EverlookDB</span>' : '';
}

const CHECK = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-7"/></svg>';
// a snowflake: frozen ("never update this one")
const FREEZE_PATH = '<path d="M8 1.5v13M2.4 4.75l11.2 6.5M2.4 11.25l11.2-6.5M6.2 2.6L8 4.2l1.8-1.6M6.2 13.4L8 11.8l1.8 1.6"/>';
const FROZEN_TIP = 'Frozen: EverlookDB never updates it (not Update all, not automatically). Unfreeze it to update again.';
const frozenBadge = () => `<span class="state frozen" title="${esc(FROZEN_TIP)}"><svg viewBox="0 0 16 16" aria-hidden="true">${FREEZE_PATH}</svg>Frozen</span>`;

// the addon "Update all" is downloading right now, if it is
const updatingNow = () => (S.busy.has('*') && S.progress.get('*')?.id) || null;

function busyLabel(key) {
  return S.busy.get(key) ?? (key === updatingNow() ? 'Updating…' : null);
}

function busyAttrs(key) {
  const label = busyLabel(key);
  return label ? `disabled aria-busy="true" data-pkey="${esc(key)}" data-busy="${esc(label)}"` : '';
}

const mb = (n) => `${(n / 1048576).toFixed(n < 10485760 ? 1 : 0)} MB`;

// what a busy button says, and how full its bar is (null: no bar), from the last "progress" event for it
function progressOf(key) {
  const label = busyLabel(key);
  const p = S.progress.get(key);
  if (!label || !p) return { text: label, frac: null };
  if (key === '*' && p.count > 1) {
    // the bar covers the whole run: addons done, plus how far the current one is
    return { text: `Updating ${p.index} of ${p.count}`, frac: (p.index - 1 + (p.total ? Math.min(1, p.done / p.total) : 0)) / p.count };
  }
  if (p.total && p.done >= p.total) return { text: 'Unpacking…', frac: 1 };
  const verb = label.replace('…', '');
  if (!p.total) return { text: `${verb} ${mb(p.done)}`, frac: null };
  return { text: `${verb} ${Math.floor((p.done * 100) / p.total)}%`, frac: p.done / p.total };
}

// update the busy buttons in place (progress comes about ten times a second; no need to redraw the list)
function paintProgress() {
  for (const b of document.querySelectorAll('.btn[data-pkey]')) {
    const { text, frac } = progressOf(b.dataset.pkey);
    if (text) b.dataset.busy = text;
    if (frac === null) {
      delete b.dataset.progress;
    } else {
      b.dataset.progress = '';
      b.style.setProperty('--p', `${(Math.min(1, frac) * 100).toFixed(1)}%`);
    }
  }
}

// a version as people read it ("v664", not "Plater-v664"); what the author wrote is in the tooltip
function versionCell(raw, label, what) {
  const shown = label || raw || '';
  const tip = raw && raw !== shown ? `${what}: ${raw}` : what;
  return `<div class="row-version" title="${esc(tip)}">${esc(shown)}</div>`;
}

// the version column fits the longest version shown, so no version is ever cut; the description gives way
function fitVersions(list) {
  let longest = 0;
  for (const el of list.querySelectorAll('.row-version')) longest = Math.max(longest, el.textContent.length);
  list.style.setProperty('--ver', `max(${matchMedia('(max-width: 900px)').matches ? 90 : 110}px, ${longest + 1}ch)`);
}

// a folder with leftover files but no game: nothing is downloaded into it
const NO_GAME = 'This game isn’t installed here (only leftover files), so EverlookDB doesn’t install or update addons in it.';
const noGame = () => `<span class="state nogame" title="${esc(NO_GAME)}">Game not installed</span>`;

// folders another app (or the player) put in AddOns are in the way of an update: replace them, keeping a backup
function replaceButton(id, folders) {
  return `<button class="btn" type="button" data-act="replace" data-id="${esc(id)}" data-folders="${esc(JSON.stringify(folders))}" ${busyAttrs(id)} title="Replaces ${esc(folders.join(', '))}; the copies there now are kept in EverlookDB’s backups folder">Replace (backup kept)</button>`;
}

function mineRow(it, i) {
  const key = it.key;
  const open = S.open.has(key);
  let action;
  if (it.frozen) {
    const ver = it.version_label || it.version;
    const held = it.held && ver ? `<div class="released">Frozen at ${esc(ver)} · ${esc(it.latest_label || it.latest)} available</div>` : '';
    action = frozenBadge() + held;
  } else if (!i.game && it.update && it.catalog) {
    action = noGame();
  } else if (it.update && it.catalog && it.conflict?.length) {
    action = `${replaceButton(key, it.conflict)}<div class="released" title="${esc(it.conflict.join(', '))} ${it.conflict.length === 1 ? 'is' : 'are'} in AddOns from another app or put there by hand">${esc(it.conflict.join(', '))} in the way</div>`;
  } else if (it.update && it.catalog) {
    const hand = it.auto ? '' : ` title="EverlookDB can’t tell whether ${esc(it.latest_label || it.latest)} is newer than your ${esc(it.version_label || it.version || '')}, so it only replaces it when you click"`;
    action = `<button class="btn" type="button" data-act="install" data-id="${esc(key)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(key)}${hand}>Update to ${esc(it.latest_label || it.latest)}</button>${releasedLine(it.latest_released, it.changelog)}`;
  } else if (it.catalog && it.latest) {
    action = `<span class="state ok">${CHECK}Up to date</span>`;
  } else if (it.catalog) {
    action = `<span class="state">Not for ${esc(i.name)}</span>`;
  } else {
    action = `<span class="state" title="Installed by you or another app. EverlookDB leaves it alone.">Not from our sources</span>`;
  }
  const meta = [it.author ? `<span class="author">${esc(it.author)}</span>` : '', it.notes ? esc(it.notes) : ''].filter(Boolean).join('&ensp;');
  return `<li class="row" data-row="${esc(key)}">
    <div class="row-main">
      ${iconHtml(key, it.name)}
      <button class="row-head" type="button" data-act="toggle" data-key="${esc(key)}" aria-expanded="${open}">
        <span class="row-name"><span class="t">${esc(it.name)}</span>${oursMark(it.ours)}${ghMark(it.custom)}</span>
        <span class="row-meta">${meta || '&nbsp;'}</span>
      </button>
      ${versionCell(it.version, it.version_label, 'Installed version')}
      <div class="row-action">${action}</div>
    </div>
    ${open ? mineMore(it, i) : ''}
  </li>`;
}

function mineMore(it, i) {
  const key = it.key;
  const actions = [];
  const page = it.ours && ourPage(it.key);
  if (page) actions.push(`<button class="link-button" type="button" data-act="url" data-url="${page}">About it on everlookdb.com</button>`);
  if (it.url && it.url.startsWith('https://')) actions.push(`<button class="link-button" type="button" data-act="url" data-url="${esc(it.url)}">${page ? 'Source' : 'Project page'}</button>`);
  if (it.managed && it.previous && i.game && !it.frozen) {
    actions.push(`<button class="btn" type="button" data-act="rollback" data-id="${esc(key)}" ${busyAttrs(key)}>Go back to ${esc(it.previous)}</button>`);
  }
  if (it.catalog && !it.managed && it.latest && !it.update && !it.frozen && i.game) {
    actions.push(`<button class="btn" type="button" data-act="install" data-id="${esc(key)}" data-take="1" ${busyAttrs(key)} title="Reinstalls it from its source; the copy there now is kept in EverlookDB's backups folder">Keep it updated with EverlookDB</button>`);
  }
  if (it.catalog) {
    actions.push(
      it.frozen
        ? `<button class="btn" type="button" data-act="freeze" data-id="${esc(key)}" data-on="" ${busyAttrs(key)}>Unfreeze</button>`
        : `<button class="btn" type="button" data-act="freeze" data-id="${esc(key)}" data-on="1" ${busyAttrs(key)} title="Keeps this version: EverlookDB never updates it until you unfreeze it">Freeze (never update)</button>`,
    );
  }
  const asking = S.confirm === key && S.plan?.key === key;
  if (!asking) actions.push(`<button class="btn danger" type="button" data-act="ask-remove" data-id="${esc(key)}">Uninstall</button>`);
  const who = it.managed
    ? it.custom
      ? 'Installed by EverlookDB from its GitHub link. EverlookDB checks the repository for new releases; uninstalling stops that.'
      : 'Installed by EverlookDB.'
    : it.catalog
      ? 'Installed by you or another app. EverlookDB can update it; it only replaces these folders when you ask.'
      : 'Installed by you or another app, from a source EverlookDB doesn’t have. EverlookDB only changes it if you uninstall it here.';
  return `<div class="row-more">
    ${it.notes ? `<p>${esc(it.notes)}</p>` : ''}
    <p>${who}${it.frozen ? ' Frozen: EverlookDB never updates it until you unfreeze it.' : ''}</p>
    <p class="folders">${plural(it.folders.length, 'folder', 'folders')}: ${it.folders.map(esc).join(', ')}</p>
    ${actions.length ? `<div class="actions">${actions.join('')}</div>` : ''}
    ${asking ? confirmHtml(it, S.plan) : ''}
  </div>`;
}

// "Uninstall X?": names the addon and every folder that goes (and any kept because another addon uses them too)
function confirmHtml(it, p) {
  const n = p.folders.length;
  const kept = p.kept.length
    ? `<p class="confirm-note">Kept, because another addon uses ${p.kept.length === 1 ? 'it' : 'them'} too: ${p.kept.map(esc).join(', ')}</p>`
    : '';
  const ours = p.managed
    ? `<p class="confirm-note">EverlookDB stops updating it${it.custom ? ' (and stops checking its GitHub repository, when no other game has it)' : ''}.</p>`
    : '';
  return `<div class="confirm" role="group" aria-labelledby="confirm-title">
    <p id="confirm-title"><strong>Uninstall ${esc(p.name)}?</strong> ${n === 1 ? 'Its folder goes' : `Its ${n} folders go`} to the Recycle Bin, so you can get ${n === 1 ? 'it' : 'them'} back from there.</p>
    <p class="confirm-folders">${p.folders.map((f) => `<code>${esc(f)}</code>`).join(' ')}</p>
    ${kept}${ours}
    <div class="confirm-actions">
      <button class="btn danger" type="button" data-act="remove" data-id="${esc(p.key)}" ${busyAttrs(p.key)}>Uninstall</button>
      <button class="btn" type="button" data-act="cancel">Cancel</button>
    </div>
  </div>`;
}

function browseHtml(i) {
  if (!S.catalog) {
    if (S.loading || !S.origin) {
      return `<div class="empty"><h3>Loading the addon list…</h3>
      <p>EverlookDB is getting the newest list from everlookdb.com.</p></div>`;
    }
    if (S.origin === 'outdated') {
      return `<div class="empty"><h3>Update the app to see the addon list</h3>
      <p>The addon list has a newer format than this version of EverlookDB reads. EverlookDB updates itself within the hour, or get the newest version from everlookdb.com.</p>
      <button class="btn" type="button" data-act="url" data-url="https://everlookdb.com/contribute.html">Open everlookdb.com</button></div>`;
    }
    return `<div class="empty"><h3>The addon list isn’t here yet</h3>
      <p>EverlookDB gets it by itself as soon as everlookdb.com answers.</p>
      <button class="btn" type="button" data-act="refresh">Try now</button></div>`;
  }
  const q = S.q.trim();
  const installed = new Map(S.items.filter((it) => it.catalog).map((it) => [it.key, it]));
  // "EverlookDB addons": only our own (the set the welcome card offers), in its order, with Install all missing
  const chip = `<div class="filters"><button class="chip" type="button" data-act="ours-filter" aria-pressed="${S.oursOnly}">
    <i aria-hidden="true"></i>EverlookDB addons${S.ours.addons.length ? ` <span class="count">${S.ours.addons.length}</span>` : ''}</button></div>`;
  if (S.oursOnly) {
    // with the card's one-line summaries (the catalog leaves some empty)
    const rows = S.ours.addons
      .map((p) => { const a = S.catalog.addons.find((x) => x.id === p.id); return a && { ...a, summary: p.summary || a.summary }; })
      .filter((a) => a && (!q || Search.score(S.catIndex.get(a.id), q) > 0));
    return chip + oursBarHtml(i) + `<ul class="rows">${rows.map((a) => browseRow(a, i, installed.get(a.id), true)).join('')}</ul>`;
  }
  let all = S.catalog.addons.map((a) => ({ a, s: Search.score(S.catIndex.get(a.id), q), runs: fileFor(a, i.accepts) }));
  all = all.filter((x) => x.s > 0);
  all.sort((x, y) => (q ? y.s - x.s : 0) || (y.a.ours - x.a.ours) || x.a.name.localeCompare(y.a.name));
  const here = all.filter((x) => x.runs);
  const other = all.filter((x) => !x.runs);
  const LIMIT = 200;
  let html = chip;
  if (/github\.com\/|^[\w.-]+\/[\w.-]+$/i.test(q)) {
    return `<div class="empty"><h3>Install it from its GitHub link?</h3>
      <p>EverlookDB looks the addon up, shows you what it is, and installs it when you say so.</p>
      <button class="btn gold" type="button" data-act="gh-open" data-link="${esc(q)}">Look up ${esc(q.replace(/^https?:\/\/(www\.)?/i, ''))}</button></div>`;
  }
  if (!here.length && !other.length) {
    return `<div class="empty"><h3>No addon matches “${esc(q)}”</h3>
      <p>Try a shorter word, the author’s name, or what it does (“bags”, “boss”, “quest”).</p>
      <p>Not in the list? If it’s on GitHub, <button class="link-button" type="button" data-act="gh-open">install it from its link</button>.</p></div>`;
  }
  if (!here.length) html += `<div class="empty"><h3>Nothing for ${esc(i.name)} matches “${esc(q)}”</h3></div>`;
  html += `<ul class="rows">${here.slice(0, LIMIT).map((x) => browseRow(x.a, i, installed.get(x.a.id), true)).join('')}</ul>`;
  if (here.length > LIMIT) html += `<p class="group-title">Showing ${LIMIT} of ${here.length}. Type more to narrow it down.</p>`;
  if (other.length) {
    html += `<button class="link-button other-toggle" type="button" data-act="other" aria-expanded="${S.showOther}">${
      S.showOther ? 'Hide' : 'Show'
    } ${plural(other.length, 'addon', 'addons')} for other game versions</button>`;
    if (S.showOther) {
      html += `<ul class="rows">${other.slice(0, LIMIT).map((x) => browseRow(x.a, i, null, false)).join('')}</ul>`;
    }
  }
  return html;
}

function browseRow(a, i, it, runs) {
  let action;
  if (!i.game) {
    action = it ? `<span class="state ok">${CHECK}Installed</span>` : noGame();
  } else if (!runs) {
    action = `<span class="state">Not for ${esc(i.name)}</span>`;
  } else if (it && it.frozen) {
    action = frozenBadge();
  } else if (it && it.update) {
    action = it.conflict?.length
      ? replaceButton(a.id, it.conflict)
      : `<button class="btn" type="button" data-act="install" data-id="${esc(a.id)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(a.id)}>Update to ${esc(a.version_label || a.version)}</button>${releasedLine(a.released, notesUrl(a))}`;
  } else if (it) {
    action = `<span class="state ok">${CHECK}Installed</span>`;
  } else {
    action = `<button class="btn" type="button" data-act="install" data-id="${esc(a.id)}" ${busyAttrs(a.id)}>Install</button>`;
  }
  const page = a.ours && ourPage(a.id);
  const where = page ? 'everlookdb.com' : a.source === 'tukui' ? 'Tukui' : a.source === 'github' ? 'GitHub' : a.source;
  return `<li class="row${runs ? '' : ' dim'}" data-row="${esc(a.id)}">
    <div class="row-main">
      ${iconHtml(a.id, a.name)}
      <button class="row-head" type="button" data-act="url" data-url="${esc(page || a.url)}" title="Open ${esc(a.name)} on ${esc(where)}">
        <span class="row-name"><span class="t">${esc(a.name)}</span>${oursMark(a.ours)}${ghMark(a.custom)}</span>
        <span class="row-meta"><span class="author">${esc(a.author)}</span>&ensp;${esc(a.summary)}</span>
      </button>
      ${versionCell(a.version, a.version_label, 'Latest version')}
      <div class="row-action">${action}</div>
    </div>
  </li>`;
}

// an exact local date and time people read at a glance: "today 00:01", "yesterday 22:23", "17 Nov 2026, 14:05"
function when(iso) {
  const d = iso ? new Date(iso) : null;
  if (!d || isNaN(d)) return '';
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const day = new Date(d); day.setHours(0, 0, 0, 0);
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const days = Math.round((today - day) / 86400000);
  if (days === 0) return `today ${time}`;
  if (days === 1) return `yesterday ${time}`;
  if (days === -1) return `tomorrow ${time}`;
  return `${d.toLocaleDateString([], { day: 'numeric', month: 'short', year: 'numeric' })}, ${time}`;
}
const exact = (iso) => { const d = iso ? new Date(iso) : null; return d && !isNaN(d) ? d.toLocaleString() : ''; };

// under an Update button: when that version came out, and a link to what changed in it
const releasedLine = (iso, notes) => {
  const day = when(iso) ? `<span title="${esc(exact(iso))}">released ${esc(when(iso))}</span>` : '';
  const link = notes && notes.startsWith('https://') ? `<button class="link-button" type="button" data-act="url" data-url="${esc(notes)}" title="${esc(notes)}">What’s new</button>` : '';
  return day || link ? `<div class="released">${[day, link].filter(Boolean).join(' · ')}</div>` : '';
};
// a catalog entry's release notes: its own link, or its GitHub release page (as catalog.rs changelog_url)
function notesUrl(a) {
  if ((a.changelog || '').startsWith('https://')) return a.changelog;
  const m = /^https:\/\/github\.com\/([^/?#\s]+\/[^/?#\s]+)\/?$/.exec(a.url || '');
  return m && a.version && !/[?#\s"<>]/.test(a.version) ? `https://github.com/${m[1]}/releases/tag/${a.version}` : null;
}

// the automatic check, for the footer: when it last ran, what it did, and when it runs next
function autoLine() {
  const a = S.auto;
  const parts = [];
  if (a) {
    let did = a.off ? 'checked only (automatic updates are off)' : a.failed ? `${a.updated} updated, ${a.failed} failed` : a.updated ? `updated ${plural(a.updated, 'addon', 'addons')}` : a.waiting ? '' : 'nothing to update';
    if (!a.off && a.waiting) did = [did, `${plural(a.waiting, 'update waits', 'updates wait')} until WoW is closed`].filter(Boolean).join(', ');
    parts.push(`Auto check ran ${when(a.at)}: ${did}`);
  }
  if (S.next) parts.push(`next ${when(S.next)}`);
  return parts.join(' · ');
}

function renderStatus(text) {
  const n = (S.catalog?.addons || []).filter((a) => !a.custom).length;
  const line =
    text ||
    (S.loading ? (S.catalog ? 'Loading the newest addon list… (showing the one from last time)' : 'Loading the addon list…') : null) ||
    {
      live: `Addon list checked ${when(S.checked)}`,
      cache: S.checked ? `Addon list checked ${when(S.checked)}` : 'Addon list from last time',
      sample: 'Sample addon list: the everlookdb.com catalog isn’t live yet',
      outdated: 'Update the app: the addon list has a newer format than this version reads',
      none: 'Waiting for the addon list',
    }[S.origin] ||
    'Loading the addon list…';
  const auto = text ? '' : autoLine();
  $('status').innerHTML = `<span>${esc(line)}${n ? `&ensp;${plural(n, 'addon', 'addons')} from GitHub and Tukui` : ''}${auto ? `<br><span class="auto-line" title="${esc(S.auto ? exact(S.auto.at) : '')}">${esc(auto)}</span>` : ''}</span>
    <button class="link-button" type="button" data-act="refresh">Check now</button>`;
}

// ------------------------------------------------------------------------------------------------ notices

// where a changed addon takes effect: a /reload when the game is running, else the next start
function reloadLine(changes) {
  if (!changes.length) return '';
  const n = changes.length;
  return changes.some((c) => c.running)
    ? `<p class="where">WoW is running: type /reload in the game to load ${n === 1 ? 'it' : 'them'}.</p>`
    : `<p class="where">${n === 1 ? 'It loads' : 'They load'} the next time you start WoW (or type /reload if it’s open).</p>`;
}

// The one place for messages (#inbox, at the top of the main pane, where the eye already is: nothing hides behind a
// bell). Every notice lands here: what an action did, what keeps failing in the background (health.rs: shown only
// after half an hour, one line when the site is down), what the player can fix, the app's own update. A notice
// replaces the last one of its kind; a dismissed one stays dismissed (the background's across restarts); at most
// INBOX_MAX show, newest first. Quiet state (a file's last upload, the Price Feed's age) stays in the rail.
const INBOX_MAX = 3;
const Inbox = { items: new Map(), dismissed: new Set() };
try {
  for (const k of JSON.parse(localStorage.getItem('inbox-dismissed') || '[]')) Inbox.dismissed.add(k);
} catch {}
const noteKey = (n) => `${n.kind}|${n.title}|${n.since || n.at}`;

// { kind, tone: 'ok' | 'info' | 'bad' | 'local' | 'down', title, body (html), report (the text a report is about) }
function notify(n) {
  const old = Inbox.items.get(n.kind);
  if (old && old.title === n.title && old.body === n.body && n.keep) return;
  Inbox.items.set(n.kind, { at: Date.now(), ...n });
  renderInbox();
}
function unnotify(kind) {
  if (Inbox.items.delete(kind)) renderInbox();
}
function dismissNote(kind) {
  const n = Inbox.items.get(kind);
  if (!n) return;
  Inbox.items.delete(kind);
  if (n.since) {
    Inbox.dismissed.add(noteKey(n));
    try {
      localStorage.setItem('inbox-dismissed', JSON.stringify([...Inbox.dismissed].slice(-50)));
    } catch {}
  }
  renderInbox();
}

const NOTE_ICON = {
  ok: '<path d="M3.5 8.5l3 3 6-7"/>',
  info: '<circle cx="8" cy="8" r="5.5"/><path d="M8 7.5v3M8 5.2v.1"/>',
  bad: '<circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.5M8 10.8v.1"/>',
  local: '<circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.5M8 10.8v.1"/>',
  down: '<path d="M2.5 6.5a8 8 0 0 1 11 0M4.5 8.8a5 5 0 0 1 7 0M6.8 11a1.8 1.8 0 0 1 2.4 0"/>',
};

// problems first (something to do on this PC, then the site down, then a failure), quiet lines after; newest first within
const RANK = { local: 0, down: 1, bad: 2, ok: 3, info: 4 };
function renderInbox() {
  const box = $('inbox');
  const list = [...Inbox.items.values()].filter((n) => !Inbox.dismissed.has(noteKey(n))).sort((a, b) => (RANK[a.tone] ?? 3) - (RANK[b.tone] ?? 3) || b.at - a.at).slice(0, INBOX_MAX);
  box.hidden = !list.length;
  // a report box being typed in is left alone (paintReport)
  if (box.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
  box.innerHTML = list
    .map(
      (n) => `<div class="notice ${esc(n.tone)}">
        <svg class="note-icon" viewBox="0 0 16 16" aria-hidden="true">${NOTE_ICON[n.tone] || NOTE_ICON.info}</svg>
        <div class="note-body"><h3>${esc(n.title)}</h3>${n.body || ''}${n.report != null ? reportSlot(n.report) : ''}</div>
        <button class="note-x" type="button" data-act="dismiss" data-kind="${esc(n.kind)}" title="Dismiss" aria-label="Dismiss">
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg></button>
      </div>`
    )
    .join('');
  paintReports();
}

// the background's lines (health.rs, the "health" event): they come and go with what is failing
function renderHealth(h) {
  S.health = h;
  R.waiting = h.reports_waiting || 0;
  const now = new Set();
  for (const l of h.lines || []) {
    const kind = `health:${l.kind}`;
    now.add(kind);
    notify({ kind, tone: l.tone, title: l.text, report: l.report ? l.text : null, since: l.since, at: l.since * 1000, keep: true });
  }
  for (const k of [...Inbox.items.keys()]) if (k.startsWith('health:') && !now.has(k)) Inbox.items.delete(k);
  renderInbox();
  renderShared(S.shared);
  paintReport($('report-foot'));
}

function changesHtml(changes, failed) {
  const several = new Set(changes.map((c) => c.install)).size > 1;
  const rows = changes
    .map(
      (c) =>
        `<li>${esc(c.name)}&ensp;${c.from ? `<span class="from">${esc(c.from)}</span> → ` : ''}<span class="to">${esc(c.to)}</span>${
          several ? `&ensp;<span class="where">${esc(c.install)}</span>` : ''
        }</li>`
    )
    .concat(
      failed.map((f) =>
        typeof f === 'string'
          ? `<li>${esc(f)}</li>`
          : `<li><strong>${esc(f.name)}</strong>: ${esc(f.message)}${f.conflict?.length ? ` ${replaceButton(f.id, f.conflict)}` : ''}</li>`
      )
    )
    .join('');
  return (rows ? `<ul>${rows}</ul>` : '') + reloadLine(changes);
}

// what an action the player started did (`kind`: the automatic updates have their own, so neither hides the other)
function showChanges(title, changes, failed = [], kind = 'action') {
  notify({
    kind,
    tone: failed.length ? 'bad' : 'ok',
    title,
    body: changesHtml(changes, failed),
    report: failed.length ? `${title}: ${failed.map((f) => (typeof f === 'string' ? f : `${f.name}: ${f.message}`)).join('; ')}` : null,
  });
}

function showError(text) {
  notify({ kind: 'action', tone: 'bad', title: text, report: text });
  invoke('note_error', { source: 'window', message: text }).catch(() => {});
}

// an install that failed: its sentence, and "Replace (backup kept)" when folders are in the way
function showFailure(e, id) {
  if (!(e && typeof e === 'object' && e.conflict?.length && id)) return showError(errText(e));
  notify({ kind: 'action', tone: 'bad', title: errText(e), body: `<p>${replaceButton(id, e.conflict)}</p>`, report: errText(e) });
}

// ------------------------------------------------------------------------------------------------ actions

async function run(key, label, fn) {
  if (S.busy.has(key)) return;
  S.busy.set(key, label);
  renderList();
  try {
    await fn();
  } catch (e) {
    showFailure(e, key === '*' ? null : key);
  } finally {
    S.busy.delete(key);
    S.progress.delete(key);
    forgetIcons(key);
    if (key === '*') S.progress.clear();
    await loadItems();
  }
}

function doInstall(id, take, extra = []) {
  const it = S.items.find((x) => x.key === id);
  const takeOver = [...new Set([...(take && it ? it.folders : []), ...extra])];
  const updating = !!it;
  return run(id, updating ? 'Updating…' : 'Installing…', async () => {
    const c = await invoke('install_addon', { install: S.sel, id, takeOver });
    showChanges(c.from ? `Updated ${c.name}` : `Installed ${c.name}`, [c]);
  });
}

async function onAction(el) {
  const act = el.dataset.act;
  switch (act) {
    case 'toggle': {
      const k = el.dataset.key;
      S.open.has(k) ? S.open.delete(k) : S.open.add(k);
      S.confirm = null;
      renderList();
      document.querySelector(`[data-act="toggle"][data-key="${CSS.escape(k)}"]`)?.focus();
      break;
    }
    case 'install':
      return doInstall(el.dataset.id, el.dataset.take === '1');
    case 'replace': {
      unnotify('action');
      const it = S.items.find((x) => x.key === el.dataset.id);
      let folders = [];
      try {
        folders = JSON.parse(el.dataset.folders || '[]');
      } catch {}
      return doInstall(el.dataset.id, !!it && !it.managed, folders);
    }
    case 'update-all':
      return run('*', 'Checking…', async () => {
        // the newest versions first, so one click never misses a release from the last hour
        await loadCatalog(true);
        const r = await invoke('update_all', { install: S.sel });
        // nothing new: the updates bar says so itself ("… is up to date · checked just now"), no second notice
        if (!r.changes.length && !r.failed.length) {
          S.allChecked = Date.now();
          return;
        }
        showChanges(r.changes.length ? `Updated ${plural(r.changes.length, 'addon', 'addons')}` : 'Nothing was updated', r.changes, r.failed);
      });
    case 'freeze': {
      const on = !!el.dataset.on;
      return run(el.dataset.id, on ? 'Freezing…' : 'Unfreezing…', () => invoke('set_frozen', { install: S.sel, id: el.dataset.id, frozen: on }));
    }
    case 'rollback':
      return run(el.dataset.id, 'Going back…', async () => {
        const c = await invoke('rollback_addon', { install: S.sel, id: el.dataset.id });
        showChanges(`${c.name} is back to ${c.to}`, [c]);
      });
    case 'ask-remove':
      return askRemove(el.dataset.id);
    case 'cancel': {
      const k = S.confirm;
      S.confirm = null;
      S.plan = null;
      renderList();
      if (k) document.querySelector(`[data-act="ask-remove"][data-id="${CSS.escape(k)}"]`)?.focus();
      break;
    }
    case 'remove': {
      const id = el.dataset.id;
      const folders = S.plan?.key === id ? S.plan.folders : [];
      return run(id, 'Removing…', async () => {
        const name = await invoke('remove_addon', { install: S.sel, id, folders });
        S.confirm = null;
        S.plan = null;
        S.open.delete(id);
        showChanges(`Removed ${name} (in the Recycle Bin)`, []);
        // an addon from a GitHub link leaves the list with its last install
        await loadCatalog(false);
      });
    }
    case 'copy-name':
      return copyName(el.dataset.name);
    case 'url':
      if (el.dataset.url) invoke('open_url', { url: el.dataset.url }).catch((e) => showError(errText(e)));
      break;
    case 'open-folder':
      invoke('open_addons_folder', { install: S.sel, folder: el.dataset.folder || null }).catch((e) => showError(errText(e)));
      break;
    case 'add-folder':
      return addFolder();
    case 'tab':
      setTab(el.dataset.tab);
      break;
    case 'other':
      S.showOther = !S.showOther;
      renderList();
      break;
    case 'refresh':
      return loadCatalog(true);
    case 'upload-now':
      return uploadNow();
    case 'copy-giveaway':
      return copyGiveaway();
    case 'gh-open':
      return openGh(el.dataset.link || '');
    case 'gh-close':
      $('gh').close();
      break;
    case 'gh-install':
      return ghInstall(el.dataset.id);
    case 'dismiss':
      dismissNote(el.dataset.kind);
      break;
    case 'report-send':
      return sendReport(el.dataset.shown || '');
    case 'report-copy':
      return copyReportId(el.dataset.id);
    case 'report-show':
      invoke('show_report_file', { path: el.dataset.file }).catch((e) => toast(errText(e)));
      break;
    case 'report-text':
      invoke('report_text', { what: R.what, shown: el.dataset.shown || '', after: el.dataset.after === '1' }).catch((e) => toast(errText(e)));
      break;
    case 'ours-install':
      return installOurs(el.dataset.where);
    case 'ours-later':
      // for good: it never comes back (Get addons, EverlookDB addons has the same in one click)
      S.ours.card = false;
      S.ours.hidden = true;
      invoke('set_setting', { name: 'ours_card_dismissed', value: true }).catch((e) => showError(errText(e)));
      renderList();
      break;
    case 'ours-close':
      S.ours.card = false;
      S.ours.hidden = true;
      renderList();
      break;
    case 'ours-filter':
      S.oursOnly = !S.oursOnly;
      renderList();
      break;
    case 'list-menu':
      return showListMenu($('list-menu').hidden);
    case 'copy-list':
      showListMenu(false);
      return copyList();
    case 'save-list':
      showListMenu(false);
      return saveList();
  }
}

// ------------------------------------------------------------------------------------------------ addon list

function showListMenu(open) {
  $('list-menu').hidden = !open;
  $('list-menu-button').setAttribute('aria-expanded', String(open));
  if (open) $('list-menu').querySelector('[role="menuitem"]').focus();
}

let copiedTimer = 0;
async function copyList() {
  try {
    const n = await invoke('copy_addon_list', { install: S.sel });
    const said = `Copied ${plural(n, 'addon', 'addons')} to the clipboard`;
    // a short label, so the toolbar doesn't move; the whole sentence for the tooltip and screen readers
    const b = $('copy-list');
    $('copy-label').textContent = 'Copied';
    b.classList.add('done');
    b.dataset.title ??= b.title;
    b.title = said;
    $('announce').textContent = said;
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => {
      $('copy-label').textContent = 'Copy list';
      b.classList.remove('done');
      b.title = b.dataset.title;
    }, 2400);
  } catch (e) {
    showError(`Couldn’t copy the addon list: ${e}`);
  }
}

async function saveList() {
  try {
    const path = await invoke('save_addon_list', { install: S.sel });
    if (path) showChanges(`Saved the addon list as ${path.split(/[\\/]/).pop()}`, []);
  } catch (e) {
    showError(errText(e));
  }
}

async function addFolder() {
  try {
    const added = await invoke('add_folder');
    if (added) await loadState().then(loadItems);
  } catch (e) {
    showError(errText(e));
  }
}

function setTab(tab, focus = true) {
  S.tab = tab;
  for (const [id, t] of [['tab-mine', 'mine'], ['tab-browse', 'browse']]) {
    const b = $(id);
    b.setAttribute('aria-selected', String(t === tab));
    b.tabIndex = t === tab ? 0 : -1;
  }
  $('q').placeholder = tab === 'mine' ? 'Search my addons' : `Search ${S.catalog ? plural(S.catalog.addons.length, 'addon', 'addons') : 'addons'}`;
  renderList();
  if (focus) $('q').focus();
}

async function selectInstall(id) {
  if (id === S.sel) return;
  S.sel = id;
  S.open.clear();
  S.icons.clear();
  S.iconWait.clear();
  S.confirm = null;
  S.plan = null;
  renderRail();
  renderHero();
  invoke('set_setting', { name: 'selected', value: id });
  await loadItems();
}

// ------------------------------------------------------------------------------------------------ uninstall

// ask Rust what would go (and what stays), then show the confirmation in the addon's row, opened
async function askRemove(key) {
  hideCtx();
  let plan;
  try {
    plan = await invoke('uninstall_plan', { install: S.sel, id: key });
  } catch (e) {
    return showError(errText(e));
  }
  S.plan = plan;
  S.confirm = key;
  S.open.add(key);
  if (S.tab !== 'mine') setTab('mine', false);
  else renderList();
  const b = document.querySelector(`[data-act="remove"][data-id="${CSS.escape(key)}"]`);
  b?.closest('.row')?.scrollIntoView({ block: 'nearest' });
  b?.focus();
}

// ------------------------------------------------------------------------------------------------ right-click menu

// line icons, like the toolbar's
const ICON = {
  update: '<path d="M8 2.5v7.5M4.5 6.5L8 10l3.5-3.5"/><path d="M3 12.5h10"/>',
  install: '<path d="M8 2.5v7.5M4.5 6.5L8 10l3.5-3.5"/><path d="M3 12.5h10"/>',
  installed: '<path d="M3.5 8.5l3 3 6-7"/>',
  folder: '<path d="M2.5 4.5a1 1 0 0 1 1-1h3l1.5 1.5h4.5a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1z"/>',
  link: '<path d="M9 3.5h3.5V7M12.5 3.5L7 9"/><path d="M11 9.5v2.5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h2.5"/>',
  copy: '<rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 3.5v-.5a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 3v5A1.5 1.5 0 0 0 4 9.5h.5"/>',
  freeze: FREEZE_PATH,
  trash: '<path d="M3 4.5h10M6.5 4.5V3a.5.5 0 0 1 .5-.5h2a.5.5 0 0 1 .5.5v1.5M4.5 4.5l.6 8a1 1 0 0 0 1 .9h3.8a1 1 0 0 0 1-.9l.6-8"/>',
};

// where an addon's link goes, said plainly
const sourceLabel = (url) =>
  /^https:\/\/github\.com\//i.test(url) ? 'View source on GitHub' : /tukui\.org/i.test(url) ? 'View on Tukui' : 'Open project page';

// the menu for the row with this key, on the tab shown: [{ label, icon, act, data, danger, off }] and '-' lines
function ctxItems(key) {
  const i = install();
  if (!i) return [];
  const out = [];
  const busy = S.busy.has(key) || S.busy.has('*');
  const links = (page, url) => {
    if (page) out.push({ label: 'About it on everlookdb.com', icon: 'link', act: 'url', data: { url: page } });
    if (url && url.startsWith('https://') && url !== page) out.push({ label: sourceLabel(url), icon: 'link', act: 'url', data: { url } });
  };
  if (S.tab === 'mine') {
    const it = S.items.find((x) => x.key === key);
    if (!it) return [];
    if (it.update && it.catalog && i.game) {
      out.push({ label: `Update to ${it.latest_label || it.latest}`, icon: 'update', act: 'install', data: { id: key, take: it.managed ? '' : '1' }, off: busy });
    }
    out.push({ label: 'Open folder', icon: 'folder', act: 'open-folder', data: { folder: it.main || '' } });
    links(it.ours && ourPage(key), it.url);
    out.push({ label: 'Copy name', icon: 'copy', act: 'copy-name', data: { name: it.name } });
    if (it.catalog) {
      out.push(
        it.frozen
          ? { label: 'Unfreeze', icon: 'freeze', act: 'freeze', data: { id: key, on: '' }, off: busy }
          : { label: 'Freeze (never update)', icon: 'freeze', act: 'freeze', data: { id: key, on: '1' }, off: busy },
      );
    }
    out.push('-');
    out.push({ label: 'Uninstall…', icon: 'trash', act: 'ask-remove', data: { id: key }, danger: true, off: busy });
    return out;
  }
  const a = (S.catalog?.addons || []).find((x) => x.id === key);
  if (!a) return [];
  const it = S.items.find((x) => x.catalog && x.key === a.id);
  if (i.game && fileFor(a, i.accepts)) {
    if (it && it.frozen) {
      out.push({ label: 'Frozen', icon: 'freeze', off: true });
    } else if (it && it.update) {
      out.push({ label: `Update to ${a.version_label || a.version}`, icon: 'update', act: 'install', data: { id: a.id, take: it.managed ? '' : '1' }, off: busy });
    } else if (it) {
      out.push({ label: 'Installed', icon: 'installed', off: true });
    } else {
      out.push({ label: 'Install', icon: 'install', act: 'install', data: { id: a.id }, off: busy });
    }
  }
  links(a.ours && ourPage(a.id), a.url);
  out.push({ label: 'Copy name', icon: 'copy', act: 'copy-name', data: { name: a.name } });
  return out;
}

const CTX = { key: null, back: null };

function openCtx(row, x, y, back) {
  const items = ctxItems(row.dataset.row);
  if (!items.length) return hideCtx();
  const menu = $('ctx');
  menu.innerHTML = items
    .map((m) => {
      if (m === '-') return '<div class="sep" role="separator"></div>';
      const data = Object.entries(m.data || {})
        .map(([k, v]) => ` data-${k}="${esc(v)}"`)
        .join('');
      return `<button type="button" role="menuitem" tabindex="-1"${m.act && !m.off ? ` data-act="${m.act}"${data}` : ''}${m.danger ? ' class="danger"' : ''}${
        m.off ? ' aria-disabled="true"' : ''
      }><svg viewBox="0 0 16 16" aria-hidden="true">${ICON[m.icon] || ''}</svg><span>${esc(m.label)}</span></button>`;
    })
    .join('');
  const name = row.querySelector('.row-name .t')?.textContent || '';
  menu.setAttribute('aria-label', name);
  document.querySelector('.row.ctx-on')?.classList.remove('ctx-on');
  row.classList.add('ctx-on');
  CTX.key = row.dataset.row;
  CTX.back = back || row.querySelector('.row-head');
  menu.hidden = false;
  // at the pointer, kept inside the window
  const w = menu.offsetWidth;
  const h = menu.offsetHeight;
  const left = Math.max(8, Math.min(x, innerWidth - w - 8));
  const top = y + h > innerHeight - 8 ? Math.max(8, y - h) : y;
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  ctxMove(menu.querySelector('[role="menuitem"]:not([aria-disabled])') ? 'first' : null);
}

function hideCtx(refocus = false) {
  const menu = $('ctx');
  if (menu.hidden) return;
  menu.hidden = true;
  document.querySelector('.row.ctx-on')?.classList.remove('ctx-on');
  if (refocus) {
    // the row may have been drawn again while the menu was open
    const head = CTX.back?.isConnected ? CTX.back : document.querySelector(`.row[data-row="${CSS.escape(CTX.key || '')}"] .row-head`);
    head?.focus();
  }
  CTX.key = null;
  CTX.back = null;
}

// arrow keys move between the items that can be chosen
function ctxMove(where) {
  if (!where) return;
  const items = [...$('ctx').querySelectorAll('[role="menuitem"]:not([aria-disabled])')];
  if (!items.length) return;
  const at = items.indexOf(document.activeElement);
  const n = items.length;
  const next = where === 'first' ? 0 : where === 'last' ? n - 1 : where === 'down' ? (at + 1) % n : (at - 1 + n) % n;
  items[next].focus();
}

$('ctx').addEventListener('keydown', (e) => {
  const move = { ArrowDown: 'down', ArrowUp: 'up', Home: 'first', End: 'last' }[e.key];
  if (move) {
    e.preventDefault();
    ctxMove(move);
  } else if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation();
    hideCtx(true);
  } else if (e.key === 'Tab') {
    e.preventDefault();
    hideCtx(true);
  }
});

// the browser's own menu (Back, Refresh, Print...) is no use in an app: only text fields keep it, for copy and paste
const TEXT_FIELD = 'textarea, input:not([type="checkbox"]):not([type="radio"]):not([type="button"]):not([type="submit"]), [contenteditable="true"]';
document.addEventListener('contextmenu', (e) => {
  if (e.target.closest(TEXT_FIELD)) return hideCtx();
  e.preventDefault();
  const row = e.target.closest('#list .row[data-row]');
  if (!row || $('gh').open) return hideCtx();
  // from the keyboard (menu key, Shift+F10) the menu opens under the focused row, not at the corner
  if (e.button !== 2 || (!e.clientX && !e.clientY)) {
    const r = (row.contains(document.activeElement) ? document.activeElement : row).getBoundingClientRect();
    return openCtx(row, r.left + 24, r.bottom - 4, document.activeElement);
  }
  openCtx(row, e.clientX, e.clientY);
});
document.addEventListener('pointerdown', (e) => {
  if (!e.target.closest('#ctx')) hideCtx();
});
document.addEventListener('scroll', () => hideCtx(), true);
window.addEventListener('resize', () => hideCtx());
window.addEventListener('blur', () => hideCtx());

// browser keys and mouse buttons that would reload, print or go "back" in the window; Rust turns them off too in
// release builds (main.rs app_keys_only)
document.addEventListener(
  'keydown',
  (e) => {
    const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    const ctrl = e.ctrlKey || e.metaKey;
    if (
      k === 'F5' ||
      k === 'BrowserBack' ||
      k === 'BrowserForward' ||
      k === 'BrowserRefresh' ||
      (ctrl && (k === 'r' || k === 'p')) ||
      (e.altKey && (k === 'ArrowLeft' || k === 'ArrowRight'))
    ) {
      e.preventDefault();
    }
  },
  true
);
for (const t of ['mousedown', 'mouseup', 'auxclick']) {
  window.addEventListener(t, (e) => {
    if (e.button === 3 || e.button === 4) e.preventDefault();
  });
}

// a short "Copied" at the bottom of the window
let toastTimer = 0;
function toast(text) {
  let t = $('toast');
  if (!t) {
    t = document.createElement('div');
    t.id = 'toast';
    t.className = 'toast';
    t.setAttribute('aria-hidden', 'true');
    document.body.append(t);
  }
  t.textContent = text;
  t.classList.add('on');
  $('announce').textContent = text;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('on'), 1800);
}

async function copyName(name) {
  hideCtx(true);
  try {
    await navigator.clipboard.writeText(name);
  } catch {
    const t = document.createElement('textarea');
    t.value = name;
    t.style.position = 'fixed';
    t.style.opacity = '0';
    document.body.append(t);
    t.select();
    document.execCommand('copy');
    t.remove();
  }
  toast(`Copied “${name}”`);
}

// ------------------------------------------------------------------------------------------------ problem reports

// "Send error logs to the developer" (src-tauri/src/report.rs): one click sends the app's log, its last errors and
// what it found, scrubbed, to everlookdb.com and shows the report's id. Every `.report` element is one: its
// data-shown is the error it sits next to ('' for the quiet "Report a problem" in the sidebar).
const R = { what: '', sending: new Set(), sent: new Map(), failed: new Map(), queued: new Set(), waiting: 0 };

// a .report placeholder next to an error
function reportSlot(shown) {
  return `<div class="report" data-shown="${esc(shown)}"></div>`;
}

function paintReport(el) {
  // never under the player's fingers: a box they are typing in is left as it is
  if (el.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
  const shown = el.dataset.shown || '';
  const foot = el.classList.contains('foot');
  const id = R.sent.get(shown);
  const fail = R.failed.get(shown);
  const busy = R.sending.has(shown);
  if (R.queued.has(shown)) {
    el.innerHTML = `<p class="report-sub">Saved: everlookdb.com can’t be reached right now, so the report goes by itself when it’s back. ${what}</p>`;
    return;
  }
  const what = `<button class="text-button quiet" type="button" data-act="report-text" data-shown="${esc(shown)}">What gets sent</button>`;
  if (id) {
    el.innerHTML = `<p class="report-done"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-7"/></svg>Sent · report <b>${esc(id)}</b>
      <button class="link-button" type="button" data-act="report-copy" data-id="${esc(id)}">Copy id</button></p>
      <p class="report-sub">Quote ${esc(id)} if you write to us. ${what.replace('What gets sent', 'What was sent').replace('data-act="report-text"', 'data-act="report-text" data-after="1"')}</p>`;
    return;
  }
  const button = foot
    ? `<button class="text-button quiet report-go" type="button" data-act="report-send" data-shown=""${busy ? ' disabled' : ''}>${busy ? 'Sending…' : 'Report a problem'}</button>`
    : `<button class="btn report-go" type="button" data-act="report-send" data-shown="${esc(shown)}"${busy ? ' disabled aria-busy="true" data-busy="Sending…"' : ''}>Send error logs to the developer</button>`;
  // the sidebar's entry stays one quiet line: no box there
  const box = foot ? '' : `<input class="report-what" type="text" maxlength="300" placeholder="What happened? (optional)" aria-label="What happened? (optional)" value="${esc(R.what)}">`;
  el.innerHTML = `<div class="report-row">${button}${box}${foot ? what : ''}</div>
    ${fail ? `<p class="report-fail">Couldn’t send it: ${esc(String(fail.error).replace(/\.$/, ''))}.${fail.file ? ` A copy is saved. <button class="link-button" type="button" data-act="report-show" data-file="${esc(fail.file)}">Show file</button>` : ''}</p>` : ''}
    ${foot ? (R.waiting ? `<p class="report-sub">${plural(R.waiting, 'report waits', 'reports wait')} for everlookdb.com and ${R.waiting === 1 ? 'goes' : 'go'} by itself.</p>` : '') : `<p class="report-sub">${what}</p>`}`;
}

function paintReports() {
  document.querySelectorAll('.report').forEach((el) => {
    if (!el.hidden) paintReport(el);
  });
}

async function sendReport(shown) {
  if (R.sending.has(shown)) return;
  R.sending.add(shown);
  R.failed.delete(shown);
  paintReports();
  try {
    const r = await invoke('send_report', { what: R.what, shown });
    if (r.id) {
      R.sent.set(shown, r.id);
      R.what = '';
      $('announce').textContent = `Sent: report ${r.id}`;
    } else if (r.queued) {
      R.queued.add(shown);
      R.what = '';
    } else {
      R.failed.set(shown, { error: r.error || 'it didn’t go through', file: r.file });
    }
  } catch (e) {
    R.failed.set(shown, { error: errText(e), file: null });
  } finally {
    R.sending.delete(shown);
    paintReports();
  }
}

async function copyReportId(id) {
  try {
    await invoke('copy_report_id', { id });
    toast(`Copied report id ${id}`);
  } catch (e) {
    toast(`Couldn’t copy: ${errText(e)}`);
  }
}

// keep what was typed, in every box (re-renders put it back)
document.addEventListener('input', (e) => {
  if (e.target.classList?.contains('report-what')) R.what = e.target.value;
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && e.target.classList?.contains('report-what')) {
    const box = e.target.closest('.report');
    if (box) sendReport(box.dataset.shown || '');
  }
});
// a box the player stopped typing in catches up with what happened meanwhile
document.addEventListener('focusout', (e) => {
  if (e.target.classList?.contains('report-what')) setTimeout(paintReports, 0);
});

// ------------------------------------------------------------------------------------------------ wiring

document.addEventListener('click', (e) => {
  if (!$('ctx').hidden) hideCtx(!!e.target.closest('#ctx') && !e.target.closest('[data-act]'));
  if (!$('list-menu').hidden && !e.target.closest('.list-tools')) showListMenu(false);
  const game = e.target.closest('[data-install]');
  if (game) return selectInstall(game.dataset.install);
  const el = e.target.closest('[data-act]');
  if (el && !el.disabled) onAction(el);
});

// the welcome card's ticks: the gold button counts what is ticked
$('list').addEventListener('change', (e) => {
  const id = e.target.dataset?.oursTick;
  if (!id) return;
  e.target.checked ? S.ours.ticked.add(id) : S.ours.ticked.delete(id);
  renderList();
  document.querySelector(`[data-ours-tick="${CSS.escape(id)}"]`)?.focus();
});

$('add-folder').addEventListener('click', addFolder);
$('tab-mine').addEventListener('click', () => setTab('mine', false));
$('tab-browse').addEventListener('click', () => setTab('browse', false));
document.querySelector('.tabs').addEventListener('keydown', (e) => {
  if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
    setTab(S.tab === 'mine' ? 'browse' : 'mine', false);
    $(S.tab === 'mine' ? 'tab-mine' : 'tab-browse').focus();
  }
});

$('q').addEventListener('input', (e) => {
  S.q = e.target.value;
  renderList();
});
$('q').addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && S.q) {
    e.preventDefault();
    e.target.value = '';
    S.q = '';
    renderList();
  } else if (e.key === 'Enter' && S.tab === 'mine' && S.q && !$('list').querySelector('.row')) {
    setTab('browse');
  }
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('ctx').hidden) {
    hideCtx(true);
    return;
  }
  if (e.key === 'Escape' && !$('list-menu').hidden) {
    showListMenu(false);
    $('list-menu-button').focus();
    return;
  }
  const typing = e.target.matches('input[type="search"], input[type="text"]');
  if ((e.key === '/' && !typing) || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f')) {
    e.preventDefault();
    $('q').focus();
    $('q').select();
  }
});

$('sw-auto').addEventListener('change', (e) => invoke('set_setting', { name: 'auto_update', value: e.target.checked }));

$('sw-app-auto').addEventListener('change', (e) => invoke('set_setting', { name: 'app_auto_update', value: e.target.checked }));
$('sw-start').addEventListener('change', (e) => invoke('set_setting', { name: 'autostart', value: e.target.checked }));
$('sw-shared').addEventListener('change', (e) => {
  invoke('set_setting', { name: 'shared_prices', value: e.target.checked });
  renderShared({ ...(S.shared || {}), on: e.target.checked, houses: e.target.checked ? (S.shared || {}).houses || [] : [], note: null });
});
$('sw-uploads').addEventListener('change', (e) => invoke('set_setting', { name: 'paused', value: !e.target.checked }));
$('upload-now').addEventListener('click', uploadNow);

// ------------------------------------------------------------------------------------------------ install from GitHub

const GH = { preview: null, error: '', looking: false };
const FLAVOR_NAMES = { retail: 'Retail', classic: 'Classic', classic_era: 'Classic Era', forever: 'WoW Forever', ptr: 'PTR', beta: 'Beta' };
const andList = (xs) => (xs.length < 2 ? xs.join('') : `${xs.slice(0, -1).join(', ')} and ${xs[xs.length - 1]}`);
const size = (n) => (n >= 1048576 ? mb(n) : `${Math.max(1, Math.round(n / 1024))} KB`);
const day = (iso) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
};

function openGh(link) {
  GH.preview = null;
  GH.error = '';
  $('gh-link').value = link;
  renderGh();
  if (!$('gh').open) $('gh').showModal();
  $('gh-link').focus();
  if (link) ghLookup();
}

async function ghLookup() {
  const link = $('gh-link').value.trim();
  if (GH.looking) return;
  GH.preview = null;
  GH.error = link ? '' : 'Paste the addon’s GitHub link first, like https://github.com/owner/addon.';
  if (!link) return renderGh();
  GH.looking = true;
  renderGh();
  try {
    GH.preview = await invoke('github_lookup', { install: S.sel, link });
  } catch (e) {
    GH.error = errText(e);
  } finally {
    GH.looking = false;
    renderGh();
  }
}

async function ghInstall(id) {
  const p = GH.preview;
  if (!p || S.busy.has(id)) return;
  S.busy.set(id, p.installed ? 'Updating…' : 'Installing…');
  GH.error = '';
  renderGh();
  try {
    const c = await invoke('install_from_github', { install: S.sel, id, takeOver: p.existing });
    $('gh').close();
    showChanges(c.from ? `Updated ${c.name}` : `Installed ${c.name}`, [c]);
    await loadCatalog(false);
  } catch (e) {
    GH.error = errText(e);
  } finally {
    S.busy.delete(id);
    S.progress.delete(id);
    forgetIcons(id);
    if ($('gh').open) renderGh();
    await loadItems();
  }
}

// one gold button at a time: "Look up" until there is something to install, then the install button
function renderGh() {
  const p = GH.preview;
  const i = install();
  const look = $('gh-look');
  look.className = p ? 'btn' : 'btn solid';
  look.disabled = GH.looking;
  look.toggleAttribute('aria-busy', GH.looking);
  if (GH.looking) look.dataset.busy = 'Looking up…';
  $('gh-error').hidden = !GH.error;
  $('gh-error').textContent = GH.error;
  const ghr = $('gh-report');
  if (ghr.hidden !== !GH.error || ghr.dataset.shown !== `Install from GitHub: ${GH.error}`) {
    ghr.hidden = !GH.error;
    ghr.dataset.shown = `Install from GitHub: ${GH.error}`;
    if (GH.error) paintReport(ghr);
  }
  const box = $('gh-preview');
  if (!p || !i) {
    box.innerHTML = '';
    return;
  }
  const a = p.addon;
  const flavors = [...new Set((a.files || []).flatMap((f) => f.flavors || []))].map((f) => FLAVOR_NAMES[f] || f);
  const notes = [];
  let button = '';
  const label = a.version_label || a.version;
  if (p.in_catalog) notes.push(['', 'It’s in the EverlookDB addon list, so it installs and updates from there.']);
  if (!i.game) {
    notes.push(['bad', NO_GAME]);
  } else if (p.fit === 'no') {
    notes.push(['bad', `This release is for ${andList(flavors)}, not for ${i.name}. There’s nothing to install here.`]);
  } else if (p.installed && p.installed === a.version) {
    notes.push(['ok', `Already installed (${p.installed}). EverlookDB keeps it updated.`]);
  } else {
    if (p.fit === 'yes') notes.push(['ok', `Made for ${i.name}.`]);
    else notes.push(['warn', `The release doesn’t say which game versions it’s for, so EverlookDB can’t tell whether it works in ${i.name}. You can install it anyway.`]);
    if (p.existing.length) {
      notes.push([
        'warn',
        `${andList(p.existing)} ${p.existing.length === 1 ? 'is' : 'are'} already in your AddOns folder and EverlookDB didn’t install ${p.existing.length === 1 ? 'it' : 'them'}. Installing replaces ${p.existing.length === 1 ? 'it' : 'them'}; the copies there now are kept in EverlookDB’s backups folder.`,
      ]);
    }
    const verb = p.installed ? `Update to ${label}` : p.existing.length ? 'Replace and install' : p.fit === 'unknown' ? 'Install anyway' : 'Install';
    button = `<button class="btn solid" type="button" data-act="gh-install" data-id="${esc(a.id)}" ${busyAttrs(a.id)}>${esc(verb)}</button>`;
  }
  const released = day(a.released);
  const where = String(a.url || '').replace(/^https:\/\//, '');
  box.innerHTML = `<article class="ghp">
    <div class="ghp-head">
      ${iconHtml(a.id, a.name)}
      <div class="ghp-title">
        <h3><span class="t">${esc(a.name)}</span>${a.custom ? ghMark(true) : oursMark(a.ours)}</h3>
        <p>${a.author ? `by <span class="author">${esc(a.author)}</span>` : ''}</p>
      </div>
    </div>
    ${a.summary ? `<p class="ghp-sum">${esc(a.summary)}</p>` : ''}
    <dl class="ghp-facts">
      <dt>Version</dt><dd><span title="${esc(a.version)}">${esc(label)}</span>${released ? `<span class="soft">released ${esc(released)}</span>` : ''}</dd>
      <dt>${(a.folders || []).length === 1 ? 'Folder' : 'Folders'}</dt><dd>${(a.folders || []).length ? a.folders.map(esc).join(', ') : '<span class="soft">listed after unpacking</span>'}</dd>
      ${p.file ? `<dt>File</dt><dd>${esc(p.file.name)}${p.file.size ? `<span class="soft">${esc(size(p.file.size))}</span>` : ''}</dd>` : ''}
      ${flavors.length ? `<dt>For</dt><dd>${esc(andList(flavors))}</dd>` : ''}
      ${a.url && a.url.startsWith('https://') ? `<dt>Source</dt><dd><button class="link-button" type="button" data-act="url" data-url="${esc(a.url)}">${esc(where)}</button></dd>` : ''}
    </dl>
    ${notes.map(([k, t]) => `<p class="ghp-note ${k}">${k === 'ok' ? CHECK : ''}<span>${esc(t)}</span></p>`).join('')}
    ${button ? `<div class="ghp-actions">${button}</div>` : ''}
  </article>`;
  paintProgress();
  if (!S.icons.has(a.id) && !S.iconWait.has(a.id)) {
    S.iconWait.add(a.id);
    loadIcons([a.id]);
  }
}

$('gh-form').addEventListener('submit', (e) => {
  e.preventDefault();
  ghLookup();
});
// a click on the dimmed backdrop (outside the sheet) closes it, unless an install is running
$('gh').addEventListener('click', (e) => {
  if (e.target === $('gh') && !GH.looking && !(GH.preview && S.busy.has(GH.preview.addon.id))) $('gh').close();
});
$('gh').addEventListener('cancel', (e) => {
  if (GH.preview && S.busy.has(GH.preview.addon.id)) e.preventDefault();
});

listen('progress', (e) => {
  const p = e.payload;
  if (p.install !== S.sel) return;
  const before = updatingNow();
  S.progress.set(p.id, p);
  if (S.busy.has('*')) S.progress.set('*', p);
  // "Update all" moved on to the next addon: its row's button turns busy, the last one's back
  const run = S.ours.run;
  if (run && !run.done && run.ids.includes(p.id)) {
    run.index = p.index;
    if (S.busy.get(p.id) === 'Waiting…') S.busy.set(p.id, 'Installing…');
    paintOurs();
  }
  if (updatingNow() !== before) renderList();
  else paintProgress();
});
// one of our addons is done in an "Install all": its row says so at once
listen('ours', (e) => {
  const p = e.payload;
  const run = S.ours.run;
  if (p.install !== S.sel || !run || run.done) return;
  run.results.set(p.id, { change: p.change, failure: p.failure });
  if (p.change) {
    const a = S.ours.addons.find((x) => x.id === p.id);
    if (a) a.installed = true;
  }
  S.busy.delete(p.id);
  S.progress.delete(p.id);
  run.index = Math.min(run.count, run.results.size + 1);
  renderList();
});
listen('uploads', (e) => renderUploads(e.payload));
listen('shared', (e) => renderShared(e.payload));
listen('catalog', () => loadCatalog(false));
listen('settings', () => loadState());
listen('installs', () => loadState().then(loadItems));
// what updated by itself (failures aren't in it: health.rs shows those when they keep failing)
listen('updated', (e) => {
  const { changes } = e.payload;
  if (changes.length) showChanges(`Updated ${plural(changes.length, 'addon', 'addons')} automatically, ${when(new Date().toISOString())}`, changes, [], 'auto');
  loadItems();
});
listen('health', (e) => renderHealth(e.payload));
// a report that waited for the server went out
listen('report-sent', (e) => {
  const { shown, id } = e.payload;
  R.queued.delete(shown);
  R.sent.set(shown, id);
  R.waiting = Math.max(0, R.waiting - 1);
  paintReports();
  paintReport($('report-foot'));
});

// the player may have changed AddOns by hand while the window was in the background
let lastFocus = Date.now();
window.addEventListener('focus', () => {
  if (Date.now() - lastFocus > 5000) loadItems();
  lastFocus = Date.now();
});
window.addEventListener('blur', () => (lastFocus = Date.now()));

(async () => {
  await loadState();
  renderStatus();
  // the list from last time comes at once (the fresh one follows with a "catalog" event); it loads the addons too
  await loadCatalog(false);
  setTab('mine', false);
})();
