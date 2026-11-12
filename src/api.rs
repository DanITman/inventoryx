// InventoryX — HTTP routes. Everything reads, except `delete`, which is guarded by
// guard.rs and carried out by remove.rs.
// GPL-3.0-or-later.

use crate::http::{Request, Response};
use crate::index::*;
use crate::kinds::{self, KindTable};
use crate::layout::{self, LayoutOpts};
use crate::platform::Platform;
use crate::scan::{self, Progress};
use crate::stats;
use serde_json::{json, Value};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, UNIX_EPOCH};

#[derive(Clone)]
pub struct Loaded {
    pub index: Index,
    pub colors: Vec<[f32; 3]>,
    pub id: u64,
    /// The target id this scan was started from, so it can be refreshed.
    pub target: String,
    /// Loaded from the saved copy rather than scanned by this run of the program.
    pub from_disk: bool,
    /// Something was deleted through InventoryX since the scan.
    pub has_removals: bool,
    /// Part of the scan is on ZFS, which InventoryX has not been tested on.
    pub zfs: bool,
}

/// Where the finished scan is saved so it survives a restart.
pub enum Store {
    Off,
    /// The `appdata` share of a pool, worked out when needed (pools mount after boot).
    Auto,
    Dir(std::path::PathBuf),
}

struct Job {
    running: bool,
    progress: Arc<Progress>,
    started: Instant,
    label: String,
    error: Option<String>,
}

pub struct App {
    loaded: RwLock<Option<Arc<Loaded>>>,
    job: Mutex<Job>,
    platform: Platform,
    store: Store,
    /// Set once the saved scan has been loaded, found damaged, or made stale by a new scan.
    load_settled: std::sync::atomic::AtomicBool,
    /// Where the scan was saved, or why it could not be.
    saved: Mutex<Result<Option<String>, String>>,
    /// One delete at a time.
    deleting: Mutex<()>,
    del_job: Mutex<DelJob>,
    /// The function that removes a path from disk. Always `remove::remove`, except in
    /// tests, which substitute a recorder so that no test ever deletes anything.
    remover: crate::remove::Remover,
}

/// A delete that passed every guard, with what is on disk right now.
struct Checked {
    paths: Vec<Vec<u8>>,
    typed: Option<String>,
    is_dir: bool,
    files: u64,
    bytes: u64,
    token: String,
    mounts: std::collections::HashSet<Vec<u8>>,
}

#[derive(Default)]
struct DelJob {
    active: bool,
    label: String,
    total: u64,
    done: Arc<std::sync::atomic::AtomicU64>,
    error: Option<String>,
}

