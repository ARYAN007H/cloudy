import { api, events, dialogs, statusKey, statusMsg } from './api.js';
import {
  $, $$, esc, fmtBytes, fmtDate, monthLabel, plural, debounce, store, icon, kindIcon, kindOf,
  previewKind, accountColor, toast,
} from './util.js';

/* ============================================================ state */
const S = {
  view: 'files', cwd: 'pool', node: null, crumbs: [], items: [], sel: new Set(), anchor: null,
  layout: store('layout') || 'list', sort: store('sort') || { key: 'name', dir: 1 },
  limit: 200, loading: false, navToken: 0,
  query: '', results: null,
  accounts: [], sync: {}, hasCreds: true, media: '', settings: null,
  photos: { items: [], done: false, loading: false },
  transfers: new Map(), trash: null,
  theme: store('theme') || 'dark', setupStep: 1, version: '',
};
const content = () => $('#content');
const accountList = () => S.accounts.map((a) => a.email);
const thumbUrl = (id, s = 256) => `${S.media}/thumb/${encodeURIComponent(id)}?s=${s}`;
const mediaUrl = (id) => `${S.media}/media/${encodeURIComponent(id)}`;
const errText = (e) => (e && e.message) || String(e);
const fail = (e, prefix = '') => toast(prefix + errText(e), { kind: 'error' });

/* ============================================================ boot */
async function init() {
  applyTheme();
  window.addEventListener('error', (e) => console.error('UI error:', e.error || e.message));
  window.addEventListener('unhandledrejection', (e) => console.error('Unhandled:', e.reason));
  let boot;
  try {
    boot = await api.boot();
  } catch (e) {
    $('#boot').innerHTML = `<img src="icon.png" alt="" class="boot-logo"><h2>Cloudy couldn't start</h2>
      <p class="muted" style="max-width:460px;text-align:center">${esc(errText(e))}</p>
      <button class="btn btn-tonal" id="retry-boot">Try again</button>`;
    $('#retry-boot').addEventListener('click', () => location.reload());
    return;
  }
  Object.assign(S, { media: boot.media_base, hasCreds: boot.has_credentials, accounts: boot.accounts, settings: boot.settings, version: boot.version });
  for (const a of S.accounts) S.sync[a.email] = { status: a.sync, files: a.files_indexed };

  buildChrome();
  $('#boot').classList.add('hidden');
  $('#app').classList.remove('hidden');
  wireGlobalEvents();
  await go(S.hasCreds ? 'files' : 'setup');
  refreshAccounts(false);   // fetch quotas/names in the background
  api.transfers().then((t) => { t.forEach((x) => S.transfers.set(x.id, x)); renderNav(); });
}

function applyTheme() {
  document.documentElement.className = S.theme === 'light' ? 'light-theme' : 'dark-theme';
}

/* ============================================================ chrome */
const NAV = [
  ['files', 'Files', 'folder'], ['photos', 'Photos', 'photos'], ['accounts', 'Accounts', 'person'],
  ['transfers', 'Transfers', 'transfer'], ['trash', 'Trash', 'trash'], ['settings', 'Settings', 'settings'],
];

function buildChrome() {
  $('#search-icon').innerHTML = icon('search');
  $('#search-clear').innerHTML = icon('close');
  $('#dropzone-icon').innerHTML = icon('upload');
  $('#new-btn').innerHTML = `${icon('add')}<span class="label">New</span>`;
  renderNav(); renderTopbar(); renderPoolMeter();
}

function renderNav() {
  const active = [...S.transfers.values()].filter((t) => !isFinished(t)).length;
  const reauth = S.accounts.filter((a) => accountState(a).key === 'NeedsReauth').length;
  $('#nav').innerHTML = NAV.map(([id, label, ic]) => {
    const badge = id === 'transfers' && active ? `<span class="badge">${active}</span>`
      : id === 'accounts' && reauth ? `<span class="badge warn">!</span>` : '';
    return `<li><button class="nav-item ${S.view === id ? 'active' : ''}" data-view="${id}" ${S.view === id ? 'aria-current="page"' : ''}>
      ${icon(ic)}<span class="label">${label}</span>${badge}</button></li>`;
  }).join('');
}

function accountState(a) {
  const live = S.sync[a.email];
  const status = live ? live.status : a.sync;
  return { key: statusKey(status), msg: statusMsg(status), files: live ? live.files : a.files_indexed };
}

function renderTopbar() {
  const states = S.accounts.map(accountState);
  const syncing = S.accounts.filter((a) => accountState(a).key === 'Syncing');
  let cls = '', txt = 'Up to date', ic = icon('check');
  if (!S.accounts.length) { txt = 'No accounts yet'; ic = icon('info'); }
  else if (syncing.length) {
    const n = syncing.reduce((s, a) => s + accountState(a).files, 0);
    txt = n ? `Indexing… ${n.toLocaleString()} files` : 'Syncing…'; ic = '<div class="spinner sm"></div>';
  } else if (states.some((s) => s.key === 'NeedsReauth')) { cls = 'warn'; txt = 'Sign-in needed'; ic = icon('warning'); }
  else if (states.some((s) => s.key === 'Error')) { cls = 'err'; txt = 'Sync problem'; ic = icon('error'); }
  else if (states.some((s) => s.key === 'Offline')) { cls = 'warn'; txt = 'Offline'; ic = icon('warning'); }
  $('#topbar-right').innerHTML = `
    <button class="sync-pill ${cls}" data-view="accounts" title="Sync status – click for details">${ic}<span class="txt ellipsis">${esc(txt)}</span></button>
    ${S.view === 'files' ? `<button class="icon-btn" data-action="toggle-layout" title="${S.layout === 'list' ? 'Grid view' : 'List view'}" aria-label="Toggle layout">${icon(S.layout === 'list' ? 'grid' : 'list')}</button>` : ''}
    <button class="icon-btn" data-action="toggle-theme" title="Toggle theme" aria-label="Toggle theme">${icon(S.theme === 'light' ? 'moon' : 'sun')}</button>`;
}

function poolTotals() {
  let limit = 0, usage = 0, unlimited = false, n = 0;
  for (const a of S.accounts) {
    if (!a.quota) continue;
    n++;
    if (!a.quota.limit) { unlimited = true; usage += a.quota.usage; } else { limit += a.quota.limit; usage += a.quota.usage; }
  }
  return { limit, usage, free: Math.max(0, limit - usage), unlimited, n };
}

const anyOffline = () => S.accounts.some((a) => ['Offline', 'Error'].includes(accountState(a).key) || a.error);

function renderPoolMeter() {
  const t = poolTotals();
  const el = $('#pool-meter');
  if (!S.accounts.length) { el.innerHTML = `<span class="txt">Add Google accounts to build your pool.</span>`; return; }
  if (!t.n) { el.innerHTML = `<span class="txt">${anyOffline() ? 'Storage info unavailable while offline' : 'Loading storage…'}</span>`; return; }
  const pct = t.limit ? Math.min(100, (t.usage / t.limit) * 100) : 0;
  el.innerHTML = `<span class="txt"><strong>${t.unlimited ? 'Unlimited' : fmtBytes(t.free) + ' free'}</strong>
    ${t.unlimited ? '' : `of ${fmtBytes(t.limit)} · `}${plural(S.accounts.length, 'account')}</span>
    <div class="bar ${pct > 90 ? 'danger' : pct > 75 ? 'warn' : ''}" title="${fmtBytes(t.usage)} used" style="margin-top:6px"><i style="width:${pct}%"></i></div>`;
}

function renderBanner() {
  const need = S.accounts.filter((a) => accountState(a).key === 'NeedsReauth');
  $('#banner').innerHTML = need.length && S.view !== 'accounts' && S.view !== 'setup'
    ? `<div class="banner warn" role="alert">${icon('warning')}<span class="grow">${need.length === 1
      ? `<b>${esc(need[0].email)}</b> needs you to sign in again.` : `${need.length} accounts need you to sign in again.`}
      Their files stay visible, but they can't sync or upload until reconnected.</span>
      <button class="btn btn-tonal sm" data-view="accounts">Fix now</button></div>` : '';
}

/* ============================================================ navigation */
async function go(view) {
  if (!S.hasCreds && view !== 'setup' && view !== 'settings') view = 'setup';
  S.view = view;
  if (view !== 'files') clearSearchUI();
  renderNav(); renderTopbar(); renderBanner();
  content().scrollTop = 0;
  ({ files: showFiles, photos: showPhotos, accounts: showAccounts, transfers: showTransfers,
    trash: showTrash, settings: showSettings, setup: showSetup }[view] || showFiles)();
}

async function refreshAccounts(force) {
  try {
    S.accounts = await api.accounts(force);
    for (const a of S.accounts) S.sync[a.email] = { status: a.sync, files: a.files_indexed };
  } catch (e) { console.warn('accounts', e); return; }
  renderNav(); renderTopbar(); renderPoolMeter(); renderBanner();
  if (S.view === 'accounts') renderAccounts();
}
const refreshAccountsSoon = debounce(() => refreshAccounts(true), 2500);

