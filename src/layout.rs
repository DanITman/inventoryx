// InventoryX — squarified cushion treemap layout.
// Ported from TreemapLayout.swift of Disk Inventory X (Bruls, Huizing & van Wijk 2000;
// van Wijk & van de Wetering 1999). Painting happens in the browser.
// GPL-3.0-or-later.

use crate::index::*;
use crate::kinds;

pub const FREE_NODE: u32 = u32::MAX - 1;
pub const OTHER_NODE: u32 = u32::MAX - 2;

#[derive(Clone, Copy, Default)]
pub struct LayoutOpts {
    pub physical: bool,
    /// Bytes of free space to show beside the root's children (0 = hidden).
    pub free: u64,
    /// Bytes used by other files on the volume (0 = hidden).
    pub other: u64,
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub x0: u16,
    pub y0: u16,
    pub x1: u16,
    pub y1: u16,
    pub node: u32,
    /// Index one past this cell's last descendant (cells are in pre-order).
    pub subtree_end: u32,
    pub kind: u16,
    pub leaf: bool,
    /// Cushion surface coefficients: x2, x1, y2, y1.
    pub cushion: [f32; 4],
}

#[derive(Clone, Copy)]
struct Rect { x: f64, y: f64, w: f64, h: f64 }

const MIN_CELL: f64 = 1.0;
const CUSHION_HEIGHT: f32 = 0.38;
const CUSHION_FALLOFF: f32 = 0.75;

fn add_ridges(c: &mut [f32; 4], r: Rect, h: f32) {
    let (w, hgt) = (r.w as f32, r.h as f32);
    if w > 0.0 {
        c[0] -= 4.0 * h / w;
        c[1] += 4.0 * h * (r.x + r.x + r.w) as f32 / w;
    }
    if hgt > 0.0 {
        c[2] -= 4.0 * h / hgt;
        c[3] += 4.0 * h * (r.y + r.y + r.h) as f32 / hgt;
    }
}

struct Ctx<'a> {
    ix: &'a Index,
    opts: LayoutOpts,
    cells: Vec<Cell>,
    max_cells: usize,
}

/// Enough for every pixel-sized file on a large screen; keeps one answer near 14 MB at most.
pub const MAX_CELLS: usize = 400_000;

pub fn layout(ix: &Index, root: u32, w: u32, h: u32, opts: LayoutOpts) -> Vec<Cell> {
    layout_capped(ix, root, w, h, opts, MAX_CELLS)
}

/// Once `max_cells` is reached, folders are drawn whole instead of being split further.
pub fn layout_capped(ix: &Index, root: u32, w: u32, h: u32, opts: LayoutOpts, max_cells: usize) -> Vec<Cell> {
    let (w, h) = (w.min(16_000), h.min(16_000));
    let mut ctx = Ctx { ix, opts, cells: Vec::with_capacity(4096), max_cells };
    let Some(node) = ix.node(root) else { return ctx.cells };
    let extra = if root == ROOT { opts.free + opts.other } else { 0 };
    if w < 1 || h < 1 || node.size(opts.physical) + extra == 0 { return ctx.cells; }
    ctx.add(root, Rect { x: 0.0, y: 0.0, w: w as f64, h: h as f64 }, [0.0; 4], CUSHION_HEIGHT, true);
    ctx.cells
}