/// Stands in for the real removal in test builds, so that a test which forgets to
/// install its own recorder fails loudly instead of deleting something.
#[cfg(test)]
fn default_remover() -> crate::remove::Remover {
    |_, _, _, _, _| panic!("a test reached the real delete; install a recorder")
}
#[cfg(not(test))]
fn default_remover() -> crate::remove::Remover { crate::remove::remove }

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const TREEMAP_JS: &str = include_str!("../web/treemap.js");
const OUTLINE_JS: &str = include_str!("../web/outline.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

fn text(b: &[u8]) -> String { String::from_utf8_lossy(b).into_owned() }

fn node_json(ix: &Index, id: u32) -> Value {
    let n = &ix.nodes[id as usize];
    json!({
        "id": id, "name": text(ix.name(id)), "dir": n.is_dir(), "logical": n.logical,
        "physical": n.physical, "kind": n.kind, "files": n.files, "children": n.child_count,
        "flags": n.flags,
        "source": ix.sources.get(n.source as usize).map(|s| s.label.clone()),
    })
}

fn node_with_path(ix: &Index, id: u32) -> Value {
    let mut v = node_json(ix, id);
    let parent = ix.nodes[id as usize].parent;
    v["folder"] = json!(text(&ix.display_path(parent)));
    v
}

impl App {
    pub fn new(platform: Platform, store: Store) -> Arc<Self> {
        Arc::new(App {
            loaded: RwLock::new(None),
            job: Mutex::new(Job { running: false, progress: Arc::new(Progress::default()),
                                  started: Instant::now(), label: String::new(), error: None }),
            platform, store,
            load_settled: std::sync::atomic::AtomicBool::new(false),
            saved: Mutex::new(Ok(None)),
            deleting: Mutex::new(()),
            del_job: Mutex::new(DelJob::default()),
            remover: default_remover(),
        })
    }

    /// Applies every guard to node `id` and looks at what is on disk now. Reads only.
    fn delete_checked(&self, l: &Loaded, id: u32) -> Result<Checked, (u16, String)> {
        use std::os::unix::ffi::OsStrExt;
        let settings = self.platform.delete_settings();
        if !settings.enabled {
            return Err((403, "Deleting is turned off. It can be turned on in the InventoryX settings.".into()));
        }
        let Some(facts) = self.platform.delete_facts() else {
            return Err((403, "Deleting is only available on Unraid.".into()));
        };
        let ix = &l.index;
        let node = &ix.nodes[id as usize];
        // Only paths that exist now: a folder of a share need not be on every disk.
        let mut paths: Vec<Vec<u8>> = ix.paths_on_disk(id).into_iter()
            .filter(|p| std::fs::symlink_metadata(std::ffi::OsStr::from_bytes(p)).is_ok())
            .collect();
        paths.sort();
        paths.dedup();
        let judge = |bytes: u64| crate::guard::judge(&facts, &crate::guard::Item {
            paths: paths.clone(), is_scan_root: id == ROOT, flags: node.flags, bytes,
        });
        if let crate::guard::Verdict::Blocked(why) = judge(node.physical.max(node.logical)) {
            return Err((403, why));
        }
        // The rules were applied to the path as written. Make sure no folder on the way
        // has since become a link to somewhere else.
        for p in &paths {
            let parent = std::path::Path::new(std::ffi::OsStr::from_bytes(p)).parent()
                .ok_or((403, "Not a usable path.".to_string()))?;
            let real = std::fs::canonicalize(parent).map_err(|e| (409, format!("{}: {e}. Refresh the scan.", parent.display())))?;
            if real != parent {
                return Err((403, format!(
                    "{} is now reached through a link. Refresh the scan and try again.", text(p))));
            }
        }

        let changed = |what: String| (409, format!("{what} Refresh the scan and try again."));
        let mounts: std::collections::HashSet<Vec<u8>> = facts.mount_points.iter().cloned().collect();
        let (mut files, mut bytes, mut newest) = (0u64, 0u64, i64::MIN);
        for p in &paths {
            let s = crate::remove::survey(p, node.is_dir(), &mounts).map_err(|e| changed(format!("Could not check {e}.")))?;
            if !node.is_dir() && node.flags & (F_HARDLINK_DUP | F_SYMLINK) == 0 && s.len != node.logical {
                return Err(changed("This file has changed since it was scanned.".into()));
            }
            files += s.files;
            bytes = bytes.saturating_add(s.bytes);
            newest = newest.max(s.newest_change);
        }
        if files != node.files as u64 {
            return Err(changed(format!(
                "This has changed since it was scanned: {} files then, {} now.", node.files, files)));
        }
        // Anything created, replaced, renamed or rewritten since the scan began moves a
        // change time forward. The user has not seen it, so it is not deleted.
        let scan_began = ix.finished.checked_sub(ix.duration).unwrap_or(ix.finished)
            .duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(i64::MAX);
        if newest >= scan_began {
            return Err(changed("Something here has changed since it was scanned.".into()));
        }

        // The size that decides whether DELETE must be typed is what is really on disk,
        // with every hard link counted in full.
        let bytes = bytes.max(node.physical).max(node.logical);
        let typed = match judge(bytes) {
            crate::guard::Verdict::Blocked(why) => return Err((403, why)),
            crate::guard::Verdict::Allowed { typed } => typed,
        };
        // Ties the confirmation the user saw to exactly this item, these paths and this
        // content. A request with another token is refused.
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |b: &[u8]| for &x in b.iter().chain(&[0xffu8]) { h = (h ^ x as u64).wrapping_mul(0x100000001b3); };
        mix(l.id.to_string().as_bytes());
        mix(&id.to_le_bytes());
        for p in &paths { mix(p); }
        mix(&[node.is_dir() as u8]);
        mix(&files.to_le_bytes());
        mix(&bytes.to_le_bytes());
        mix(&newest.to_le_bytes());
        Ok(Checked { paths, typed, is_dir: node.is_dir(), files, bytes, token: format!("{h:016x}"), mounts })
    }

    fn delete_check(&self, l: &Loaded, id: u32) -> Value {
        let ix = &l.index;
        let node = &ix.nodes[id as usize];
        let mut v = json!({
            "allowed": false, "reason": Value::Null, "typed": false, "typed_reason": Value::Null,
            "name": text(ix.name(id)), "path": text(&ix.display_path(id)), "paths": [],
            "dir": node.is_dir(), "files": node.files, "bytes": node.physical.max(node.logical),
            "token": Value::Null,
        });
        match self.delete_checked(l, id) {
            Err((_, why)) => v["reason"] = json!(why),
            Ok(c) => {
                v["allowed"] = json!(true);
                v["typed"] = json!(c.typed.is_some());
                v["typed_reason"] = json!(c.typed);
                v["paths"] = json!(c.paths.iter().map(|p| text(p)).collect::<Vec<_>>());
                v["files"] = json!(c.files);
                v["bytes"] = json!(c.bytes);
                v["token"] = json!(c.token);
            }
        }
        v
    }

    fn log(&self, line: &str) {
        eprintln!("inventoryx: {line}");
        // The log above lives in memory; the system log is what an owner will look in.
        if self.platform.is_real() {
            let _ = std::process::Command::new("logger").args(["-t", "inventoryx", "--", line]).status();
        }
    }

    /// Starts deleting node `id` for good, if every guard agrees. The removal runs in the
    /// background; `status` reports its progress and outcome.
    fn delete(self: &Arc<Self>, scan: &str, id: u32, confirm: &str, token: &str) -> Result<Value, (u16, String)> {
        let _one_at_a_time = self.deleting.lock().unwrap();
        if self.job.lock().unwrap().running { return Err((409, "A scan is running.".into())); }
        if self.del_job.lock().unwrap().active { return Err((409, "Another delete is still running.".into())); }
        let Some(l) = self.loaded() else { return Err((409, "No scan loaded.".into())) };
        // The request must name the scan it was made against: ids mean nothing in another.
        if l.id.to_string() != scan {
            return Err((409, "The scan has changed since this page loaded. Reload the page.".into()));
        }
        if id as usize >= l.index.nodes.len() { return Err((400, "Unknown item.".into())); }
        let c = self.delete_checked(&l, id)?;
        if token.is_empty() || c.token != token {
            return Err((409, "This item is not what the confirmation showed. Close the dialog and try again.".into()));
        }
        if c.typed.is_some() && confirm != "DELETE" {
            return Err((400, "Type DELETE to confirm.".into()));
        }
        let scan_id = l.id;
        let label = text(&l.index.display_path(id));
        drop(l);

        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        *self.del_job.lock().unwrap() = DelJob { active: true, label: label.clone(), total: c.files, done: done.clone(), error: None };
        let app = self.clone();
        let (files, bytes) = (c.files, c.bytes);
        let spawned = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(move || {
            let mut error = None;
            for p in &c.paths {
                app.log(&format!("deleting {} ({} files, {} bytes)", text(p), c.files, c.bytes));
                if let Err(e) = (app.remover)(p, c.is_dir, &c.mounts, c.files, &done) {
                    app.log(&format!("delete of {} failed: {e}", text(p)));
                    error = Some(format!(
                        "Could not delete {}: {e}. Part of it may already be gone; refresh the scan.", text(p)));
                    break;
                }
                app.log(&format!("deleted {}", text(p)));
            }
            if error.is_none() {
                // Take the item out of the index, then save the index again.
                let updated = {
                    let mut slot = app.loaded.write().unwrap();
                    match slot.as_mut() {
                        Some(arc) if arc.id == scan_id => {
                            let loaded = Arc::make_mut(arc);
                            loaded.index.mark_removed(id);
                            loaded.has_removals = true;
                            Some(arc.clone())
                        }
                        _ => None,
                    }
                };
                if let (Some(loaded), Some(dir)) = (updated, app.store_dir()) {
                    *app.saved.lock().unwrap() = crate::persist::save(&dir, &loaded.index, &loaded.target)
                        .map(|_| Some(dir.to_string_lossy().into_owned()))
                        .map_err(|e| format!("The scan could not be saved to {}: {e}", dir.display()));
                }
            }
            let mut job = app.del_job.lock().unwrap();
            job.active = false;
            job.error = error;
        });
        if let Err(e) = spawned {
            let mut job = self.del_job.lock().unwrap();
            job.active = false;
            job.error = Some(format!("Could not start: {e}"));
            return Err((500, e.to_string()));
        }
        Ok(json!({ "started": true, "files": files, "bytes": bytes }))
    }

    fn store_dir(&self) -> Option<std::path::PathBuf> {
        match &self.store {
            Store::Off => None,
            Store::Auto => self.platform.default_index_dir(),
            Store::Dir(d) => Some(d.clone()),
        }
    }

    /// Loads the saved scan if there is one and nothing newer is in memory. Cheap to call
    /// again: after boot the pool holding it may not be mounted yet.
    pub fn load_saved(&self) {
        if self.load_settled.load(Relaxed) || self.job.lock().unwrap().running { return; }
        let Some(dir) = self.store_dir() else { return };
        if !dir.join(crate::persist::FILE_NAME).is_file() { return; }
        self.load_settled.store(true, Relaxed);
        if let Some((index, target)) = crate::persist::load(&dir) {
            let mut loaded = App::finish(index, target);
            loaded.from_disk = true;
            let mut slot = self.loaded.write().unwrap();
            if slot.is_none() {
                *slot = Some(Arc::new(loaded));
                *self.saved.lock().unwrap() = Ok(Some(dir.to_string_lossy().into_owned()));
            }
        }
    }

    fn loaded(&self) -> Option<Arc<Loaded>> { self.loaded.read().unwrap().clone() }

    pub fn finish(index: Index, target: String) -> Loaded {
        let st = stats::compute(&index, ROOT);
        let order: Vec<u16> = st.kinds.iter().map(|k| k.kind).collect();
        let colors = kinds::palette(&order, index.kinds.count());
        let id = index.finished.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let has_removals = index.nodes.iter().any(|n| n.is_removed());
        let zfs = scan::any_on_filesystem(index.sources.iter().map(|s| s.path.as_slice()), "zfs");
        Loaded { index, colors, id, target, from_disk: false, has_removals, zfs }
    }

    /// Starts a scan in the background. Fails if one is already running.
    pub fn start_scan(self: &Arc<Self>, target: &str) -> Result<(), (u16, String)> {
        let spec = self.platform.spec_for(target).ok_or((400, format!("Unknown scan target: {target}")))?;
        // Checked before anything is replaced: a mistyped path must not discard a loaded scan.
        use std::os::unix::ffi::OsStrExt;
        if !spec.sources.iter().any(|s| std::path::Path::new(std::ffi::OsStr::from_bytes(&s.path)).is_dir()) {
            return Err((400, format!("{} is not a folder on this server", spec.label)));
        }
        let progress = Arc::new(Progress::default());
        {
            let mut job = self.job.lock().unwrap();
            if job.running { return Err((409, "A scan is already running".into())); }
            if self.del_job.lock().unwrap().active { return Err((409, "A delete is still running".into())); }
            *job = Job { running: true, progress: progress.clone(), started: Instant::now(),
                         label: spec.label.clone(), error: None };
        }
        // One scan at a time: the previous index is released before the new one is built.
        *self.loaded.write().unwrap() = None;
        self.load_settled.store(true, Relaxed);
        let app = self.clone();
        let target = target.to_string();
        let spawned = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(move || {
            let result = scan::start(spec, Arc::new(KindTable::new()), progress);
            let mut error = None;
            match result {
                Ok(index) => {
                    let loaded = Arc::new(App::finish(index, target));
                    // Ready to browse first; saving a large index takes a moment.
                    *app.loaded.write().unwrap() = Some(loaded.clone());
                    *app.saved.lock().unwrap() = match app.store_dir() {
                        None => Ok(None),
                        Some(dir) => crate::persist::save(&dir, &loaded.index, &loaded.target)
                            .map(|_| Some(dir.to_string_lossy().into_owned()))
                            .map_err(|e| format!("The scan could not be saved to {}: {e}", dir.display())),
                    };
                }
                Err(scan::ScanError::Cancelled) => {}
                Err(e) => error = Some(e.to_string()),
            }
            let mut job = app.job.lock().unwrap();
            job.running = false;
            job.error = error;
        });
        if let Err(e) = spawned {
            let mut job = self.job.lock().unwrap();
            job.running = false;
            job.error = Some(format!("Could not start the scan: {e}"));
            return Err((500, e.to_string()));
        }
        Ok(())
    }

    fn status(&self) -> Value {
        if self.loaded().is_none() { self.load_saved(); }
        let job = self.job.lock().unwrap();
        let loaded = self.loaded();
        let state = if job.running { "scanning" } else if loaded.is_some() { "ready" } else { "idle" };
        let p = &job.progress;
        let elapsed = job.started.elapsed().as_millis() as u64;
        let est = p.estimate(elapsed);
        let sources: Vec<Value> = p.sources.lock().unwrap().iter().map(|s| json!({
            "label": s.label, "fraction": s.fraction(),
            "entries": s.entries.load(Relaxed), "bytes": s.bytes.load(Relaxed),
        })).collect();
        let deleting = {
            let d = self.del_job.lock().unwrap();
            json!({ "active": d.active, "label": d.label, "total": d.total,
                    "done": d.done.load(Relaxed), "error": d.error })
        };
        let mut v = json!({
            "state": state,
            "platform": if self.platform.is_unraid() { "unraid" } else { "generic" },
            "version": env!("CARGO_PKG_VERSION"),
            "can_delete": self.platform.delete_settings().enabled,
            "error": job.error,
            "deleting": deleting,
            "progress": {
                "label": job.label, "files": p.files.load(Relaxed), "dirs": p.dirs.load(Relaxed),
                "bytes": p.bytes.load(Relaxed), "elapsed_ms": elapsed,
                "fraction": est.fraction, "eta_ms": est.eta_ms, "sources": sources,
                "cancelling": p.cancel.load(Relaxed),
                "current": p.current.lock().map(|c| c.clone()).unwrap_or_default(),
            },
        });
        if let Some(l) = loaded {
            let ix = &l.index;
            let root = &ix.nodes[0];
            v["scan"] = json!({
                "id": l.id.to_string(), "target": l.target, "label": ix.label, "display_root": text(&ix.display_root),
                "files": root.files, "nodes": ix.nodes.len(), "logical": root.logical,
                "physical": root.physical, "unreadable": ix.unreadable,
                "finished": l.id / 1000, "duration_ms": ix.duration.as_millis() as u64,
                "memory": ix.memory_bytes(), "fs_total": ix.fs_total, "fs_free": ix.fs_free,
                "other": ix.other_space(), "from_disk": l.from_disk, "zfs": l.zfs,
                "saved_to": self.saved.lock().unwrap().clone().ok().flatten(),
                "save_error": self.saved.lock().unwrap().clone().err(),
                "sources": ix.sources.iter().map(|s| s.label.clone()).collect::<Vec<_>>(),
            });
        }
        v
    }

    pub fn handle(self: &Arc<Self>, req: Request) -> Response {
        let path = req.path.trim_end_matches('/');
        let stat = |ct: &'static str, body: &str| Response { status: 200, content_type: ct, body: body.as_bytes().to_vec() };
        match (req.method.as_str(), path) {
            ("GET", "" | "/index.html") => return stat("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.js") => return stat("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/treemap.js") => return stat("text/javascript; charset=utf-8", TREEMAP_JS),
            ("GET", "/outline.js") => return stat("text/javascript; charset=utf-8", OUTLINE_JS),
            ("GET", "/style.css") => return stat("text/css; charset=utf-8", STYLE_CSS),
            ("GET", "/api/status") => return Response::json(self.status()),
            ("GET", "/api/targets") => {
                let items: Vec<Value> = self.platform.targets().into_iter().map(|t| json!({
                    "id": t.id, "label": t.label, "detail": t.detail, "group": t.group,
                    "total": t.total, "free": t.free })).collect();
                return Response::json(json!({ "targets": items }));
            }
            ("POST", "/api/scan") => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let Some(target) = body["target"].as_str() else { return Response::error(400, "Missing target") };
                return match self.start_scan(target) {
                    Ok(()) => Response::json(json!({ "ok": true })),
                    Err((code, msg)) => Response::error(code, &msg),
                };
            }
            ("POST", "/api/delete") => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let (Some(scan), Some(node)) = (body["scan"].as_str(), body["node"].as_u64()) else {
                    return Response::error(400, "Missing scan or node");
                };
                let Ok(node) = u32::try_from(node) else { return Response::error(400, "Unknown item.") };
                let (confirm, token) = (body["confirm"].as_str().unwrap_or(""), body["token"].as_str().unwrap_or(""));
                return match self.delete(scan, node, confirm, token) {
                    Ok(v) => Response::json(v),
                    Err((code, msg)) => Response::error(code, &msg),
                };
            }
            ("POST", "/api/cancel") => {
                self.job.lock().unwrap().progress.cancel.store(true, Relaxed);
                return Response::json(json!({ "ok": true }));
            }
            ("POST", "/api/unload") => {
                if self.job.lock().unwrap().running { return Response::error(409, "A scan is running"); }
                *self.loaded.write().unwrap() = None;
                return Response::json(json!({ "ok": true }));
            }
            _ => {}
        }
        if req.method != "GET" || !path.starts_with("/api/") { return Response::error(404, "Not found"); }
        let Some(l) = self.loaded() else { return Response::error(409, "No scan loaded") };
        let ix = &l.index;
        // Items deleted since the scan no longer exist as far as the page is concerned.
        let node_param = |key: &str| req.q_u32(key)
            .filter(|&id| (id as usize) < ix.nodes.len() && !ix.nodes[id as usize].is_removed());
        let live = |v: Value| -> Value {
            // After a delete, child counts are recounted without the deleted items.
            if !l.has_removals { return v; }
            let mut v = v;
            if let Some(id) = v["id"].as_u64() { v["children"] = json!(ix.live_children(id as u32).count()); }
            v
        };
        match path {
            "/api/kinds" => Response::json(json!({ "names": ix.kinds.names(), "colors": l.colors })),
            "/api/children" => {
                let Some(id) = node_param("node") else { return Response::error(400, "Bad node") };
                let offset = req.q_u32("offset").unwrap_or(0);
                let limit = req.q_u32("limit").unwrap_or(500).clamp(1, 5000);
                let total = ix.live_children(id).count();
                let items: Vec<Value> = ix.live_children(id).skip(offset as usize).take(limit as usize)
                    .map(|c| live(node_json(ix, c))).collect();
                Response::json(json!({ "total": total, "offset": offset.min(total as u32), "items": items }))
            }
            "/api/node" => {
                let Some(id) = node_param("id") else { return Response::error(400, "Bad node") };
                let ancestors: Vec<Value> = ix.ancestors(id).into_iter().map(|a| {
                    let parent = ix.nodes[a as usize].parent;
                    let index = if l.has_removals {
                        ix.live_children(parent).position(|c| c == a).unwrap_or(0) as u32
                    } else {
                        a.saturating_sub(ix.nodes[parent as usize].first_child)
                    };
                    json!({ "id": a, "name": text(ix.name(a)), "index": index })
                }).collect();
                Response::json(json!({
                    "node": live(node_json(ix, id)), "path": text(&ix.display_path(id)),
                    "real_path": text(&ix.real_path(id)), "ancestors": ancestors,
                }))
            }
            "/api/delete-check" => {
                if l.id.to_string() != req.q("scan") {
                    return Response::error(409, "The scan has changed since this page loaded. Reload the page.");
                }
                let Some(id) = node_param("node") else { return Response::error(400, "Bad node") };
                Response::json(self.delete_check(&l, id))
            }
            "/api/treemap" => {
                let Some(root) = node_param("root") else { return Response::error(400, "Bad root") };
                let (w, h) = (req.q_u32("w").unwrap_or(0).min(8192), req.q_u32("h").unwrap_or(0).min(8192));
                let opts = LayoutOpts {
                    physical: req.q_flag("physical"),
                    free: if req.q_flag("free") { ix.fs_free } else { 0 },
                    other: if req.q_flag("other") { ix.other_space() } else { 0 },
                };
                let cells = layout::layout(ix, root, w, h, opts);
                Response { status: 200, content_type: "application/octet-stream", body: layout::encode(&cells, w, h) }
            }
            "/api/stats" => {
                let Some(root) = node_param("root") else { return Response::error(400, "Bad root") };
                let st = stats::compute(ix, root);
                let largest = if req.q_flag("physical") { &st.largest_physical } else { &st.largest_logical };
                Response::json(json!({
                    "kinds": st.kinds.iter().map(|k| json!({
                        "kind": k.kind, "name": ix.kinds.name(k.kind), "logical": k.logical,
                        "physical": k.physical, "count": k.count })).collect::<Vec<_>>(),
                    "largest": largest.iter().map(|&id| node_with_path(ix, id)).collect::<Vec<_>>(),
                }))
            }
            "/api/search" => {
                let (hits, total) = stats::search(ix, req.q("q"), req.q_flag("physical"), 300);
                Response::json(json!({
                    "total": total,
                    "hits": hits.iter().map(|&id| node_with_path(ix, id)).collect::<Vec<_>>(),
                }))
            }
            _ => Response::error(404, "Not found"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::testutil::*;
    use std::collections::HashMap;
    use std::os::unix::ffi::OsStrExt;

    fn get(app: &Arc<App>, path: &str, query: &[(&str, &str)]) -> Response {
        app.handle(Request { method: "GET".into(), path: path.into(),
            query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: vec![] })
    }

    fn post(app: &Arc<App>, path: &str, body: Value) -> Response {
        app.handle(Request { method: "POST".into(), path: path.into(), query: HashMap::new(),
                             body: body.to_string().into_bytes() })
    }

    fn body(r: &Response) -> Value { serde_json::from_slice(&r.body).unwrap() }

    fn wait_ready(app: &Arc<App>) {
        for _ in 0..500 {
            if body(&get(app, "/api/status", &[]))["state"] != "scanning" { return; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("scan did not finish");
    }

    #[test]
    fn scan_then_browse_and_search() {
        let root = fixture("api");
        write(&root.join("films/big \"quoted\" <b>.mkv"), 40_000);
        write(&root.join("films/line\nbreak.mkv"), 20_000);
        write(&root.join("pic.jpg"), 3_000);
        let store = fixture("api-store");
        let app = App::new(Platform { unraid_mnt: None, pools_cfg: "/nonexistent".into(), boot_config: "/nonexistent".into() }, Store::Dir(store.clone()));
        assert_eq!(get(&app, "/api/children", &[("node", "0")]).status, 409);
        let target = format!("path:{}", root.display());
        assert_eq!(post(&app, "/api/scan", json!({ "target": target })).status, 200);
        wait_ready(&app);
        let st = body(&get(&app, "/api/status", &[]));
        assert_eq!(st["state"], "ready");
        assert_eq!(st["can_delete"], false);
        assert_eq!(st["scan"]["files"], 3);
        assert_eq!(st["scan"]["logical"], 63_000);

        let kids = body(&get(&app, "/api/children", &[("node", "0")]));
        assert_eq!(kids["total"], 2);
        assert_eq!(kids["items"][0]["name"], "films");
        let films = kids["items"][0]["id"].as_u64().unwrap().to_string();
        let page = body(&get(&app, "/api/children", &[("node", &films), ("offset", "1"), ("limit", "1")]));
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["name"], "line\nbreak.mkv");

        let hits = body(&get(&app, "/api/search", &[("q", "QUOTED")]));
        assert_eq!(hits["total"], 1);
        assert_eq!(hits["hits"][0]["name"], "big \"quoted\" <b>.mkv");
        let id = hits["hits"][0]["id"].as_u64().unwrap().to_string();
        let node = body(&get(&app, "/api/node", &[("id", &id)]));
        assert_eq!(node["ancestors"].as_array().unwrap().len(), 3);
        assert_eq!(node["ancestors"][2]["index"], 0);
        assert!(node["path"].as_str().unwrap().ends_with("/films/big \"quoted\" <b>.mkv"));

        let map = get(&app, "/api/treemap", &[("root", "0"), ("w", "400"), ("h", "300")]);
        assert_eq!(map.content_type, "application/octet-stream");
        let count = u32::from_le_bytes(map.body[0..4].try_into().unwrap()) as usize;
        assert!(count >= 5);
        assert_eq!(map.body.len(), 16 + count * layout::CELL_BYTES);

        let stats = body(&get(&app, "/api/stats", &[("root", "0")]));
        assert_eq!(stats["kinds"][0]["name"], "Matroska Video");
        assert_eq!(stats["largest"].as_array().unwrap().len(), 3);
        let kinds = body(&get(&app, "/api/kinds", &[]));
        assert_eq!(kinds["names"].as_array().unwrap().len(), kinds["colors"].as_array().unwrap().len());

        assert_eq!(get(&app, "/api/node", &[("id", "999999")]).status, 400);
        assert_eq!(get(&app, "/api/nope", &[]).status, 404);
        assert_eq!(get(&app, "/", &[]).content_type, "text/html; charset=utf-8");

        // A new run of the program opens straight into the saved scan.
        for _ in 0..200 {
            if !body(&get(&app, "/api/status", &[]))["scan"]["saved_to"].is_null() { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(st["scan"]["from_disk"], false);
        let again = App::new(Platform { unraid_mnt: None, pools_cfg: "/nonexistent".into(), boot_config: "/nonexistent".into() }, Store::Dir(store));
        let st2 = body(&get(&again, "/api/status", &[]));
        assert_eq!(st2["state"], "ready");
        assert_eq!(st2["scan"]["from_disk"], true);
        assert_eq!(st2["scan"]["id"], st["scan"]["id"]);
        assert_eq!(st2["scan"]["target"], target);
        assert_eq!(body(&get(&again, "/api/search", &[("q", "quoted")]))["total"], 1);
    }

    #[test]
    fn invalid_utf8_name_becomes_valid_json() {
        let ix_root = fixture("utf8");
        write(&ix_root.join("ok"), 1);
        let mut index = scan_paths(&[&ix_root]);
        // Rewrite the stored name to bytes that are not UTF-8, as a Linux disk may hold.
        let off = index.nodes[1].name_off as usize;
        index.names[off] = 0xff;
        let v = node_json(&index, 1);
        assert_eq!(v["name"], "\u{fffd}k");
        assert!(serde_json::from_str::<Value>(&v.to_string()).is_ok());
    }

    #[test]
    fn second_scan_while_running_is_refused_and_bad_target_rejected() {
        let root = fixture("busy");
        write(&root.join("f"), 1);
        let app = App::new(Platform { unraid_mnt: None, pools_cfg: "/nonexistent".into(), boot_config: "/nonexistent".into() }, Store::Off);
        app.job.lock().unwrap().running = true;
        let target = format!("path:{}", String::from_utf8_lossy(root.as_os_str().as_bytes()));
        assert_eq!(post(&app, "/api/scan", json!({ "target": target })).status, 409);
        assert_eq!(post(&app, "/api/unload", json!({})).status, 409);
        app.job.lock().unwrap().running = false;
        assert_eq!(post(&app, "/api/scan", json!({ "target": "share:Media" })).status, 400);
        assert_eq!(post(&app, "/api/scan", json!({})).status, 400);
        // A target that does not exist is refused up front, so it cannot discard a loaded scan.
        assert_eq!(post(&app, "/api/scan", json!({ "target": target })).status, 200);
        wait_ready(&app);
        let r = post(&app, "/api/scan", json!({ "target": "path:/definitely/not/here" }));
        assert_eq!(r.status, 400);
        assert!(body(&r)["error"].as_str().unwrap().contains("not a folder"));
        assert_eq!(body(&get(&app, "/api/status", &[]))["state"], "ready");
    }

    // ----- deleting: every test uses a recorder in place of the real removal -----

    use std::collections::HashSet;
    use std::sync::atomic::AtomicU64;

    static REMOVED: Mutex<Vec<(Vec<u8>, bool)>> = Mutex::new(Vec::new());

    fn record_only(path: &[u8], is_dir: bool, _: &HashSet<Vec<u8>>, _: u64, _: &AtomicU64) -> std::io::Result<()> {
        REMOVED.lock().unwrap().push((path.to_vec(), is_dir));
        Ok(())
    }

    fn always_fails(_: &[u8], _: bool, _: &HashSet<Vec<u8>>, _: u64, _: &AtomicU64) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not allowed"))
    }

    fn recorded(under: &std::path::Path) -> Vec<(String, bool)> {
        let prefix = std::fs::canonicalize(under).unwrap();
        REMOVED.lock().unwrap().iter()
            .filter(|(p, _)| p.starts_with(prefix.as_os_str().as_bytes()))
            .map(|(p, d)| (String::from_utf8_lossy(&p[prefix.as_os_str().len()..]).into_owned(), *d)).collect()
    }

    /// A pretend Unraid server with a scan of disk1 loaded. The files are made to look a
    /// few seconds old, as on a real server, where a scan is never of brand-new files.
    fn server(name: &str, cfg: &str, remover: crate::remove::Remover) -> (Arc<App>, std::path::PathBuf, String) {
        let root = std::fs::canonicalize(fixture(name)).unwrap();
        let mnt = root.join("mnt");
        write(&mnt.join("disk1/Media/films/a.mkv"), 40_000);
        write(&mnt.join("disk1/Media/films/b.mkv"), 20_000);
        write(&mnt.join("disk1/Media/old/c.mkv"), 5_000);
        write(&mnt.join("disk1/system/docker/docker.img"), 9_000);
        write(&mnt.join("disk2/Media/films/d.mkv"), 7_000);
        write(&mnt.join("user/placeholder"), 1);
        write(&root.join("config/plugins/inventoryx/inventoryx.cfg"), 0);
        std::fs::write(root.join("config/plugins/inventoryx/inventoryx.cfg"), cfg).unwrap();
        // Change times cannot be set, so wait until they are in the past before scanning.
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let platform = Platform { unraid_mnt: Some(mnt), pools_cfg: root.join("pools"), boot_config: root.join("config") };
        let mut app = App::new(platform, Store::Off);
        Arc::get_mut(&mut app).unwrap().remover = remover;
        assert_eq!(post(&app, "/api/scan", json!({ "target": "disk:disk1" })).status, 200);
        wait_ready(&app);
        let scan = body(&get(&app, "/api/status", &[]))["scan"]["id"].as_str().unwrap().to_string();
        (app, root, scan)
    }

    fn id_of(app: &Arc<App>, name: &str) -> u64 {
        let hits = body(&get(app, "/api/search", &[("q", name)]));
        hits["hits"].as_array().unwrap().iter().find(|h| h["name"] == name)
            .unwrap_or_else(|| panic!("{name} not found"))["id"].as_u64().unwrap()
    }

    fn check(app: &Arc<App>, scan: &str, id: u64) -> Value {
        body(&get(app, "/api/delete-check", &[("scan", scan), ("node", &id.to_string())]))
    }

    /// Asks for a delete the way the page does: check first, then send the token back.
    fn delete(app: &Arc<App>, scan: &str, id: u64, confirm: &str) -> Response {
        let token = check(app, scan, id)["token"].as_str().unwrap_or("").to_string();
        let r = post(app, "/api/delete", json!({ "scan": scan, "node": id, "confirm": confirm, "token": token }));
        for _ in 0..500 {
            if body(&get(app, "/api/status", &[]))["deleting"]["active"] == false { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        r
    }

    const ON: &str = "DELETE_ENABLED=\"yes\"\nTYPED_DELETE_GB=\"10\"\n";

    #[test]
    #[should_panic(expected = "real delete")]
    fn test_builds_cannot_reach_the_real_delete() {
        (default_remover())(b"/nonexistent/x", false, &HashSet::new(), 0, &AtomicU64::new(0)).ok();
    }

    #[test]
    fn deleting_is_off_until_turned_on() {
        let (app, root, scan) = server("del-off", "TYPED_DELETE_GB=\"10\"\n", record_only);
        assert_eq!(body(&get(&app, "/api/status", &[]))["can_delete"], false);
        let a = id_of(&app, "a.mkv");
        let c = check(&app, &scan, a);
        assert_eq!(c["allowed"], false);
        assert!(c["reason"].as_str().unwrap().contains("turned off"));
        assert_eq!(delete(&app, &scan, a, "DELETE").status, 403);
        assert!(recorded(&root).is_empty());
    }

    #[test]
    fn small_file_deletes_after_plain_confirmation_and_leaves_the_index() {
        let (app, root, scan) = server("del-file", ON, record_only);
        assert_eq!(body(&get(&app, "/api/status", &[]))["can_delete"], true);
        let a = id_of(&app, "a.mkv");
        let c = check(&app, &scan, a);
        assert_eq!((c["allowed"].clone(), c["typed"].clone()), (json!(true), json!(false)));
        assert!(c["paths"][0].as_str().unwrap().ends_with("/mnt/disk1/Media/films/a.mkv"));
        let token = c["token"].as_str().unwrap().to_string();

        // a request made against another scan, or without the token the dialog was given, is refused
        assert_eq!(post(&app, "/api/delete", json!({ "scan": "12345", "node": a, "token": token })).status, 409);
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan, "node": a })).status, 409);
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan, "node": a, "token": "0000000000000000" })).status, 409);
        // the token of one item does not delete another
        let b = id_of(&app, "b.mkv");
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan, "node": b, "token": token })).status, 409);
        assert!(recorded(&root).is_empty());

        let before = body(&get(&app, "/api/status", &[]))["scan"]["files"].as_u64().unwrap();
        let r = delete(&app, &scan, a, "");
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        assert_eq!(recorded(&root), [("/mnt/disk1/Media/films/a.mkv".to_string(), false)]);
        let st = body(&get(&app, "/api/status", &[]));
        assert_eq!(st["deleting"]["error"], Value::Null);
        assert_eq!(st["scan"]["files"].as_u64().unwrap(), before - 1);
        assert_eq!(body(&get(&app, "/api/search", &[("q", "a.mkv")]))["total"], 0);
        let films = id_of(&app, "films");
        let kids = body(&get(&app, "/api/children", &[("node", &films.to_string())]));
        assert_eq!(kids["total"], 1);
        assert_eq!(kids["items"][0]["name"], "b.mkv");
        assert_eq!(body(&get(&app, "/api/node", &[("id", &films.to_string())]))["node"]["children"], 1);
        assert_eq!(get(&app, "/api/node", &[("id", &a.to_string())]).status, 400);
        // a second request for the same item is refused, not repeated
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan, "node": a, "token": token })).status, 403);
        assert_eq!(recorded(&root).len(), 1);
    }

    #[test]
    fn whole_share_needs_the_word_typed_exactly() {
        let (app, root, scan) = server("del-share", ON, record_only);
        let media = id_of(&app, "Media");
        let c = check(&app, &scan, media);
        assert_eq!((c["allowed"].clone(), c["typed"].clone()), (json!(true), json!(true)));
        assert_eq!(c["files"], 3);
        for wrong in ["", "delete", "DELETE ", "yes"] {
            assert_eq!(delete(&app, &scan, media, wrong).status, 400, "{wrong:?}");
        }
        assert!(recorded(&root).is_empty());
        let r = delete(&app, &scan, media, "DELETE");
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        // only disk1 was scanned, so only disk1's copy is touched
        assert_eq!(recorded(&root), [("/mnt/disk1/Media".to_string(), true)]);
    }

    #[test]
    fn large_items_need_typing_per_the_setting() {
        let (app, root, scan) = server("del-big", "DELETE_ENABLED=\"yes\"\nTYPED_DELETE_GB=\"0\"\n", record_only);
        let b = id_of(&app, "b.mkv");
        assert_eq!(delete(&app, &scan, b, "").status, 400);
        assert!(recorded(&root).is_empty());
    }

    #[test]
    fn protected_and_root_items_are_refused() {
        let (app, root, scan) = server("del-protected", ON, record_only);
        for name in ["docker.img", "docker", "system"] {
            let id = id_of(&app, name);
            let r = delete(&app, &scan, id, "DELETE");
            assert_eq!(r.status, 403, "{name}");
            assert!(body(&r)["error"].as_str().unwrap().contains("protected"), "{name}");
        }
        assert_eq!(delete(&app, &scan, 0, "DELETE").status, 403);
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan, "node": 99_999_999u64 })).status, 400);
        assert_eq!(post(&app, "/api/delete", json!({ "scan": scan })).status, 400);
        assert!(recorded(&root).is_empty());
    }

    #[test]
    fn override_allows_the_image_only_with_typing() {
        let cfg = "DELETE_ENABLED=\"yes\"\nALLOW_PROTECTED=\"yes\"\n";
        let (app, root, scan) = server("del-override", cfg, record_only);
        let img = id_of(&app, "docker.img");
        assert_eq!(delete(&app, &scan, img, "").status, 400);
        assert!(recorded(&root).is_empty());
        assert_eq!(delete(&app, &scan, img, "DELETE").status, 200);
        assert_eq!(recorded(&root).len(), 1);
    }

    #[test]
    fn anything_changed_since_the_scan_is_refused() {
        let (app, root, scan) = server("del-changed", ON, record_only);
        // a file appears in a folder after the scan: the user never saw it
        write(&root.join("mnt/disk1/Media/old/new-and-unseen.mkv"), 123);
        let old = id_of(&app, "old");
        let r = delete(&app, &scan, old, "");
        assert_eq!(r.status, 409);
        assert!(body(&r)["error"].as_str().unwrap().contains("1 files then, 2 now"));
        // a file grows after the scan
        write(&root.join("mnt/disk1/Media/films/b.mkv"), 20_001);
        assert_eq!(delete(&app, &scan, id_of(&app, "b.mkv"), "").status, 409);
        assert!(recorded(&root).is_empty());
    }

    #[test]
    fn same_count_but_different_content_is_refused() {
        let (app, root, scan) = server("del-swapped", ON, record_only);
        // One file out, one file in: the count is unchanged, the content is not.
        // (Renaming stands in for "replaced by something else".)
        std::fs::rename(root.join("mnt/disk1/Media/old/c.mkv"), root.join("mnt/disk1/Media/old/upgraded.mkv")).unwrap();
        let old = id_of(&app, "old");
        let c = check(&app, &scan, old);
        assert_eq!(c["allowed"], false);
        assert!(c["reason"].as_str().unwrap().contains("changed since it was scanned"), "{c}");
        // the same file rewritten in place with the same length
        write(&root.join("mnt/disk1/Media/films/a.mkv"), 40_000);
        assert_eq!(delete(&app, &scan, id_of(&app, "a.mkv"), "").status, 409);
        assert!(recorded(&root).is_empty());
    }

    #[test]
    fn a_folder_that_became_a_link_is_refused() {
        let (app, root, scan) = server("del-link", ON, record_only);
        let elsewhere = fixture("del-link-elsewhere");
        write(&elsewhere.join("c.mkv"), 5_000);
        // `old` is moved away and replaced by a link to another folder holding a look-alike.
        std::fs::rename(root.join("mnt/disk1/Media/old"), root.join("mnt/disk1/Media/old-moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("mnt/disk1/Media/old")).unwrap();
        let c = id_of(&app, "c.mkv");
        let r = delete(&app, &scan, c, "");
        assert_eq!(r.status, 403, "{}", String::from_utf8_lossy(&r.body));
        assert!(body(&r)["error"].as_str().unwrap().contains("through a link"));
        assert!(recorded(&root).is_empty());
        assert!(elsewhere.join("c.mkv").exists());
    }

    #[test]
    fn a_failed_delete_reports_and_keeps_the_index() {
        let (app, _root, scan) = server("del-fails", ON, always_fails);
        let a = id_of(&app, "a.mkv");
        assert_eq!(delete(&app, &scan, a, "").status, 200);
        let st = body(&get(&app, "/api/status", &[]));
        assert!(st["deleting"]["error"].as_str().unwrap().contains("refresh the scan"));
        assert_eq!(body(&get(&app, "/api/search", &[("q", "a.mkv")]))["total"], 1);
    }
}