/* ============================================================ Files */
function uploadTarget() { return ['root', 'accounts'].includes(S.cwd) || S.results ? 'pool' : S.cwd; }
const isFinished = (t) => ['Completed', 'Cancelled', 'Failed'].includes(statusKey(t.status));

async function showFiles(id = S.cwd) {
  if (S.results) { renderFiles(); return; }
  await loadFolder(id);
}

async function loadFolder(id, { keepSel = false } = {}) {
  const token = ++S.navToken;
  S.loading = true;
  if (!keepSel) { S.sel.clear(); S.anchor = null; S.limit = 200; }
  if (!S.items.length || S.cwd !== id) renderFiles(true);
  S.cwd = id;
  try {
    const v = await api.view(id);
    if (token !== S.navToken) return;
    Object.assign(S, { node: v.node, crumbs: v.breadcrumbs, items: v.children, loading: false });
    if (keepSel) for (const s of [...S.sel]) if (!S.items.some((i) => i.id === s)) S.sel.delete(s);
  } catch (e) {
    if (token !== S.navToken) return;
    S.loading = false;
    if (e.code === 'not_found' && id !== 'pool') return loadFolder('pool');
    content().innerHTML = errorState(e, 'retry-files');
    return;
  }
  if (S.view === 'files') renderFiles();
}

function errorState(e, retryId) {
  return `<div class="empty">${icon('error', 'big')}<h2>Something went wrong</h2><p>${esc(errText(e))}</p>
    <button class="btn btn-tonal" id="${retryId}">Try again</button></div>`;
}

function sortedItems() {
  const src = S.results || S.items;
  const { key, dir } = S.sort;
  return [...src].sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    let r = 0;
    if (key === 'size') r = a.size - b.size;
    else if (key === 'modified') r = a.modified - b.modified;
    else r = a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' });
    return (r || a.name.localeCompare(b.name)) * dir;
  });
}

function crumbLabel(c) {
  return { root: 'Cloudy', pool: 'All files', accounts: 'By account' }[c.id] || c.name;
}

function renderFilesHead() {
  const head = $('#files-head');
  if (!head) return;
  if (S.sel.size) {
    const items = selectedItems();
    const one = items.length === 1;
    const canEdit = items.every((i) => !i.synthetic && !i.read_only);
    head.innerHTML = `<div class="selbar"><button class="icon-btn" data-action="clear-sel" aria-label="Clear selection">${icon('close')}</button>
      <b>${S.sel.size} selected</b>
      <button class="btn btn-text" data-action="download">${icon('download')}Download</button>
      ${canEdit ? `<button class="btn btn-text" data-action="move">${icon('move')}Move</button>` : ''}
      ${canEdit && one ? `<button class="btn btn-text" data-action="rename">${icon('edit')}Rename</button>` : ''}
      ${canEdit ? `<button class="btn btn-text" data-action="trash">${icon('trash')}Delete</button>` : ''}</div>`;
    return;
  }
  if (S.results) {
    head.innerHTML = `<div class="page-title">${plural(S.results.length, 'result')} for “${esc(S.query)}”</div>
      <button class="btn btn-outline sm" data-action="clear-search">Clear search</button>`;
    return;
  }
  const crumbs = S.crumbs.filter((c) => c.id !== 'root');
  head.innerHTML = `<nav class="crumbs" aria-label="Breadcrumb">${crumbs.map((c, i) =>
    (i ? icon('right') : '') + `<button class="crumb ${i === crumbs.length - 1 ? 'cur' : ''} ellipsis" ${i === crumbs.length - 1 ? 'aria-current="page"' : `data-open="${esc(c.id)}"`}>${esc(crumbLabel(c))}</button>`).join('')}</nav>
    <button class="btn btn-outline sm" data-action="new-folder">${icon('newfolder')}New folder</button>
    <button class="btn btn-tonal sm" data-action="upload-menu">${icon('upload')}Upload</button>`;
}

function accDots(item) {
  const all = accountList();
  return (item.accounts || []).map((a) => `<span class="dot" style="background:${accountColor(a, all)}" title="${esc(a)}"></span>`).join('');
}

function thumbOrKind(item, size) {
  if (item.has_thumb) {
    return `<span class="kind kind-box" style="position:relative">${kindIcon(item)}<img class="thumb-sm" style="position:absolute;inset:0;opacity:0" loading="lazy" decoding="async" alt="" src="${thumbUrl(item.id, size)}" data-thumb></span>`;
  }
  return kindIcon(item);
}

function rowHtml(item) {
  const sel = S.sel.has(item.id) ? ' sel' : '';
  return `<div class="row${sel}" role="row" data-id="${esc(item.id)}" aria-selected="${!!sel}">
    <div class="name">${thumbOrKind(item, 64)}<span class="t">${esc(item.name)}${S.results && item.path ? `<br><span class="path">${esc(item.path)}</span>` : ''}</span></div>
    <div class="col-acct accts">${accDots(item)}</div>
    <div class="col-mod cell-muted">${item.synthetic ? '' : fmtDate(item.modified)}</div>
    <div class="cell-muted">${item.is_dir ? (item.synthetic ? '' : '—') : fmtBytes(item.size)}</div></div>`;
}

function cardHtml(item) {
  const sel = S.sel.has(item.id) ? ' sel' : '';
  const media = item.has_thumb
    ? `<div class="fallback">${kindIcon(item)}</div><img class="" loading="lazy" decoding="async" alt="" src="${thumbUrl(item.id, 320)}" data-thumb-card>`
    : `<div class="fallback">${kindIcon(item)}</div>`;
  const play = kindOf(item) === 'video' ? `<span class="play-badge">${icon('play')}</span>` : '';
  return `<div class="card${sel}" data-id="${esc(item.id)}" role="gridcell" aria-selected="${!!sel}">
    <div class="card-media">${media}${play}</div>
    <div class="card-meta"><span class="t ellipsis" title="${esc(item.name)}">${esc(item.name)}</span><span class="accts">${accDots(item)}</span></div></div>`;
}

function sortBtn(key, label) {
  const on = S.sort.key === key;
  return `<button data-sort="${key}">${label}${on ? (S.sort.dir > 0 ? ' ▲' : ' ▼') : ''}</button>`;
}

let moreObserver = null;
function renderFiles(loading = false) {
  const c = content();
  const top = c.scrollTop;
  if (loading && !S.items.length) {
    c.innerHTML = `<div class="page-head" id="files-head"></div>${Array.from({ length: 8 }, () => '<div class="skeleton"></div>').join('')}`;
    renderFilesHead();
    return;
  }
  const items = sortedItems();
  const head = `<div class="page-head" id="files-head"></div>`;
  let body;
  if (!items.length) {
    body = emptyFolder();
  } else {
    const slice = items.slice(0, S.limit);
    body = S.layout === 'grid'
      ? `<div class="grid" id="file-list" role="grid">${slice.map(cardHtml).join('')}</div>`
      : `<div role="grid"><div class="list-head" role="row"><div>${sortBtn('name', 'Name')}</div><div class="col-acct">Accounts</div><div class="col-mod">${sortBtn('modified', 'Modified')}</div><div>${sortBtn('size', 'Size')}</div></div>
         <div id="file-list">${slice.map(rowHtml).join('')}</div></div>`;
    if (items.length > S.limit) body += '<div class="more-sentinel" id="more"></div>';
  }
  c.innerHTML = head + body;
  c.scrollTop = top;
  renderFilesHead();
  observeMore(items);
}

function emptyFolder() {
  if (S.results) return `<div class="empty">${icon('search', 'big')}<h2>No matches</h2><p>Nothing in your pool matches “${esc(S.query)}”.</p></div>`;
  if (!S.accounts.length) {
    return `<div class="empty">${icon('cloud', 'big')}<h2>Your pool is empty</h2>
      <p>Add one or more Google accounts. Their free 15&nbsp;GB each is combined into a single drive.</p>
      <button class="btn btn-primary" data-action="add-account">${icon('add')}Add a Google account</button></div>`;
  }
  const syncing = S.accounts.some((a) => accountState(a).key === 'Syncing');
  if (syncing && S.cwd === 'pool') {
    return `<div class="empty"><div class="spinner"></div><h2>Indexing your drives…</h2><p>Files appear here as they're found. Large drives can take a few minutes the first time.</p></div>`;
  }
  const writable = !['root', 'accounts'].includes(S.cwd);
  return `<div class="empty">${icon('folder', 'big')}<h2>${writable ? 'This folder is empty' : 'Nothing here yet'}</h2>
    ${writable ? `<p>Drag files here, or use Upload.</p><button class="btn btn-tonal" data-action="upload-menu">${icon('upload')}Upload</button>` : ''}</div>`;
}

