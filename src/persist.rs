// InventoryX — saving the index to disk and loading it back.
//
// The saved scan lets the page open straight into the last result after a reboot or a
// restart of the program. The file is the program's own; it is created or overwritten in
// place and marked complete only after everything has been written, so a crash halfway
// leaves a file that is ignored, never a wrong one.
// GPL-3.0-or-later.

use crate::index::*;
use crate::kinds::KindTable;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"IXINDEX\0";
const VERSION: u32 = 1;
const HEADER: usize = 40;
const VALID_AT: u64 = 8;
pub const FILE_NAME: &str = "last-scan.ixindex";

fn checksum(bytes: &[u8]) -> u64 {
    // FNV-1a, eight bytes at a time: enough to catch truncation and corruption.
    let mut h: u64 = 0xcbf29ce484222325;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x100000001b3);
    }
    for &b in chunks.remainder() { h = (h ^ b as u64).wrapping_mul(0x100000001b3); }
    h
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> { buf: &'a [u8], at: usize }

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.buf.get(self.at..end)?;
        self.at = end;
        Some(s)
    }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }
    fn string(&mut self) -> Option<String> { String::from_utf8(self.bytes()?.to_vec()).ok() }
}

fn encode(ix: &Index, target: &str) -> Vec<u8> {
    let node_bytes = ix.nodes.len() * std::mem::size_of::<Node>();
    let mut p = Vec::with_capacity(node_bytes + ix.names.len() + 4096);
    put(&mut p, target.as_bytes());
    put(&mut p, ix.label.as_bytes());
    put(&mut p, &ix.display_root);
    p.extend_from_slice(&(ix.sources.len() as u64).to_le_bytes());
    for s in &ix.sources {
        put(&mut p, s.label.as_bytes());
        put(&mut p, &s.path);
    }
    let kinds = ix.kinds.names();
    p.extend_from_slice(&(kinds.len() as u64).to_le_bytes());
    for k in &kinds { put(&mut p, k.as_bytes()); }
    let finished = ix.finished.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    for v in [ix.unreadable as u64, finished, ix.duration.as_millis() as u64, ix.fs_total, ix.fs_free] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    // Node is plain integers with no hidden padding (see index.rs), so its memory is its
    // file format. The header records its size; another layout is refused on load.
    let raw = unsafe { std::slice::from_raw_parts(ix.nodes.as_ptr() as *const u8, node_bytes) };
    put(&mut p, raw);
    put(&mut p, &ix.names);

    let mut out = Vec::with_capacity(HEADER + p.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[0u8; 8]); // byte 8: 1 once the file is complete
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(std::mem::size_of::<Node>() as u32).to_le_bytes());
    out.extend_from_slice(&checksum(&p).to_le_bytes());
    out.extend_from_slice(&(p.len() as u64).to_le_bytes());
    out.extend_from_slice(&p);
    out
}

/// Writes the index to `dir`, creating that one folder if needed (never its parents).
pub fn save(dir: &Path, ix: &Index, target: &str) -> std::io::Result<PathBuf> {
    if !dir.is_dir() { std::fs::create_dir(dir)?; }
    let path = dir.join(FILE_NAME);
    let bytes = encode(ix, target);
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    file.write_all_at(&[1], VALID_AT)?;
    file.sync_all()?;
    Ok(path)
}

