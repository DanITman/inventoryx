// InventoryX — the rules that decide whether something may be deleted.
//
// Pure logic: no file is touched here. Everything is decided from paths and facts about
// the server, so the rules can be tested exhaustively against made-up paths. The server
// applies them to every delete request; the page cannot bypass them.
// GPL-3.0-or-later.

use crate::index::{F_MOUNT, F_REMOVED};

/// What the server looks like, as far as deleting is concerned.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// `/mnt` on a real server.
    pub mnt: Vec<u8>,
    /// Every mount point on the server (from the kernel's mount table).
    pub mount_points: Vec<Vec<u8>>,
    /// Protected files or folders: a label and every path each can be reached by.
    pub protected: Vec<(String, Vec<u8>)>,
    /// Items larger than this need the word DELETE typed.
    pub typed_bytes: u64,
    /// The advanced setting "Allow deletion of protected files".
    pub allow_protected: bool,
}

/// The thing the user asked to delete.
#[derive(Clone, Debug, Default)]
pub struct Item {
    /// Real paths on disk. A folder of a share that spans disks has one per disk.
    pub paths: Vec<Vec<u8>>,
    pub is_scan_root: bool,
    pub flags: u8,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Never, with the reason shown to the user.
    Blocked(String),
    /// Allowed after confirmation. `typed` holds the reason when the user must type DELETE.
    Allowed { typed: Option<String> },
}

/// Unraid's merged views of the disks and pools.
const VIEWS: [&[u8]; 3] = [b"user", b"user0", b"rootshare"];
/// Areas under /mnt that hold mount points of devices and network shares.
const MOUNT_AREAS: [&[u8]; 4] = [b"disks", b"remotes", b"addons", b"rootshare"];

fn show(p: &[u8]) -> String { String::from_utf8_lossy(p).into_owned() }

/// `inner` is `outer` or lies inside it.
fn within(inner: &[u8], outer: &[u8]) -> bool {
    inner == outer || (inner.starts_with(outer) && inner.get(outer.len()) == Some(&b'/'))
        || (outer == b"/" && inner.starts_with(b"/"))
}

/// Path components below /mnt, or None if the path is not a clean absolute path there.
fn below_mnt<'a>(mnt: &[u8], path: &'a [u8]) -> Option<Vec<&'a [u8]>> {
    if mnt.is_empty() { return None; }
    let rest = path.strip_prefix(mnt)?.strip_prefix(b"/")?;
    let parts: Vec<&[u8]> = rest.split(|&b| b == b'/').collect();
    // No empty, `.` or `..` components: the path must mean exactly what it says.
    if parts.iter().any(|c| c.is_empty() || *c == b"." || *c == b"..") { return None; }
    Some(parts)
}

/// Every path a file under `/mnt/user` or `/mnt/user0` can also be reached by: the same
/// relative path on each disk and pool. Other paths stand for themselves.
pub fn expand_user_path(mnt: &[u8], path: &[u8], mount_points: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let bases = [[mnt, b"/user/"].concat(), [mnt, b"/user0/"].concat()];
    let rest = bases.iter().find_map(|p| path.strip_prefix(p.as_slice()));
    let Some(rest) = rest else { return vec![path.to_vec()] };
    let mut out = Vec::new();
    for mp in mount_points {
        // Direct children of /mnt: disks, pools, user, user0.
        if below_mnt(mnt, mp).map_or(false, |parts| parts.len() == 1) {
            let mut p = mp.clone();
            p.push(b'/');
            p.extend_from_slice(rest);
            out.push(p);
        }
    }
    for base in &bases {
        let p = [base.as_slice(), rest].concat();
        if !out.contains(&p) { out.push(p); }
    }
    out
}

