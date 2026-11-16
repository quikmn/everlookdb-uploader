// EverlookDB window. Rust does every file and network job; this file only asks it (Tauri commands) and draws.
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

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
    showError(String(e));
  }
  renderList();
}

async function loadCatalog(refresh) {
  if (refresh) renderStatus('Checking the addon list…');
  const r = await invoke('get_catalog', { refresh });
  S.catalog = r.catalog;
  S.origin = r.origin;
  S.checked = r.checked;
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

function renderUploads(u) {
  if (!u) return;
  $('upload-text').textContent = u.paused ? 'Paused' : u.line;
  $('upload-dot').className = 'dot ' + (u.paused ? '' : u.kind);
  $('sw-uploads').checked = !u.paused;
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

const fileFor = (a, accepts) => accepts.some((f) => (a.files || []).some((file) => (file.flavors || []).includes(f)));

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
  for (const el of $('list').querySelectorAll(`.ico[data-icon="${CSS.escape(key)}"]`)) {
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
  const updates = i.game ? S.items.filter((it) => it.update && it.managed) : [];
  const bar =
    updates.length && !q
      ? `<div class="updates-bar"><p>${plural(updates.length, 'update', 'updates')} ready</p>
         <button class="btn solid" type="button" data-act="update-all" ${busyAttrs('*')}>Update all</button></div>`
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

function mineRow(it, i) {
  const key = it.key;
  const open = S.open.has(key);
  let action;
  if (!i.game && it.update && it.catalog) {
    action = noGame();
  } else if (it.update && it.catalog) {
    action = `<button class="btn gold" type="button" data-act="install" data-id="${esc(key)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(key)}>Update to ${esc(it.latest_label || it.latest)}</button>`;
  } else if (it.catalog && it.latest) {
    action = `<span class="state ok">${CHECK}Up to date</span>`;
  } else if (it.catalog) {
    action = `<span class="state">Not for ${esc(i.name)}</span>`;
  } else {
    action = `<span class="state" title="Installed by you or another app. EverlookDB leaves it alone.">Not from our sources</span>`;
  }
  const meta = [it.author ? `<span class="author">${esc(it.author)}</span>` : '', it.notes ? esc(it.notes) : ''].filter(Boolean).join('&ensp;');
  return `<li class="row">
    <div class="row-main">
      ${iconHtml(key, it.name)}
      <button class="row-head" type="button" data-act="toggle" data-key="${esc(key)}" aria-expanded="${open}">
        <span class="row-name"><span class="t">${esc(it.name)}</span>${oursMark(it.ours)}</span>
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
  if (it.managed) {
    if (S.confirm === key) {
      actions.push(`<span class="confirm">Remove ${esc(it.name)} and its ${plural(it.folders.length, 'folder', 'folders')}?
        <button class="btn danger" type="button" data-act="remove" data-id="${esc(key)}" ${busyAttrs(key)}>Remove</button>
        <button class="btn" type="button" data-act="cancel">Cancel</button></span>`);
    } else {
      actions.push(`<button class="btn danger" type="button" data-act="ask-remove" data-id="${esc(key)}">Remove</button>`);
    }
  }
  const who = it.managed
    ? 'Installed by EverlookDB.'
    : it.catalog
      ? 'Installed by you or another app. EverlookDB can update it; it only replaces these folders when you ask.'
      : 'Installed by you or another app, from a source EverlookDB doesn’t have. EverlookDB never changes it.';
  return `<div class="row-more">
    ${it.notes ? `<p>${esc(it.notes)}</p>` : ''}
    <p>${who}</p>
    <p class="folders">${plural(it.folders.length, 'folder', 'folders')}: ${it.folders.map(esc).join(', ')}</p>
    ${actions.length ? `<div class="actions">${actions.join('')}</div>` : ''}
  </div>`;
}

function browseHtml(i) {
  if (!S.catalog) {
    return `<div class="empty"><h3>The addon list didn’t load</h3>
      <p>EverlookDB couldn’t reach everlookdb.com. Check your connection and try again.</p>
      <button class="btn gold" type="button" data-act="refresh">Try again</button></div>`;
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
  if (!here.length && !other.length) {
    return `<div class="empty"><h3>No addon matches “${esc(q)}”</h3>
      <p>Try a shorter word, the author’s name, or what it does (“bags”, “boss”, “quest”).</p></div>`;
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
    action = `<button class="btn gold" type="button" data-act="install" data-id="${esc(a.id)}" data-take="${it.managed ? '' : '1'}" ${busyAttrs(a.id)}>Update to ${esc(a.version_label || a.version)}</button>`;
  } else if (it) {
    action = `<span class="state ok">${CHECK}Installed</span>`;
  } else {
    action = `<button class="btn gold" type="button" data-act="install" data-id="${esc(a.id)}" ${busyAttrs(a.id)}>Install</button>`;
  }
  const page = a.ours && ourPage(a.id);
  const where = page ? 'everlookdb.com' : a.source === 'tukui' ? 'Tukui' : a.source === 'github' ? 'GitHub' : a.source;
  return `<li class="row${runs ? '' : ' dim'}">
    <div class="row-main">
      ${iconHtml(a.id, a.name)}
      <button class="row-head" type="button" data-act="url" data-url="${esc(page || a.url)}" title="Open ${esc(a.name)} on ${esc(where)}">
        <span class="row-name"><span class="t">${esc(a.name)}</span>${oursMark(a.ours)}</span>
        <span class="row-meta"><span class="author">${esc(a.author)}</span>&ensp;${esc(a.summary)}</span>
      </button>
      ${versionCell(a.version, a.version_label, 'Latest version')}
      <div class="row-action">${action}</div>
    </div>
  </li>`;
}

function renderStatus(text) {
  const n = S.catalog?.addons.length || 0;
  const line =
    text ||
    {
      live: `Addon list checked at ${S.checked}`,
      cache: 'Offline: using the addon list from last time',
      sample: 'Sample addon list: the everlookdb.com catalog isn’t live yet',
      none: 'The addon list couldn’t load',
    }[S.origin] ||
    'Loading the addon list…';
  $('status').innerHTML = `<span>${esc(line)}${n ? `&ensp;${plural(n, 'addon', 'addons')} from GitHub and Tukui` : ''}</span>
    <button class="link-button" type="button" data-act="refresh">Check now</button>`;
}

// ------------------------------------------------------------------------------------------------ notices

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
      .join('')}${failed.map((f) => `<li>${esc(f)}</li>`).join('')}</ul>
    <button class="link-button close" type="button" data-act="dismiss">Dismiss</button>`;
  n.hidden = false;
}

function showError(text) {
  const n = $('notice');
  n.className = 'notice bad';
  n.innerHTML = `<h3>${esc(text)}</h3><button class="link-button close" type="button" data-act="dismiss">Dismiss</button>`;
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
    showError(String(e));
  } finally {
    S.busy.delete(key);
    S.progress.delete(key);
    forgetIcons(key);
    if (key === '*') S.progress.clear();
    await loadItems();
  }
}

function doInstall(id, take) {
  const it = S.items.find((x) => x.key === id);
  const takeOver = take && it ? it.folders : [];
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
    case 'update-all':
      return run('*', 'Updating…', async () => {
        const r = await invoke('update_all', { install: S.sel });
        if (r.changes.length || r.failed.length) {
          showChanges(r.changes.length ? `Updated ${plural(r.changes.length, 'addon', 'addons')}` : 'Nothing was updated', r.changes, r.failed);
        }
      });
    case 'rollback':
      return run(el.dataset.id, 'Going back…', async () => {
        const c = await invoke('rollback_addon', { install: S.sel, id: el.dataset.id });
        showChanges(`${c.name} is back to ${c.to}`, [c]);
      });
    case 'ask-remove':
      S.confirm = el.dataset.id;
      renderList();
      document.querySelector('[data-act="remove"]')?.focus();
      break;
    case 'cancel':
      S.confirm = null;
      renderList();
      break;
    case 'remove':
      return run(el.dataset.id, 'Removing…', async () => {
        const name = await invoke('remove_addon', { install: S.sel, id: el.dataset.id });
        S.confirm = null;
        S.open.delete(el.dataset.id);
        showChanges(`Removed ${name}`, []);
      });
    case 'url':
      if (el.dataset.url) invoke('open_url', { url: el.dataset.url }).catch((e) => showError(String(e)));
      break;
    case 'open-folder':
      invoke('open_addons_folder', { install: S.sel }).catch((e) => showError(String(e)));
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
    showError(String(e));
  }
}

async function addFolder() {
  try {
    const added = await invoke('add_folder');
    if (added) await loadState().then(loadItems);
  } catch (e) {
    showError(String(e));
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
  renderRail();
  renderHero();
  invoke('set_setting', { name: 'selected', value: id });
  await loadItems();
}

// ------------------------------------------------------------------------------------------------ wiring

document.addEventListener('click', (e) => {
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
  invoke('set_setting', { name: 'update_hours', value: h }).catch((e) => showError(String(e)));
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
$('upload-now').addEventListener('click', () => {
  invoke('upload_now');
  $('upload-text').textContent = 'Uploading…';
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
  showChanges(changes.length ? `Updated ${plural(changes.length, 'addon', 'addons')} by itself` : 'An automatic update didn’t work', changes, failed);
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
  await loadItems();
  await loadCatalog(false);
  setTab('mine', false);
})();
