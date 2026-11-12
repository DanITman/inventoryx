// InventoryX — the last look at an item before a delete, and the delete itself.
//
// Both use one walk. It opens every folder relative to its parent's open handle and never
// follows a symbolic link, so it cannot leave the tree it was pointed at. It refuses to
// enter a folder on another filesystem (a mounted disk, a ZFS dataset, a btrfs subvolume)
// or one that is a mount point.
//
// `survey` runs the walk and only counts. `remove` runs the same walk and unlinks each
// entry it visits; that one `unlinkat` call is the only line in this program that deletes
// anything. By the owner's standing rule no test calls `remove`: the walk is tested
// through `survey`, and `remove` is verified by review only.
// GPL-3.0-or-later.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One entry met by the walk: `name` inside the folder open as `dirfd`.
pub struct Visit<'a> {
    pub dirfd: RawFd,
    pub name: &'a CStr,
    pub stat: &'a libc::stat,
    pub is_dir: bool,
}

fn err(msg: &str) -> io::Error { io::Error::new(io::ErrorKind::Other, msg.to_string()) }

fn is_dir(st: &libc::stat) -> bool { (st.st_mode as u32) & (libc::S_IFMT as u32) == libc::S_IFDIR as u32 }

fn open_dir(dirfd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_at(dirfd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn stat_fd(fd: &OwnedFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 { return Err(io::Error::last_os_error()); }
    Ok(st)
}

/// Names in the folder open as `fd`, without `.` and `..`.
fn list(fd: &OwnedFd) -> io::Result<Vec<CString>> {
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 { return Err(io::Error::last_os_error()); }
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() { break; }
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." { names.push(name.to_owned()); }
    }
    unsafe { libc::closedir(dir) };
    Ok(names)
}

fn walk_dir(fd: &OwnedFd, dev: u64, path: &mut Vec<u8>, mounts: &HashSet<Vec<u8>>,
            visit: &mut dyn FnMut(&Visit) -> io::Result<()>) -> io::Result<()> {
    for name in list(fd)? {
        let st = stat_at(fd.as_raw_fd(), &name)?;
        let dir = is_dir(&st);
        if dir {
            let len = path.len();
            path.push(b'/');
            path.extend_from_slice(name.to_bytes());
            let result = (|| {
                if st.st_dev as u64 != dev || mounts.contains(path.as_slice()) {
                    return Err(err(&format!("{} is another filesystem", String::from_utf8_lossy(path))));
                }
                let child = open_dir(fd.as_raw_fd(), &name)?;
                // The folder just opened must be the one just examined.
                let opened = stat_fd(&child)?;
                if opened.st_dev as u64 != dev || opened.st_ino != st.st_ino {
                    return Err(err("a folder changed while it was being read"));
                }
                walk_dir(&child, dev, path, mounts, visit)
            })();
            path.truncate(len);
            result?;
        }
        visit(&Visit { dirfd: fd.as_raw_fd(), name: &name, stat: &st, is_dir: dir })?;
    }
    Ok(())
}

/// Visits everything inside `path`, deepest first, then `path` itself.
///
/// The caller must have checked that the parent of `path` is a real folder reached without
/// symbolic links. `expect_dir` is what the scan recorded; anything else is an error.
pub fn walk(path: &[u8], expect_dir: bool, mounts: &HashSet<Vec<u8>>,
            visit: &mut dyn FnMut(&Visit) -> io::Result<()>) -> io::Result<()> {
    let slash = path.iter().rposition(|&b| b == b'/').ok_or_else(|| err("not an absolute path"))?;
    let (parent, name) = (&path[..slash], &path[slash + 1..]);
    if parent.is_empty() || name.is_empty() || name == b"." || name == b".." { return Err(err("not a usable path")); }
    let parent_c = CString::new(parent).map_err(|_| err("bad path"))?;
    let name_c = CString::new(name).map_err(|_| err("bad path"))?;
    let parent_fd = open_dir(libc::AT_FDCWD, &parent_c)?;
    let parent_st = stat_fd(&parent_fd)?;
    let st = stat_at(parent_fd.as_raw_fd(), &name_c)?;
    let dir = is_dir(&st);
    if dir != expect_dir { return Err(err("this is no longer the same kind of item as when it was scanned")); }
    if dir {
        // A folder on a different device than its parent is the root of a mounted
        // filesystem, a dataset or a subvolume: not something to remove from here.
        if st.st_dev != parent_st.st_dev || mounts.contains(path) {
            return Err(err("this folder is a separate filesystem, dataset or subvolume"));
        }
        let fd = open_dir(parent_fd.as_raw_fd(), &name_c)?;
        let opened = stat_fd(&fd)?;
        if opened.st_dev != st.st_dev || opened.st_ino != st.st_ino {
            return Err(err("the folder changed while it was being read"));
        }
        let mut buf = path.to_vec();
        walk_dir(&fd, st.st_dev as u64, &mut buf, mounts, visit)?;
    }
    visit(&Visit { dirfd: parent_fd.as_raw_fd(), name: &name_c, stat: &st, is_dir: dir })
}

#[derive(Debug, Default, PartialEq)]
pub struct Survey {
    /// Files, symlinks and other non-folders.
    pub files: u64,
    /// Real size of those files, each counted in full (hard links are not discounted).
    pub bytes: u64,
    /// The latest change time (ctime) of anything in the tree, folders included.
    pub newest_change: i64,
    /// Length and modification time of the item itself.
    pub len: u64,
    pub mtime: i64,
}

/// What is on disk at `path` right now. Reads only.
pub fn survey(path: &[u8], expect_dir: bool, mounts: &HashSet<Vec<u8>>) -> Result<Survey, String> {
    let mut s = Survey { newest_change: i64::MIN, ..Default::default() };
    walk(path, expect_dir, mounts, &mut |v| {
        s.newest_change = s.newest_change.max(v.stat.st_ctime as i64);
        if !v.is_dir {
            s.files += 1;
            s.bytes = s.bytes.saturating_add((v.stat.st_size as u64).max(v.stat.st_blocks as u64 * 512));
        }
        // The item itself is visited last.
        s.len = v.stat.st_size as u64;
        s.mtime = v.stat.st_mtime as i64;
        Ok(())
    }).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(path)))?;
    Ok(s)
}

