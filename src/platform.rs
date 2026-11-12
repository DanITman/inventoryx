// InventoryX — what can be scanned on this machine.
// On Unraid: the whole server, array disks, pools, user shares and unassigned devices.
// Shares are read from the underlying disks directly, not through /mnt/user.
// GPL-3.0-or-later.

use crate::index::Source;
use crate::scan::{fs_space, ScanSpec};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Target {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub group: &'static str,
    pub total: u64,
    pub free: u64,
}

pub struct Platform {
    /// `/mnt` on Unraid; None elsewhere.
    pub unraid_mnt: Option<PathBuf>,
    pub pools_cfg: PathBuf,
    /// `/boot/config` on Unraid: Docker, VM and plugin settings.
    pub boot_config: PathBuf,
}

/// The plugin's settings that matter for deleting.
#[derive(Debug, PartialEq)]
pub struct DeleteSettings {
    pub enabled: bool,
    pub typed_bytes: u64,
    pub allow_protected: bool,
}

/// Reads `KEY="value"` lines, as Unraid writes its .cfg files.
pub fn parse_cfg(text: &str) -> std::collections::HashMap<String, String> {
    text.lines().filter_map(|l| {
        let (k, v) = l.trim().split_once('=')?;
        Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
    }).collect()
}

fn bytes(p: &Path) -> Vec<u8> { p.as_os_str().as_bytes().to_vec() }

