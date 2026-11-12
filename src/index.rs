// InventoryX — compact in-memory file index.
// GPL-3.0-or-later.

use crate::kinds::KindTable;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const F_DIR: u8 = 1;
pub const F_SYMLINK: u8 = 2;
pub const F_UNREADABLE: u8 = 4;
pub const F_MOUNT: u8 = 8;
pub const F_HARDLINK_DUP: u8 = 16;
/// Deleted through InventoryX since the scan. Kept in place so ids stay valid; hidden
/// from every listing and counted as nothing.
pub const F_REMOVED: u8 = 32;

/// Marks a folder that exists on more than one source (a share spread over several disks).
pub const SOURCE_MERGED: u8 = 255;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub logical: u64,
    pub physical: u64,
    pub name_off: u32,
    pub parent: u32,
    pub first_child: u32,
    pub child_count: u32,
    /// Number of files at or below this node.
    pub files: u32,
    /// Last modification time, seconds since 1970 (0 if unknown).
    pub mtime: u32,
    pub name_len: u16,
    pub kind: u16,
    pub flags: u8,
    pub source: u8,
    /// Explicit, so the struct has no hidden padding and can be written to disk as it is.
    pub reserved: u16,
}

const _: () = assert!(std::mem::size_of::<Node>() == 48);

impl Node {
    pub fn is_dir(&self) -> bool { self.flags & F_DIR != 0 }
    pub fn is_removed(&self) -> bool { self.flags & F_REMOVED != 0 }
    pub fn size(&self, physical: bool) -> u64 { if physical { self.physical } else { self.logical } }
}

#[derive(Clone, Debug)]
pub struct Source {
    pub label: String,
    pub path: Vec<u8>,
}

#[derive(Clone)]
pub struct Index {
    pub nodes: Vec<Node>,
    pub names: Vec<u8>,
    /// ASCII-lower-cased copy of `names`, same offsets, for search.
    pub lower: Vec<u8>,
    pub kinds: Arc<KindTable>,
    pub label: String,
    pub display_root: Vec<u8>,
    pub sources: Vec<Source>,
    pub unreadable: u32,
    pub finished: SystemTime,
    pub duration: Duration,
    pub fs_total: u64,
    pub fs_free: u64,
}

pub const ROOT: u32 = 0;

impl Index {
    pub fn node(&self, id: u32) -> Option<&Node> { self.nodes.get(id as usize) }

    pub fn name(&self, id: u32) -> &[u8] {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    pub fn lower_name(&self, id: u32) -> &[u8] {
        let n = &self.nodes[id as usize];
        &self.lower[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    /// Children, sorted by physical size, largest first.
    pub fn children(&self, id: u32) -> Range<u32> {
        let n = &self.nodes[id as usize];
        n.first_child..n.first_child + n.child_count
    }

    /// Ids from the root down to `id`, inclusive.
    pub fn ancestors(&self, id: u32) -> Vec<u32> {
        let mut chain = vec![id];
        let mut cur = id;
        while cur != ROOT {
            cur = self.nodes[cur as usize].parent;
            chain.push(cur);
        }
        chain.reverse();
        chain
    }

    fn relative(&self, id: u32, out: &mut Vec<u8>) {
        for a in self.ancestors(id).into_iter().skip(1) {
            if out.last() != Some(&b'/') { out.push(b'/'); }
            out.extend_from_slice(self.name(a));
        }
    }

    /// Path as the user knows it (for a share: under `/mnt/user`).
    pub fn display_path(&self, id: u32) -> Vec<u8> {
        let mut p = self.display_root.clone();
        self.relative(id, &mut p);
        p
    }

    /// Path on the disk that actually holds the item, when it lives on exactly one.
    pub fn real_path(&self, id: u32) -> Vec<u8> {
        let src = self.nodes[id as usize].source;
        match self.sources.get(src as usize) {
            Some(s) => {
                let mut p = s.path.clone();
                self.relative(id, &mut p);
                p
            }
            None => self.display_path(id),
        }
    }

    /// Children that have not been deleted since the scan.
    pub fn live_children(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        self.children(id).filter(|&c| !self.nodes[c as usize].is_removed())
    }

    /// Records that `id` (and everything below it) was deleted: it disappears from
    /// listings and its size and file count come off every folder above it.
    pub fn mark_removed(&mut self, id: u32) {
        if id == ROOT || self.nodes[id as usize].is_removed() { return; }
        let (logical, physical, files) = {
            let n = &self.nodes[id as usize];
            (n.logical, n.physical, n.files)
        };
        let mut up = self.nodes[id as usize].parent;
        loop {
            let n = &mut self.nodes[up as usize];
            n.logical = n.logical.saturating_sub(logical);
            n.physical = n.physical.saturating_sub(physical);
            n.files = n.files.saturating_sub(files);
            if up == ROOT { break; }
            up = n.parent;
        }
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            stack.extend(self.children(cur));
            let n = &mut self.nodes[cur as usize];
            n.flags |= F_REMOVED;
            n.logical = 0;
            n.physical = 0;
            n.files = 0;
        }
        self.fs_free = self.fs_free.saturating_add(physical).min(self.fs_total);
    }

    /// Whether every link points inside the index, so that no lookup can go out of bounds.
    /// Used on an index loaded from disk.
    pub fn is_consistent(&self, kind_count: usize) -> bool {
        let n = self.nodes.len();
        if n == 0 || self.names.len() != self.lower.len() || self.nodes[0].parent != 0 { return false; }
        self.nodes.iter().enumerate().all(|(id, node)| {
            let children_ok = (node.first_child as u64 + node.child_count as u64) <= n as u64
                // children come after their parent, which also rules out cycles
                && (node.child_count == 0 || node.first_child as usize > id);
            let parent = &self.nodes[(node.parent as usize).min(n - 1)];
            let parent_ok = id == 0 || ((node.parent as usize) < id
                && (parent.first_child as usize..parent.first_child as usize + parent.child_count as usize).contains(&id));
            children_ok && parent_ok
                && (node.name_off as u64 + node.name_len as u64) <= self.names.len() as u64
                && (node.kind as usize) < kind_count
                && (node.is_dir() || node.child_count == 0)
        })
    }

    /// Every place `id` may be on disk: one path, or for a folder that spans disks, the
    /// same relative path on each of them (not all need exist).
    pub fn paths_on_disk(&self, id: u32) -> Vec<Vec<u8>> {
        if (self.nodes[id as usize].source as usize) < self.sources.len() {
            return vec![self.real_path(id)];
        }
        self.sources.iter().map(|s| {
            let mut p = s.path.clone();
            self.relative(id, &mut p);
            p
        }).collect()
    }

    pub fn memory_bytes(&self) -> u64 {
        (self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity() + self.lower.capacity()) as u64
    }

    /// Space used on the scanned filesystems by things outside the scan.
    pub fn other_space(&self) -> u64 {
        let used = self.fs_total.saturating_sub(self.fs_free);
        used.saturating_sub(self.nodes[ROOT as usize].physical)
    }
}