fn judge_path(facts: &Facts, path: &[u8], bytes: u64) -> Verdict {
    let blocked = |why: &str| Verdict::Blocked(why.to_string());
    let Some(parts) = below_mnt(&facts.mnt, path) else {
        return blocked("Only items under /mnt can be deleted. The flash drive and the operating system are protected.");
    };
    if parts.len() == 1 {
        return blocked("A whole disk, pool or mount area cannot be deleted.");
    }
    if MOUNT_AREAS.contains(&parts[0]) && parts.len() == 2 {
        return blocked("A mounted device or network share cannot be deleted.");
    }
    // The merged views hide which disk, pool or dataset a file is really on, so the mount
    // and protection checks cannot be trusted there. Share and disk scans use real paths.
    if VIEWS.contains(&parts[0]) {
        return blocked("Items scanned through /mnt/user cannot be deleted from here. Scan the share, disk or pool from the start screen instead.");
    }
    if parts[0] == b"remotes" {
        return blocked("Files on network shares cannot be deleted from InventoryX.");
    }
    if facts.mount_points.iter().any(|mp| within(mp, path)) {
        return blocked("This is, or contains, a mounted filesystem. It cannot be deleted from InventoryX.");
    }
    let mut typed = None;
    for (label, protected) in &facts.protected {
        if within(path, protected) || within(protected, path) {
            if !facts.allow_protected {
                return Verdict::Blocked(format!(
                    "{} is protected: {}. It can only be deleted after turning on \"Allow deletion of protected files\" in the InventoryX settings, which is not recommended.",
                    label, show(protected)));
            }
            typed = Some(format!("This is, or contains, the {label}."));
        }
    }
    if typed.is_none() && parts.len() == 2 {
        typed = Some("This is a whole share (a top-level folder).".to_string());
    }
    if typed.is_none() && bytes > facts.typed_bytes {
        typed = Some("This is a large item.".to_string());
    }
    Verdict::Allowed { typed }
}

