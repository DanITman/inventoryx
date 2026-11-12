// InventoryX — file outline: a virtualised tree whose folders load their children on demand.
// GPL-3.0-or-later.
'use strict';

const ROW_H = 24, PAGE = 500, REVEAL_LIMIT = 20000;

class Outline {
  /// opts: fetchChildren(id, offset, limit), onSelect(node), onOpen(node),
  ///       sizeText(node), kindName(kind), kindColor(kind)
  constructor(el, opts) {
    this.el = el;
    this.opts = opts;
    this.root = null;
    this.rows = [];
    this.selected = null;
    el.textContent = '';
    this.head = document.createElement('div');
    this.head.className = 'row head';
    for (const [cls, label] of [['twisty', ''], ['name', 'Name'], ['size', 'Size'], ['kind', 'Kind']]) {
      const s = document.createElement('span');
      s.className = cls;
      s.textContent = label;
      this.head.appendChild(s);
    }
    this.body = document.createElement('div');
    this.body.className = 'outline-body';
    el.append(this.head, this.body);
    el.addEventListener('scroll', () => this.render());
    this.body.addEventListener('click', e => this.click(e, false));
    this.body.addEventListener('dblclick', e => this.click(e, true));
    this.body.addEventListener('contextmenu', e => {
      const el = e.target.closest('.row');
      const row = el && this.rows[+el.dataset.i];
      if (!row || !row.item || !this.opts.onContext) return;
      e.preventDefault();
      this.selected = row.item.node.id;
      this.render();
      this.opts.onContext(row.item.node, e);
    });
    el.addEventListener('keydown', e => this.key(e));
    new ResizeObserver(() => this.render()).observe(el);
  }

  item(node, parent) {
    return { node, parent, depth: parent ? parent.depth + 1 : 0, expanded: false, kids: [], loading: false };
  }

  async setRoot(node) {
    this.root = this.item(node, null);
    this.selected = null;
    this.el.scrollTop = 0;
    await this.expand(this.root);
  }

  /// Loads children until `upTo` are present. One load per folder at a time, so two
  /// quick clicks cannot fetch the same page twice.
  load(item, upTo) {
    const run = async () => {
      while (item.kids.length < Math.min(upTo, item.node.children)) {
        const want = Math.min(Math.max(upTo - item.kids.length, PAGE), 5000);
        const page = await this.opts.fetchChildren(item.node.id, item.kids.length, want);
        if (!page.items.length) break;
        for (const n of page.items) item.kids.push(this.item(n, item));
      }
    };
    item.pending = (item.pending || Promise.resolve()).then(run, run);
    return item.pending;
  }

  async expand(item) {
    if (!item.node.dir || !item.node.children) return;
    if (!item.kids.length && !item.loading) {
      item.loading = true;
      try { await this.load(item, PAGE); } finally { item.loading = false; }
    }
    item.expanded = true;
    this.flatten();
  }

  collapse(item) {
    item.expanded = false;
    this.flatten();
  }

  flatten() {
    const rows = [];
    const walk = item => {
      rows.push({ item });
      if (!item.expanded) return;
      for (const k of item.kids) walk(k);
      if (item.kids.length < item.node.children) rows.push({ more: item });
    };
    if (this.root) walk(this.root);
    this.rows = rows;
    this.render();
  }

  refresh() { this.render(); }

  render() {
    const top = this.el.scrollTop, height = this.el.clientHeight;
    const first = Math.max(0, Math.floor(top / ROW_H) - 5);
    const last = Math.min(this.rows.length, Math.ceil((top + height) / ROW_H) + 5);
    this.body.style.height = this.rows.length * ROW_H + 'px';
    const frag = document.createDocumentFragment();
    for (let i = first; i < last; i++) frag.appendChild(this.rowEl(this.rows[i], i));
    this.body.replaceChildren(frag);
  }

  rowEl(row, i) {
    const el = document.createElement('div');
    el.className = 'row';
    el.style.top = i * ROW_H + 'px';
    el.dataset.i = i;
    const span = (cls, text) => {
      const s = document.createElement('span');
      s.className = cls;
      if (text != null) s.textContent = text;
      el.appendChild(s);
      return s;
    };
    if (row.more) {
      el.classList.add('more');
      el.style.paddingLeft = 8 + (row.more.depth + 1) * 14 + 'px';
      const left = row.more.node.children - row.more.kids.length;
      span('name', `Show more (${left.toLocaleString()} not shown)`);
      return el;
    }
    const { node, depth } = row.item;
    if (this.selected === node.id) el.classList.add('selected');
    el.style.paddingLeft = 8 + depth * 14 + 'px';
    span('twisty', node.dir && node.children ? (row.item.expanded ? '▾' : '▸') : '');
    const sw = span('swatch' + (node.dir ? ' folder' : ''));
    sw.style.background = this.opts.kindColor(node.dir ? 0 : node.kind);
    const name = span('name', node.name);
    name.title = node.name;
    span('size', this.opts.sizeText(node));
    let kind = node.dir ? `${node.files.toLocaleString()} file${node.files === 1 ? '' : 's'}` : this.opts.kindName(node.kind);
    if (node.flags & 4) kind = 'Could not be read';
    else if (node.flags & 8) kind = 'Other filesystem';
    span('kind dim', kind);
    return el;
  }

