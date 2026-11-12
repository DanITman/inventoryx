// InventoryX — application logic.
// GPL-3.0-or-later.
'use strict';

const $ = id => document.getElementById(id);

const prefs = {
  get(key, fallback) {
    try { const v = localStorage.getItem('ix.' + key); return v == null ? fallback : JSON.parse(v); }
    catch { return fallback; }
  },
  set(key, value) { try { localStorage.setItem('ix.' + key, JSON.stringify(value)); } catch { /* optional */ } },
};

const S = {
  status: null,
  scan: null,          // summary of the loaded scan
  kinds: { names: [], colors: [] },
  zoom: 0,             // node id the treemap shows
  sel: null,           // { node, path, real_path, ancestors }
  chain: [],           // ids from the scan root to the selection
  hoverCell: -1,
  highlight: null,     // kind id
  showStart: false,
  physical: prefs.get('physical', true),
  free: prefs.get('free', false),
  other: prefs.get('other', false),
  mapToken: 0,
};

async function api(path, params, body) {
  const qs = params ? '?' + new URLSearchParams(params) : '';
  const init = body === undefined ? {} : {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': window.IX_CSRF || '' },
    body: JSON.stringify(body),
  };
  const res = await fetch('api/' + path + qs, init);
  if (res.status === 401 || res.redirected) throw new Error('Your Unraid session has ended. Reload the page to log in.');
  if (path === 'treemap' && res.ok) return res.arrayBuffer();
  let data = null;
  try { data = await res.json(); } catch { /* not JSON */ }
  if (!res.ok) throw new Error((data && data.error) || `Request failed (${res.status})`);
  // Unraid answers a refused request with an empty page; never mistake that for success.
  if (data == null) throw new Error('The server did not accept the request. Reload the page and try again.');
  return data;
}

function fmtSize(bytes) {
  if (bytes < 1000) return `${bytes} bytes`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let v = bytes, u = -1;
  do { v /= 1000; u++; } while (v >= 999.5 && u < units.length - 1);
  return `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${units[u]}`;
}
const fmtCount = n => Number(n).toLocaleString();
const nodeSize = n => S.physical ? n.physical : n.logical;
const css = c => c ? `rgb(${c.map(v => Math.round(Math.min(1, v) * 255)).join(',')})` : '#999';
const kindColor = k => css(S.kinds.colors[k]);
const kindName = k => S.kinds.names[k] || 'Other';