impl Ctx<'_> {
    fn add(&mut self, id: u32, rect: Rect, parent_cushion: [f32; 4], height: f32, top: bool) {
        let mut cushion = parent_cushion;
        add_ridges(&mut cushion, rect, height);

        let pseudo = id >= OTHER_NODE;
        let node = if pseudo { None } else { Some(&self.ix.nodes[id as usize]) };
        let has_extra = top && id == ROOT && self.opts.free + self.opts.other > 0;
        let subdivide = node.map_or(false, |n| n.is_dir() && (n.child_count > 0 || has_extra))
            && rect.w >= MIN_CELL * 2.0 && rect.h >= MIN_CELL * 2.0
            && self.cells.len() < self.max_cells;

        let kind = match node {
            None => if id == FREE_NODE { kinds::FREE } else { kinds::OTHER },
            Some(n) if subdivide => n.kind,
            Some(_) => self.color_kind(id),
        };
        let index = self.cells.len();
        self.cells.push(Cell {
            x0: rect.x.round() as u16, y0: rect.y.round() as u16,
            x1: (rect.x + rect.w).round() as u16, y1: (rect.y + rect.h).round() as u16,
            node: id, subtree_end: index as u32 + 1, kind, leaf: !subdivide, cushion,
        });
        if !subdivide { return; }

        let physical = self.opts.physical;
        let mut items: Vec<(u32, f64)> = self.ix.children(id)
            .map(|c| (c, self.ix.nodes[c as usize].size(physical) as f64))
            .filter(|&(_, s)| s > 0.0)
            .collect();
        if has_extra {
            if self.opts.other > 0 { items.push((OTHER_NODE, self.opts.other as f64)); }
            if self.opts.free > 0 { items.push((FREE_NODE, self.opts.free as f64)); }
        }
        // Children are stored by physical size; any other order needs a sort.
        if !physical || has_extra {
            items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        }
        let total: f64 = items.iter().map(|i| i.1).sum();
        if total > 0.0 {
            let placed = squarify(&items, total, rect);
            for (child, r) in placed {
                // Hard stop: one folder with millions of pixel-sized files. The largest
                // children come first, so only the smallest go undrawn.
                if self.cells.len() >= self.max_cells * 2 { break; }
                if r.w >= MIN_CELL && r.h >= MIN_CELL {
                    self.add(child, r, cushion, height * CUSHION_FALLOFF, false);
                }
            }
        }
        self.cells[index].subtree_end = self.cells.len() as u32;
    }

    /// Folders too small to subdivide take the colour of their largest file.
    fn color_kind(&self, id: u32) -> u16 {
        let mut n = &self.ix.nodes[id as usize];
        if !n.is_dir() { return n.kind; }
        loop {
            if n.child_count == 0 { return kinds::FOLDER; }
            let first = &self.ix.nodes[n.first_child as usize];
            if !first.is_dir() { return first.kind; }
            n = first;
        }
    }
}

/// Lays out `items` (largest first) in rows whose aspect ratios stay close to 1.
fn squarify(items: &[(u32, f64)], total: f64, rect: Rect) -> Vec<(u32, Rect)> {
    let mut out = Vec::with_capacity(items.len());
    let mut remaining = rect;
    let mut remaining_total = total;
    let mut start = 0;
    while start < items.len() {
        let short = remaining.w.min(remaining.h);
        if short <= 0.0 || remaining_total <= 0.0 { break; }
        let scale = remaining.w * remaining.h / remaining_total;

        let mut end = start;
        let mut row_sum = 0.0;
        let mut best_worst = f64::INFINITY;
        let (mut row_max, mut row_min) = (0.0f64, f64::INFINITY);
        while end < items.len() {
            let s = items[end].1 * scale;
            let new_sum = row_sum + items[end].1;
            let (new_max, new_min) = (row_max.max(s), row_min.min(s));
            let sum_area = new_sum * scale;
            let side2 = short * short;
            let worst = (side2 * new_max / (sum_area * sum_area)).max(sum_area * sum_area / (side2 * new_min));
            if worst > best_worst { break; }
            best_worst = worst;
            row_sum = new_sum;
            row_max = new_max;
            row_min = new_min;
            end += 1;
        }
        if end == start { end = start + 1; row_sum = items[start].1; }

        let row_area = row_sum * scale;
        let horizontal = remaining.w >= remaining.h;
        let thickness = row_area / short;
        let mut offset = 0.0;
        let last_row = end == items.len();
        for i in start..end {
            let length = items[i].1 / row_sum * short;
            let last = i == end - 1;
            let r = if horizontal {
                Rect { x: remaining.x, y: remaining.y + offset,
                       w: if last_row { remaining.w } else { thickness },
                       h: if last { remaining.h - offset } else { length } }
            } else {
                Rect { x: remaining.x + offset, y: remaining.y,
                       w: if last { remaining.w - offset } else { length },
                       h: if last_row { remaining.h } else { thickness } }
            };
            out.push((items[i].0, r));
            offset += length;
        }
        if horizontal {
            remaining = Rect { x: remaining.x + thickness, w: (remaining.w - thickness).max(0.0), ..remaining };
        } else {
            remaining = Rect { y: remaining.y + thickness, h: (remaining.h - thickness).max(0.0), ..remaining };
        }
        remaining_total -= row_sum;
        start = end;
    }
    out
}