/// The signature of `remove`, so tests can put a recorder in its place.
pub type Remover = fn(&[u8], bool, &HashSet<Vec<u8>>, u64, &AtomicU64) -> io::Result<()>;

/// Deletes `path` for good. NEVER CALLED BY A TEST.
///
/// Callers must already have applied `guard::judge`, checked the confirmation, and run
/// `survey` to see that the item is what the scan showed. Stops with an error if it meets
/// more than `max_files` files: more than the user was shown.
pub fn remove(path: &[u8], expect_dir: bool, mounts: &HashSet<Vec<u8>>, max_files: u64,
              done: &AtomicU64) -> io::Result<()> {
    let mut files = 0u64;
    walk(path, expect_dir, mounts, &mut |v| {
        if !v.is_dir {
            files += 1;
            if files > max_files { return Err(err("found more files than were shown; stopped")); }
        }
        let flag = if v.is_dir { libc::AT_REMOVEDIR } else { 0 };
        // The only call in InventoryX that deletes. It unlinks `name` inside the folder
        // already open as `dirfd`; it cannot follow a link or reach outside that folder.
        if unsafe { libc::unlinkat(v.dirfd, v.name.as_ptr(), flag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !v.is_dir { done.fetch_add(1, Relaxed); }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn b(p: &Path) -> Vec<u8> { std::fs::canonicalize(p).unwrap().as_os_str().as_bytes().to_vec() }
    fn none() -> HashSet<Vec<u8>> { HashSet::new() }

    #[test]
    fn walk_counts_without_following_links_and_visits_parents_last() {
        let root = fixture("walk");
        write(&root.join("a/one"), 10);
        write(&root.join("a/b/two"), 20);
        let outside = fixture("walk-outside");
        write(&outside.join("x"), 5);
        write(&outside.join("y"), 5);
        std::os::unix::fs::symlink(&outside, root.join("a/link")).unwrap();

        let s = survey(&b(&root.join("a")), true, &none()).unwrap();
        // one, two and the link itself; never the two files behind the link
        assert_eq!(s.files, 3);
        assert!(s.bytes >= 30 && s.bytes < 1_000_000);
        assert!(s.newest_change > 1_600_000_000);

        let mut order = Vec::new();
        walk(&b(&root.join("a")), true, &none(), &mut |v| {
            order.push((v.name.to_string_lossy().into_owned(), v.is_dir));
            Ok(())
        }).unwrap();
        let pos = |n: &str| order.iter().position(|(name, _)| name == n).unwrap();
        assert!(pos("two") < pos("b") && pos("b") < pos("a"), "{order:?}");
        assert_eq!(order.last().unwrap(), &("a".to_string(), true));
        assert_eq!(order.iter().find(|(n, _)| n == "link").unwrap().1, false, "a link is never a folder");
        assert_eq!(order.len(), 5);
        // the outside folder is untouched and unvisited
        assert!(!order.iter().any(|(n, _)| n == "x" || n == "y"));
    }

    #[test]
    fn single_items_and_type_changes() {
        let root = fixture("walk-single");
        write(&root.join("f"), 10);
        std::fs::create_dir(root.join("d")).unwrap();
        let f = survey(&b(&root.join("f")), false, &none()).unwrap();
        assert_eq!((f.files, f.len), (1, 10));
        assert_eq!(survey(&b(&root.join("d")), true, &none()).unwrap().files, 0);
        // scanned as a file, now a folder (or the reverse): refused
        assert!(survey(&b(&root.join("f")), true, &none()).is_err());
        assert!(survey(&b(&root.join("d")), false, &none()).is_err());
        // a link where a folder was scanned is not entered
        let target = fixture("walk-target");
        write(&target.join("precious"), 1);
        std::os::unix::fs::symlink(&target, root.join("was-a-folder")).unwrap();
        let link = [b(&root).as_slice(), b"/was-a-folder"].concat();
        assert!(survey(&link, true, &none()).is_err());
        assert!(survey(b"/definitely/not/here", false, &none()).is_err());
        assert!(survey(b"relative", false, &none()).is_err());
        assert!(survey(b"/", true, &none()).is_err());
    }

    #[test]
    fn mount_points_inside_or_at_the_item_stop_the_walk() {
        let root = fixture("walk-mount");
        write(&root.join("a/b/c/file"), 1);
        let a = b(&root.join("a"));
        let inner: HashSet<Vec<u8>> = [[a.as_slice(), b"/b/c"].concat()].into_iter().collect();
        assert!(survey(&a, true, &inner).unwrap_err().contains("another filesystem"));
        let itself: HashSet<Vec<u8>> = [a.clone()].into_iter().collect();
        assert!(survey(&a, true, &itself).is_err());
        assert!(survey(&a, true, &none()).is_ok());
    }
}
