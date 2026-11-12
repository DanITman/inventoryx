// InventoryX — parallel directory scanner.
//
// Reads only. Several sources (for example the same share on several disks) are merged
// into one tree. Hard links are counted once; other filesystems are not entered, except
// btrfs subvolumes and ZFS child datasets of the filesystem being scanned.
// GPL-3.0-or-later.

use crate::index::*;
use crate::kinds::{self, KindTable};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
pub struct ScanSpec {
    pub label: String,
    pub display_root: Vec<u8>,
    pub sources: Vec<Source>,
}

/// Progress of one source (one disk). When the source is a whole filesystem, the number
/// of entries or bytes it holds is known up front, which gives a real percentage.
#[derive(Default)]
pub struct SourceProgress {
    pub label: String,
    pub expected_entries: u64,
    pub expected_bytes: u64,
    pub entries: AtomicU64,
    pub bytes: AtomicU64,
}

impl SourceProgress {
    pub fn fraction(&self) -> Option<f64> {
        let f = if self.expected_entries > 0 {
            self.entries.load(Relaxed) as f64 / self.expected_entries as f64
        } else if self.expected_bytes > 0 {
            self.bytes.load(Relaxed) as f64 / self.expected_bytes as f64
        } else {
            return None;
        };
        Some(f.min(1.0))
    }
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
    pub sources: Mutex<Arc<Vec<SourceProgress>>>,
}

pub struct Estimate {
    /// Overall share of the work done, when it can be known.
    pub fraction: Option<f64>,
    pub eta_ms: Option<u64>,
}

impl Progress {
    /// Disks are read in parallel, so the scan ends when the slowest one does.
    pub fn estimate(&self, elapsed_ms: u64) -> Estimate {
        let sources = self.sources.lock().unwrap().clone();
        let by_entries: Vec<&SourceProgress> = sources.iter().filter(|s| s.expected_entries > 0).collect();
        let fraction = if !by_entries.is_empty() {
            let done: u64 = by_entries.iter().map(|s| s.entries.load(Relaxed).min(s.expected_entries)).sum();
            Some(done as f64 / by_entries.iter().map(|s| s.expected_entries).sum::<u64>() as f64)
        } else if !sources.is_empty() && sources.iter().all(|s| s.expected_bytes > 0) {
            let done: u64 = sources.iter().map(|s| s.bytes.load(Relaxed).min(s.expected_bytes)).sum();
            Some(done as f64 / sources.iter().map(|s| s.expected_bytes).sum::<u64>() as f64)
        } else {
            None
        };
        let mut eta = Some(0u64);
        for s in sources.iter() {
            eta = match (eta, s.fraction()) {
                (Some(worst), Some(f)) if f >= 1.0 => Some(worst),
                (Some(worst), Some(f)) if f >= 0.01 =>
                    Some(worst.max((elapsed_ms as f64 * (1.0 - f) / f) as u64)),
                _ => None,
            };
        }
        if fraction.is_none() { eta = None; }
        Estimate { fraction: fraction.map(|f| f.min(0.99)), eta_ms: eta }
    }
}

#[derive(Debug, PartialEq)]
pub enum ScanError {
    Cancelled,
    NoSources(String),
    TooLarge,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ScanError::Cancelled => write!(f, "Scan cancelled"),
            ScanError::NoSources(s) => write!(f, "{s}"),
            ScanError::TooLarge => write!(f, "Too many files for one scan; scan a smaller part"),
        }
    }
}

const NONE: u32 = u32::MAX;

struct Entry {
    logical: u64,
    physical: u64,
    mtime: u32,
    name_off: u32,
    files: u32,
    subdir: u32,
    name_len: u16,
    kind: u16,
    flags: u8,
}

struct DirOut {
    id: u32,
    parent: u32,
    parent_entry: u32,
    source: u8,
    unreadable: bool,
    own_physical: u64,
    names: Vec<u8>,
    entries: Vec<Entry>,
}

impl DirOut {
    fn name(&self, e: &Entry) -> &[u8] { &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize] }
}

struct Task {
    dir: u32,
    parent: u32,
    parent_entry: u32,
    path: PathBuf,
    source: u8,
    dev: u64,
    own_physical: u64,
}

struct Queue {
    tasks: Mutex<Vec<Task>>,
    cv: Condvar,
    /// Tasks queued anywhere (shared or in a worker's own stack) or being processed.
    pending: AtomicUsize,
    idle: AtomicUsize,
}