function observeMore(items) {
  if (moreObserver) moreObserver.disconnect();
  const sentinel = $('#more');
  if (!sentinel) return;
  moreObserver = new IntersectionObserver((entries) => {
    if (!entries.some((e) => e.isIntersecting)) return;
    moreObserver.disconnect();
    const start = S.limit;
    S.limit += 200;
    const list = $('#file-list');
    list.insertAdjacentHTML('beforeend', items.slice(start, S.limit).map(S.layout === 'grid' ? cardHtml : rowHtml).join(''));
    if (items.length > S.limit) observeMore(items); else sentinel.remove();
  }, { root: content(), rootMargin: '600px' });
  moreObserver.observe(sentinel);
}

const selectedItems = () => (S.results || S.items).filter((i) => S.sel.has(i.id));
const findItem = (id) => (S.results || S.items).find((i) => i.id === id);

function updateSelectionUI() {
  $$('#file-list [data-id]').forEach((el) => {
    const on = S.sel.has(el.dataset.id);
    el.classList.toggle('sel', on);
    el.setAttribute('aria-selected', on);
  });
  renderFilesHead();
}

function selectItem(id, ev) {
  const order = sortedItems().map((i) => i.id);
  if (ev && ev.shiftKey && S.anchor && order.includes(S.anchor)) {
    const [a, b] = [order.indexOf(S.anchor), order.indexOf(id)].sort((x, y) => x - y);
    S.sel = new Set(order.slice(a, b + 1));
  } else if (ev && (ev.ctrlKey || ev.metaKey)) {
    S.sel.has(id) ? S.sel.delete(id) : S.sel.add(id);
    S.anchor = id;
  } else {
    S.sel = new Set([id]);
    S.anchor = id;
  }
  updateSelectionUI();
}

function openItem(id) {
  const item = findItem(id) || S.photos.items.find((i) => i.id === id);
  if (!item) return;
  if (item.is_dir) { clearSearchUI(); S.results = null; S.view = 'files'; renderNav(); renderTopbar(); loadFolder(item.id); return; }
  const list = (S.view === 'photos' ? S.photos.items : sortedItems()).filter((i) => !i.is_dir && previewKind(i) !== 'none');
  openPreview(item, list.length ? list : [item]);
}

/* ============================================================ Photos */
const PAGE = 240;
async function showPhotos() {
  S.photos = { items: [], done: false, loading: false };
  content().innerHTML = `<div class="page-head"><div class="page-title">Photos & videos</div></div><div id="photo-root"></div><div id="photo-more" class="more-sentinel"></div>`;
  await loadPhotoPage();
}

async function loadPhotoPage(reload = false) {
  const P = S.photos;
  if (P.loading || (P.done && !reload)) return;
  P.loading = true;
  try {
    const want = reload ? Math.max(PAGE, P.items.length) : PAGE;
    const page = await api.media(reload ? 0 : P.items.length, want);
    if (S.view !== 'photos') return;
    if (reload) { P.items = page; P.done = page.length < want; }
    else { P.items = P.items.concat(page); P.done = page.length < PAGE; }
    renderPhotos();
  } catch (e) {
    $('#photo-root') && ($('#photo-root').innerHTML = errorState(e, 'retry-photos'));
  } finally { P.loading = false; }
}

function renderPhotos() {
  const root = $('#photo-root');
  if (!root) return;
  const P = S.photos;
  if (!P.items.length) {
    const syncing = S.accounts.some((a) => accountState(a).key === 'Syncing');
    root.innerHTML = `<div class="empty">${syncing ? '<div class="spinner"></div>' : icon('photos', 'big')}
      <h2>${syncing ? 'Looking for your photos…' : 'No photos or videos yet'}</h2>
      <p>${syncing ? 'They will appear as your drives are indexed.' : 'Photos and videos stored in any of your accounts show up here.'}</p></div>`;
    return;
  }
  let html = '', last = '';
  for (const p of P.items) {
    const m = monthLabel(p.modified);
    if (m !== last) { html += `${last ? '</div>' : ''}<div class="month">${esc(m)}</div><div class="photos">`; last = m; }
    html += `<button class="photo" data-photo="${esc(p.id)}" aria-label="${esc(p.name)}"><div class="fallback">${kindIcon(p)}</div>
      <img loading="lazy" decoding="async" alt="" src="${thumbUrl(p.id, 320)}" data-thumb-card>${kindOf(p) === 'video' ? `<span class="play-badge">${icon('play')}</span>` : ''}</button>`;
  }
  root.innerHTML = html + '</div>';
  const sentinel = $('#photo-more');
  if (sentinel && !P.done) {
    const io = new IntersectionObserver((en) => {
      if (en.some((e) => e.isIntersecting)) { io.disconnect(); loadPhotoPage(); }
    }, { root: content(), rootMargin: '800px' });
    io.observe(sentinel);
  }
}

/* ============================================================ Accounts */
function showAccounts() { renderAccounts(); }

function statusChip(a) {
  const st = accountState(a);
  switch (st.key) {
    case 'Syncing': return `<span class="chip"><span class="spinner sm"></span>Indexing ${st.files ? st.files.toLocaleString() + ' files' : ''}</span>`;
    case 'NeedsReauth': return `<span class="chip warn">${icon('warning')}Sign-in needed</span>`;
    case 'Offline': return `<span class="chip warn">Offline – retrying</span>`;
    case 'Error': return `<span class="chip err" title="${esc(st.msg)}">${icon('error')}Sync problem</span>`;
    default: return `<span class="chip ok">${icon('check')}${st.files.toLocaleString()} files</span>`;
  }
}

function renderAccounts() {
  if (S.view !== 'accounts') return;
  const t = poolTotals();
  const pct = t.limit ? Math.min(100, (t.usage / t.limit) * 100) : 0;
  const hero = S.accounts.length ? `<section class="pool-hero">
      <div class="small" style="opacity:.8">Your combined storage pool</div>
      <div class="big-num">${t.n ? (t.unlimited ? 'Unlimited' : fmtBytes(t.free) + ' free') : (anyOffline() ? 'Offline' : '…')}</div>
      <div class="bar"><i style="width:${pct}%"></i></div>
      <div class="small" style="opacity:.85">${t.n ? `${fmtBytes(t.usage)} used${t.limit ? ` of ${fmtBytes(t.limit)}` : ''} · ` : ''}${plural(S.accounts.length, 'account')}. Each file is stored whole in one account – the biggest file you can add is the free space of your emptiest account.</div>
    </section>` : '';
  const cards = S.accounts.map((a) => {
    const q = a.quota, st = accountState(a);
    const qp = q && q.limit ? Math.min(100, (q.usage / q.limit) * 100) : 0;
    const color = accountColor(a.email, accountList());
    return `<article class="acct" data-account="${esc(a.email)}">
      <div class="acct-top"><div class="avatar" style="background:${color}">${esc((a.display_name || a.email)[0].toUpperCase())}</div>
        <div class="grow"><div class="ellipsis" style="font-weight:600">${esc(a.display_name || a.email.split('@')[0])}</div><div class="small muted ellipsis">${esc(a.email)}</div></div>${statusChip(a)}</div>
      ${q ? `<div><div class="bar ${qp > 90 ? 'danger' : qp > 75 ? 'warn' : ''}"><i style="width:${qp}%"></i></div>
        <div class="small muted" style="margin-top:6px" title="Usage includes Gmail and Google Photos">${fmtBytes(q.usage)} used${q.limit ? ` of ${fmtBytes(q.limit)}` : ''}${q.limit ? ` · ${fmtBytes(q.limit - q.usage)} free` : ''}</div></div>`
        : `<div class="small muted">${a.error ? esc(a.error) : (st.key === 'Offline' ? 'Offline – storage details unavailable right now.' : 'Loading storage…')}</div>`}
      ${st.key === 'NeedsReauth' ? `<div class="err-text">Google needs you to sign in again. Tip: if your Google Cloud app is still in “Testing”, sign-ins expire after 7 days – publish it to avoid this.</div>` : ''}
      <div class="acct-actions">
        ${st.key === 'NeedsReauth' ? `<button class="btn btn-primary sm" data-action="reconnect" data-email="${esc(a.email)}">Reconnect</button>` : `<button class="btn btn-tonal sm" data-browse="${esc(a.email)}">${icon('folder')}Browse</button>`}
        <button class="btn btn-outline sm" data-action="sync-account" data-email="${esc(a.email)}" title="Check for changes now">${icon('sync')}Sync</button>
        <button class="btn btn-text btn-danger sm" data-action="remove-account" data-email="${esc(a.email)}">Remove</button></div></article>`;
  }).join('');
  content().innerHTML = `<div class="page-head"><div class="page-title">Accounts</div>
      <button class="btn btn-outline sm" data-action="refresh-accounts">${icon('sync')}Refresh</button></div>${hero}
    <div class="acct-grid">${cards}<button class="acct add" data-action="add-account">${icon('add')}<b>Add a Google account</b>
      <span class="small muted">Every account adds up to 15&nbsp;GB of free storage</span></button></div>`;
}