fn subdirs(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p).into_iter().flatten().flatten()
        .filter(|e| e.file_type().map_or(false, |t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort();
    v
}

impl Platform {
    pub fn detect() -> Self {
        let unraid = Path::new("/var/local/emhttp").is_dir() && Path::new("/boot/config").is_dir();
        Platform {
            unraid_mnt: unraid.then(|| PathBuf::from("/mnt")),
            pools_cfg: PathBuf::from("/boot/config/pools"),
            boot_config: PathBuf::from("/boot/config"),
        }
    }

    pub fn is_unraid(&self) -> bool { self.unraid_mnt.is_some() }

    fn disks(&self, mnt: &Path) -> Vec<String> {
        let mut v: Vec<(u32, String)> = subdirs(mnt).into_iter()
            .filter_map(|n| n.strip_prefix("disk").and_then(|d| d.parse::<u32>().ok()).map(|i| (i, n.clone())))
            .collect();
        v.sort();
        v.into_iter().map(|(_, n)| n).collect()
    }

    fn pools(&self, mnt: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.pools_cfg).into_iter().flatten().flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".cfg").map(str::to_string))
            .filter(|n| mnt.join(n).is_dir())
            .collect();
        v.sort();
        v
    }

    /// Mounted array disks and pools, in display order.
    fn volumes(&self, mnt: &Path) -> Vec<String> {
        let mnt_dev = std::fs::metadata(mnt).map(|m| m.dev()).ok();
        let real_mount = |n: &String| {
            // On a real server a stopped array leaves no mount; in tests any folder counts.
            !self.is_real() || std::fs::metadata(mnt.join(n)).map(|m| Some(m.dev()) != mnt_dev).unwrap_or(false)
        };
        self.disks(mnt).into_iter().chain(self.pools(mnt)).filter(real_mount).collect()
    }

    pub fn is_real(&self) -> bool { self.unraid_mnt.as_deref() == Some(Path::new("/mnt")) }

    fn shares(&self, mnt: &Path) -> Vec<String> {
        let mut all: Vec<String> = self.volumes(mnt).iter().flat_map(|v| subdirs(&mnt.join(v))).collect();
        all.sort();
        all.dedup();
        all
    }

    fn cfg(&self, relative: &str) -> std::collections::HashMap<String, String> {
        parse_cfg(&std::fs::read_to_string(self.boot_config.join(relative)).unwrap_or_default())
    }

    /// Deleting is off unless this is Unraid and the owner turned it on in the settings.
    /// Anything unreadable or odd in the settings means the safer choice.
    pub fn delete_settings(&self) -> DeleteSettings {
        let cfg = self.cfg("plugins/inventoryx/inventoryx.cfg");
        let gb = cfg.get("TYPED_DELETE_GB").and_then(|v| v.parse::<u64>().ok()).unwrap_or(10);
        DeleteSettings {
            enabled: self.is_unraid() && cfg.get("DELETE_ENABLED").map(String::as_str) == Some("yes"),
            // Capped, so a huge number cannot switch the rule off.
            typed_bytes: gb.min(1000).saturating_mul(1_000_000_000),
            allow_protected: cfg.get("ALLOW_PROTECTED").map(String::as_str) == Some("yes"),
        }
    }

    /// The Docker and VM images, as configured in Unraid, with their labels.
    pub fn protected_paths(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let docker = self.cfg("docker.cfg");
        // In folder mode this setting names the folder; it is protected the same way.
        out.push(("Docker image".to_string(),
                  docker.get("DOCKER_IMAGE_FILE").cloned().unwrap_or_else(|| "/mnt/user/system/docker/docker.img".into())));
        let vm = self.cfg("domain.cfg");
        out.push(("VM image".to_string(),
                  vm.get("IMAGE_FILE").cloned().unwrap_or_else(|| "/mnt/user/system/libvirt/libvirt.img".into())));
        out.into_iter().map(|(l, p)| (l, p.trim_end_matches('/').as_bytes().to_vec())).collect()
    }

    /// Everything the delete rules need to know about this server. None off Unraid.
    pub fn delete_facts(&self) -> Option<crate::guard::Facts> {
        let mnt_path = self.unraid_mnt.as_ref()?;
        let mnt = bytes(mnt_path);
        let mount_points = if self.is_real() {
            crate::scan::mount_points()
        } else {
            self.pretend_mounts(mnt_path)
        };
        let settings = self.delete_settings();
        let mut protected = Vec::new();
        for (label, path) in self.protected_paths() {
            // Unraid's settings say /mnt/...; a pretend server in tests lives elsewhere.
            let path = match path.strip_prefix(b"/mnt") {
                Some(rest) if !self.is_real() => [mnt.as_slice(), rest].concat(),
                _ => path,
            };
            for p in crate::guard::expand_user_path(&mnt, &path, &mount_points) {
                protected.push((label.clone(), p));
            }
        }
        Some(crate::guard::Facts {
            mnt, mount_points, protected,
            typed_bytes: settings.typed_bytes, allow_protected: settings.allow_protected,
        })
    }

    /// Stand-in mount table for a pretend server in tests. A real server always uses the
    /// kernel's table; without one, nothing can be deleted.
    fn pretend_mounts(&self, _mnt: &Path) -> Vec<Vec<u8>> {
        #[cfg(test)]
        {
            let mut v = vec![b"/".to_vec(), bytes(_mnt)];
            for name in self.volumes(_mnt).into_iter().chain(["user".to_string(), "user0".to_string()]) {
                v.push(bytes(&_mnt.join(name)));
            }
            return v;
        }
        #[cfg(not(test))]
        Vec::new()
    }

    /// Where the saved scan goes when left on automatic: an `inventoryx` folder inside the
    /// `appdata` share of the first pool that has one. Never the flash drive, and never a
    /// pool that is not mounted.
    pub fn default_index_dir(&self) -> Option<PathBuf> {
        let mnt = self.unraid_mnt.as_ref()?;
        let vols = self.volumes(mnt);
        self.pools(mnt).into_iter().filter(|p| vols.contains(p))
            .map(|p| mnt.join(p).join("appdata"))
            .find(|p| p.is_dir())
            .map(|p| p.join("inventoryx"))
    }

    pub fn targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        let Some(mnt) = &self.unraid_mnt else {
            for (label, path) in [("Computer", "/".to_string()), ("Home", std::env::var("HOME").unwrap_or_default())] {
                if path.is_empty() { continue; }
                let (total, free) = fs_space(path.as_bytes());
                out.push(Target { id: format!("path:{path}"), label: label.into(), detail: path, group: "Volumes", total, free });
            }
            return out;
        };
        let vols = self.volumes(mnt);
        let (mut total, mut free) = (0, 0);
        let (mut array_total, mut array_free, mut array_disks) = (0, 0, 0);
        let mut vol_targets = Vec::new();
        for v in &vols {
            let p = mnt.join(v);
            let (t, f) = fs_space(&bytes(&p));
            total += t;
            free += f;
            let is_disk = v.strip_prefix("disk").map_or(false, |d| d.parse::<u32>().is_ok());
            if is_disk {
                array_total += t;
                array_free += f;
                array_disks += 1;
            }
            vol_targets.push(Target {
                id: format!("{}:{v}", if is_disk { "disk" } else { "pool" }),
                label: v.clone(), detail: p.to_string_lossy().into_owned(),
                group: if is_disk { "Array disks" } else { "Pools" }, total: t, free: f,
            });
        }
        if !vols.is_empty() {
            out.push(Target {
                id: "server".into(), label: "Whole server".into(),
                detail: format!("{} disks and pools, by share", vols.len()), group: "Server", total, free,
            });
        }
        if array_disks > 0 {
            out.push(Target {
                id: "array".into(), label: "Array".into(),
                detail: format!("{array_disks} array disks, by share"), group: "Server",
                total: array_total, free: array_free,
            });
        }
        out.extend(vol_targets);
        for s in self.shares(mnt) {
            let n = vols.iter().filter(|v| mnt.join(v).join(&s).is_dir()).count();
            out.push(Target {
                id: format!("share:{s}"), label: s.clone(),
                detail: format!("on {n} {}", if n == 1 { "disk" } else { "disks" }),
                group: "Shares", total: 0, free: 0,
            });
        }
        for d in subdirs(&mnt.join("disks")) {
            let p = mnt.join("disks").join(&d);
            let (t, f) = fs_space(&bytes(&p));
            out.push(Target { id: format!("path:{}", p.display()), label: d, detail: p.to_string_lossy().into_owned(),
                              group: "Unassigned devices", total: t, free: f });
        }
        out
    }

    pub fn spec_for(&self, id: &str) -> Option<ScanSpec> {
        let (kind, name) = id.split_once(':').unwrap_or((id, ""));
        if kind == "path" {
            if !name.starts_with('/') { return None; }
            let trimmed = if name.len() > 1 { name.trim_end_matches('/') } else { name };
            return Some(ScanSpec {
                label: trimmed.to_string(), display_root: trimmed.as_bytes().to_vec(),
                sources: vec![Source { label: trimmed.to_string(), path: trimmed.as_bytes().to_vec() }],
            });
        }
        let mnt = self.unraid_mnt.as_ref()?;
        let vols = self.volumes(mnt);
        let src = |v: &String, sub: Option<&str>| {
            let p = match sub { Some(s) => mnt.join(v).join(s), None => mnt.join(v) };
            Source { label: v.clone(), path: bytes(&p) }
        };
        match kind {
            "server" => Some(ScanSpec {
                label: "Whole server".into(), display_root: bytes(&mnt.join("user")),
                sources: vols.iter().map(|v| src(v, None)).collect(),
            }),
            "array" => {
                let disks = self.disks(mnt);
                let sources: Vec<Source> = vols.iter().filter(|v| disks.contains(v)).map(|v| src(v, None)).collect();
                if sources.is_empty() { return None; }
                // /mnt/user0 is Unraid's view of the shares without the pools.
                Some(ScanSpec { label: "Array".into(), display_root: bytes(&mnt.join("user0")), sources })
            }
            "disk" | "pool" => {
                if !vols.iter().any(|v| v == name) { return None; }
                Some(ScanSpec { label: name.into(), display_root: bytes(&mnt.join(name)),
                                sources: vec![src(&name.to_string(), None)] })
            }
            "share" => {
                if name.is_empty() || name.contains('/') || name.starts_with('.') { return None; }
                let sources: Vec<Source> = vols.iter().filter(|v| mnt.join(v).join(name).is_dir())
                    .map(|v| src(v, Some(name))).collect();
                if sources.is_empty() { return None; }
                Some(ScanSpec { label: name.into(), display_root: bytes(&mnt.join("user").join(name)), sources })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;

    fn fake() -> (Platform, PathBuf) {
        let root = fixture("unraid");
        let mnt = root.join("mnt");
        write(&mnt.join("disk1/Media/a.mkv"), 10);
        write(&mnt.join("disk2/Media/b.mkv"), 10);
        write(&mnt.join("disk10/media/c.mkv"), 10);
        write(&mnt.join("cache/appdata/x"), 10);
        write(&mnt.join("user/Media/a.mkv"), 10);
        write(&root.join("pools/cache.cfg"), 1);
        std::fs::create_dir_all(mnt.join("disks")).unwrap();
        (Platform { unraid_mnt: Some(mnt.clone()), pools_cfg: root.join("pools"), boot_config: root.join("config") }, mnt)
    }

    #[test]
    fn lists_server_disks_pools_and_shares() {
        let (p, _) = fake();
        let ids: Vec<String> = p.targets().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["server", "array", "disk:disk1", "disk:disk2", "disk:disk10", "pool:cache",
                         "share:Media", "share:appdata", "share:media"]);
    }

    #[test]
    fn delete_settings_default_to_off_and_safe() {
        let (p, _) = fake();
        assert_eq!(p.delete_settings(), DeleteSettings { enabled: false, typed_bytes: 10_000_000_000, allow_protected: false });
        write(&p.boot_config.join("plugins/inventoryx/placeholder"), 1);
        std::fs::write(p.boot_config.join("plugins/inventoryx/inventoryx.cfg"),
            "TYPED_DELETE_GB=\"25\"\nALLOW_PROTECTED=\"yes\"\nDELETE_ENABLED=\"yes\"\n").unwrap();
        assert_eq!(p.delete_settings(), DeleteSettings { enabled: true, typed_bytes: 25_000_000_000, allow_protected: true });
        std::fs::write(p.boot_config.join("plugins/inventoryx/inventoryx.cfg"), "TYPED_DELETE_GB=\"999999999\"\n").unwrap();
        assert_eq!(p.delete_settings().typed_bytes, 1_000_000_000_000);
        std::fs::write(p.boot_config.join("plugins/inventoryx/inventoryx.cfg"),
            "TYPED_DELETE_GB=\"lots\"\nALLOW_PROTECTED=\"true\"\nDELETE_ENABLED=\"YES\"\n").unwrap();
        assert_eq!(p.delete_settings(), DeleteSettings { enabled: false, typed_bytes: 10_000_000_000, allow_protected: false });
        // never on a machine that is not Unraid, whatever a file says
        let other = Platform { unraid_mnt: None, pools_cfg: "/nonexistent".into(), boot_config: p.boot_config.clone() };
        std::fs::write(p.boot_config.join("plugins/inventoryx/inventoryx.cfg"), "DELETE_ENABLED=\"yes\"\n").unwrap();
        assert!(!other.delete_settings().enabled);
    }

    #[test]
    fn protected_paths_come_from_unraid_settings() {
        let (p, _) = fake();
        let labels: Vec<(String, String)> = p.protected_paths().into_iter()
            .map(|(l, path)| (l, String::from_utf8(path).unwrap())).collect();
        assert_eq!(labels, [("Docker image".to_string(), "/mnt/user/system/docker/docker.img".to_string()),
                            ("VM image".to_string(), "/mnt/user/system/libvirt/libvirt.img".to_string())]);
        write(&p.boot_config.join("docker.cfg"), 0);
        std::fs::write(p.boot_config.join("docker.cfg"),
            "DOCKER_ENABLED=\"yes\"\nDOCKER_IMAGE_FILE=\"/mnt/cache/docker-data/\"\n").unwrap();
        assert_eq!(p.protected_paths()[0].1, b"/mnt/cache/docker-data".to_vec());
    }

    #[test]
    fn share_reads_underlying_disks_not_user_mount() {
        let (p, mnt) = fake();
        let spec = p.spec_for("share:Media").unwrap();
        assert_eq!(spec.display_root, bytes(&mnt.join("user/Media")));
        let paths: Vec<Vec<u8>> = spec.sources.iter().map(|s| s.path.clone()).collect();
        let mut expected = vec![bytes(&mnt.join("disk1/Media")), bytes(&mnt.join("disk2/Media"))];
        if !case_sensitive_fs() { expected.push(bytes(&mnt.join("disk10/Media"))); }
        assert_eq!(paths, expected);
        assert_eq!(p.spec_for("server").unwrap().sources.len(), 4);
        let array = p.spec_for("array").unwrap();
        assert_eq!(array.sources.len(), 3, "array disks only, no pools");
        assert_eq!(array.display_root, bytes(&mnt.join("user0")));
        assert!(p.spec_for("share:../etc").is_none());
        assert!(p.spec_for("share:Nope").is_none());
        assert!(p.spec_for("disk:user").is_none());
        assert!(p.spec_for("path:relative").is_none());
        assert_eq!(p.default_index_dir(), Some(mnt.join("cache/appdata/inventoryx")));
        let none = Platform { unraid_mnt: None, pools_cfg: "/nonexistent".into(), boot_config: "/nonexistent".into() };
        assert_eq!(none.default_index_dir(), None);
        assert_eq!(p.spec_for("path:/tmp/").unwrap().display_root, b"/tmp".to_vec());
    }
}