impl Queue {
    fn take(&self) -> Option<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        loop {
            if let Some(t) = tasks.pop() { return Some(t); }
            if self.pending.load(Relaxed) == 0 { return None; }
            self.idle.fetch_add(1, Relaxed);
            tasks = self.cv.wait_timeout(tasks, Duration::from_millis(50)).unwrap().0;
            self.idle.fetch_sub(1, Relaxed);
        }
    }

    fn finish_one(&self) {
        if self.pending.fetch_sub(1, Relaxed) == 1 {
            let _guard = self.tasks.lock().unwrap();
            self.cv.notify_all();
        }
    }
}

/// Which directories on another device may still be entered.
struct MountPolicy {
    have_table: bool,
    mounts: HashMap<Vec<u8>, (String, String)>,
}

/// Mount point, filesystem type and source of a mount.
type FsInfo = (Vec<u8>, String, String);

impl MountPolicy {
    fn load() -> Self {
        Self::from_table(&std::fs::read("/proc/self/mountinfo").unwrap_or_default())
    }

    fn from_table(text: &[u8]) -> Self {
        let mut mounts = HashMap::new();
        let have_table = !text.is_empty();
        for line in text.split(|&b| b == b'\n') {
            let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            let Some(sep) = fields.iter().position(|f| *f == b"-") else { continue };
            if fields.len() < 5 || sep + 2 >= fields.len() { continue; }
            mounts.insert(unescape(fields[4]), (
                String::from_utf8_lossy(fields[sep + 1]).into_owned(),
                String::from_utf8_lossy(fields[sep + 2]).into_owned(),
            ));
        }
        MountPolicy { have_table, mounts }
    }

    /// The mount that holds `path`.
    fn fs_of(&self, path: &[u8]) -> FsInfo {
        let mut best: Option<(&Vec<u8>, &(String, String))> = None;
        for (mp, info) in &self.mounts {
            let inside = path == mp.as_slice() || mp.as_slice() == b"/"
                || (path.starts_with(mp) && path.get(mp.len()) == Some(&b'/'));
            if inside && best.map_or(true, |(b, _)| mp.len() > b.len()) { best = Some((mp, info)); }
        }
        best.map(|(mp, i)| (mp.clone(), i.0.clone(), i.1.clone())).unwrap_or_default()
    }

    fn is_mount_point(&self, path: &[u8]) -> bool { self.mounts.contains_key(path) }

    /// `path` is a directory whose device differs from the one being scanned.
    /// Anything not positively recognised is left alone.
    fn may_enter(&self, path: &[u8], root_fs: &FsInfo) -> bool {
        if !self.have_table { return false; }
        match self.mounts.get(path) {
            // Not a mount point, yet a different device: a btrfs subvolume, as long as the
            // folder still belongs to the mount being scanned.
            None => root_fs.1 == "btrfs" && self.fs_of(path).0 == root_fs.0,
            // A ZFS child dataset of the pool being scanned; never a snapshot.
            Some((fs, src)) => fs == "zfs" && root_fs.1 == "zfs" && !src.contains('@')
                && src.starts_with(&format!("{}/", root_fs.2)),
        }
    }
}

fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' && i + 3 < field.len() && field[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let v = field[i + 1..i + 4].iter().fold(0u32, |a, b| a * 8 + (b - b'0') as u32);
            out.push(v as u8);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    out
}

struct Shared {
    queue: Queue,
    next_dir: AtomicU32,
    hardlinks: Vec<Mutex<HashSet<(u64, u64)>>>,
    kinds: Arc<KindTable>,
    policy: MountPolicy,
    root_fs: Vec<FsInfo>,
    progress: Arc<Progress>,
    per_source: Arc<Vec<SourceProgress>>,
}

fn worker(sh: &Shared) -> Vec<DirOut> {
    let mut out = Vec::new();
    let mut local: Vec<Task> = Vec::new();
    let mut ext_cache: HashMap<Vec<u8>, u16> = HashMap::new();
    loop {
        let task = match local.pop() {
            Some(t) => t,
            None => match sh.queue.take() { Some(t) => t, None => break },
        };
        if !sh.progress.cancel.load(Relaxed) {
            out.push(read_dir(sh, task, &mut local, &mut ext_cache));
            // Share work when other threads are waiting.
            if local.len() > 1 && sh.queue.idle.load(Relaxed) > 0 {
                let keep = local.len() / 2;
                let mut tasks = sh.queue.tasks.lock().unwrap();
                tasks.extend(local.drain(..local.len() - keep));
                sh.queue.cv.notify_all();
            }
        }
        sh.queue.finish_one();
    }
    out
}

