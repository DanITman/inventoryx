// InventoryX — per-kind totals, largest files and name search over the index.
// GPL-3.0-or-later.

use crate::index::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(Clone, Debug, Default)]
pub struct KindStat {
    pub kind: u16,
    pub logical: u64,
    pub physical: u64,
    pub count: u64,
}

pub struct Stats {
    /// Largest physical size first.
    pub kinds: Vec<KindStat>,
    pub largest_physical: Vec<u32>,
    pub largest_logical: Vec<u32>,
}

pub const LARGEST_LIMIT: usize = 200;

fn top(heap: BinaryHeap<Reverse<(u64, Reverse<u32>)>>) -> Vec<u32> {
    let mut v: Vec<(u64, Reverse<u32>)> = heap.into_iter().map(|r| r.0).collect();
    v.sort_by(|a, b| b.cmp(a));
    v.into_iter().map(|(_, id)| id.0).collect()
}

fn offer(heap: &mut BinaryHeap<Reverse<(u64, Reverse<u32>)>>, size: u64, id: u32, limit: usize) {
    if size == 0 || limit == 0 { return; }
    if heap.len() < limit {
        heap.push(Reverse((size, Reverse(id))));
    } else if heap.peek().map_or(false, |m| (size, Reverse(id)) > m.0) {
        heap.pop();
        heap.push(Reverse((size, Reverse(id))));
    }
}

/// Statistics of the files at or below `root`.
pub fn compute(ix: &Index, root: u32) -> Stats {
    let mut kinds: Vec<KindStat> = Vec::new();
    let mut phys = BinaryHeap::new();
    let mut logi = BinaryHeap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some(n) = ix.node(id) else { continue };
        if n.is_removed() { continue; }
        if n.is_dir() {
            stack.extend(ix.children(id));
            continue;
        }
        let k = n.kind as usize;
        if kinds.len() <= k { kinds.resize(k + 1, KindStat::default()); }
        let s = &mut kinds[k];
        s.kind = n.kind;
        s.logical += n.logical;
        s.physical += n.physical;
        s.count += 1;
        offer(&mut phys, n.physical, id, LARGEST_LIMIT);
        offer(&mut logi, n.logical, id, LARGEST_LIMIT);
    }
    kinds.retain(|k| k.count > 0);
    kinds.sort_by(|a, b| b.physical.cmp(&a.physical).then(a.kind.cmp(&b.kind)));
    Stats { kinds, largest_physical: top(phys), largest_logical: top(logi) }
}