fn decode(buf: &[u8]) -> Option<(Index, String)> {
    if buf.len() < HEADER || &buf[..8] != MAGIC || buf[8] != 1 { return None; }
    let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    if u32_at(16) != VERSION || u32_at(20) as usize != std::mem::size_of::<Node>() { return None; }
    let payload = &buf[HEADER..];
    if u64_at(32) != payload.len() as u64 || u64_at(24) != checksum(payload) { return None; }

    let mut r = Reader { buf: payload, at: 0 };
    let target = r.string()?;
    let label = r.string()?;
    let display_root = r.bytes()?.to_vec();
    let mut sources = Vec::new();
    for _ in 0..r.u64()?.min(256) {
        sources.push(Source { label: r.string()?, path: r.bytes()?.to_vec() });
    }
    let mut kinds = Vec::new();
    for _ in 0..r.u64()?.min(u16::MAX as u64) { kinds.push(r.string()?); }
    let unreadable = r.u64()? as u32;
    let finished = UNIX_EPOCH + Duration::from_millis(r.u64()?);
    let duration = Duration::from_millis(r.u64()?);
    let fs_total = r.u64()?;
    let fs_free = r.u64()?;
    let raw = r.bytes()?;
    let size = std::mem::size_of::<Node>();
    if raw.is_empty() || raw.len() % size != 0 { return None; }
    let count = raw.len() / size;
    let mut nodes: Vec<Node> = Vec::with_capacity(count);
    // Every bit pattern is a valid Node; the links are checked by `Index::is_consistent`.
    unsafe {
        std::ptr::copy_nonoverlapping(raw.as_ptr(), nodes.as_mut_ptr() as *mut u8, raw.len());
        nodes.set_len(count);
    }
    let names = r.bytes()?.to_vec();
    let lower = names.to_ascii_lowercase();
    let kind_count = kinds.len();
    let ix = Index {
        nodes, names, lower, kinds: Arc::new(KindTable::with_names(kinds)), label, display_root,
        sources, unreadable, finished, duration, fs_total, fs_free,
    };
    ix.is_consistent(kind_count).then_some((ix, target))
}

/// Loads the saved index from `dir`, or None if there is none or it is damaged.
pub fn load(dir: &Path) -> Option<(Index, String)> {
    use std::os::unix::fs::MetadataExt;
    let path = dir.join(FILE_NAME);
    // Only a file this program's own user wrote, that nobody else can write to.
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.is_file() || md.uid() != unsafe { libc::geteuid() } || md.mode() & 0o022 != 0 { return None; }
    decode(&std::fs::read(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;

    fn sample() -> (Index, PathBuf) {
        let root = fixture("persist");
        write(&root.join("films/a.mkv"), 40_000);
        write(&root.join("films/b.mkv"), 20_000);
        write(&root.join("pic.jpg"), 3_000);
        (scan_paths(&[&root]), fixture("persist-store"))
    }

    #[test]
    fn round_trip_keeps_everything() {
        let (ix, store) = sample();
        let path = save(&store, &ix, "share:Media").unwrap();
        assert!(path.ends_with(FILE_NAME));
        let (back, target) = load(&store).unwrap();
        assert_eq!(target, "share:Media");
        assert_eq!(back.nodes.len(), ix.nodes.len());
        assert_eq!(back.label, ix.label);
        assert_eq!(back.finished, UNIX_EPOCH + Duration::from_millis(
            ix.finished.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64));
        for id in 0..ix.nodes.len() as u32 {
            assert_eq!(back.name(id), ix.name(id));
            assert_eq!(back.nodes[id as usize].physical, ix.nodes[id as usize].physical);
            assert_eq!(back.display_path(id), ix.display_path(id));
            assert_eq!(back.real_path(id), ix.real_path(id));
            assert_eq!(back.kinds.name(back.nodes[id as usize].kind), ix.kinds.name(ix.nodes[id as usize].kind));
        }
        assert_eq!(crate::stats::search(&back, "MKV", false, 10).1, 2);
        // a file others could have written is not trusted
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&store).is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // saving again over the old file works
        save(&store, &ix, "share:Media").unwrap();
        assert!(load(&store).is_some());
    }

    #[test]
    fn damaged_or_unfinished_files_are_ignored() {
        let (ix, store) = sample();
        let good = encode(&ix, "t");
        let mut marked = good.clone();
        marked[8] = 1;
        assert!(decode(&marked).is_some());
        assert!(decode(&good).is_none(), "not marked complete");
        assert!(decode(&marked[..marked.len() - 9]).is_none(), "truncated");
        let mut flipped = marked.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0x40;
        assert!(decode(&flipped).is_none(), "corrupted");
        let mut other = marked.clone();
        other[16] = 99;
        assert!(decode(&other).is_none(), "other version");
        assert!(decode(b"short").is_none());
        assert!(load(&store).is_none(), "nothing saved yet");
    }

    #[test]
    fn broken_links_are_refused_even_with_a_good_checksum() {
        let (mut ix, _) = sample();
        ix.nodes[1].first_child = 1_000_000;
        ix.nodes[1].child_count = 5;
        let mut bytes = encode(&ix, "t");
        bytes[8] = 1;
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn save_does_not_create_parent_folders() {
        let (ix, store) = sample();
        assert!(save(&store.join("missing/deeper"), &ix, "t").is_err());
        assert!(!store.join("missing").exists());
    }
}