fn read_dir(sh: &Shared, task: Task, local: &mut Vec<Task>, ext_cache: &mut HashMap<Vec<u8>, u16>) -> DirOut {
    let mut d = DirOut {
        id: task.dir, parent: task.parent, parent_entry: task.parent_entry, source: task.source,
        unreadable: false, own_physical: task.own_physical, names: Vec::new(), entries: Vec::new(),
    };
    if let Ok(mut cur) = sh.progress.current.try_lock() {
        *cur = task.path.to_string_lossy().into_owned();
    }
    let iter = match std::fs::read_dir(&task.path) {
        Ok(i) => i,
        Err(_) => { d.unreadable = true; return d; }
    };
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut new_tasks = 0usize;
    for (n, ent) in iter.enumerate() {
        // A single folder can hold millions of entries; stay responsive to Cancel.
        if n & 1023 == 1023 && sh.progress.cancel.load(Relaxed) { break; }
        let Ok(ent) = ent else { d.unreadable = true; continue };
        // DirEntry::metadata does not follow symlinks.
        let Ok(md) = ent.metadata() else { continue };
        let os_name = ent.file_name();
        let name = os_name.as_bytes();
        if name.len() > u16::MAX as usize { continue; }
        let ft = md.file_type();
        let mut e = Entry {
            logical: 0, physical: 0, mtime: md.mtime().clamp(0, u32::MAX as i64) as u32,
            name_off: d.names.len() as u32, files: 0, subdir: NONE,
            name_len: name.len() as u16, kind: kinds::OTHER_FILE, flags: 0,
        };
        d.names.extend_from_slice(name);
        if ft.is_dir() {
            e.kind = kinds::FOLDER;
            e.flags = F_DIR;
            let path = task.path.join(&os_name);
            // ZFS exposes every snapshot under .zfs; reading it would count the pool many times.
            let enter = name != b".zfs" && (md.dev() == task.dev
                || sh.policy.may_enter(path.as_os_str().as_bytes(), &sh.root_fs[task.source as usize]));
            if enter {
                let id = sh.next_dir.fetch_add(1, Relaxed);
                e.subdir = id;
                local.push(Task {
                    dir: id, parent: task.dir, parent_entry: d.entries.len() as u32, path,
                    source: task.source, dev: md.dev(), own_physical: md.blocks() * 512,
                });
                new_tasks += 1;
            } else {
                e.flags |= F_MOUNT;
            }
        } else {
            e.files = 1;
            e.logical = md.len();
            e.physical = md.blocks() * 512;
            if ft.is_symlink() {
                e.kind = kinds::SYMLINK;
                e.flags = F_SYMLINK;
            } else if ft.is_file() {
                e.kind = match KindTable::extension(name) {
                    None => if md.mode() & 0o111 != 0 { kinds::EXECUTABLE } else { kinds::DOCUMENT },
                    Some(ext) => match ext_cache.get(&ext) {
                        Some(&k) => k,
                        None => { let k = sh.kinds.kind_for_ext(&ext); ext_cache.insert(ext, k); k }
                    },
                };
                if md.nlink() > 1 {
                    let key = (md.dev(), md.ino());
                    let shard = &sh.hardlinks[(md.ino() as usize) % sh.hardlinks.len()];
                    if !shard.lock().unwrap().insert(key) {
                        e.flags |= F_HARDLINK_DUP;
                        e.logical = 0;
                        e.physical = 0;
                    }
                }
            }
            files += 1;
            bytes += e.physical;
        }
        d.entries.push(e);
    }
    sh.queue.pending.fetch_add(new_tasks, Relaxed);
    sh.progress.files.fetch_add(files, Relaxed);
    sh.progress.bytes.fetch_add(bytes, Relaxed);
    sh.progress.dirs.fetch_add(1, Relaxed);
    if let Some(sp) = sh.per_source.get(task.source as usize) {
        sp.entries.fetch_add(files + 1, Relaxed);
        sp.bytes.fetch_add(bytes, Relaxed);
    }
    d
}