  click(e, double) {
    const el = e.target.closest('.row');
    if (!el) return;
    const row = this.rows[+el.dataset.i];
    if (!row) return;
    if (row.more) {
      if (!double) this.load(row.more, row.more.kids.length + PAGE).then(() => this.flatten());
      return;
    }
    if (e.target.classList.contains('twisty')) {
      if (!double) row.item.expanded ? this.collapse(row.item) : this.expand(row.item);
      return;
    }
    if (double) {
      this.opts.onOpen(row.item.node);
    } else {
      this.selected = row.item.node.id;
      this.render();
      this.opts.onSelect(row.item.node);
    }
  }

  key(e) {
    const i = this.rows.findIndex(r => r.item && r.item.node.id === this.selected);
    if (i < 0) return;
    const item = this.rows[i].item;
    let target = null;
    if (e.key === 'ArrowDown') target = this.rows.slice(i + 1).find(r => r.item);
    else if (e.key === 'ArrowUp') target = this.rows.slice(0, i).reverse().find(r => r.item);
    else if (e.key === 'ArrowRight') {
      if (item.node.dir && !item.expanded) this.expand(item);
      else target = this.rows[i + 1] && this.rows[i + 1].item ? this.rows[i + 1] : null;
    } else if (e.key === 'ArrowLeft') {
      if (item.expanded) this.collapse(item);
      else if (item.parent) target = { item: item.parent };
    } else return;
    e.preventDefault();
    if (target) {
      this.selected = target.item.node.id;
      this.scrollTo(target.item);
      this.opts.onSelect(target.item.node);
    }
  }

  scrollTo(item) {
    const i = this.rows.findIndex(r => r.item === item);
    if (i < 0) return;
    const y = i * ROW_H, top = this.el.scrollTop, h = this.el.clientHeight - ROW_H;
    if (y < top) this.el.scrollTop = y;
    else if (y + ROW_H > top + h) this.el.scrollTop = y + ROW_H - h;
    this.render();
  }

  /// Expands the folders along `ancestors` ([{id, index}] from the scan root down) and
  /// selects the last one.
  async reveal(ancestors) {
    if (!this.root || !ancestors.length || ancestors[0].id !== this.root.node.id) return;
    let item = this.root;
    for (let a = 1; a < ancestors.length; a++) {
      // Deep inside a huge folder: stop at the folder rather than load every sibling.
      if (ancestors[a].index >= REVEAL_LIMIT) break;
      await this.load(item, ancestors[a].index + 1);
      item.expanded = true;
      const next = item.kids[ancestors[a].index];
      if (!next || next.node.id !== ancestors[a].id) break;
      item = next;
    }
    this.selected = item.node.id;
    this.flatten();
    this.scrollTo(item);
  }

  /// Takes a deleted item out of the tree in place: open folders stay open and the
  /// scroll position stays put. `chain` is the ids from the scan root down to the deleted
  /// item; the folders above it are re-read so their sizes and counts are current.
  async removeItem(chain, fetchNode) {
    if (!this.root || chain[0] !== this.root.node.id) return;
    const above = [this.root];
    for (let i = 1; i < chain.length - 1; i++) {
      const next = above[above.length - 1].kids.find(k => k.node.id === chain[i]);
      if (!next) break;
      above.push(next);
    }
    const gone = chain[chain.length - 1];
    // Only when the folder that held it is loaded is there a row to take out.
    if (above.length === chain.length - 1) {
      const parent = above[above.length - 1];
      parent.kids = parent.kids.filter(k => k.node.id !== gone);
    }
    if (this.selected === gone) this.selected = null;
    const fresh = await Promise.all(above.map(it => fetchNode(it.node.id).catch(() => null)));
    above.forEach((it, i) => { if (fresh[i]) it.node = fresh[i]; });
    this.flatten();
  }

  clearSelection() {
    this.selected = null;
    this.render();
  }
}