/* ============================================================ Transfers */
function showTransfers() { renderTransfers(); }

function renderTransfers() {
  if (S.view !== 'transfers') return;
  const list = [...S.transfers.values()].reverse();
  const done = list.filter(isFinished).length;
  const rows = list.map((t) => {
    const k = statusKey(t.status), pct = t.total_bytes ? Math.min(100, (t.progress_bytes / t.total_bytes) * 100) : (k === 'Completed' ? 100 : 0);
    const label = { Pending: 'Waiting…', InProgress: `${fmtBytes(t.progress_bytes)} of ${fmtBytes(t.total_bytes)}`, Paused: 'Paused', Completed: 'Done', Cancelled: 'Cancelled', Failed: statusMsg(t.status) || 'Failed' }[k];
    return `<div class="titem" data-tid="${esc(t.id)}"><span class="kind">${icon(t.direction === 'Download' ? 'download' : 'upload')}</span>
      <div class="grow"><div class="ellipsis">${esc(t.file_name)}</div>
        <div class="small ${k === 'Failed' ? 'err-text' : 'muted'} ellipsis">${esc(label)} · ${esc(t.account)}</div>
        ${['InProgress', 'Pending'].includes(k) ? `<div class="bar"><i style="width:${pct}%"></i></div>` : ''}</div>
      ${!isFinished(t) ? `<button class="icon-btn" data-action="cancel-transfer" data-id="${esc(t.id)}" aria-label="Cancel">${icon('close')}</button>` : ''}
      ${k === 'Failed' && t.direction !== 'Download' ? `<button class="btn btn-tonal sm" data-action="retry-transfer" data-id="${esc(t.id)}">Retry</button>` : ''}</div>`;
  }).join('');
  content().innerHTML = `<div class="page-head"><div class="page-title">Transfers</div>
    ${done ? `<button class="btn btn-outline sm" data-action="clear-transfers">Clear finished</button>` : ''}</div>
    ${list.length ? `<div class="tlist">${rows}</div>` : `<div class="empty">${icon('transfer', 'big')}<h2>No transfers</h2><p>Uploads and downloads appear here with live progress.</p></div>`}`;
}
const renderTransfersSoon = (() => { let q = false; return () => { if (q) return; q = true; requestAnimationFrame(() => { q = false; renderTransfers(); }); }; })();

/* ============================================================ Trash */
async function showTrash() {
  content().innerHTML = `<div class="page-head"><div class="page-title">Trash</div></div>${Array.from({ length: 4 }, () => '<div class="skeleton"></div>').join('')}`;
  try { S.trash = await api.trashList(); } catch (e) { content().innerHTML = errorState(e, 'retry-trash'); return; }
  renderTrash();
}

function renderTrash() {
  if (S.view !== 'trash') return;
  const items = S.trash || [];
  const total = items.reduce((s, i) => s + i.size, 0);
  const rows = items.map((i) => `<div class="trash-row" data-t="${esc(i.account)}|${esc(i.id)}">
    <div class="name" style="display:flex;gap:12px;align-items:center;min-width:0">${kindIcon({ is_dir: i.is_folder, mime_type: i.mime_type, name: i.name })}<span class="ellipsis">${esc(i.name)}</span></div>
    <div class="small muted ellipsis">${esc(i.account)}</div><div class="cell-muted">${i.is_folder ? '' : fmtBytes(i.size)}</div>
    <div><button class="icon-btn" data-action="restore-trashed" title="Restore" aria-label="Restore">${icon('restore')}</button>
    <button class="icon-btn" data-action="delete-trashed" title="Delete forever" aria-label="Delete forever">${icon('forever')}</button></div></div>`).join('');
  content().innerHTML = `<div class="page-head"><div class="page-title">Trash</div>
    ${items.length ? `<button class="btn btn-outline sm btn-danger" data-action="empty-trash">${icon('forever')}Empty trash</button>` : ''}</div>
    <div class="callout">${icon('info')}<span>Items in Google Drive's trash <b>still use storage</b> until they are deleted forever${total ? ` – emptying it frees <b>${fmtBytes(total)}</b>` : ''}. Google also empties trash automatically after 30 days.</span></div>
    ${items.length ? rows : `<div class="empty">${icon('trash', 'big')}<h2>Trash is empty</h2><p>Deleted files appear here so you can restore them or free up space.</p></div>`}`;
}

/* ============================================================ Settings */
async function showSettings() {
  try { S.settings = await api.settings(); } catch (e) { fail(e); }
  renderSettings();
}

function renderSettings() {
  if (S.view !== 'settings') return;
  const s = S.settings || {};
  const opt = (v, label, cur) => `<option value="${v}" ${cur === v ? 'selected' : ''}>${label}</option>`;
  content().innerHTML = `<div class="page-head"><div class="page-title">Settings</div></div><div class="settings">
    <section class="sgroup"><h3>Where new files go</h3>
      <div class="srow"><div><div>Placement</div><div class="hint">Which account stores each new upload.</div></div>
        <select class="input" id="set-policy">${opt('most_free', 'Account with the most free space', s.upload_policy)}${opt('round_robin', 'Rotate between accounts', s.upload_policy)}${opt('pinned', 'Always the same account', s.upload_policy)}</select></div>
      <div class="srow ${s.upload_policy === 'pinned' ? '' : 'hidden'}" id="pinned-row"><div>Account</div>
        <select class="input" id="set-pinned">${S.accounts.map((a) => opt(a.email, esc(a.email), s.pinned_account)).join('')}</select></div></section>
    <section class="sgroup"><h3>Sync & cache</h3>
      <div class="srow"><div><div>Check for changes every</div><div class="hint">Changes made elsewhere appear after this interval.</div></div>
        <select class="input" id="set-interval">${[[30, '30 seconds'], [60, '1 minute'], [300, '5 minutes'], [900, '15 minutes']].map(([v, l]) => opt(String(v), l, String(s.sync_interval_secs))).join('')}</select></div>
      <div class="srow"><div><div>Cache size limit</div><div class="hint">Downloaded files &amp; thumbnails kept on disk (${s.cache_used_mb ?? 0} MB used).</div></div>
        <select class="input" id="set-cache">${[[512, '512 MB'], [2048, '2 GB'], [5120, '5 GB'], [10240, '10 GB']].map(([v, l]) => opt(String(v), l, String(s.cache_size_mb))).join('')}</select></div>
      <div class="srow"><div>Clear cached files</div><button class="btn btn-outline sm" data-action="clear-cache">Clear cache</button></div></section>
    <section class="sgroup"><h3>Google access</h3>
      <div class="srow"><div><div>OAuth client</div><div class="hint">${s.client_id_hint ? esc(s.client_id_hint) : 'Not configured'}</div></div>
        <button class="btn btn-outline sm" data-action="edit-credentials">${s.client_id_hint ? 'Change' : 'Set up'}</button></div>
      <div class="srow"><div><div>Where sign-ins are stored</div><div class="hint">${s.secure_storage ? 'In your system keyring.' : 'In a private file (permissions 600) because no keyring service was found.'}</div></div>
        <span class="chip ${s.secure_storage ? 'ok' : ''}">${icon('lock')}${s.secure_storage ? 'Keyring' : 'File'}</span></div></section>
    <section class="sgroup"><h3>Keyboard shortcuts</h3>
      <div class="srow"><span>Search</span><kbd>/</kbd></div><div class="srow"><span>Select all</span><kbd>Ctrl</kbd>+<kbd>A</kbd></div>
      <div class="srow"><span>Open / Rename / Delete</span><span><kbd>Enter</kbd> <kbd>F2</kbd> <kbd>Del</kbd></span></div>
      <div class="srow"><span>Go up a folder</span><kbd>Backspace</kbd></div><div class="srow"><span>Upload files</span><kbd>Ctrl</kbd>+<kbd>U</kbd></div></section>
    <p class="hint" style="text-align:center">Cloudy ${esc(S.version)}</p></div>`;
}

async function saveSetting(update) {
  try { S.settings = await api.updateSettings(update); toast('Saved'); renderSettings(); } catch (e) { fail(e); showSettings(); }
}

/* ============================================================ Setup wizard */
const CONSOLE = {
  project: 'https://console.cloud.google.com/projectcreate',
  drive: 'https://console.cloud.google.com/apis/library/drive.googleapis.com',
  consent: 'https://console.cloud.google.com/apis/credentials/consent',
  creds: 'https://console.cloud.google.com/apis/credentials',
};
const ext = (url) => `<button class="btn btn-outline sm" data-external="${url}">${icon('external')}Open</button>`;