fn statvfs(path: &[u8]) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 { return None; }
    let unit = s.f_frsize as u64;
    Some((s.f_blocks as u64 * unit, s.f_bavail as u64 * unit))
}

/// Every mount point on this machine (empty where the kernel's table cannot be read).
/// Whether any of `paths` is on a filesystem of type `fs` (as named in the mount table).
pub fn any_on_filesystem<'a>(paths: impl Iterator<Item = &'a [u8]>, fs: &str) -> bool {
    let policy = MountPolicy::load();
    paths.into_iter().any(|p| policy.fs_of(p).1 == fs)
}

pub fn mount_points() -> Vec<Vec<u8>> { MountPolicy::load().mounts.into_keys().collect() }

/// Total and available bytes of the filesystem holding `path`.
pub fn fs_space(path: &[u8]) -> (u64, u64) { statvfs(path).unwrap_or((0, 0)) }

/// Bytes and inodes in use on the filesystem holding `path` (inodes are 0 where the
/// filesystem does not count them, as on btrfs).
fn fs_usage(path: &[u8]) -> (u64, u64) {
    let Ok(c) = std::ffi::CString::new(path) else { return (0, 0) };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 { return (0, 0); }
    ((s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * s.f_frsize as u64,
     (s.f_files as u64).saturating_sub(s.f_ffree as u64))
}

pub fn start(spec: ScanSpec, kinds: Arc<KindTable>, progress: Arc<Progress>) -> Result<Index, ScanError> {
    let began = Instant::now();
    let policy = MountPolicy::load();
    let mut roots = Vec::new();
    let mut sources = Vec::new();
    let mut root_fs = Vec::new();
    let mut seen_dev = HashSet::new();
    let (mut fs_total, mut fs_free) = (0u64, 0u64);
    let mut per_source = Vec::new();
    for s in &spec.sources {
        if sources.len() >= SOURCE_MERGED as usize { break; }
        // The real path: `..`, doubled slashes and symlinks must not confuse the mount check.
        let Ok(path) = std::fs::canonicalize(OsStr::from_bytes(&s.path)) else { continue };
        let Ok(md) = std::fs::metadata(&path) else { continue };
        if !md.is_dir() { continue; }
        let s = &Source { label: s.label.clone(), path: path.as_os_str().as_bytes().to_vec() };
        if seen_dev.insert(md.dev()) {
            let (t, f) = fs_space(&s.path);
            fs_total += t;
            fs_free += f;
        }
        // Scanning a whole filesystem: what it holds is known before the first read.
        let (expected_bytes, expected_entries) =
            if policy.is_mount_point(&s.path) { fs_usage(&s.path) } else { (0, 0) };
        per_source.push(SourceProgress { label: s.label.clone(), expected_entries, expected_bytes,
                                         ..Default::default() });
        root_fs.push(policy.fs_of(&s.path));
        roots.push(Task {
            dir: sources.len() as u32, parent: NONE, parent_entry: NONE, path,
            source: sources.len() as u8, dev: md.dev(), own_physical: md.blocks() * 512,
        });
        sources.push(s.clone());
    }
    if roots.is_empty() {
        return Err(ScanError::NoSources(format!("Nothing to scan: {} is not a readable folder", spec.label)));
    }
    let n_roots = roots.len();
    let per_source = Arc::new(per_source);
    *progress.sources.lock().unwrap() = per_source.clone();
    let sh = Shared {
        queue: Queue {
            pending: AtomicUsize::new(n_roots), idle: AtomicUsize::new(0),
            tasks: Mutex::new(roots), cv: Condvar::new(),
        },
        next_dir: AtomicU32::new(n_roots as u32),
        hardlinks: (0..64).map(|_| Mutex::new(HashSet::new())).collect(),
        kinds: kinds.clone(), policy, root_fs, progress: progress.clone(), per_source,
    };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 32);
    let outs: Vec<Vec<DirOut>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| worker(&sh))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    if progress.cancel.load(Relaxed) { return Err(ScanError::Cancelled); }

    let n_dirs = sh.next_dir.load(Relaxed) as usize;
    let mut dirs: Vec<Option<DirOut>> = (0..n_dirs).map(|_| None).collect();
    for d in outs.into_iter().flatten() {
        let id = d.id as usize;
        dirs[id] = Some(d);
    }
    build(spec, sources, dirs, n_roots, kinds, fs_total, fs_free, began.elapsed())
}

