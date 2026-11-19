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
  renderList();
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
  showHours(S.app.update_hours);
  $('sw-start').checked = S.app.autostart;
  $('start-label').textContent = S.app.autostart_label;
  $('app-version').textContent = `EverlookDB ${S.app.version}`;
}

// each file's last answer: green taken, amber partly taken (or failed once), red refused or failing
const FILE_DOT = { ok: 'ok', partial: 'busy', retry: 'busy', failed: 'error' };

function renderUploads(u) {
  if (!u) return;
  renderGiveaway(u.giveaway);
  $('upload-text').textContent = u.paused ? 'Paused' : u.line;
  $('upload-dot').className = 'dot ' + (u.paused ? '' : u.kind);
  $('sw-uploads').checked = !u.paused;

  const files = u.files || [];
  const list = $('upload-files');
  list.hidden = !files.length;
  // the rows say it all; the one summary line is for when there are none (or uploads are paused)
  $('upload-text').parentElement.hidden = files.length > 0 && !u.paused;
  list.innerHTML = files
    .map(
      (f) => `<li class="ufile ${esc(f.level)}">
        <span class="dot ${FILE_DOT[f.level] || ''}"></span>
        <div class="ufile-body">
          <p class="ufile-head"><span class="ufile-name">${esc(f.label)}</span>${f.when ? `<span class="ufile-when">${esc(f.when)}</span>` : ''}</p>
          <p class="ufile-say">${esc(f.headline)}</p>
          ${f.details.length ? `<ul class="ufile-why">${f.details.map((d) => `<li>${esc(d)}</li>`).join('')}</ul>` : ''}
          ${f.next && !u.paused ? `<p class="ufile-next">${esc(f.next)}</p>` : ''}
        </div>
      </li>`
    )
    .join('');

  // refused or failing again and again: said at the top of the window too, until it goes through
  const bad = files.filter((f) => f.level === 'failed');
  const alert = $('upload-alert');
  alert.hidden = !bad.length;
  if (!bad.length) return;
  alert.innerHTML = `<h3>${esc(bad.length === 1 ? `${bad[0].label} isn't being uploaded` : `${bad.map((f) => f.label).join(', ')} aren't being uploaded`)}</h3>
    <ul>${bad
      .map(
        (f) =>
          `<li>${bad.length > 1 ? `<span class="where">${esc(f.label)}:</span> ` : ''}${esc(f.headline)}${
            f.next && !u.paused ? ` <span class="where">· ${esc(f.next)}</span>` : ''
          }</li>${f.details.map((d) => `<li class="where">${esc(d)}</li>`).join('')}`
      )
      .join('')}</ul>
    <button class="link-button close" type="button" data-act="upload-now">Try now</button>`;
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
  if (!S.items.length) {
    return `<div class="empty"><h3>No addons in ${esc(i.name)} yet</h3>
      <p>Find boss mods, bag addons, unit frames and more, and install them with one click.</p>
      <button class="btn gold" type="button" data-act="tab" data-tab="browse">Get addons</button></div>`;
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
      ? `<div class="updates-bar"><p>${updates.length ? `${plural(updates.length, 'update', 'updates')} ready${handNote}` : 'Everything is up to date'}</p>
         <button class="btn solid" type="button" data-act="update-all" ${busyAttrs('*')} title="Checks every addon for a new version, then updates all that have one">Update all</button></div>`
      : '';
  return bar + `<ul class="rows">${items.map((it) => mineRow(it, i)).join('')}</ul>`;
}

// our own addons' pages on everlookdb.com, by GitHub repository name
const OUR_PAGES = {
  everlookdb: 'https://everlookdb.com/contribute.html',
  everlookdbauctions: 'https://everlookdb.com/auctions-addon.html',
  lattice: 'https://everlookdb.com/lattice.html',
  latticepartyraidframes: 'https://everlookdb.com/lattice.html',
  whack: 'https://everlookdb.com/whack.html',
  craftcolors: 'https://everlookdb.com/craftcolors.html',
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
  if (!i.game && it.update && it.catalog) {
    action = noGame();
  } else if (it.update && it.catalog && it.conflict?.length) {
    action = `${replaceButton(key, it.conflict)}<div class="released" title="${esc(it.conflict.join(', '))} ${it.conflict.length === 1 ? 'is' : 'are'} in AddOns from another app or put there by hand">${esc(it.conflict.join(', '))} in the way</div>`;
  } else if (it.update && it.catalog) {
    const hand = it.auto ? '' : ` title="EverlookDB can’t tell whether ${esc(it.latest_label || it.latest)} is newer than your ${esc(it.version_label || it.version || '')}, so it only replaces it when you click"`;
    action = `<button class="btn" type="button" data-act="install" data-id="${esc(key)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(key)}${hand}>Update to ${esc(it.latest_label || it.latest)}</button>${releasedLine(it.latest_released)}`;
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
  if (it.managed && it.previous && i.game) {
    actions.push(`<button class="btn" type="button" data-act="rollback" data-id="${esc(key)}" ${busyAttrs(key)}>Go back to ${esc(it.previous)}</button>`);
  }
  if (it.catalog && !it.managed && it.latest && !it.update && i.game) {
    actions.push(`<button class="btn" type="button" data-act="install" data-id="${esc(key)}" data-take="1" ${busyAttrs(key)} title="Reinstalls it from its source; the copy there now is kept in EverlookDB's backups folder">Keep it updated with EverlookDB</button>`);
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
    <p>${who}</p>
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
    return `<div class="empty"><h3>The addon list didn’t load</h3>
      <p>EverlookDB couldn’t reach everlookdb.com. Check your connection and try again.</p>
      <button class="btn" type="button" data-act="refresh">Try again</button></div>`;
  }
  const q = S.q.trim();
  const installed = new Map(S.items.filter((it) => it.catalog).map((it) => [it.key, it]));
  let all = S.catalog.addons.map((a) => ({ a, s: Search.score(S.catIndex.get(a.id), q), runs: fileFor(a, i.accepts) }));
  all = all.filter((x) => x.s > 0);
  all.sort((x, y) => (q ? y.s - x.s : 0) || (y.a.ours - x.a.ours) || x.a.name.localeCompare(y.a.name));
  const here = all.filter((x) => x.runs);
  const other = all.filter((x) => !x.runs);
  const LIMIT = 200;
  let html = '';
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
  } else if (it && it.update) {
    action = it.conflict?.length
      ? replaceButton(a.id, it.conflict)
      : `<button class="btn" type="button" data-act="install" data-id="${esc(a.id)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(a.id)}>Update to ${esc(a.version_label || a.version)}</button>${releasedLine(a.released)}`;
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

// under an Update button: when that version came out
const releasedLine = (iso) => (when(iso) ? `<div class="released" title="${esc(exact(iso))}">released ${esc(when(iso))}</div>` : '');

// the automatic check, for the footer: when it last ran, what it did, and when it runs next
function autoLine() {
  const a = S.auto;
  const parts = [];
  if (a) {
    let did = a.off ? 'checked only (updating by itself is off)' : a.failed ? `${a.updated} updated, ${a.failed} failed` : a.updated ? `updated ${plural(a.updated, 'addon', 'addons')}` : a.waiting ? '' : 'nothing to update';
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
      cache: 'Offline: using the addon list from last time',
      sample: 'Sample addon list: the everlookdb.com catalog isn’t live yet',
      outdated: 'Update the app: the addon list has a newer format than this version reads',
      none: 'The addon list couldn’t load',
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

function showChanges(title, changes, failed = []) {
  const n = $('notice');
  n.className = 'notice' + (failed.length && !changes.length ? ' bad' : '');
  const several = new Set(changes.map((c) => c.install)).size > 1;
  n.innerHTML = `<h3>${esc(title)}</h3>
    <ul>${changes
      .map(
        (c) =>
          `<li>${esc(c.name)}&ensp;${c.from ? `<span class="from">${esc(c.from)}</span> → ` : ''}<span class="to">${esc(c.to)}</span>${
            several ? `&ensp;<span class="where">${esc(c.install)}</span>` : ''
          }</li>`
      )
      .join('')}${failed
      .map((f) =>
        typeof f === 'string'
          ? `<li>${esc(f)}</li>`
          : `<li><strong>${esc(f.name)}</strong>: ${esc(f.message)}${f.conflict?.length ? ` ${replaceButton(f.id, f.conflict)}` : ''}</li>`
      )
      .join('')}</ul>
    ${reloadLine(changes)}
    <button class="link-button close" type="button" data-act="dismiss">Dismiss</button>`;
  n.hidden = false;
}

function showError(text) {
  const n = $('notice');
  n.className = 'notice bad';
  n.innerHTML = `<h3>${esc(text)}</h3><button class="link-button close" type="button" data-act="dismiss">Dismiss</button>`;
  n.hidden = false;
}

// an install that failed: its sentence, and "Replace (backup kept)" when folders are in the way
function showFailure(e, id) {
  if (!(e && typeof e === 'object' && e.conflict?.length && id)) return showError(errText(e));
  const n = $('notice');
  n.className = 'notice bad';
  n.innerHTML = `<h3>${esc(errText(e))}</h3><p>${replaceButton(id, e.conflict)}</p><button class="link-button close" type="button" data-act="dismiss">Dismiss</button>`;
  n.hidden = false;
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
      $('notice').hidden = true;
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
        showChanges(
          r.changes.length ? `Updated ${plural(r.changes.length, 'addon', 'addons')}` : r.failed.length ? 'Nothing was updated' : 'Everything is up to date',
          r.changes,
          r.failed,
        );
      });
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
      invoke('open_addons_folder', { install: S.sel }).catch((e) => showError(errText(e)));
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
      $('notice').hidden = true;
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
    out.push({ label: 'Open folder', icon: 'folder', act: 'open-folder', data: {} });
    links(it.ours && ourPage(key), it.url);
    out.push({ label: 'Copy name', icon: 'copy', act: 'copy-name', data: { name: it.name } });
    out.push('-');
    out.push({ label: 'Uninstall…', icon: 'trash', act: 'ask-remove', data: { id: key }, danger: true, off: busy });
    return out;
  }
  const a = (S.catalog?.addons || []).find((x) => x.id === key);
  if (!a) return [];
  const it = S.items.find((x) => x.catalog && x.key === a.id);
  if (i.game && fileFor(a, i.accepts)) {
    if (it && it.update) {
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

// ------------------------------------------------------------------------------------------------ wiring

document.addEventListener('click', (e) => {
  if (!$('ctx').hidden) hideCtx(!!e.target.closest('#ctx') && !e.target.closest('[data-act]'));
  if (!$('list-menu').hidden && !e.target.closest('.list-tools')) showListMenu(false);
  const game = e.target.closest('[data-install]');
  if (game) return selectInstall(game.dataset.install);
  const el = e.target.closest('[data-act]');
  if (el && !el.disabled) onAction(el);
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

// ------------------------------------------------------------------------------------------------ update interval

function clampHours(v) {
  const n = Math.round(Number(String(v).trim()));
  if (!Number.isFinite(n)) return null;
  return Math.min(S.app.max_hours, Math.max(S.app.min_hours, n));
}

function showHours(h) {
  if (document.activeElement !== $('hours')) $('hours').value = String(h);
  $('hours-unit').textContent = h === 1 ? 'hour' : 'hours';
  $('hours-less').disabled = h <= S.app.min_hours;
  $('hours-more').disabled = h >= S.app.max_hours;
}

// typed text that isn't a number goes back to the setting; anything else is kept in range and saved
function setHours(v) {
  const h = clampHours(v) ?? S.app.update_hours;
  $('hours').value = String(h);
  showHours(h);
  if (h === S.app.update_hours) return;
  S.app.update_hours = h;
  invoke('set_setting', { name: 'update_hours', value: h }).catch((e) => showError(errText(e)));
}

$('hours-less').addEventListener('click', () => setHours(S.app.update_hours - 1));
$('hours-more').addEventListener('click', () => setHours(S.app.update_hours + 1));
$('hours').addEventListener('blur', (e) => setHours(e.target.value));
$('hours').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    setHours(e.target.value);
    e.target.select();
  } else if (e.key === 'Escape') {
    e.target.value = String(S.app.update_hours);
    e.target.blur();
  } else if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
    e.preventDefault();
    setHours((clampHours(e.target.value) ?? S.app.update_hours) + (e.key === 'ArrowUp' ? 1 : -1));
    e.target.select();
  }
});
$('sw-start').addEventListener('change', (e) => invoke('set_setting', { name: 'autostart', value: e.target.checked }));
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
  if (updatingNow() !== before) renderList();
  else paintProgress();
});
listen('uploads', (e) => renderUploads(e.payload));
listen('catalog', () => loadCatalog(false));
listen('settings', () => loadState());
listen('installs', () => loadState().then(loadItems));
listen('updated', (e) => {
  const { changes, failed } = e.payload;
  const at = when(new Date().toISOString());
  showChanges(changes.length ? `Updated ${plural(changes.length, 'addon', 'addons')} by itself, ${at}` : `An automatic update didn’t work, ${at}`, changes, failed);
  loadItems();
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