pub const CELL_BYTES: usize = 36;

/// 16-byte header (count, width, height, 0) then 36 bytes per cell, little-endian:
/// u16 x0 y0 x1 y1, u32 node, u32 subtree_end, u16 kind, u16 leaf, f32 x2 x1 y2 y1.
pub fn encode(cells: &[Cell], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + cells.len() * CELL_BYTES);
    for v in [cells.len() as u32, w, h, 0] { out.extend_from_slice(&v.to_le_bytes()); }
    for c in cells {
        for v in [c.x0, c.y0, c.x1, c.y1] { out.extend_from_slice(&v.to_le_bytes()); }
        out.extend_from_slice(&c.node.to_le_bytes());
        out.extend_from_slice(&c.subtree_end.to_le_bytes());
        out.extend_from_slice(&c.kind.to_le_bytes());
        out.extend_from_slice(&(c.leaf as u16).to_le_bytes());
        for v in c.cushion { out.extend_from_slice(&v.to_le_bytes()); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;

    fn area(c: &Cell) -> i64 { (c.x1 as i64 - c.x0 as i64) * (c.y1 as i64 - c.y0 as i64) }

    #[test]
    fn areas_follow_sizes_and_cells_are_preorder() {
        let root = fixture("layout");
        write(&root.join("a.bin"), 600_000);
        write(&root.join("b.bin"), 300_000);
        write(&root.join("d/c.bin"), 100_000);
        let ix = scan_paths(&[&root]);
        let cells = layout(&ix, 0, 1000, 500, LayoutOpts { physical: false, ..Default::default() });
        assert_eq!(cells[0].node, 0);
        assert_eq!(cells[0].subtree_end as usize, cells.len());
        assert!(!cells[0].leaf);
        let a = cells.iter().find(|c| ix.name(c.node) == b"a.bin").unwrap();
        let b = cells.iter().find(|c| ix.name(c.node) == b"b.bin").unwrap();
        let c = cells.iter().find(|c| ix.name(c.node) == b"c.bin").unwrap();
        let total = 500_000.0;
        for (cell, share) in [(a, 0.6), (b, 0.3), (c, 0.1)] {
            assert!(cell.leaf);
            assert!((area(cell) as f64 - total * share).abs() < 2.0 * 1500.0, "{}", area(cell));
        }
        // pre-order with nested subtree ends
        for (i, cell) in cells.iter().enumerate() {
            assert!(cell.subtree_end as usize > i && cell.subtree_end as usize <= cells.len());
            assert!(cell.x1 <= 1000 && cell.y1 <= 500);
        }
        assert_eq!(encode(&cells, 1000, 500).len(), 16 + cells.len() * CELL_BYTES);
    }

    #[test]
    fn cell_count_is_capped() {
        let root = fixture("cap");
        for i in 0..40 { write(&root.join(format!("d{}/f{i}.bin", i % 4)), 1000 + i); }
        let ix = scan_paths(&[&root]);
        let opts = LayoutOpts { physical: false, ..Default::default() };
        let all = layout(&ix, 0, 800, 600, opts);
        assert!(all.len() > 40);
        let capped = layout_capped(&ix, 0, 800, 600, opts, 10);
        assert!(capped.len() <= 20, "{}", capped.len());
        assert_eq!(capped[0].subtree_end as usize, capped.len());
        // what is left unsplit is still painted
        assert!(capped.iter().any(|c| c.leaf && ix.nodes[c.node as usize].is_dir()));
    }

    #[test]
    fn empty_tree_and_free_space() {
        let root = fixture("empty");
        let ix = scan_paths(&[&root]);
        assert!(layout(&ix, 0, 800, 600, LayoutOpts { physical: false, ..Default::default() }).is_empty());
        assert!(layout(&ix, 99, 800, 600, LayoutOpts::default()).is_empty());
        let cells = layout(&ix, 0, 800, 600, LayoutOpts { physical: true, free: 10, other: 30 });
        let free = cells.iter().find(|c| c.node == FREE_NODE).unwrap();
        let other = cells.iter().find(|c| c.node == OTHER_NODE).unwrap();
        assert_eq!(free.kind, kinds::FREE);
        assert!(area(other) > area(free) * 2);
    }
}
