// InventoryX — cushion treemap painting and hit-testing.
// Ported from TreemapRenderer.swift and TreemapLayout.swift of Disk Inventory X.
// GPL-3.0-or-later.
'use strict';

const FREE_NODE = 0xFFFFFFFE, OTHER_NODE = 0xFFFFFFFD;
const CELL_BYTES = 36, HEADER_BYTES = 16;

class Treemap {
  constructor(canvas, overlay) {
    this.canvas = canvas;
    this.overlay = overlay;
    this.count = 0;
    this.width = 0;
    this.height = 0;
  }

  /// Device-pixel size the map should be laid out for.
  pixelSize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const r = this.canvas.parentElement.getBoundingClientRect();
    return { w: Math.max(0, Math.floor(r.width * dpr)), h: Math.max(0, Math.floor(r.height * dpr)), dpr };
  }

  /// `buffer` is the binary answer of api/treemap.
  setData(buffer) {
    const head = new Uint32Array(buffer, 0, 4);
    this.count = head[0];
    this.width = head[1];
    this.height = head[2];
    const n = this.count;
    this.x0 = new Uint16Array(n); this.y0 = new Uint16Array(n);
    this.x1 = new Uint16Array(n); this.y1 = new Uint16Array(n);
    this.node = new Uint32Array(n); this.end = new Uint32Array(n);
    this.kind = new Uint16Array(n); this.leaf = new Uint8Array(n);
    this.cushion = new Float32Array(n * 4);
    const dv = new DataView(buffer, HEADER_BYTES);
    for (let i = 0, o = 0; i < n; i++, o += CELL_BYTES) {
      this.x0[i] = dv.getUint16(o, true); this.y0[i] = dv.getUint16(o + 2, true);
      this.x1[i] = dv.getUint16(o + 4, true); this.y1[i] = dv.getUint16(o + 6, true);
      this.node[i] = dv.getUint32(o + 8, true); this.end[i] = dv.getUint32(o + 12, true);
      this.kind[i] = dv.getUint16(o + 16, true); this.leaf[i] = dv.getUint16(o + 18, true);
      for (let k = 0; k < 4; k++) this.cushion[i * 4 + k] = dv.getFloat32(o + 20 + k * 4, true);
    }
    for (const c of [this.canvas, this.overlay]) { c.width = this.width; c.height = this.height; }
  }

  /// `colors` is indexed by kind: [[r, g, b], ...] in 0..1. Other kinds are dimmed when
  /// `highlight` is a kind id.
  paint(colors, highlight) {
    const w = this.width, h = this.height;
    if (!w || !h) return;
    const ctx = this.canvas.getContext('2d');
    const image = ctx.createImageData(w, h);
    const px = new Uint32Array(image.data.buffer);
    px.fill(0xFF202020);
    const ambient = 0.2, diffuse = 0.8;
    const len = Math.sqrt(1 + 1 + 100);
    const lx = -1 / len, ly = -1 / len, lz = 10 / len;
    for (let i = 0; i < this.count; i++) {
      if (!this.leaf[i]) continue;
      const x0 = this.x0[i], x1 = Math.min(w, this.x1[i]), y0 = this.y0[i], y1 = Math.min(h, this.y1[i]);
      if (x1 <= x0 || y1 <= y0) continue;
      const kind = this.kind[i];
      let c = colors[kind] || [0.55, 0.55, 0.55];
      if (highlight != null && highlight !== kind) {
        const g = (c[0] + c[1] + c[2]) / 3 * 0.35;
        c = [g, g, g];
      }
      const r = c[0] * 255 * 1.15, g = c[1] * 255 * 1.15, b = c[2] * 255 * 1.15;
      const cx2 = this.cushion[i * 4], cx1 = this.cushion[i * 4 + 1];
      const cy2 = this.cushion[i * 4 + 2], cy1 = this.cushion[i * 4 + 3];
      for (let y = y0; y < y1; y++) {
        const ny = -(2 * cy2 * (y + 0.5) + cy1);
        const nyly = ny * ly + lz, ny2 = ny * ny + 1;
        let p = y * w + x0;
        for (let x = x0; x < x1; x++, p++) {
          const nx = -(2 * cx2 * (x + 0.5) + cx1);
          const cosa = (nx * lx + nyly) / Math.sqrt(nx * nx + ny2);
          const k = ambient + (cosa > 0 ? diffuse * cosa : 0);
          const R = r * k, G = g * k, B = b * k;
          px[p] = 0xFF000000 | ((B > 255 ? 255 : B) << 16) | ((G > 255 ? 255 : G) << 8) | (R > 255 ? 255 : R);
        }
      }
    }
    ctx.putImageData(image, 0, 0);
  }

  /// Index of the deepest cell at device pixel (x, y), or -1.
  hit(x, y) {
    if (!this.count || !this.contains(0, x, y)) return -1;
    let index = 0;
    for (;;) {
      let child = index + 1, found = -1;
      const end = this.end[index];
      while (child < end) {
        if (this.contains(child, x, y)) { found = child; break; }
        child = this.end[child];
      }
      if (found < 0) return index;
      index = found;
    }
  }

  contains(i, x, y) { return x >= this.x0[i] && x < this.x1[i] && y >= this.y0[i] && y < this.y1[i]; }

  /// Cell of the last id in `chain` (node ids from the scan root down), or of its closest
  /// ancestor that has a cell. -1 if the chain does not pass through the map's root.
  cellFor(chain) {
    if (!this.count) return -1;
    const start = chain.indexOf(this.node[0]);
    if (start < 0) return -1;
    let index = 0;
    for (let c = start + 1; c < chain.length; c++) {
      let child = index + 1, found = -1;
      const end = this.end[index];
      while (child < end) {
        if (this.node[child] === chain[c]) { found = child; break; }
        child = this.end[child];
      }
      if (found < 0) break;
      index = found;
    }
    return index;
  }

  /// Outlines the selected and hovered cells.
  drawOverlay(selected, hovered) {
    const ctx = this.overlay.getContext('2d');
    ctx.clearRect(0, 0, this.overlay.width, this.overlay.height);
    const box = (i, outer, inner, width) => {
      if (i < 0 || i >= this.count) return;
      const x = this.x0[i], y = this.y0[i], w = this.x1[i] - x, h = this.y1[i] - y;
      ctx.lineWidth = width;
      ctx.strokeStyle = outer;
      ctx.strokeRect(x + width / 2, y + width / 2, Math.max(1, w - width), Math.max(1, h - width));
      if (inner && w > width * 4 && h > width * 4) {
        ctx.strokeStyle = inner;
        ctx.strokeRect(x + width * 1.5, y + width * 1.5, w - width * 3, h - width * 3);
      }
    };
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    if (hovered !== selected) box(hovered, 'rgba(255,255,255,0.75)', null, dpr);
    box(selected, '#ffffff', '#000000', 1.5 * dpr);
  }
}