function showSetup() {
  const step = S.setupStep;
  const stepper = `<div class="stepper">${[1, 2, 3].map((n) => `<i class="${n <= step ? 'on' : ''}"></i>`).join('')}</div>`;
  let body;
  if (step === 1) {
    body = `<h1>Let's connect Cloudy to Google</h1>
      <p class="muted">Cloudy uses <b>your own</b> free Google API credentials, so your files never pass through anyone else. This takes about 5 minutes, once.</p>${stepper}
      <ol class="steps">
        <li><span><b>Create a project</b> in Google Cloud (any name).</span>${ext(CONSOLE.project)}</li>
        <li><span><b>Enable the Google Drive API</b> for that project.</span>${ext(CONSOLE.drive)}</li>
        <li><span><b>OAuth consent screen:</b> choose “External”, fill in the app name and your email, and add <b>every Google account you plan to link</b> as a Test user.</span>${ext(CONSOLE.consent)}</li>
        <li><span><b>Create credentials → OAuth client ID</b>, application type <b>Desktop app</b>.</span>${ext(CONSOLE.creds)}</li></ol>
      <div class="callout">${icon('warning')}<span>While the app is in “Testing”, Google makes sign-ins <b>expire after 7 days</b>. To avoid that, set the publishing status to <b>In production</b> on the consent screen. You'll see an “unverified app” notice when signing in – choose <i>Advanced → Continue</i>; it's your own app.</span></div>
      <div class="modal-actions" style="justify-content:flex-start"><button class="btn btn-primary" data-action="setup-next">I have my Client ID</button>
        ${S.hasCreds ? '<button class="btn btn-text" data-view="settings">Cancel</button>' : ''}</div>`;
  } else if (step === 2) {
    body = `<h1>Paste your credentials</h1><p class="muted">From the credentials page: <b>Client ID</b> and <b>Client secret</b> (or import the JSON file you can download there).</p>${stepper}
      <div class="field"><label for="cid">Client ID</label><input class="input" id="cid" placeholder="123456789-abc….apps.googleusercontent.com" autocomplete="off" spellcheck="false"></div>
      <div class="field"><label for="csec">Client secret</label><input class="input" id="csec" type="password" placeholder="GOCSPX-…" autocomplete="off" spellcheck="false"></div>
      <div id="setup-err" class="err-text"></div>
      <div class="modal-actions" style="justify-content:flex-start"><button class="btn btn-primary" data-action="save-credentials">Save & continue</button>
        <button class="btn btn-outline" data-action="import-credentials">Import JSON file…</button><button class="btn btn-text" data-action="setup-back">Back</button></div>
      <p class="hint" style="margin-top:12px">${icon('lock')} Stored in your system keyring when available, otherwise in a private file.</p>`;
  } else {
    body = `<h1>Add your first account</h1><p class="muted">A browser window opens for Google sign-in. You can repeat this for as many accounts as you like – each adds up to 15&nbsp;GB.</p>${stepper}
      <div class="modal-actions" style="justify-content:flex-start"><button class="btn btn-primary" data-action="add-account">${icon('add')}Add Google account</button>
        <button class="btn btn-text" data-action="setup-done">Skip for now</button></div>`;
  }
  content().innerHTML = `<div class="onboard">${body}</div>`;
}

async function saveCredentials() {
  const id = $('#cid').value, sec = $('#csec').value, err = $('#setup-err');
  err.textContent = '';
  try {
    await api.setCredentials(id, sec);
    S.hasCreds = true; S.settings = await api.settings();
    toast('Credentials saved');
    S.setupStep = S.accounts.length ? 0 : 3;
    S.accounts.length ? go('files') : showSetup();
  } catch (e) { err.textContent = errText(e); }
}

async function importCredentials() {
  const path = await dialogs.json();
  if (!path) return;
  try {
    await api.importCredentialsFile(path);
    S.hasCreds = true; S.settings = await api.settings();
    toast('Credentials imported');
    S.setupStep = S.accounts.length ? 0 : 3;
    S.accounts.length ? go('files') : showSetup();
  } catch (e) { $('#setup-err').textContent = errText(e); }
}

/* ============================================================ Modals */
function modal(html, { onClose, wide } = {}) {
  const root = $('#modal-root');
  const scrim = document.createElement('div');
  scrim.className = 'scrim';
  scrim.innerHTML = `<div class="modal" role="dialog" aria-modal="true" ${wide ? 'style="width:min(640px,calc(100vw - 32px))"' : ''}>${html}</div>`;
  root.appendChild(scrim);
  const close = (reason) => { scrim.remove(); document.removeEventListener('keydown', onKey, true); onClose && onClose(reason); };
  const onKey = (e) => { if (e.key === 'Escape' && root.lastElementChild === scrim) { e.stopPropagation(); close('esc'); } };
  document.addEventListener('keydown', onKey, true);
  scrim.addEventListener('mousedown', (e) => { if (e.target === scrim) close('scrim'); });
  const first = $('input, button.btn-primary, button', scrim);
  first && first.focus();
  return { el: $('.modal', scrim), close };
}

function prompt({ title, label, value = '', ok = 'OK', sub = '' }) {
  return new Promise((resolve) => {
    let done = false;
    const m = modal(`<h2>${esc(title)}</h2>${sub ? `<p class="sub">${esc(sub)}</p>` : ''}
      <div class="field"><label for="pv">${esc(label)}</label><input class="input" id="pv" value="${esc(value)}" autocomplete="off" spellcheck="false"></div>
      <div class="err-text" id="pe"></div>
      <div class="modal-actions"><button class="btn btn-text" data-m="cancel">Cancel</button><button class="btn btn-primary" data-m="ok">${esc(ok)}</button></div>`,
      { onClose: () => { if (!done) resolve(null); } });
    const input = $('#pv', m.el);
    const sel = value.lastIndexOf('.') > 0 ? value.lastIndexOf('.') : value.length;
    input.focus(); input.setSelectionRange(0, sel);
    const submit = () => {
      const v = input.value.trim();
      if (!v) { $('#pe', m.el).textContent = 'Please enter a name.'; return; }
      if (v.includes('/')) { $('#pe', m.el).textContent = 'Names can’t contain “/”.'; return; }
      done = true; m.close('ok'); resolve(v);
    };
    m.el.addEventListener('click', (e) => { const b = e.target.closest('[data-m]'); if (!b) return; b.dataset.m === 'ok' ? submit() : m.close('cancel'); });
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter') submit(); });
  });
}

function confirmBox({ title, body, ok = 'OK', danger = false }) {
  return new Promise((resolve) => {
    let done = false;
    const m = modal(`<h2>${esc(title)}</h2><p class="sub">${body}</p>
      <div class="modal-actions"><button class="btn btn-text" data-m="cancel">Cancel</button><button class="btn ${danger ? 'btn-danger solid' : 'btn-primary'}" data-m="ok">${esc(ok)}</button></div>`,
      { onClose: () => { if (!done) resolve(false); } });
    m.el.addEventListener('click', (e) => { const b = e.target.closest('[data-m]'); if (!b) return; done = true; m.close(); resolve(b.dataset.m === 'ok'); });
  });
}

/** Choose a destination folder inside the pool or an account. */
function pickFolder({ title, ok = 'Move here', exclude = [] }) {
  return new Promise((resolve) => {
    let done = false, cur = 'pool';
    const m = modal(`<h2>${esc(title)}</h2><div id="pf-crumbs" class="small muted"></div><div class="pick-list" id="pf-list" style="margin:10px 0"></div>
      <div class="modal-actions"><button class="btn btn-text" data-m="cancel">Cancel</button><button class="btn btn-primary" id="pf-ok" data-m="ok">${esc(ok)}</button></div>`,
      { onClose: () => { if (!done) resolve(null); }, wide: true });
    const load = async (id) => {
      try {
        const v = await api.view(id);
        cur = id;
        $('#pf-crumbs', m.el).textContent = v.breadcrumbs.filter((c) => c.id !== 'root').map(crumbLabel).join(' › ');
        const folders = v.children.filter((c) => c.is_dir && !exclude.includes(c.id));
        $('#pf-list', m.el).innerHTML = `${v.node.parent_id && v.node.id !== 'pool' ? `<button class="pick-item" data-up="${esc(v.node.parent_id)}">${icon('back')}<span>Up</span></button>` : ''}
          ${folders.map((f) => `<button class="pick-item" data-go="${esc(f.id)}">${kindIcon(f)}<span class="ellipsis">${esc(f.name)}</span></button>`).join('') || '<div class="pick-item muted">No subfolders</div>'}
          ${id === 'pool' ? `<button class="pick-item" data-go="accounts">${icon('person')}<span>Browse by account…</span></button>` : ''}`;
        $('#pf-ok', m.el).disabled = v.node.synthetic && v.node.id !== 'pool';
      } catch (e) { fail(e); }
    };
    m.el.addEventListener('click', (e) => {
      const g = e.target.closest('[data-go],[data-up]');
      if (g) return load(g.dataset.go || g.dataset.up);
      const b = e.target.closest('[data-m]');
      if (!b) return;
      if (b.dataset.m === 'ok') { done = true; m.close(); resolve(cur); } else m.close();
    });
    load('pool');
  });
}