struct Item {
    logical: u64,
    physical: u64,
    mtime: u32,
    files: u32,
    dir: u32,
    entry: u32,
    sub_start: u32,
    sub_len: u32,
    kind: u16,
    flags: u8,
    source: u8,
}

#[allow(clippy::too_many_arguments)]
fn build(spec: ScanSpec, sources: Vec<Source>, mut dirs: Vec<Option<DirOut>>, n_roots: usize,
         kinds: Arc<KindTable>, fs_total: u64, fs_free: u64, duration: Duration) -> Result<Index, ScanError> {
    // Node ids and name offsets are 32-bit.
    let entries: u64 = dirs.iter().flatten().map(|d| d.entries.len() as u64).sum();
    let name_bytes: u64 = dirs.iter().flatten().map(|d| d.names.len() as u64).sum();
    if entries >= (u32::MAX - 8) as u64 || name_bytes >= (u32::MAX - 2048) as u64 {
        return Err(ScanError::TooLarge);
    }
    // Totals, bottom-up. A directory's id is always greater than its parent's.
    let mut totals = vec![(0u64, 0u64, 0u32); dirs.len()];
    let mut unreadable = 0u32;
    for id in (0..dirs.len()).rev() {
        let Some(d) = &dirs[id] else { continue };
        if d.unreadable { unreadable += 1; }
        let mut t = (0u64, d.own_physical, 0u32);
        for e in &d.entries {
            t.0 = t.0.saturating_add(e.logical);
            t.1 = t.1.saturating_add(e.physical);
            t.2 = t.2.saturating_add(e.files);
        }
        totals[id] = t;
        let (parent, pe, bad) = (d.parent, d.parent_entry, d.unreadable);
        if parent != NONE {
            if let Some(p) = &mut dirs[parent as usize] {
                let e = &mut p.entries[pe as usize];
                e.logical = t.0;
                e.physical = t.1;
                e.files = t.2;
                if bad { e.flags |= F_UNREADABLE; }
            }
        }
    }

    let total_entries: usize = dirs.iter().flatten().map(|d| d.entries.len()).sum();
    let mut nodes: Vec<Node> = Vec::with_capacity(total_entries + 1);
    let mut names: Vec<u8> = Vec::with_capacity(dirs.iter().flatten().map(|d| d.names.len()).sum::<usize>() + 64);

    let label = spec.label.as_bytes();
    let label = &label[..label.len().min(1000)];
    names.extend_from_slice(label);
    let mut root = Node {
        logical: 0, physical: 0, name_off: 0, parent: 0, first_child: 0, child_count: 0, files: 0,
        mtime: 0, name_len: label.len() as u16, kind: kinds::FOLDER, flags: F_DIR,
        source: if n_roots == 1 { 0 } else { SOURCE_MERGED }, reserved: 0,
    };
    for t in &totals[..n_roots] {
        root.logical = root.logical.saturating_add(t.0);
        root.physical = root.physical.saturating_add(t.1);
        root.files = root.files.saturating_add(t.2);
    }
    nodes.push(root);

    // Breadth-first: each folder's children are stored next to each other.
    let mut queue: std::collections::VecDeque<(u32, Vec<u32>)> = Default::default();
    queue.push_back((0, (0..n_roots as u32).collect()));
    let mut items: Vec<Item> = Vec::new();
    let mut subs: Vec<u32> = Vec::new();
    while let Some((node_id, group)) = queue.pop_front() {
        items.clear();
        subs.clear();
        let mut by_name: HashMap<&[u8], usize> = HashMap::new();
        let merging = group.len() > 1;
        for &did in &group {
            let Some(d) = &dirs[did as usize] else { continue };
            for (i, e) in d.entries.iter().enumerate() {
                let is_dir = e.flags & F_DIR != 0;
                if merging && is_dir {
                    if let Some(&at) = by_name.get(d.name(e)) {
                        let it = &mut items[at];
                        it.logical = it.logical.saturating_add(e.logical);
                        it.physical = it.physical.saturating_add(e.physical);
                        it.files = it.files.saturating_add(e.files);
                        it.mtime = it.mtime.max(e.mtime);
                        it.flags |= e.flags;
                        if it.source != d.source { it.source = SOURCE_MERGED; }
                        if e.subdir != NONE {
                            // Sub-directory lists of a merged item are chained through `subs`
                            // as (dir id, previous link) pairs.
                            subs.push(e.subdir);
                            subs.push(it.sub_start);
                            it.sub_start = subs.len() as u32 - 2;
                            it.sub_len += 1;
                        }
                        continue;
                    }
                    by_name.insert(d.name(e), items.len());
                }
                let mut it = Item {
                    logical: e.logical, physical: e.physical, mtime: e.mtime, files: e.files, dir: did, entry: i as u32,
                    sub_start: NONE, sub_len: 0, kind: e.kind, flags: e.flags, source: d.source,
                };
                if e.subdir != NONE {
                    subs.push(e.subdir);
                    subs.push(NONE);
                    it.sub_start = subs.len() as u32 - 2;
                    it.sub_len = 1;
                }
                items.push(it);
            }
        }
        let name_of = |it: &Item| -> &[u8] {
            let d = dirs[it.dir as usize].as_ref().unwrap();
            d.name(&d.entries[it.entry as usize])
        };
        items.sort_by(|a, b| b.physical.cmp(&a.physical).then_with(|| name_of(a).cmp(name_of(b))));

        let first = nodes.len() as u32;
        nodes[node_id as usize].first_child = first;
        nodes[node_id as usize].child_count = items.len() as u32;
        for it in &items {
            let name = name_of(it);
            let id = nodes.len() as u32;
            nodes.push(Node {
                logical: it.logical, physical: it.physical, name_off: names.len() as u32, parent: node_id,
                first_child: 0, child_count: 0, files: it.files, mtime: it.mtime, name_len: name.len() as u16,
                kind: it.kind, flags: it.flags, source: it.source, reserved: 0,
            });
            names.extend_from_slice(name);
            if it.sub_len > 0 {
                let mut group = Vec::with_capacity(it.sub_len as usize);
                let mut link = it.sub_start;
                while link != NONE {
                    group.push(subs[link as usize]);
                    link = subs[link as usize + 1];
                }
                group.reverse();
                queue.push_back((id, group));
            }
        }
    }
    drop(dirs);
    let lower = names.to_ascii_lowercase();
    Ok(Index {
        nodes, names, lower, kinds, label: spec.label, display_root: spec.display_root, sources,
        unreadable, finished: SystemTime::now(), duration, fs_total, fs_free,
    })
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    /// A fresh fixture folder. Fixtures are left in the temp directory on purpose: this
    /// project's tests never delete anything.
    pub fn fixture(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "inventoryx-test-{}-{}-{}-{name}", std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(),
            N.fetch_add(1, Relaxed)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    pub fn scan_paths(paths: &[&Path]) -> Index {
        let spec = ScanSpec {
            label: "test".into(),
            display_root: b"/display".to_vec(),
            sources: paths.iter().enumerate().map(|(i, p)| Source {
                label: format!("s{i}"), path: p.as_os_str().as_bytes().to_vec() }).collect(),
        };
        start(spec, Arc::new(KindTable::new()), Arc::new(Progress::default())).unwrap()
    }

    /// Whether the temp directory tells `Media` from `media` (Linux does, macOS usually not).
    pub fn case_sensitive_fs() -> bool {
        let p = fixture("CaseProbe");
        !p.parent().unwrap().join(p.file_name().unwrap().to_string_lossy().to_lowercase()).exists()
    }

    pub fn find(ix: &Index, parent: u32, name: &[u8]) -> Option<u32> {
        ix.children(parent).find(|&c| ix.name(c) == name)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn sizes_sum_and_children_sorted() {
        let root = fixture("sum");
        write(&root.join("a/big.mkv"), 30_000);
        write(&root.join("a/small.txt"), 10);
        write(&root.join("b/mid.jpg"), 5_000);
        let ix = scan_paths(&[&root]);
        let r = &ix.nodes[0];
        assert_eq!(r.logical, 35_010);
        assert_eq!(r.files, 3);
        let a = find(&ix, 0, b"a").unwrap();
        assert_eq!(ix.nodes[a as usize].logical, 30_010);
        assert_eq!(ix.nodes[a as usize].files, 2);
        let kids: Vec<u32> = ix.children(a).collect();
        assert_eq!(ix.name(kids[0]), b"big.mkv");
        assert!(ix.nodes[kids[0] as usize].physical >= ix.nodes[kids[1] as usize].physical);
        // every node's parent contains it
        for id in 1..ix.nodes.len() as u32 {
            let p = ix.nodes[id as usize].parent;
            assert!(ix.children(p).contains(&id));
        }
        assert_eq!(ix.display_path(kids[0]), b"/display/a/big.mkv".to_vec());
        let real = std::fs::canonicalize(&root).unwrap();
        assert_eq!(ix.real_path(kids[0]), real.join("a/big.mkv").as_os_str().as_bytes().to_vec());
    }

    #[test]
    fn hard_link_counted_once() {
        let root = fixture("hl");
        write(&root.join("one"), 8_000);
        std::fs::hard_link(root.join("one"), root.join("two")).unwrap();
        let ix = scan_paths(&[&root]);
        assert_eq!(ix.nodes[0].logical, 8_000);
        assert_eq!(ix.nodes[0].files, 2);
    }

    #[test]
    fn sources_merge_by_name_case_sensitively() {
        let d1 = fixture("d1");
        let d2 = fixture("d2");
        write(&d1.join("Media/x.mkv"), 1_000);
        write(&d2.join("Media/y.mkv"), 2_000);
        write(&d2.join("Media/sub/z.mkv"), 4_000);
        write(&d1.join("Media/sub/w.mkv"), 8_000);
        // A third source, so the test also holds on a case-insensitive development disk.
        let d3 = fixture("d3");
        write(&d3.join("media/lower.mkv"), 500);
        let ix = scan_paths(&[&d1, &d2, &d3]);
        assert_eq!(ix.nodes[0].child_count, 2, "Media and media stay separate");
        let m = find(&ix, 0, b"Media").unwrap();
        assert_eq!(ix.nodes[m as usize].logical, 15_000);
        assert_eq!(ix.nodes[m as usize].source, SOURCE_MERGED);
        assert_eq!(ix.nodes[m as usize].child_count, 3);
        let sub = find(&ix, m, b"sub").unwrap();
        assert_eq!(ix.nodes[sub as usize].logical, 12_000);
        assert_eq!(ix.nodes[sub as usize].child_count, 2);
        let y = find(&ix, m, b"y.mkv").unwrap();
        let real = std::fs::canonicalize(&d2).unwrap();
        assert_eq!(ix.real_path(y), real.join("Media/y.mkv").as_os_str().as_bytes().to_vec());
        assert_eq!(ix.display_path(y), b"/display/Media/y.mkv".to_vec());
        let lower = find(&ix, 0, b"media").unwrap();
        assert_eq!(ix.nodes[lower as usize].source, 2);
    }

    #[test]
    fn unreadable_folder_is_counted_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 { return; }
        let root = fixture("perm");
        write(&root.join("ok/f"), 100);
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let ix = scan_paths(&[&root]);
        assert_eq!(ix.unreadable, 1);
        let locked = find(&ix, 0, b"locked").unwrap();
        assert!(ix.nodes[locked as usize].flags & F_UNREADABLE != 0);
        assert_eq!(ix.nodes[0].logical, 100);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_name_survives() {
        let root = fixture("bytes");
        let name = OsStr::from_bytes(b"bad\xff\"name<\n.bin");
        write(&root.join(name), 10);
        let ix = scan_paths(&[&root]);
        assert_eq!(ix.name(ix.children(0).start), b"bad\xff\"name<\n.bin");
    }

    #[test]
    fn cancel_and_missing_source() {
        let root = fixture("cancel");
        write(&root.join("f"), 1);
        let p = Arc::new(Progress::default());
        p.cancel.store(true, Relaxed);
        let spec = ScanSpec { label: "t".into(), display_root: b"/".to_vec(),
            sources: vec![Source { label: "s".into(), path: root.as_os_str().as_bytes().to_vec() }] };
        assert_eq!(start(spec, Arc::new(KindTable::new()), p).err(), Some(ScanError::Cancelled));
        let spec = ScanSpec { label: "gone".into(), display_root: b"/".to_vec(),
            sources: vec![Source { label: "s".into(), path: b"/definitely/not/here".to_vec() }] };
        assert!(matches!(start(spec, Arc::new(KindTable::new()), Arc::new(Progress::default())),
                         Err(ScanError::NoSources(_))));
    }

    const TABLE: &[u8] = b"\
22 1 0:20 / / rw - rootfs rootfs rw\n\
23 22 0:21 / /proc rw - proc proc rw\n\
40 22 0:40 / /mnt/cache rw - btrfs /dev/nvme0n1p1 rw\n\
41 22 9:1 / /mnt/disk1 rw - xfs /dev/md1p1 rw\n\
42 22 0:50 / /mnt/tank rw - zfs tank rw\n\
43 42 0:51 / /mnt/tank/data rw - zfs tank/data rw\n\
44 42 0:52 / /mnt/tank/data/.zfs/snapshot/s1 rw - zfs tank/data@s1 rw\n\
45 40 0:60 / /mnt/cache/docker/overlay/merged rw - overlay overlay rw\n\
46 22 0:70 / /mnt/other rw - zfs other/tank rw\n";

    #[test]
    fn mount_policy_fails_closed() {
        let p = MountPolicy::from_table(TABLE);
        let cache = p.fs_of(b"/mnt/cache");
        let tank = p.fs_of(b"/mnt/tank");
        let disk1 = p.fs_of(b"/mnt/disk1");
        let rootfs = p.fs_of(b"/");
        assert_eq!(cache.1, "btrfs");
        // the filesystem type is that of the deepest mount holding the path
        assert_eq!(p.fs_of(b"/mnt/tank/data/films").1, "zfs");
        assert_eq!(p.fs_of(b"/mnt/tankard").1, "rootfs");
        assert_eq!(p.fs_of(b"/mnt/disk1/films").1, "xfs");
        // btrfs subvolume: different device, not a mount point, inside the same mount
        assert!(p.may_enter(b"/mnt/cache/appdata/subvol", &cache));
        // mounts inside the pool that are something else
        assert!(!p.may_enter(b"/mnt/cache/docker/overlay/merged", &cache));
        // ZFS child dataset yes, snapshot and foreign pool no
        assert!(p.may_enter(b"/mnt/tank/data", &tank));
        assert!(!p.may_enter(b"/mnt/tank/data/.zfs/snapshot/s1", &tank));
        assert!(!p.may_enter(b"/mnt/other", &tank));
        // unknown different-device folders on non-btrfs roots are never entered
        assert!(!p.may_enter(b"/mnt/disk1/odd", &disk1));
        assert!(!p.may_enter(b"/proc", &rootfs));
        assert!(!p.may_enter(b"/var/../proc", &rootfs));
        assert!(!p.may_enter(b"/newly/mounted", &rootfs));
        assert!(!MountPolicy::from_table(b"").may_enter(b"/x", &rootfs));
    }

    #[test]
    fn source_paths_are_canonical() {
        let root = fixture("canon");
        write(&root.join("a/f"), 10);
        let odd = root.join("a/../a/.");
        let ix = scan_paths(&[&odd]);
        let real = std::fs::canonicalize(root.join("a")).unwrap();
        assert_eq!(ix.sources[0].path, real.as_os_str().as_bytes().to_vec());
        assert_eq!(ix.nodes[0].logical, 10);
    }

    #[test]
    fn estimate_follows_the_slowest_disk() {
        let p = Progress::default();
        let src = |label: &str, expected: u64, done: u64| SourceProgress {
            label: label.into(), expected_entries: expected, entries: AtomicU64::new(done), ..Default::default() };
        *p.sources.lock().unwrap() = Arc::new(vec![src("disk1", 1000, 100), src("disk2", 100, 100)]);
        let e = p.estimate(10_000);
        assert!((e.fraction.unwrap() - 200.0 / 1100.0).abs() < 1e-9);
        assert_eq!(e.eta_ms, Some(90_000), "disk1 is 10% done after 10 s");
        // too early to say
        *p.sources.lock().unwrap() = Arc::new(vec![src("disk1", 1000, 5)]);
        assert_eq!(p.estimate(10_000).eta_ms, None);
        // a folder, not a whole disk: nothing to measure against
        *p.sources.lock().unwrap() = Arc::new(vec![src("share", 0, 50)]);
        let e = p.estimate(10_000);
        assert!(e.fraction.is_none() && e.eta_ms.is_none());
        // never claims to be finished while still running
        *p.sources.lock().unwrap() = Arc::new(vec![src("disk1", 100, 250)]);
        assert_eq!(p.estimate(10_000).fraction, Some(0.99));
    }

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape(b"/mnt/my\\040disk"), b"/mnt/my disk".to_vec());
        assert_eq!(unescape(b"/plain"), b"/plain".to_vec());
    }
}
