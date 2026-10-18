// Drives the real frontend (main.js) in jsdom against a mocked Tauri backend.
import { JSDOM } from 'jsdom';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const FE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../frontend');
const scenario = process.argv[2] || 'main';
const html = fs.readFileSync(`${FE}/index.html`, 'utf8').replace(/<script[^>]*><\/script>/, '');
const dom = new JSDOM(html, { url: 'http://localhost/', pretendToBeVisual: true, runScripts: 'outside-only' });
const w = dom.window;
Object.assign(globalThis, { window: w, document: w.document, localStorage: w.localStorage, Image: w.Image,
  innerWidth: 1200, innerHeight: 800, requestAnimationFrame: w.requestAnimationFrame.bind(w), CSS: { escape: (s) => String(s).replace(/(["\\])/g, '\\$1') },
  IntersectionObserver: class { observe() {} disconnect() {} unobserve() {} } });
Object.defineProperty(globalThis, 'navigator', { value: w.navigator, configurable: true });
w.HTMLElement.prototype.scrollIntoView = () => {};

// ---------------------------------------------------------------- mock backend
const now = Date.now();
const N = (id, name, o = {}) => ({ id, name, is_dir: false, size: 1234, mime_type: 'text/plain', modified: now - 864e5, read_only: false,
  workspace: false, accounts: ['a@x.com'], has_thumb: false, child_count: 0, parent_id: 'pool', synthetic: false, ...o });
const root = N('root', 'Cloudy', { is_dir: true, synthetic: true, parent_id: null });
const pool = N('pool', 'Pool', { is_dir: true, synthetic: true, parent_id: 'root' });
const photos = N('pool_f1', 'Photos', { is_dir: true, accounts: ['a@x.com', 'b@x.com'], child_count: 2 });
const evil = N('pool_evil', '<img src=x onerror=alert(1)>.txt');
const nodes = {
  pool: [photos, N('pool_f2', 'notes.txt'), N('pool_img', 'cat.jpg', { mime_type: 'image/jpeg', has_thumb: true, size: 5e6 }), evil, N('pool_vid', 'clip.mp4', { mime_type: 'video/mp4', size: 9e7 })],
  pool_f1: [N('pool_img2', 'dog.jpg', { mime_type: 'image/jpeg', has_thumb: true, parent_id: 'pool_f1' }), N('pool_img3', 'bird.png', { mime_type: 'image/png', has_thumb: true, parent_id: 'pool_f1' })],
};
const byId = Object.fromEntries([root, pool, photos, ...nodes.pool, ...nodes.pool_f1].map((n) => [n.id, n]));
const GB = 1024 ** 3;
const accounts = [
  { email: 'a@x.com', display_name: 'Alice', quota: { limit: 15 * GB, usage: 4 * GB, usage_in_drive: 3 * GB }, sync: 'Idle', files_indexed: 120, error: null },
  { email: 'b@x.com', display_name: null, quota: { limit: 15 * GB, usage: 14 * GB, usage_in_drive: 1 }, sync: 'NeedsReauth', files_indexed: 5, error: null },
];
const settings = { sync_interval_secs: 60, cache_size_mb: 2048, cache_used_mb: 12, upload_policy: 'most_free', pinned_account: null, secure_storage: false, client_id_hint: '123456789-abc…', version: '0.1.0' };
const calls = [];
const listeners = {};
let addAccountResolve;
const hasCreds = scenario !== 'nocreds';
const handlers = {
  cmd_boot: () => ({ version: '0.1.0', has_credentials: hasCreds, media_base: 'http://127.0.0.1:1/tok', accounts, settings }),
  cmd_get_accounts: () => accounts,
  cmd_view: ({ id }) => {
    const node = byId[id]; if (!node) throw { code: 'not_found', message: 'gone' };
    const crumbs = id === 'pool' ? [root, pool] : [root, pool, node];
    return { node, breadcrumbs: crumbs, children: nodes[id] || [] };
  },
  cmd_search: ({ query }) => nodes.pool.filter((n) => n.name.includes(query)).map((n) => ({ ...n, path: `/Pool/${n.name}` })),
  cmd_list_media: ({ offset }) => offset ? [] : [...nodes.pool_f1, nodes.pool[2]],
  cmd_transfers: () => [], cmd_get_settings: () => settings,
  cmd_list_trash: () => [{ account: 'a@x.com', id: 't1', name: 'old.zip', size: 3e8, mime_type: 'application/zip', modified_time: '', is_folder: false }],
  cmd_add_account: () => new Promise((res) => { addAccountResolve = res; }),
  cmd_update_settings: ({ update }) => { Object.assign(settings, update); return settings; },
  cmd_set_credentials: () => null,
};
w.__TAURI__ = {
  core: { invoke: async (cmd, args) => { calls.push([cmd, args]); if (!(cmd in handlers)) return null; return handlers[cmd](args || {}); } },
  event: { listen: async (name, cb) => { (listeners[name] ||= []).push(cb); return () => {}; } },
  dialog: { open: async () => null },
};
const emit = (name, payload) => (listeners[name] || []).forEach((cb) => cb({ payload }));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];
const waitFor = async (what, fn, ms = 3000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = fn(); if (v) return v; await sleep(20); } throw new Error('timeout: ' + what); };
const click = (el) => el.dispatchEvent(new w.MouseEvent('click', { bubbles: true, cancelable: true, ctrlKey: false }));
const dbl = (el) => el.dispatchEvent(new w.MouseEvent('dblclick', { bubbles: true }));
const called = (cmd) => calls.filter(([c]) => c === cmd);
const errors = [];
w.addEventListener('error', (e) => errors.push(e.error || e.message));
process.on('unhandledRejection', (e) => { console.error('UNHANDLED', e); process.exit(2); });

await import(`${FE}/main.js`);
let step = 0; const ok = (m) => console.log(`  ✓ ${++step}. ${m}`);

if (scenario === 'nocreds') {
  await waitFor('setup screen', () => $('.onboard h1'));
  assert.match($('.onboard h1').textContent, /connect Cloudy to Google/);
  assert.equal($$('[data-external]').length, 4, 'four console links'); ok('first run shows the setup wizard with Google console links');
  click($('[data-action="setup-next"]'));
  await waitFor('step 2', () => $('#cid'));
  $('#cid').value = '1-x.apps.googleusercontent.com'; $('#csec').value = 'sec';
  click($('[data-action="save-credentials"]'));
  await waitFor('credentials saved', () => called('cmd_set_credentials').length);
  assert.deepEqual(called('cmd_set_credentials')[0][1], { clientId: '1-x.apps.googleusercontent.com', clientSecret: 'sec' });
  ok('credentials are sent to the backend');
  console.log('nocreds scenario passed'); process.exit(0);
}

await waitFor('app visible', () => !$('#app').classList.contains('hidden'));
await waitFor('rows', () => $$('#file-list .row').length);
assert.equal($$('#nav .nav-item').length, 6); ok('app boots, sidebar has 6 destinations');
assert.equal($$('#file-list .row').length, 5);
const names = $$('#file-list .row .t').map((e) => e.textContent.trim());
assert.equal(names[0], 'Photos', 'folders sort first'); ok(`files listed, folders first: ${names.slice(0, 3).join(', ')}…`);
assert.equal($('#file-list img[src="x"]'), null, 'file names must be escaped'); ok('hostile file name is rendered as text, not HTML (XSS-safe)');
assert.match($('#pool-meter').textContent, /free\s+of/); ok(`pool meter: ${$('#pool-meter').textContent.replace(/\s+/g, ' ').trim()}`);
assert.match($('#banner').textContent, /b@x\.com/); ok('banner warns that b@x.com needs sign-in');

// selection
click($$('#file-list .row')[1]);
assert.match($('.selbar').textContent, /1 selected/); ok('click selects, selection bar appears');
const ctl = new w.MouseEvent('click', { bubbles: true, ctrlKey: true });
$$('#file-list .row')[2].dispatchEvent(ctl);
assert.match($('.selbar').textContent, /2 selected/); ok('ctrl+click extends the selection');
document.dispatchEvent(new w.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
assert.equal($('.selbar'), null); ok('Escape clears the selection');

// navigation
dbl($$('#file-list .row')[0]);
await waitFor('folder opened', () => called('cmd_view').some(([, a]) => a.id === 'pool_f1'));
await waitFor('breadcrumb', () => $$('.crumb').some((c) => c.textContent === 'Photos'));
assert.equal($$('#file-list .row').length, 2); ok('double-click opens a folder; breadcrumbs show All files › Photos');
click($('.crumb[data-open="pool"]'));
await waitFor('back at pool', () => $$('#file-list .row').length === 5); ok('breadcrumb navigates back');

// layout
click($('[data-action="toggle-layout"]'));
assert.ok($('.grid') && $$('.card').length === 5); ok('grid layout renders cards');
assert.ok($$(".card img[data-thumb-card]").length === 1); assert.match($('.card img').src, /127\.0\.0\.1:1\/tok\/thumb\//); ok('thumbnails point at the loopback media server');
click($('[data-action="toggle-layout"]'));

// search
const s = $('#search'); s.value = 'cat'; s.dispatchEvent(new w.Event('input', { bubbles: true }));
await waitFor('search results', () => called('cmd_search').length && $('.page-title'));
assert.match($('.page-title').textContent, /1 result for “cat”/); ok('search shows results with count');
click($('[data-action="clear-search"]')); await waitFor('search cleared', () => $$('#file-list .row').length === 5); ok('clearing search restores the folder');

// preview
click($$('#file-list .row').find((r) => r.textContent.includes('cat.jpg'))); dbl($$('#file-list .row').find((r) => r.textContent.includes('cat.jpg')));
await waitFor('lightbox', () => $('.lightbox img'));
assert.match($('.lightbox img').src, /\/media\/pool_img$/); ok('double-click on an image opens the lightbox with a media-server URL');
document.dispatchEvent(new w.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
assert.equal($('.lightbox'), null); ok('Escape closes the lightbox');

// photos
click($('[data-view="photos"]'));
await waitFor('photos', () => $$('.photo').length === 3); assert.ok($('.month')); ok('Photos view lists photos grouped by month');
click($$('.photo')[0]); await waitFor('photo lightbox', () => $('.lightbox'));
assert.match($('.lb-title').textContent, /1 \/ 3/);
document.dispatchEvent(new w.KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
assert.match($('.lb-title').textContent, /2 \/ 3/); ok('arrow keys move through photos');
document.dispatchEvent(new w.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));

// accounts + add-account flow
click($('[data-view="accounts"]'));
await waitFor('account cards', () => $$('.acct[data-account]').length === 2);
assert.match($('.pool-hero').textContent, /free/); assert.match(document.body.textContent, /Reconnect/); ok('Accounts: pool total, per-account cards, Reconnect for the signed-out one');
click($('.acct.add'));
await waitFor('sign-in dialog', () => $('.scrim .modal'));
emit('cloudy://auth-url', 'https://accounts.google.com/o/oauth2/v2/auth?x=1');
await waitFor('link shown', () => $('.linkbox'));
assert.match($('.modal').textContent, /Waiting for you to finish/); ok('add-account dialog shows the sign-in link (copyable) while waiting');
assert.match($('.modal .tips').textContent, /Something went wrong/); assert.match($('.modal .tips').textContent, /Chromium/); ok('dialog explains what to do when Google shows “Something went wrong”');
$('.scrim').dispatchEvent(new w.MouseEvent('mousedown', { bubbles: true }));
assert.ok($('.scrim .modal'), 'a click on the backdrop must not close the dialog'); assert.equal(called('cmd_cancel_add_account').length, 0);
ok('clicking the dimmed backdrop does not cancel a pending sign-in');
addAccountResolve({ email: 'c@x.com', display_name: 'C', already_linked: false });
await waitFor('dialog closed', () => !$('.scrim'));
assert.match(document.body.textContent, /Added c@x\.com/); ok('successful sign-in closes the dialog and confirms');
click($('.acct.add')); await waitFor('dialog 2', () => $('.scrim .modal'));
click($('[data-m="cancel"]')); await waitFor('cancelled', () => !$('.scrim'));
assert.equal(called('cmd_cancel_add_account').length >= 1, true); ok('cancel aborts the pending sign-in on the backend');

// transfers
emit('cloudy://event', { type: 'transfer', id: 't1', file_name: 'movie.mkv', account: 'a@x.com', progress_bytes: 50, total_bytes: 100, status: 'InProgress', direction: 'Upload' });
assert.match($('#nav [data-view="transfers"]').textContent, /1/);
click($('[data-view="transfers"]')); await waitFor('transfer row', () => $('.titem'));
assert.match($('.titem').textContent, /movie\.mkv/); assert.equal($('.titem .bar i').style.width, '50%'); ok('transfers: live progress bar and nav badge');
emit('cloudy://event', { type: 'transfer', id: 't1', file_name: 'movie.mkv', account: 'a@x.com', progress_bytes: 50, total_bytes: 100, status: { Failed: 'No space' }, direction: 'Upload' });
await sleep(60); assert.match(document.body.textContent, /No space/); assert.ok($('[data-action="retry-transfer"]')); ok('failed transfer shows the reason and a Retry button');

// trash
click($('[data-view="trash"]')); await waitFor('trash rows', () => $('.trash-row'));
assert.match(document.body.textContent, /still use storage/); ok('trash lists items and explains it still uses quota');
click($('[data-action="empty-trash"]')); await waitFor('confirm', () => $('.scrim .modal'));
assert.match($('.modal').textContent, /permanently deleted/); click($('[data-m="cancel"]')); ok('emptying trash asks for confirmation');

// settings
click($('[data-view="settings"]')); await waitFor('settings', () => $('#set-policy'));
const sel = $('#set-policy'); sel.value = 'round_robin'; sel.dispatchEvent(new w.Event('change', { bubbles: true }));
await waitFor('saved', () => called('cmd_update_settings').length);
assert.equal(called('cmd_update_settings')[0][1].update.upload_policy, 'round_robin'); ok('changing a setting calls the backend immediately');

// sync status event
emit('cloudy://event', { type: 'sync', account: 'a@x.com', status: 'Syncing', files_indexed: 4200 });
assert.match($('.sync-pill').textContent, /Indexing… 4,200 files/); ok('top bar shows live indexing progress from engine events');
emit('cloudy://event', { type: 'sync', account: 'a@x.com', status: { Error: 'boom' }, files_indexed: 4200 });
assert.match($('.sync-pill').className, /err|warn/); ok('sync errors are surfaced in the top bar');

assert.deepEqual(errors, [], 'no uncaught UI errors');
console.log(`\nAll ${step} frontend checks passed`);
process.exit(0);