/* ---- add-account sign-in dialog ---- */
async function openAddAccount({ reconnect = null } = {}) {
  if (!S.hasCreds) { go('setup'); return; }
  let url = '', state = 'wait', error = null, unlisten = null, dlg = null;
  const render = () => {
    if (!dlg) return;
    dlg.el.innerHTML = state === 'wait' ? `
      <h2>${reconnect ? 'Reconnect' : 'Add'} a Google account</h2>
      <p class="sub">${reconnect ? `Sign in as <b>${esc(reconnect)}</b>` : 'Choose the Google account you want to add'} in the browser window that just opened.</p>
      <div style="display:flex;align-items:center;gap:12px"><div class="spinner"></div><span>Waiting for you to finish in the browser…</span></div>
      <p class="hint" style="margin-top:12px">${reconnect ? '' : 'Tip: use “Use another account” in Google’s picker to add an account other than the one you’re signed into.'}</p>
      ${url ? `<div class="linkbox"><span>${esc(url)}</span><button class="btn btn-outline sm" data-m="copy">${icon('copy')}Copy</button></div>
        <p class="hint" style="margin-top:6px">Browser didn't open? Copy the link and paste it into any browser on this computer.</p>` : ''}
      <div class="modal-actions">${url ? `<button class="btn btn-outline" data-m="again">${icon('external')}Open again</button>` : ''}<button class="btn btn-text" data-m="cancel">Cancel</button></div>`
      : `<h2>Couldn't add the account</h2><p class="err-text" style="margin:8px 0 4px">${esc(errText(error))}</p>
      ${hintFor(error)}<div class="modal-actions"><button class="btn btn-text" data-m="cancel">Close</button>
      ${error.code === 'no_credentials' ? `<button class="btn btn-primary" data-m="setup">Set up credentials</button>` : `<button class="btn btn-primary" data-m="retry">Try again</button>`}</div>`;
  };
  const hintFor = (e) => ({
    needs_reauth: '<p class="hint">If you just removed Cloudy’s access in your Google account, try again.</p>',
    timeout: '<p class="hint">The sign-in window timed out. Try again and finish within 5 minutes.</p>',
    account_mismatch: '<p class="hint">Reconnect must use the same Google account. To use a different one, remove this account and add the other.</p>',
    offline: '<p class="hint">Check your internet connection and try again.</p>',
  }[e.code] || '');
  dlg = modal('', { onClose: (why) => { if (state === 'wait' && why !== 'ok') api.cancelAdd().catch(() => {}); unlisten && unlisten(); } });
  dlg.el.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-m]');
    if (!b) return;
    const act = b.dataset.m;
    if (act === 'cancel') { state = 'closed'; api.cancelAdd().catch(() => {}); dlg.close('cancel'); }
    else if (act === 'copy') { await copyText(url); toast('Link copied'); }
    else if (act === 'again') api.openExternal(url).catch((er) => fail(er));
    else if (act === 'setup') { dlg.close('ok'); S.setupStep = 2; go('setup'); }
    else if (act === 'retry') { dlg.close('ok'); openAddAccount({ reconnect }); }
  });
  render(); // show the waiting state right away, before the listener is registered
  unlisten = await events.authUrl((u) => { url = u; render(); });
  try {
    const res = reconnect ? await api.reconnect(reconnect) : await api.addAccount(null);
    state = 'done'; dlg.close('ok');
    toast(res.already_linked ? `${res.email} was already linked – sign-in refreshed` : `Added ${res.email}. Indexing its files…`, { kind: '' });
    await refreshAccounts(true);
    if (S.setupStep === 3) S.setupStep = 0;
    go(S.view === 'setup' ? 'files' : S.view === 'accounts' ? 'accounts' : S.view);
  } catch (e) {
    if (state !== 'wait') return;               // cancelled by the user
    if (e.code === 'cancelled') { dlg.close('cancel'); return; }
    state = 'error'; error = e; render();
  }
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); return; } catch { /* fall through */ }
  const ta = document.createElement('textarea');
  ta.value = text; document.body.appendChild(ta); ta.select();
  try { document.execCommand('copy'); } finally { ta.remove(); }
}

/* ============================================================ Menus */
function showMenu(x, y, entries) {
  const root = $('#menu-root');
  closeMenu();
  const el = document.createElement('div');
  el.className = 'menu'; el.setAttribute('role', 'menu');
  el.innerHTML = entries.map((e) => e === '-' ? '<hr>' : `<button role="menuitem" data-act="${e.act}" class="${e.danger ? 'danger' : ''}">${icon(e.icon)}<span>${esc(e.label)}</span></button>`).join('');
  root.appendChild(el);
  const r = el.getBoundingClientRect();
  el.style.left = `${Math.max(8, Math.min(x, innerWidth - r.width - 8))}px`;
  el.style.top = `${Math.max(8, Math.min(y, innerHeight - r.height - 8))}px`;
  $('button', el).focus();
  el.addEventListener('click', (e) => { const b = e.target.closest('[data-act]'); if (!b) return; closeMenu(); runAction(b.dataset.act); });
  el.addEventListener('keydown', (e) => {
    const btns = $$('button', el), i = btns.indexOf(document.activeElement);
    if (e.key === 'ArrowDown') { btns[(i + 1) % btns.length].focus(); e.preventDefault(); }
    if (e.key === 'ArrowUp') { btns[(i - 1 + btns.length) % btns.length].focus(); e.preventDefault(); }
  });
}
const closeMenu = () => { $('#menu-root').innerHTML = ''; };

function contextEntries() {
  const items = selectedItems();
  if (!items.length) return [];
  const one = items.length === 1;
  const canEdit = items.every((i) => !i.synthetic && !i.read_only);
  const e = [];
  if (one) e.push({ act: 'open', label: items[0].is_dir ? 'Open' : 'Preview', icon: items[0].is_dir ? 'folder' : 'play' });
  e.push({ act: 'download', label: 'Download', icon: 'download' });
  if (S.results && one && !items[0].synthetic) e.push({ act: 'reveal', label: 'Show in folder', icon: 'folder' });
  if (canEdit) { e.push('-'); if (one) e.push({ act: 'rename', label: 'Rename', icon: 'edit' }); e.push({ act: 'move', label: 'Move to…', icon: 'move' }); e.push({ act: 'trash', label: 'Move to trash', icon: 'trash', danger: true }); }
  return e.filter((x) => x === '-' || !items.some((i) => i.synthetic && x.act === 'download' && !i.is_dir));
}

function uploadMenu(btn) {
  const r = btn.getBoundingClientRect();
  showMenu(r.left, r.bottom + 4, [
    { act: 'upload-files', label: 'Upload files…', icon: 'upload' },
    { act: 'upload-folder', label: 'Upload folder…', icon: 'folder' }, '-',
    { act: 'new-folder', label: 'New folder', icon: 'newfolder' }]);
}

/* ============================================================ Actions */
async function runAction(act, data = {}) {
  try {
    switch (act) {
      case 'open': { const [i] = selectedItems(); i && openItem(i.id); break; }
      case 'reveal': { const [i] = selectedItems(); if (i && i.parent_id) { clearSearchUI(); S.results = null; S.view = 'files'; renderNav(); renderTopbar(); loadFolder(i.parent_id); } break; }
      case 'upload-files': { const p = await dialogs.files(); if (p.length) await doUpload(p); break; }
      case 'upload-folder': { const p = await dialogs.folder('Choose a folder to upload'); if (p) await doUpload([p]); break; }
      case 'new-folder': {
        const name = await prompt({ title: 'New folder', label: 'Folder name', ok: 'Create' });
        if (!name) break;
        if (S.view !== 'files') await go('files');
        await api.mkdir(name, uploadTarget()); toast(`Created “${name}”`); await loadFolder(S.cwd, { keepSel: true }); break;
      }
      case 'download': await doDownload(); break;
      case 'rename': {
        const [i] = selectedItems(); if (!i) break;
        const name = await prompt({ title: 'Rename', label: 'New name', value: i.name, ok: 'Rename' });
        if (name && name !== i.name) { await api.rename(i.id, name); toast('Renamed'); await loadFolder(S.cwd, { keepSel: true }); }
        break;
      }
      case 'move': {
        const items = selectedItems(); if (!items.length) break;
        const dest = await pickFolder({ title: `Move ${plural(items.length, 'item')} to…`, exclude: items.map((i) => i.id) });
        if (!dest) break;
        const r = await api.move(items.map((i) => i.id), dest);
        r.failed.length ? toast(`${r.ok} moved, ${r.failed.length} failed: ${r.failed[0]}`, { kind: 'error' }) : toast(`Moved ${plural(r.ok, 'item')}`);
        S.sel.clear(); await loadFolder(S.cwd); break;
      }
      case 'trash': {
        const items = selectedItems(); if (!items.length) break;
        const ok = await confirmBox({ title: `Move ${plural(items.length, 'item')} to trash?`,
          body: `${items.length === 1 ? `<b>${esc(items[0].name)}</b>` : 'These items'} will move to Google Drive's trash. They keep using storage until the trash is emptied.`, ok: 'Move to trash', danger: true });
        if (!ok) break;
        const r = await api.trash(items.map((i) => i.id));
        r.failed.length ? toast(`${r.failed.length} couldn't be deleted: ${r.failed[0]}`, { kind: 'error' }) : toast(`Moved ${plural(r.ok, 'item')} to trash`);
        S.sel.clear();
        S.results ? await runSearch(S.query) : await loadFolder(S.cwd);
        refreshAccountsSoon(); break;
      }
      default: break;
    }
  } catch (e) { fail(e); }
}