/// Files and folders whose name contains `query`, ignoring ASCII case, largest first.
/// Returns at most `limit` ids and the total number of matches.
pub fn search(ix: &Index, query: &str, physical: bool, limit: usize) -> (Vec<u32>, u64) {
    let needle = query.as_bytes().to_ascii_lowercase();
    if needle.is_empty() || ix.nodes.len() <= 1 { return (Vec::new(), 0); }
    let finder = memchr::memmem::Finder::new(&needle);
    let total_nodes = ix.nodes.len() as u32;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16) as u32;
    let chunk = (total_nodes / threads).max(50_000);
    let ranges: Vec<(u32, u32)> = (0..)
        .map(|i| (1 + i * chunk, (1 + (i + 1) * chunk).min(total_nodes)))
        .take_while(|r| r.0 < total_nodes)
        .collect();
    let parts: Vec<(BinaryHeap<Reverse<(u64, Reverse<u32>)>>, u64)> = std::thread::scope(|s| {
        let handles: Vec<_> = ranges.iter().map(|&(a, b)| {
            let finder = &finder;
            s.spawn(move || {
                let mut heap = BinaryHeap::new();
                let mut count = 0u64;
                for id in a..b {
                    if !ix.nodes[id as usize].is_removed() && finder.find(ix.lower_name(id)).is_some() {
                        count += 1;
                        // +1 so that empty files still rank.
                        offer(&mut heap, ix.nodes[id as usize].size(physical) + 1, id, limit);
                    }
                }
                (heap, count)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all = BinaryHeap::new();
    let mut count = 0;
    for (heap, c) in parts {
        count += c;
        for Reverse((size, Reverse(id))) in heap { offer(&mut all, size, id, limit); }
    }
    (top(all), count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;

    #[test]
    fn totals_match_root_and_largest_is_ordered() {
        let root = fixture("stats");
        write(&root.join("a/one.mkv"), 9_000);
        write(&root.join("a/two.mkv"), 5_000);
        write(&root.join("b/pic.jpg"), 700);
        let ix = scan_paths(&[&root]);
        let st = compute(&ix, 0);
        assert_eq!(st.kinds.iter().map(|k| k.logical).sum::<u64>(), ix.nodes[0].logical);
        assert_eq!(st.kinds.iter().map(|k| k.count).sum::<u64>(), 3);
        assert_eq!(ix.kinds.name(st.kinds[0].kind), "Matroska Video");
        assert_eq!(st.kinds[0].count, 2);
        let names: Vec<&[u8]> = st.largest_logical.iter().map(|&i| ix.name(i)).collect();
        assert_eq!(names, vec![&b"one.mkv"[..], b"two.mkv", b"pic.jpg"]);
        let a = find(&ix, 0, b"a").unwrap();
        assert_eq!(compute(&ix, a).kinds.len(), 1);
    }

    #[test]
    fn deleted_items_vanish_from_totals_stats_and_search() {
        let root = fixture("removed");
        write(&root.join("a/one.mkv"), 9_000);
        write(&root.join("a/sub/two.mkv"), 5_000);
        write(&root.join("b/pic.jpg"), 700);
        let mut ix = scan_paths(&[&root]);
        let a = find(&ix, 0, b"a").unwrap();
        let sub = find(&ix, a, b"sub").unwrap();
        ix.mark_removed(sub);
        assert_eq!(ix.nodes[0].logical, 9_700);
        assert_eq!(ix.nodes[0].files, 2);
        assert_eq!(ix.nodes[a as usize].logical, 9_000);
        assert_eq!(ix.live_children(a).count(), 1);
        assert_eq!(search(&ix, "two", false, 10).1, 0);
        assert_eq!(search(&ix, "sub", false, 10).1, 0);
        let st = compute(&ix, 0);
        assert_eq!(st.kinds.iter().map(|k| k.count).sum::<u64>(), 2);
        assert_eq!(st.kinds.iter().map(|k| k.logical).sum::<u64>(), 9_700);
        // removing twice, or the root, changes nothing
        ix.mark_removed(sub);
        ix.mark_removed(0);
        assert_eq!(ix.nodes[0].logical, 9_700);
        ix.mark_removed(a);
        assert_eq!((ix.nodes[0].logical, ix.nodes[0].files), (700, 1));
        assert!(ix.is_consistent(ix.kinds.count()));
    }

    #[test]
    fn search_ignores_case_matches_folders_largest_first() {
        let root = fixture("search");
        write(&root.join("Holiday/IMG_001.JPG"), 300);
        write(&root.join("Holiday/img_002.jpg"), 900);
        write(&root.join("notes.txt"), 50);
        write(&root.join("empty-img"), 0);
        let ix = scan_paths(&[&root]);
        let (hits, total) = search(&ix, "IMG", false, 10);
        assert_eq!(total, 3);
        assert_eq!(ix.name(hits[0]), b"img_002.jpg");
        assert_eq!(ix.name(hits[1]), b"IMG_001.JPG");
        assert_eq!(ix.name(hits[2]), b"empty-img");
        let (hits, _) = search(&ix, "holiday", false, 10);
        assert_eq!(ix.name(hits[0]), b"Holiday");
        assert_eq!(search(&ix, "", false, 10).1, 0);
        assert_eq!(search(&ix, "img", false, 1).0.len(), 1);
        assert_eq!(search(&ix, "zzz", false, 10).0.len(), 0);
    }
}