function ago(unixSeconds) {
  const s = Math.max(0, Date.now() / 1000 - unixSeconds);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} days ago`;
}

function show(view) {
  for (const v of ['start', 'scanning', 'doc']) $(v).hidden = v !== view;
}

// ---------- start screen ----------

async function loadTargets() {
  const box = $('targets');
  try {
    const { targets } = await api('targets');
    box.textContent = '';
    if (!targets.length) {
      const p = document.createElement('p');
      p.className = 'dim';
      p.textContent = 'No disks are mounted. If the array is stopped, start it, or enter a folder below.';
      box.appendChild(p);
    }
    let group = null, cards = null;
    for (const t of targets) {
      if (t.group !== group) {
        group = t.group;
        const h = document.createElement('h2');
        h.className = 'group-title';
        h.textContent = group;
        cards = document.createElement('div');
        cards.className = 'cards';
        box.append(h, cards);
      }
      const card = document.createElement('button');
      card.className = 'card';
      const name = document.createElement('span');
      name.className = 'card-name';
      name.textContent = t.label;
      const detail = document.createElement('span');
      detail.className = 'card-detail';
      card.append(name, detail);
      if (t.total > 0) {
        const used = t.total - t.free;
        detail.textContent = `${fmtSize(used)} used of ${fmtSize(t.total)}`;
        const meter = document.createElement('div');
        meter.className = 'meter' + (used / t.total > 0.9 ? ' high' : '');
        const fill = document.createElement('div');
        fill.style.width = (used / t.total * 100).toFixed(1) + '%';
        meter.appendChild(fill);
        card.appendChild(meter);
        card.title = t.detail;
      } else {
        detail.textContent = t.detail;
      }
      card.addEventListener('click', () => startScan(t.id, t.label));
      cards.appendChild(card);
    }
  } catch (e) {
    startError(e.message);
  }
}

function startError(message) {
  $('start-error').textContent = message || '';
  $('start-error').hidden = !message;
}

async function startScan(target, label) {
  if (S.scan && S.scan.target !== target &&
      !confirm(`Scan ${label || target}? This replaces the current scan of ${S.scan.label}.`)) return;
  startError('');
  try {
    await api('scan', null, { target });
    S.showStart = false;
    S.scan = null;
    await poll(true);
  } catch (e) {
    S.showStart = true;
    show('start');
    startError(e.message);
  }
}

// ---------- polling ----------

let pollTimer = null;
async function poll(now) {
  clearTimeout(pollTimer);
  let delay = 5000;
  try {
    const st = await api('status');
    S.status = st;
    await applyStatus(st);
    if (st.state === 'scanning') delay = 400;
  } catch (e) {
    // Nothing else is on screen yet when the very first request fails.
    if (!S.status) show('start');
    startError(e.message);
    lastError = e.message;
  }
  pollTimer = setTimeout(poll, delay);
}

let lastState = null, lastError = '';

function fmtDuration(ms) {
  const s = Math.round(ms / 1000);
  if (s < 90) return `${s} s`;
  if (s < 5400) return `${Math.round(s / 60)} min`;
  return `${Math.floor(s / 3600)} h ${Math.round(s % 3600 / 60)} min`;
}

function showProgress(p) {
  const bar = $('scan-bar');
  const known = p.fraction != null;
  bar.classList.toggle('indeterminate', !known);
  bar.style.width = known ? (p.fraction * 100).toFixed(1) + '%' : '';
  let text = '';
  if (p.cancelling) text = 'Stopping…';
  else if (known) {
    text = `${Math.floor(p.fraction * 100)}%`;
    text += p.eta_ms == null ? ' · estimating time left…'
      : p.eta_ms < 5000 ? ' · almost done' : ` · about ${fmtDuration(p.eta_ms)} left`;
  } else {
    text = 'Scanning a folder, so the total is not known in advance.';
  }
  $('scan-eta').textContent = text;

  const box = $('scan-sources');
  const rows = p.sources.length > 1 || (p.sources[0] && p.sources[0].fraction != null) ? p.sources : [];
  if (box.childElementCount !== rows.length * 3) {
    box.textContent = '';
    for (const s of rows) {
      const name = document.createElement('span');
      name.textContent = s.label;
      const meter = document.createElement('div');
      meter.className = 'meter';
      meter.appendChild(document.createElement('div'));
      const num = document.createElement('span');
      num.className = 'num';
      box.append(name, meter, num);
    }
  }
  rows.forEach((s, i) => {
    const meter = box.children[i * 3 + 1], num = box.children[i * 3 + 2];
    meter.firstChild.style.width = s.fraction == null ? '0' : (s.fraction * 100).toFixed(1) + '%';
    meter.style.visibility = s.fraction == null ? 'hidden' : '';
    num.textContent = s.fraction != null && s.fraction >= 1 ? 'done'
      : `${fmtCount(s.entries)} items · ${fmtSize(s.bytes)}`;
  });
}
async function applyStatus(st) {
  if (st.state === 'scanning') {
    show('scanning');
    const p = st.progress;
    $('scan-title').textContent = `Scanning ${p.label}…`;
    $('scan-files').textContent = fmtCount(p.files);
    $('scan-dirs').textContent = fmtCount(p.dirs);
    $('scan-bytes').textContent = fmtSize(p.bytes);
    $('scan-time').textContent = fmtDuration(p.elapsed_ms);
    $('scan-current').textContent = p.current;
    showProgress(p);
  } else if (st.state === 'ready') {
    if (!S.scan || S.scan.id !== st.scan.id) await openScan(st.scan);
    else { S.scan = st.scan; updateStatusBar(); }
    if (!S.showStart) show('doc');
    else show('start');
    if (lastState !== 'ready' && !S.showStart) requestAnimationFrame(loadMap);
  } else {
    S.scan = null;
    closeDeleteDialog();
    show('start');
    if (lastState !== 'idle') loadTargets();
    // Only replace the message when the server's own error changes, so a message from
    // the last click is not wiped by the next poll.
    if ((st.error || '') !== lastError) { lastError = st.error || ''; startError(lastError); }
  }
  $('back-to-scan').hidden = !S.scan;
  lastState = st.state;
}

// ---------- document ----------

const map = new Treemap($('map-canvas'), $('map-overlay'));
const outline = new Outline($('outline'), {
  fetchChildren: (id, offset, limit) => api('children', { node: id, offset, limit }),
  onSelect: node => select(node.id, 'outline'),
  onOpen: node => { if (node.dir) zoomTo(node.id); },
  onContext: (node, e) => showMenu(e.clientX, e.clientY, node.id, 'outline'),
  sizeText: n => fmtSize(nodeSize(n)),
  kindName, kindColor,
});

async function openScan(scan) {
  // A confirmation for the previous scan must not survive into this one.
  closeDeleteDialog();
  S.scan = scan;
  S.zoom = 0;
  S.sel = null;
  S.chain = [];
  S.highlight = null;
  S.kinds = await api('kinds');
  const root = await api('node', { id: 0 });
  await outline.setRoot(root.node);
  $('tb-browse').hidden = S.status.platform !== 'unraid';
  updateNotice();
  await setCrumbs();
  updateStatusBar();
  updateButtons();
  loadStats();
  requestAnimationFrame(loadMap);
}

const NO_TRASH = 'Unraid has no trash. This will be gone for good and cannot be undone.';

function updateNotice() {
  const s = S.scan;
  if (!s) return;
  const lines = [];
  if (s.zfs) lines.push('This scan includes ZFS, which InventoryX has not been tested on. Sizes and free space may be wrong, and snapshots are not counted.');
  const n = s.unreadable;
  if (n) lines.push(`${fmtCount(n)} folder${n === 1 ? '' : 's'} could not be read and ${n === 1 ? 'is' : 'are'} not counted.`);
  const age = Date.now() / 1000 - s.finished;
  if (s.from_disk && age > 3600) lines.push(`This is the saved scan from ${ago(s.finished)}. Refresh to see the current state.`);
  else if (age > 86400) lines.push(`This scan is from ${ago(s.finished)}. Refresh to see the current state.`);
  if (s.save_error) lines.push(s.save_error);
  $('notice').hidden = !lines.length;
  $('notice').textContent = lines.join('  ');
}

function updateStatusBar() {
  const s = S.scan;
  if (!s) return;
  updateNotice();
  $('status-scan').textContent =
    `${fmtCount(s.files)} files · ${fmtSize(S.physical ? s.physical : s.logical)} · scanned ${ago(s.finished)} ` +
    `in ${(s.duration_ms / 1000).toFixed(1)} s · index ${fmtSize(s.memory)}` +
    (s.saved_to ? ' · saved' : '');
  $('status-scan').title = s.saved_to ? `Saved in ${s.saved_to}, so it is still here after a restart.` : '';
  let text = '';
  if (S.sel) {
    const n = S.sel.node;
    text = `${S.sel.path}  —  ${fmtSize(nodeSize(n))}`;
    text += n.dir ? `, ${fmtCount(n.files)} file${n.files === 1 ? '' : 's'}` : `, ${kindName(n.kind)}`;
    if (n.source && s.sources.length > 1) text += `, on ${n.source}`;
  }
  $('status-sel').textContent = text;
  $('status-sel').title = text;
}

function updateButtons() {
  const sel = S.sel && S.sel.node;
  $('tb-zoom-out').disabled = S.zoom === 0;
  $('tb-zoom-in').disabled = !zoomTarget();
  $('tb-parent').disabled = !sel || sel.id === 0;
  $('tb-copy').disabled = !sel;
  $('tb-browse').disabled = !sel;
  $('tb-delete').hidden = !(S.status && S.status.can_delete);
  $('tb-delete').disabled = !sel || sel.id === 0;
  $('opt-free').disabled = $('opt-other').disabled = !S.scan || !S.scan.fs_total;
}

/// The folder Zoom In goes to: the selection if it is a folder, otherwise its parent.
function zoomTarget() {
  if (!S.sel) return null;
  const n = S.sel.node;
  if (n.dir) return n.id !== S.zoom ? n.id : null;
  const parent = S.chain[S.chain.length - 2];
  return parent != null && parent !== S.zoom ? parent : null;
}

async function setCrumbs() {
  const info = await api('node', { id: S.zoom });
  const box = $('crumbs');
  box.textContent = '';
  info.ancestors.forEach((a, i) => {
    if (i) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = '›';
      box.appendChild(sep);
    }
    const b = document.createElement('button');
    b.className = 'crumb';
    b.textContent = a.name;
    b.addEventListener('click', () => zoomTo(a.id));
    box.appendChild(b);
  });
  box.scrollLeft = box.scrollWidth;
}

async function zoomTo(id) {
  S.zoom = id;
  updateButtons();
  await Promise.all([setCrumbs(), loadMap(), loadStats()]);
}

async function loadMap() {
  if (!S.scan || $('doc').hidden) return;
  const { w, h } = map.pixelSize();
  if (w < 4 || h < 4) return;
  const token = ++S.mapToken;
  try {
    const buf = await api('treemap', {
      root: S.zoom, w, h, physical: S.physical ? 1 : 0, free: S.free ? 1 : 0, other: S.other ? 1 : 0,
    });
    if (token !== S.mapToken) return;
    map.setData(buf);
    map.paint(S.kinds.colors, S.highlight);
    $('map-empty').hidden = map.count > 0;
    S.hoverCell = -1;
    drawOverlay();
  } catch (e) {
    $('map-empty').hidden = false;
    $('map-empty').textContent = e.message;
  }
}

function drawOverlay() {
  map.drawOverlay(S.chain.length ? map.cellFor(S.chain) : -1, S.hoverCell);
}

async function select(id, from) {
  if (id >= OTHER_NODE) return;
  const info = await api('node', { id });
  S.sel = info;
  S.chain = info.ancestors.map(a => a.id);
  if (!S.chain.includes(S.zoom)) await zoomTo(0);
  if (from !== 'outline') await outline.reveal(info.ancestors);
  drawOverlay();
  updateStatusBar();
  updateButtons();
}

// ---------- right-click menu ----------

function hideMenu() { $('ctx').hidden = true; }

/// Selects the item, then offers the toolbar's actions for it where the pointer is.
async function showMenu(x, y, id, from) {
  hideMenu();
  if (id >= OTHER_NODE) return;
  try { await select(id, from); } catch { return; }
  const menu = $('ctx');
  menu.textContent = '';
  const add = (label, button, cls) => {
    if (button.hidden) return;
    const b = document.createElement('button');
    b.textContent = label;
    b.disabled = button.disabled;
    b.setAttribute('role', 'menuitem');
    if (cls) b.className = cls;
    // Every entry presses the toolbar button, so it follows exactly the same path;
    // Delete still opens the confirmation and nothing is removed from this menu.
    b.addEventListener('click', () => { hideMenu(); button.click(); });
    menu.appendChild(b);
  };
  add('Zoom In', $('tb-zoom-in'));
  add('Select Parent', $('tb-parent'));
  add('Copy Path', $('tb-copy'));
  add('Show in File Manager', $('tb-browse'));
  if (!$('tb-delete').hidden) {
    menu.appendChild(document.createElement('hr'));
    add('Delete…', $('tb-delete'), 'danger');
  }
  menu.hidden = false;
  const r = menu.getBoundingClientRect();
  menu.style.left = Math.max(4, Math.min(x, window.innerWidth - r.width - 4)) + 'px';
  menu.style.top = Math.max(4, Math.min(y, window.innerHeight - r.height - 4)) + 'px';
}

// ---------- deleting ----------

let delArm = null;

let delBusy = false;

function closeDeleteDialog() {
  clearTimeout(delArm);
  if ($('del-dialog').open) $('del-dialog').close();
}

async function askDelete() {
  const dlg = $('del-dialog'), go = $('del-go'), input = $('del-input');
  if (!S.sel || !S.scan || dlg.open || delBusy) return;
  // Fixed now: the dialog is about this item in this scan, whatever happens meanwhile.
  const id = S.sel.node.id, scanId = S.scan.id;
  delBusy = true;
  let check;
  try { check = await api('delete-check', { scan: scanId, node: id }); }
  catch (e) { check = { allowed: false, reason: e.message, name: S.sel.node.name, paths: [], bytes: 0, files: 0 }; }
  finally { delBusy = false; }
  if (!S.scan || S.scan.id !== scanId) return;

  clearTimeout(delArm);
  $('del-error').hidden = true;
  $('del-name').textContent = check.name;
  const list = $('del-paths');
  list.textContent = '';
  for (const p of (check.paths && check.paths.length ? check.paths : [check.path || S.sel.path])) {
    const li = document.createElement('li');
    li.textContent = p;
    list.appendChild(li);
  }
  input.value = '';
  go.disabled = true;

  if (!check.allowed) {
    $('del-title').textContent = 'This cannot be deleted';
    $('del-size').textContent = check.reason;
    $('del-warn').hidden = true;
    $('del-typed').hidden = true;
    go.hidden = true;
    $('del-cancel').textContent = 'Close';
  } else {
    $('del-title').textContent = 'Delete permanently?';
    $('del-size').textContent = check.dir
      ? `${fmtSize(check.bytes)} in ${fmtCount(check.files)} file${check.files === 1 ? '' : 's'}`
        + (check.paths.length > 1 ? `, on ${check.paths.length} disks` : '')
      : fmtSize(check.bytes);
    $('del-warn').hidden = false;
    $('del-warn').textContent = NO_TRASH + (S.scan && S.scan.zfs ? ' This is on ZFS, which InventoryX has not been tested on.' : '');
    $('del-typed').hidden = !check.typed;
    $('del-typed-reason').textContent = check.typed_reason || '';
    go.hidden = false;
    $('del-cancel').textContent = 'Cancel';
    // Without typing, the button only becomes live after a pause, so a stray
    // double-click on the toolbar cannot carry through.
    if (!check.typed) delArm = setTimeout(() => { go.disabled = false; }, 1500);
  }
  input.oninput = () => { go.disabled = input.value !== 'DELETE'; };
  go.onclick = async () => {
    go.disabled = true;
    input.disabled = true;
    $('del-cancel').disabled = true;
    const fail = message => {
      $('del-error').textContent = message;
      $('del-error').hidden = false;
      $('del-cancel').disabled = false;
      $('del-cancel').textContent = 'Close';
      go.hidden = true;
    };
    try {
      await api('delete', null, { scan: scanId, node: id, token: check.token,
                                  confirm: check.typed ? input.value : '' });
      // The removal runs on the server; watch it until it ends.
      for (;;) {
        const st = await api('status');
        const d = st.deleting;
        if (!d.active) {
          if (d.error) return fail(d.error);
          break;
        }
        $('del-size').textContent = `Deleting… ${fmtCount(d.done)} of ${fmtCount(d.total)} files`;
        await new Promise(r => setTimeout(r, 500));
      }
      dlg.close();
      await afterDelete();
    } catch (e) {
      fail(e.message);
    }
  };
  input.disabled = false;
  $('del-cancel').disabled = false;
  dlg.showModal();
  // Cancel has the focus, so Enter or Space never deletes.
  $('del-cancel').focus();
}

async function afterDelete() {
  const chain = S.chain.slice();
  const parent = chain.length > 1 ? chain[chain.length - 2] : 0;
  S.sel = null;
  S.chain = [];
  // If the view was inside what was deleted, step out to the folder that held it.
  try { await api('node', { id: S.zoom }); } catch { S.zoom = parent; }
  // The file tree keeps its open folders and its place; only the deleted row goes.
  await outline.removeItem(chain, id => api('node', { id }).then(info => info.node));
  await poll(true);
  await Promise.all([setCrumbs(), loadMap(), loadStats()]);
  await select(parent, 'toolbar');
}

// ---------- inspector ----------

async function loadStats() {
  if (!S.scan) return;
  const st = await api('stats', { root: S.zoom, physical: S.physical ? 1 : 0 });
  const row = (cls) => { const r = document.createElement('div'); r.className = 'row ' + (cls || ''); return r; };
  const span = (parent, cls, text) => {
    const s = document.createElement('span');
    s.className = cls;
    if (text != null) s.textContent = text;
    parent.appendChild(s);
    return s;
  };

  const kinds = $('kinds-list');
  kinds.textContent = '';
  const head = row('head');
  span(head, 'swatch').style.visibility = 'hidden';
  span(head, 'name', 'Kind'); span(head, 'size', 'Size'); span(head, 'count', 'Files');
  kinds.appendChild(head);
  const sorted = st.kinds.slice().sort((a, b) => (S.physical ? b.physical - a.physical : b.logical - a.logical));
  for (const k of sorted) {
    const r = row(S.highlight === k.kind ? 'selected' : '');
    span(r, 'swatch').style.background = kindColor(k.kind);
    span(r, 'name', k.name).title = k.name;
    span(r, 'size', fmtSize(S.physical ? k.physical : k.logical));
    span(r, 'count dim', fmtCount(k.count));
    r.addEventListener('click', () => {
      S.highlight = S.highlight === k.kind ? null : k.kind;
      for (const el of kinds.querySelectorAll('.row.selected')) el.classList.remove('selected');
      if (S.highlight != null) r.classList.add('selected');
      map.paint(S.kinds.colors, S.highlight);
    });
    kinds.appendChild(r);
  }

  const largest = $('largest-list');
  largest.textContent = '';
  for (const n of st.largest) largest.appendChild(fileRow(n));
}

function fileRow(n) {
  const r = document.createElement('div');
  r.className = 'row two';
  const sw = document.createElement('span');
  sw.className = 'swatch' + (n.dir ? ' folder' : '');
  sw.style.background = kindColor(n.dir ? 0 : n.kind);
  const name = document.createElement('span');
  name.className = 'name';
  const a = document.createElement('span');
  a.textContent = n.name;
  const b = document.createElement('span');
  b.className = 'dim';
  b.textContent = n.folder;
  name.append(a, b);
  name.title = `${n.folder}/${n.name}`;
  const size = document.createElement('span');
  size.className = 'size';
  size.textContent = fmtSize(nodeSize(n));
  r.append(sw, name, size);
  r.addEventListener('click', () => select(n.id, 'list'));
  return r;
}

// ---------- search ----------

let searchTimer = null, searchToken = 0;
function runSearch() {
  const q = $('search').value.trim();
  const box = $('search-results');
  if (!q) { box.hidden = true; return; }
  const token = ++searchToken;
  api('search', { q, physical: S.physical ? 1 : 0 }).then(res => {
    if (token !== searchToken) return;
    box.textContent = '';
    const count = document.createElement('div');
    count.className = 'search-count';
    count.textContent = res.total === 0 ? 'No matches'
      : res.total > res.hits.length ? `${fmtCount(res.total)} matches, largest ${res.hits.length} shown`
      : `${fmtCount(res.total)} match${res.total === 1 ? '' : 'es'}`;
    box.appendChild(count);
    for (const n of res.hits) {
      const r = fileRow(n);
      r.addEventListener('click', () => { box.hidden = true; });
      box.appendChild(r);
    }
    box.hidden = false;
  }).catch(e => {
    box.textContent = e.message;
    box.hidden = false;
  });
}

// ---------- wiring ----------

function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
  // Plain http (the usual Unraid setup) has no clipboard API.
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  try { document.execCommand('copy'); } finally { ta.remove(); }
  return Promise.resolve();
}

function flash(button, text) {
  const old = button.textContent;
  button.textContent = text;
  setTimeout(() => { button.textContent = old; }, 1200);
}

function init() {
  $('opt-physical').checked = S.physical;
  $('opt-free').checked = S.free;
  $('opt-other').checked = S.other;
  const option = (id, key) => $(id).addEventListener('change', e => {
    S[key] = e.target.checked;
    prefs.set(key, S[key]);
    loadMap();
    if (key === 'physical') { loadStats(); outline.refresh(); updateStatusBar(); }
  });
  option('opt-physical', 'physical');
  option('opt-free', 'free');
  option('opt-other', 'other');

  $('tb-volumes').addEventListener('click', () => { S.showStart = true; show('start'); loadTargets(); });
  $('back-to-scan').addEventListener('click', () => { S.showStart = false; show('doc'); requestAnimationFrame(loadMap); });
  $('tb-zoom-in').addEventListener('click', () => { const t = zoomTarget(); if (t != null) zoomTo(t); });
  $('tb-zoom-out').addEventListener('click', async () => {
    const info = await api('node', { id: S.zoom });
    const parent = info.ancestors[info.ancestors.length - 2];
    if (parent) zoomTo(parent.id);
  });
  $('tb-parent').addEventListener('click', () => {
    const parent = S.chain[S.chain.length - 2];
    if (parent == null) return;
    // Like the Mac app: selecting above the zoomed folder zooms out with it.
    if (S.chain[S.chain.length - 1] === S.zoom) S.zoom = parent, setCrumbs(), loadMap(), loadStats();
    select(parent, 'toolbar');
  });
  $('tb-refresh').addEventListener('click', () => { if (S.scan) startScan(S.scan.target, S.scan.label); });
  $('tb-copy').addEventListener('click', () => {
    if (S.sel) copyText(S.sel.path).then(() => flash($('tb-copy'), 'Copied'));
  });
  $('tb-browse').addEventListener('click', () => {
    if (!S.sel) return;
    const dir = S.sel.node.dir ? S.sel.path : S.sel.path.replace(/\/[^/]*$/, '') || '/';
    window.open('/Main/Browse?dir=' + encodeURIComponent(dir), '_blank', 'noopener');
  });
  $('tb-delete').addEventListener('click', askDelete);
  $('del-cancel').addEventListener('click', closeDeleteDialog);
  // Escape must not dismiss the dialog while a delete is under way.
  $('del-dialog').addEventListener('cancel', e => { if ($('del-cancel').disabled) e.preventDefault(); });
  $('del-dialog').addEventListener('close', () => clearTimeout(delArm));
  $('scan-cancel').addEventListener('click', () => api('cancel', null, {}).then(() => poll(true)));
  $('path-form').addEventListener('submit', e => {
    e.preventDefault();
    const p = $('path-input').value.trim();
    if (!p.startsWith('/')) return startError('Enter a full path that starts with /.');
    startScan('path:' + p, p);
  });

  const inspector = $('inspector');
  inspector.hidden = !prefs.get('inspector', true);
  $('tb-inspector').classList.toggle('on', !inspector.hidden);
  $('tb-inspector').addEventListener('click', () => {
    inspector.hidden = !inspector.hidden;
    prefs.set('inspector', !inspector.hidden);
    $('tb-inspector').classList.toggle('on', !inspector.hidden);
  });
  const tab = (which) => {
    $('tab-kinds').classList.toggle('active', which === 'kinds');
    $('tab-largest').classList.toggle('active', which === 'largest');
    $('kinds-list').hidden = which !== 'kinds';
    $('largest-list').hidden = which !== 'largest';
  };
  $('tab-kinds').addEventListener('click', () => tab('kinds'));
  $('tab-largest').addEventListener('click', () => tab('largest'));

  // Treemap mouse handling.
  const overlay = $('map-overlay'), tip = $('tip');
  const cellAt = e => {
    const r = overlay.getBoundingClientRect();
    if (!r.width || !r.height) return -1;
    return map.hit(Math.floor((e.clientX - r.left) * map.width / r.width),
                   Math.floor((e.clientY - r.top) * map.height / r.height));
  };
  const names = new Map();
  let tipTimer = null;
  overlay.addEventListener('mousemove', e => {
    const cell = cellAt(e);
    const r = overlay.getBoundingClientRect();
    tip.style.left = Math.min(e.clientX - r.left + 14, r.width - 40) + 'px';
    tip.style.top = Math.max(0, e.clientY - r.top - 30) + 'px';
    if (cell === S.hoverCell) return;
    S.hoverCell = cell;
    drawOverlay();
    clearTimeout(tipTimer);
    if (cell < 0) { tip.hidden = true; return; }
    const id = map.node[cell];
    const showTip = text => { if (S.hoverCell === cell) { tip.textContent = text; tip.hidden = false; } };
    if (id === FREE_NODE) return showTip(`Free space — ${fmtSize(S.scan.fs_free)}`);
    if (id === OTHER_NODE) return showTip(`Other space — ${fmtSize(S.scan.other)}`);
    const key = S.scan.id + ':' + id;
    if (names.has(key)) return showTip(names.get(key));
    tipTimer = setTimeout(async () => {
      try {
        const info = await api('node', { id });
        if (names.size > 5000) names.clear();
        names.set(key, `${info.node.name} — ${fmtSize(nodeSize(info.node))}`);
        showTip(names.get(key));
      } catch { /* hover only */ }
    }, 50);
  });
  overlay.addEventListener('mouseleave', () => { S.hoverCell = -1; tip.hidden = true; drawOverlay(); });
  overlay.addEventListener('click', e => { const c = cellAt(e); if (c >= 0) select(map.node[c], 'map'); });
  overlay.addEventListener('dblclick', () => { const t = zoomTarget(); if (t != null) zoomTo(t); });
  overlay.addEventListener('contextmenu', e => {
    e.preventDefault();
    const c = cellAt(e);
    if (c >= 0) showMenu(e.clientX, e.clientY, map.node[c], 'map');
  });
  document.addEventListener('pointerdown', e => { if (!e.target.closest('#ctx')) hideMenu(); });
  window.addEventListener('blur', hideMenu);
  window.addEventListener('resize', hideMenu);
  $('outline').addEventListener('scroll', hideMenu);

  let resizeTimer = null;
  new ResizeObserver(() => { clearTimeout(resizeTimer); resizeTimer = setTimeout(loadMap, 150); }).observe($('map'));

  // Splitter between outline and treemap.
  const out = $('outline');
  const saved = prefs.get('outlineWidth', null);
  if (saved) out.style.flexBasis = saved + 'px';
  $('split').addEventListener('pointerdown', e => {
    e.preventDefault();
    $('split').setPointerCapture(e.pointerId);
    const move = ev => {
      const w = Math.max(180, Math.min(ev.clientX - out.getBoundingClientRect().left, window.innerWidth - 300));
      out.style.flexBasis = w + 'px';
    };
    const up = () => {
      $('split').removeEventListener('pointermove', move);
      prefs.set('outlineWidth', parseInt(out.style.flexBasis, 10));
    };
    $('split').addEventListener('pointermove', move);
    $('split').addEventListener('pointerup', up, { once: true });
  });

  // Search.
  $('search').addEventListener('input', () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 120); });
  $('search').addEventListener('focus', () => { if ($('search').value.trim()) runSearch(); });
  document.addEventListener('keydown', e => {
    if (e.key === 'Escape') { hideMenu(); $('search-results').hidden = true; $('search').blur(); }
    if (e.key === '/' && !/INPUT|TEXTAREA/.test(document.activeElement.tagName) && !$('doc').hidden) {
      e.preventDefault();
      $('search').focus();
    }
  });
  document.addEventListener('click', e => {
    if (!e.target.closest('.search')) $('search-results').hidden = true;
  });

  setInterval(updateStatusBar, 30000);
  poll(true);
}

init();