async function doUpload(paths) {
  const n = await api.upload(paths, uploadTarget());
  toast(`Uploading ${plural(n, 'file')}…`, { action: { label: 'View', run: () => go('transfers') } });
}

async function doDownload() {
  const items = selectedItems(); if (!items.length) return;
  const dir = await dialogs.folder('Choose where to save');
  if (!dir) return;
  const n = await api.download(items.map((i) => i.id), dir);
  toast(`Downloading ${plural(n, 'file')}…`, { action: { label: 'Show', run: () => api.openPath(dir).catch(fail) } });
}

/* ============================================================ Search */
const runSearchDebounced = debounce((q) => runSearch(q), 250);
async function runSearch(q) {
  q = q.trim();
  if (!q) { clearSearchUI(); if (S.view === 'files') showFiles(); return; }
  S.query = q;
  try {
    const r = await api.search(q);
    if (S.query !== q) return;
    S.results = r; S.sel.clear();
    if (S.view !== 'files') { S.view = 'files'; renderNav(); renderTopbar(); }
    renderFiles();
  } catch (e) { fail(e); }
}
function clearSearchUI() {
  S.results = null; S.query = '';
  const i = $('#search'); if (i) i.value = '';
  $('#search-clear') && $('#search-clear').classList.add('hidden');
}

/* ============================================================ Preview */
function openPreview(item, list) {
  let idx = Math.max(0, list.findIndex((i) => i.id === item.id));
  const root = $('#modal-root');
  const lb = document.createElement('div');
  lb.className = 'lightbox'; lb.setAttribute('role', 'dialog'); lb.setAttribute('aria-modal', 'true');
  root.appendChild(lb);
  const close = () => { lb.remove(); document.removeEventListener('keydown', onKey, true); };
  const draw = async () => {
    const it = list[idx], kind = previewKind(it), url = mediaUrl(it.id);
    lb.innerHTML = `<div class="lb-top"><button class="icon-btn" data-lb="close" aria-label="Close">${icon('back')}</button>
        <div class="lb-title"><div class="ellipsis">${esc(it.name)}</div><div class="small" style="opacity:.7">${list.length > 1 ? `${idx + 1} / ${list.length} · ` : ''}${fmtBytes(it.size)} · ${fmtDate(it.modified, true)}</div></div>
        <button class="icon-btn" data-lb="download" aria-label="Download" title="Download">${icon('download')}</button></div>
      <div class="lb-stage" id="lb-stage">${list.length > 1 ? `<button class="icon-btn lb-nav prev" data-lb="prev" aria-label="Previous">${icon('left')}</button><button class="icon-btn lb-nav next" data-lb="next" aria-label="Next">${icon('right')}</button>` : ''}
        ${kind === 'image' ? `<img alt="${esc(it.name)}" src="${url}" id="lb-media">` : kind === 'video' ? `<video controls autoplay src="${url}" id="lb-media"></video>` : kind === 'audio' ? `<div class="lb-info">${kindIcon(it)}<audio controls autoplay src="${url}" id="lb-media"></audio></div>`
        : kind === 'text' ? '<pre id="lb-media">Loading…</pre>'
        : `<div class="lb-info">${kindIcon(it)}<div>${esc(it.name)}</div><div class="small" style="opacity:.7">No preview for this file type</div><button class="btn btn-primary" data-lb="download">${icon('download')}Download</button></div>`}</div>`;
    const media = $('#lb-media', lb);
    if (media) media.addEventListener('error', () => { const st = $('#lb-stage', lb); if (st && !$('.lb-err', st)) st.insertAdjacentHTML('beforeend', `<div class="lb-info lb-err"><div class="err-text">Couldn't load this file.</div></div>`); });
    if (kind === 'text') {
      try { const r = await fetch(url); const t = await r.text(); if (list[idx] === it && media) media.textContent = t.slice(0, 200000); }
      catch { if (media) media.textContent = 'Could not load the file.'; }
    }
    // Warm the neighbouring images so next/prev feels instant.
    for (const n of [list[idx + 1], list[idx - 1]]) if (n && previewKind(n) === 'image') new Image().src = mediaUrl(n.id);
  };
  const step = (d) => { if (list.length > 1) { idx = (idx + d + list.length) % list.length; draw(); } };
  const onKey = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); close(); }
    else if (e.key === 'ArrowRight') { step(1); e.preventDefault(); }
    else if (e.key === 'ArrowLeft') { step(-1); e.preventDefault(); }
  };
  document.addEventListener('keydown', onKey, true);
  lb.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-lb]');
    if (b) {
      const a = b.dataset.lb;
      if (a === 'close') close(); else if (a === 'prev') step(-1); else if (a === 'next') step(1);
      else if (a === 'download') {
        const dir = await dialogs.folder('Choose where to save').catch(() => null);
        if (dir) api.download([list[idx].id], dir).then(() => toast('Downloading…')).catch(fail);
      }
    } else if (e.target === lb || e.target.id === 'lb-stage') close();
  });
  draw();
}

/* ============================================================ Global wiring */
function wireGlobalEvents() {
  const root = document.body;

  // images: reveal on load, drop the broken ones so the icon underneath shows
  root.addEventListener('load', (e) => {
    const t = e.target;
    if (t.tagName !== 'IMG') return;
    if (t.hasAttribute('data-thumb')) t.style.opacity = '1';
    else if (t.hasAttribute('data-thumb-card')) t.classList.add('ok');
  }, true);
  root.addEventListener('error', (e) => { if (e.target.tagName === 'IMG' && e.target.hasAttribute('data-thumb-card')) e.target.remove(); else if (e.target.tagName === 'IMG' && e.target.hasAttribute('data-thumb')) e.target.remove(); }, true);

  root.addEventListener('click', (e) => {
    const t = e.target;
    const view = t.closest('[data-view]'); if (view) return go(view.dataset.view);
    const ext = t.closest('[data-external]'); if (ext) return api.openExternal(ext.dataset.external).catch(fail);
    const open = t.closest('[data-open]'); if (open) { S.results = null; return loadFolder(open.dataset.open); }
    const sort = t.closest('[data-sort]');
    if (sort) { const k = sort.dataset.sort; S.sort = { key: k, dir: S.sort.key === k ? -S.sort.dir : 1 }; store('sort', S.sort); return renderFiles(); }
    const ph = t.closest('[data-photo]'); if (ph) return openItem(ph.dataset.photo);
    const br = t.closest('[data-browse]'); if (br) { S.view = 'files'; renderNav(); renderTopbar(); S.results = null; return loadFolder(br.dataset.browse); }
    const row = t.closest('#file-list [data-id]');
    if (row) return selectItem(row.dataset.id, e);
    const a = t.closest('[data-action]');
    if (a) return handleAction(a, e);
    if (t.id === 'retry-files') return loadFolder(S.cwd);
    if (t.id === 'retry-photos') return showPhotos();
    if (t.id === 'retry-trash') return showTrash();
    if (t.closest('.page-head, .menu, .banner')) return;
    if (t.closest('#content') && !t.closest('.row,.card') && S.sel.size) { S.sel.clear(); updateSelectionUI(); }
    if (!t.closest('.menu')) closeMenu();
  });
  root.addEventListener('dblclick', (e) => { const r = e.target.closest('#file-list [data-id]'); if (r) openItem(r.dataset.id); });
  root.addEventListener('contextmenu', (e) => {
    const r = e.target.closest('#file-list [data-id]');
    if (!r) return;
    e.preventDefault();
    if (!S.sel.has(r.dataset.id)) { S.sel = new Set([r.dataset.id]); S.anchor = r.dataset.id; updateSelectionUI(); }
    const entries = contextEntries(); if (entries.length) showMenu(e.clientX, e.clientY, entries);
  });

  // settings controls
  root.addEventListener('change', (e) => {
    const id = e.target.id;
    if (id === 'set-policy') saveSetting({ upload_policy: e.target.value, pinned_account: e.target.value === 'pinned' ? (S.accounts[0] || {}).email : null });
    else if (id === 'set-pinned') saveSetting({ upload_policy: 'pinned', pinned_account: e.target.value });
    else if (id === 'set-interval') saveSetting({ sync_interval_secs: +e.target.value });
    else if (id === 'set-cache') saveSetting({ cache_size_mb: +e.target.value });
  });

  const search = $('#search');
  search.addEventListener('input', () => { $('#search-clear').classList.toggle('hidden', !search.value); runSearchDebounced(search.value); });
  search.addEventListener('keydown', (e) => { if (e.key === 'Escape') { search.blur(); handleAction({ dataset: { action: 'clear-search' } }); } });

  document.addEventListener('keydown', onKeydown);
  wireBackendEvents();
}