/// Decides whether `item` may be deleted. Anything not clearly allowed is blocked.
pub fn judge(facts: &Facts, item: &Item) -> Verdict {
    let blocked = |why: &str| Verdict::Blocked(why.to_string());
    if facts.mount_points.is_empty() {
        return blocked("The server's mounts could not be read, so nothing can be deleted safely.");
    }
    if item.is_scan_root { return blocked("The top of the scan cannot be deleted."); }
    if item.flags & F_REMOVED != 0 { return blocked("This has already been deleted."); }
    if item.flags & F_MOUNT != 0 { return blocked("This is another filesystem. It cannot be deleted from InventoryX."); }
    if item.paths.is_empty() { return blocked("This item no longer exists on disk. Refresh the scan."); }
    let mut typed = None;
    for path in &item.paths {
        match judge_path(facts, path, item.bytes) {
            Verdict::Blocked(why) => return Verdict::Blocked(why),
            Verdict::Allowed { typed: Some(why) } => typed = typed.or(Some(why)),
            Verdict::Allowed { typed: None } => {}
        }
    }
    Verdict::Allowed { typed }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn facts() -> Facts {
        let mounts: Vec<Vec<u8>> = ["/", "/boot", "/mnt/disks", "/mnt/remotes", "/mnt/addons", "/mnt/rootshare",
            "/mnt/disk1", "/mnt/disk2", "/mnt/cache", "/mnt/user", "/mnt/user0", "/mnt/disks/usbstick",
            "/mnt/cache/appdata/thing/mounted", "/proc", "/var/lib/docker"]
            .iter().map(|s| s.as_bytes().to_vec()).collect();
        let mut protected = Vec::new();
        for (label, p) in [("Docker image", "/mnt/user/system/docker/docker.img"),
                           ("VM image", "/mnt/user/system/libvirt/libvirt.img")] {
            for path in expand_user_path(b"/mnt", p.as_bytes(), &mounts) { protected.push((label.to_string(), path)); }
        }
        Facts { mnt: b"/mnt".to_vec(), mount_points: mounts, protected, typed_bytes: 10 * GB, allow_protected: false }
    }

    fn item(path: &str, bytes: u64) -> Item {
        Item { paths: vec![path.as_bytes().to_vec()], bytes, ..Default::default() }
    }

    fn is_blocked(v: &Verdict) -> bool { matches!(v, Verdict::Blocked(_)) }
    fn simple(v: &Verdict) -> bool { *v == Verdict::Allowed { typed: None } }
    fn typed(v: &Verdict) -> bool { matches!(v, Verdict::Allowed { typed: Some(_) }) }

    #[test]
    fn ordinary_files_and_folders() {
        let f = facts();
        assert!(simple(&judge(&f, &item("/mnt/disk1/Media/Movies/old.mkv", 2 * GB))));
        assert!(simple(&judge(&f, &item("/mnt/cache/appdata/plex/Cache", 10 * GB))), "exactly 10 GB is not over");
        assert!(typed(&judge(&f, &item("/mnt/disk1/Media/Movies/huge.mkv", 10 * GB + 1))));
    }

    #[test]
    fn flash_os_and_anything_outside_mnt_are_never_deletable() {
        let mut f = facts();
        f.allow_protected = true;
        for p in ["/boot", "/boot/config/go", "/boot/bzroot", "/", "/usr/bin/php", "/etc/passwd", "/var/log/syslog",
                  "/mnt", "/mntx/disk1/a/b", "/mnt/../boot/config", "/mnt/disk1/../../boot/x", "/mnt/disk1/./a/b",
                  "/mnt/disk1//a/b", "/mnt/disk1/a/b/", "relative/path", "", "/root/.ssh/id_rsa", "/tmp/x/y"] {
            assert!(is_blocked(&judge(&f, &item(p, 1))), "{p}");
        }
    }

    #[test]
    fn mount_points_are_never_deletable() {
        let mut f = facts();
        f.allow_protected = true;
        for p in ["/mnt/disk1", "/mnt/cache", "/mnt/user", "/mnt/user0", "/mnt/disks", "/mnt/remotes", "/mnt/addons",
                  "/mnt/rootshare", "/mnt/disks/usbstick", "/mnt/disks/notmounted", "/mnt/remotes/nas_share",
                  "/mnt/newpool",
                  // contains a mounted filesystem
                  "/mnt/cache/appdata/thing", "/mnt/cache/appdata/thing/mounted", "/mnt/cache/appdata"] {
            assert!(is_blocked(&judge(&f, &item(p, 1))), "{p}");
        }
        // below a device mount is fine
        assert!(simple(&judge(&f, &item("/mnt/disks/usbstick/old/file.bin", 1))));
        // a sibling whose name merely starts the same is not "inside" the mount
        assert!(simple(&judge(&f, &item("/mnt/cache/appdata/thing2/file", 1))));
    }

    #[test]
    fn docker_and_vm_images_are_protected_by_every_route() {
        let f = facts();
        for p in ["/mnt/cache/system/docker/docker.img", "/mnt/user/system/docker/docker.img",
                  "/mnt/disk1/system/docker/docker.img", "/mnt/user0/system/libvirt/libvirt.img",
                  "/mnt/cache/system/libvirt/libvirt.img",
                  // folders that contain them
                  "/mnt/cache/system/docker", "/mnt/cache/system", "/mnt/user/system", "/mnt/disk2/system/libvirt"] {
            let v = judge(&f, &item(p, 1));
            // Paths through the merged views are refused outright; the rest name the reason.
            let through_view = p.starts_with("/mnt/user");
            assert!(matches!(&v, Verdict::Blocked(why) if through_view || why.contains("protected")), "{p}: {v:?}");
        }
        // other things in the same share are ordinary
        assert!(simple(&judge(&f, &item("/mnt/cache/system/other/file", 1))));
        assert!(simple(&judge(&f, &item("/mnt/cache/system/docker/docker.img.bak", 1))));
    }

    #[test]
    fn docker_folder_mode_protects_the_folder_and_everything_in_it() {
        let mut f = facts();
        f.protected = expand_user_path(b"/mnt", b"/mnt/user/system/docker-data", &f.mount_points)
            .into_iter().map(|p| ("Docker folder".to_string(), p)).collect();
        for p in ["/mnt/cache/system/docker-data", "/mnt/cache/system/docker-data/overlay2/abc/file",
                  "/mnt/cache/system"] {
            assert!(is_blocked(&judge(&f, &item(p, 1))), "{p}");
        }
    }

    #[test]
    fn override_unlocks_only_the_images_and_still_needs_typing() {
        let mut f = facts();
        f.allow_protected = true;
        assert!(typed(&judge(&f, &item("/mnt/cache/system/docker/docker.img", 1))));
        assert!(typed(&judge(&f, &item("/mnt/cache/system/libvirt", 1))));
        // the override does nothing for the flash drive, the OS or mount points
        assert!(is_blocked(&judge(&f, &item("/boot/config", 1))));
        assert!(is_blocked(&judge(&f, &item("/mnt/cache", 1))));
    }

    #[test]
    fn whole_shares_always_need_typing() {
        let f = facts();
        for p in ["/mnt/disk1/Media", "/mnt/disk2/isos", "/mnt/cache/backup"] {
            assert!(typed(&judge(&f, &item(p, 0))), "{p}");
        }
        assert!(simple(&judge(&f, &item("/mnt/disk1/Media/one-folder", 0))));
    }

    #[test]
    fn merged_views_and_network_shares_are_refused() {
        let mut f = facts();
        f.allow_protected = true;
        for p in ["/mnt/user/Media/Movies/old.mkv", "/mnt/user/Media", "/mnt/user0/backup/x",
                  "/mnt/user/system/libvirt/libvirt.img", "/mnt/user/appdata/plex/Cache/file",
                  "/mnt/rootshare/anything/file", "/mnt/remotes/nas_share/folder/file"] {
            assert!(is_blocked(&judge(&f, &item(p, 1))), "{p}");
        }
    }

    #[test]
    fn item_level_rules() {
        let f = facts();
        let ok = item("/mnt/disk1/Media/a", 1);
        assert!(is_blocked(&judge(&f, &Item { is_scan_root: true, ..ok.clone() })));
        assert!(is_blocked(&judge(&f, &Item { flags: F_MOUNT, ..ok.clone() })));
        assert!(is_blocked(&judge(&f, &Item { flags: F_REMOVED, ..ok.clone() })));
        assert!(is_blocked(&judge(&f, &Item { paths: vec![], ..ok.clone() })));
        // no mount table: nothing can be judged safe
        assert!(is_blocked(&judge(&Facts { mount_points: vec![], ..f.clone() }, &ok)));
        // a share folder spread over disks: one bad path blocks the lot, one typed reason is enough
        let spread = Item { paths: vec![b"/mnt/disk1/Media/x".to_vec(), b"/mnt/cache/system".to_vec()], bytes: 1, ..Default::default() };
        assert!(is_blocked(&judge(&f, &spread)));
        let spread = Item { paths: vec![b"/mnt/disk1/Media/x".to_vec(), b"/mnt/disk2/Media".to_vec()], bytes: 1, ..Default::default() };
        assert!(typed(&judge(&f, &spread)));
    }

    #[test]
    fn user_paths_expand_to_every_disk_and_pool() {
        let f = facts();
        let all = expand_user_path(b"/mnt", b"/mnt/user/system/docker/docker.img", &f.mount_points);
        for p in ["/mnt/disk1/system/docker/docker.img", "/mnt/disk2/system/docker/docker.img",
                  "/mnt/cache/system/docker/docker.img", "/mnt/user/system/docker/docker.img",
                  "/mnt/user0/system/docker/docker.img"] {
            assert!(all.contains(&p.as_bytes().to_vec()), "{p}");
        }
        assert_eq!(expand_user_path(b"/mnt", b"/mnt/cache/x.img", &f.mount_points), vec![b"/mnt/cache/x.img".to_vec()]);
        // without a known /mnt nothing is under it
        assert!(is_blocked(&judge(&Facts { mnt: vec![], ..f.clone() }, &item("/mnt/disk1/Media/a", 1))));
    }
}