function handleAction(el, ev) {
  const a = el.dataset.action, d = el.dataset;
  switch (a) {
    case 'toggle-theme': S.theme = S.theme === 'light' ? 'dark' : 'light'; store('theme', S.theme); applyTheme(); renderTopbar(); break;
    case 'toggle-layout': S.layout = S.layout === 'list' ? 'grid' : 'list'; store('layout', S.layout); renderTopbar(); renderFiles(); break;
    case 'clear-sel': S.sel.clear(); updateSelectionUI(); break;
    case 'clear-search': clearSearchUI(); if (S.view === 'files') showFiles(); break;
    case 'upload-menu': uploadMenu(ev && ev.target ? ev.target.closest('button') : $('#new-btn')); break;
    case 'add-account': openAddAccount(); break;
    case 'reconnect': openAddAccount({ reconnect: d.email }); break;
    case 'sync-account': api.sync(d.email).then(() => toast('Checking for changes…')).catch(fail); break;
    case 'refresh-accounts': refreshAccounts(true).then(() => api.sync(null)); break;
    case 'remove-account': removeAccount(d.email); break;
    case 'new-folder': case 'download': case 'rename': case 'move': case 'trash': runAction(a); break;
    case 'cancel-transfer': api.cancelTransfer(d.id).catch(fail); break;
    case 'retry-transfer': api.retryTransfer(d.id).catch(fail); break;
    case 'clear-transfers': api.clearTransfers().then(() => { for (const [k, v] of S.transfers) if (isFinished(v)) S.transfers.delete(k); renderTransfers(); renderNav(); }); break;
    case 'restore-trashed': case 'delete-trashed': trashRowAction(a, el); break;
    case 'empty-trash': emptyTrash(); break;
    case 'clear-cache': api.clearCache().then(() => { toast('Cache cleared'); showSettings(); }).catch(fail); break;
    case 'edit-credentials': S.setupStep = 1; go('setup'); break;
    case 'setup-next': S.setupStep = 2; showSetup(); break;
    case 'setup-back': S.setupStep = 1; showSetup(); break;
    case 'setup-done': S.setupStep = 0; go('files'); break;
    case 'save-credentials': saveCredentials(); break;
    case 'import-credentials': importCredentials(); break;
    default: break;
  }
}

async function removeAccount(email) {
  const ok = await confirmBox({ title: `Remove ${email}?`, danger: true, ok: 'Remove',
    body: `Cloudy will forget this account and its index. <b>Nothing is deleted from Google</b> – the files stay in that account; they just disappear from your pool.` });
  if (!ok) return;
  try { await api.removeAccount(email); toast(`Removed ${email}`); await refreshAccounts(true); S.items = []; if (S.view === 'files') showFiles(); } catch (e) { fail(e); }
}

async function trashRowAction(a, el) {
  const row = el.closest('[data-t]'); if (!row) return;
  const [account, id] = row.dataset.t.split('|');
  const item = (S.trash || []).find((i) => i.account === account && i.id === id);
  try {
    if (a === 'restore-trashed') { await api.restore(account, id); toast(`Restored “${item ? item.name : 'item'}”`); }
    else {
      if (!(await confirmBox({ title: 'Delete forever?', body: `<b>${esc(item ? item.name : 'This item')}</b> will be permanently deleted. This can't be undone.`, ok: 'Delete forever', danger: true }))) return;
      await api.deleteForever(account, id); toast('Deleted forever'); refreshAccountsSoon();
    }
    S.trash = (S.trash || []).filter((i) => !(i.account === account && i.id === id)); renderTrash();
  } catch (e) { fail(e); }
}

async function emptyTrash() {
  const total = (S.trash || []).reduce((s, i) => s + i.size, 0);
  if (!(await confirmBox({ title: 'Empty trash in all accounts?', ok: 'Empty trash', danger: true,
    body: `Everything in the trash will be <b>permanently deleted</b>${total ? `, freeing ${fmtBytes(total)}` : ''}. This can't be undone.` }))) return;
  try { await api.emptyTrash(null); toast('Trash emptied'); S.trash = []; renderTrash(); refreshAccountsSoon(); } catch (e) { fail(e); }
}

function onKeydown(e) {
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
  if ($('#modal-root').childElementCount) return;
  const mod = e.ctrlKey || e.metaKey;
  if (e.key === '/' && !typing) { e.preventDefault(); $('#search').focus(); return; }
  if (typing) return;
  if (e.key === 'Escape') { closeMenu(); if (S.sel.size) { S.sel.clear(); updateSelectionUI(); } return; }
  if (mod && e.key.toLowerCase() === 'u') { e.preventDefault(); runAction('upload-files'); return; }
  if (S.view !== 'files') return;
  if (mod && e.key.toLowerCase() === 'a') { e.preventDefault(); S.sel = new Set(sortedItems().map((i) => i.id)); updateSelectionUI(); }
  else if (e.key === 'Delete' && S.sel.size) runAction('trash');
  else if (e.key === 'F2' && S.sel.size === 1) runAction('rename');
  else if (e.key === 'Enter' && S.sel.size === 1) runAction('open');
  else if (e.key === 'Backspace' && !S.results && S.node && S.node.parent_id && S.node.parent_id !== 'root') { e.preventDefault(); loadFolder(S.node.parent_id); }
  else if (['ArrowDown', 'ArrowUp'].includes(e.key)) {
    const order = sortedItems().map((i) => i.id); if (!order.length) return;
    const cur = order.indexOf(S.anchor);
    const next = order[Math.min(order.length - 1, Math.max(0, cur + (e.key === 'ArrowDown' ? 1 : -1)))];
    e.preventDefault(); selectItem(next, e);
    const el = $(`#file-list [data-id="${CSS.escape(next)}"]`); el && el.scrollIntoView({ block: 'nearest' });
  }
}

/* ---- events from the Rust side ---- */
function wireBackendEvents() {
  const treeChanged = debounce(() => {
    if (S.view === 'files' && !S.results && !S.loading) loadFolder(S.cwd, { keepSel: true });
    else if (S.view === 'photos' && S.photos.items.length) loadPhotoPage(true);
    else if (S.view === 'photos') showPhotos();
  }, 400);
  const accountsChanged = debounce(() => refreshAccounts(true), 300);

  events.engine((ev) => {
    switch (ev.type) {
      case 'sync': {
        const was = S.sync[ev.account] && statusKey(S.sync[ev.account].status);
        S.sync[ev.account] = { status: ev.status, files: ev.files_indexed };
        renderTopbar(); renderNav(); renderBanner();
        if (S.view === 'accounts') renderAccountsSoon();
        if (was === 'Syncing' && statusKey(ev.status) === 'Idle') refreshAccountsSoon();
        break;
      }
      case 'tree_changed': treeChanged(); break;
      case 'accounts_changed': accountsChanged(); break;
      case 'transfer': {
        const prev = S.transfers.get(ev.id);
        const { type, ...info } = ev;
        S.transfers.set(ev.id, info);
        const k = statusKey(info.status);
        if (prev && !isFinished(prev) && k === 'Failed') toast(`${info.file_name}: ${statusMsg(info.status)}`, { kind: 'error', action: info.direction === 'Upload' ? { label: 'Retry', run: () => api.retryTransfer(info.id) } : null });
        if (prev && !isFinished(prev) && k === 'Completed') { refreshAccountsSoon(); }
        renderNav();
        if (S.view === 'transfers') renderTransfersSoon();
        break;
      }
      default: break;
    }
  });

  // drag & drop of files from the desktop
  events.drag((kind, p) => {
    const dz = $('#dropzone');
    if (kind === 'enter' || kind === 'over') {
      if (!S.hasCreds || !S.accounts.length) return;
      $('#dropzone-sub').textContent = uploadTarget() === 'pool' ? 'into your pool' : `into “${S.node ? S.node.name : 'this folder'}”`;
      dz.classList.remove('hidden');
    } else if (kind === 'leave') dz.classList.add('hidden');
    else if (kind === 'drop') {
      dz.classList.add('hidden');
      if (p.paths && p.paths.length && S.accounts.length) doUpload(p.paths).catch(fail);
    }
  });
}
const renderAccountsSoon = debounce(renderAccounts, 120);

init();
