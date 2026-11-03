// SPDX-License-Identifier: GPL-3.0-only
//! The offline-folder keeper (`--offline-keeper`), run by its own systemd user unit alongside
//! the Drive mount. Each cycle it reads the pinned folders from the config, works out what to do
//! with [`pin::plan`], and does it:
//!
//! - syncs each local copy with the remote using `sync/bisync` *inside the Drive mount's rclone
//!   process* (its rc API), so there is no second rclone config or login;
//! - resolves conflicts (see `conflict.rs`);
//! - starts the in-place mount for each folder (`<app>-pin@<id>.service`);
//! - on unpin, pushes last changes, unmounts and deletes the local copy.
//!
//! What it did is written to `offline.json` for the panel, the settings window and the menu.

use crate::config::{expand, Config};
use crate::conflict::{self, Remote};
use crate::pin::{self, OfflineState, Phase, PinState, Step};
use crate::rc;
use crate::units;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CYCLE: Duration = Duration::from_secs(5);
const JOB_POLL: Duration = Duration::from_millis(500);

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

// ---- rc jobs ----

/// Run an rc method as an async job and wait for it, so long transfers don't hit the HTTP
/// timeout. Returns the job's output.
fn rc_job(cfg: &Config, method: &str, mut params: Value) -> Result<Value, String> {
    params["_async"] = json!(true);
    let started = rc::rc_request(cfg, method, &params)?;
    let id = started
        .get("jobid")
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{method}: rclone didn't return a job id"))?;
    loop {
        std::thread::sleep(JOB_POLL);
        let st = rc::rc_request(cfg, "job/status", &json!({ "jobid": id }))
            .map_err(|e| format!("lost contact with rclone during {method}: {e}"))?;
        if st.get("finished").and_then(Value::as_bool) == Some(true) {
            return if st.get("success").and_then(Value::as_bool) == Some(true) {
                Ok(st.get("output").cloned().unwrap_or(Value::Null))
            } else {
                Err(st
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_string())
            };
        }
    }
}

fn bisync(cfg: &Config, remote: &str, local: &Path, resync: bool) -> Result<(), String> {
    bisync_in(cfg, remote, local, &pin::workdir(), resync)
}

/// bisync's "all files were changed" safety abort.
const ALL_CHANGED: &str = "all files were changed";

fn bisync_in(
    cfg: &Config,
    remote: &str,
    local: &Path,
    workdir: &Path,
    resync: bool,
) -> Result<(), String> {
    let run = |force: bool| {
        let mut p = json!({
            "path1": remote,
            "path2": local.to_string_lossy(),
            "workdir": workdir.to_string_lossy(),
            // rclone 1.60 rejects the call without it.
            "checkSync": "true",
        });
        if resync {
            p["resync"] = json!(true);
        }
        if force {
            p["force"] = json!(true);
        }
        rc_job(cfg, "sync/bisync", p).map(|_| ())
    };
    match run(false) {
        // This check guards against every timestamp shifting at once (a FAT disk after a DST
        // change); here it mostly fires on small folders where the one file was edited. It
        // aborts before anything is applied and only after the too-many-deletes check has
        // passed, so repeating at once with `force` (which skips both) deletes nothing more.
        Err(e) if e.contains(ALL_CHANGED) => run(true),
        r => r,
    }
}

/// The remote side of conflict resolution, through the mount's rc API.
struct RcRemote<'a> {
    cfg: &'a Config,
    remote: String,
    local: String,
}

impl Remote for RcRemote<'_> {
    fn exists(&self, path: &str) -> Result<bool, String> {
        let r = rc::rc_request(
            self.cfg,
            "operations/stat",
            &json!({ "fs": self.remote, "remote": path }),
        )?;
        Ok(r.get("item").is_some_and(|i| !i.is_null()))
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let p = json!({ "srcFs": self.remote, "srcRemote": from, "dstFs": self.remote, "dstRemote": to });
        rc_job(self.cfg, "operations/movefile", p).map(|_| ())
    }
    fn upload(&self, local: &str, to: &str) -> Result<(), String> {
        let p = json!({ "srcFs": self.local, "srcRemote": local, "dstFs": self.remote, "dstRemote": to });
        rc_job(self.cfg, "operations/copyfile", p).map(|_| ())
    }
    fn delete(&self, path: &str) -> Result<(), String> {
        let p = json!({ "fs": self.remote, "remote": path });
        rc_job(self.cfg, "operations/deletefile", p).map(|_| ())
    }
}

// ---- classifying failures ----

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// Probably transient (offline, rate limits): retry with backoff.
    Retry(String),
    /// bisync has no baseline to compare against; a resync rebuilds it without deleting.
    NeedsResync,
    /// A previous run was cut off and left its lock behind.
    StaleLock(PathBuf),
    /// Stop until the user looks.
    Attention(String),
}

fn classify(err: &str) -> Failure {
    let e = err.to_lowercase();
    if e.contains("cannot find prior path1 or path2 listings") {
        Failure::NeedsResync
    } else if let Some(lock) = err.split("prior lock file found: ").nth(1) {
        Failure::StaleLock(PathBuf::from(lock.trim()))
    } else if e.contains("directory not found") {
        Failure::Attention("The folder wasn't found in the cloud (moved or deleted?)".into())
    } else if e.contains("bisync aborted") || e.contains("must run --resync") {
        Failure::Attention(
            "Sync stopped for safety (too many deletions, or an interrupted run). \
             Repair merges both sides without deleting anything."
                .into(),
        )
    } else {
        Failure::Retry(err.to_string())
    }
}

// ---- local change detection ----

/// A cheap fingerprint of a folder tree: names, sizes and modification times.
fn fingerprint(root: &Path) -> u64 {
    fn walk(dir: &Path, h: &mut u64) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let Ok(md) = e.metadata() else { continue };
            let mut mix = |bytes: &[u8]| {
                for b in bytes {
                    *h ^= u64::from(*b);
                    *h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
            };
            mix(e.file_name().as_encoded_bytes());
            mix(&md.len().to_le_bytes());
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            mix(&mtime.to_le_bytes());
            if md.is_dir() {
                walk(&e.path(), h);
            }
        }
    }
    let mut h = 0xcbf2_9ce4_8422_2325;
    walk(root, &mut h);
    h
}

// ---- the keeper ----

struct Keeper {
    cfg: Config,
    state: OfflineState,
    /// Fingerprint of each local copy right after its last successful sync.
    synced_print: HashMap<String, u64>,
}

fn repair_dir() -> PathBuf {
    pin::state_dir().join("repair")
}

/// Ask the keeper to repair `rel` on its next cycle (from the UI or the CLI).
pub fn request_repair(rel: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(repair_dir())?;
    std::fs::write(repair_dir().join(pin::pin_id(rel)), rel)
}

fn desktop_notify(text: &str) {
    let _ = Command::new("notify-send")
        .args([
            "--app-name=rclone Status",
            "--icon=folder-sync-symbolic",
            "Offline folders",
            text,
        ])
        .spawn();
}

impl Keeper {
    fn save(&mut self) {
        self.state.updated = now();
        let _ = self.state.save_to(&pin::state_file());
    }

    fn pin(&mut self, rel: &str) -> &mut PinState {
        if self.state.pin(rel).is_none() {
            self.state.pins.push(PinState {
                rel: rel.to_string(),
                ..PinState::default()
            });
        }
        self.state.pin_mut(rel).expect("just added")
    }

    fn remote_for(&self, rel: &str) -> Result<String, String> {
        let fs = rc::mount_fs(&self.cfg.mount_path).ok_or("The Drive mount is down")?;
        Ok(pin::remote(&fs, rel))
    }

    fn cycle(&mut self) {
        self.cfg = Config::load(crate::APP_ID);
        self.state = OfflineState::load();
        let online = rc::is_mounted(&self.cfg.mount_path)
            && rc::rc_request(&self.cfg, "rc/noop", &json!({})).is_ok();
        let mounted: HashSet<String> = self
            .state
            .pins
            .iter()
            .filter(|p| pin_is_mounted(&self.cfg, &p.rel))
            .map(|p| p.rel.clone())
            .collect();
        let changed: HashSet<String> = self
            .state
            .pins
            .iter()
            .filter(|p| p.initialized)
            .filter(|p| {
                self.synced_print
                    .get(&p.rel)
                    .is_none_or(|f| *f != fingerprint(&pin::local_dir(&p.rel)))
            })
            .map(|p| p.rel.clone())
            .collect();
        let repair: HashSet<String> = std::fs::read_dir(repair_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        let facts = pin::Facts {
            now: now(),
            interval: i64::try_from(self.cfg.sync_interval_secs)
                .unwrap_or(300)
                .max(30),
            online,
            mounted: &mounted,
            changed: &changed,
            repair: &repair,
        };
        let steps = pin::plan(&self.cfg.pins, &self.state, &facts);
        for step in steps {
            self.run(step, online);
            self.save();
        }
        if self.state.updated == 0 {
            self.save();
        }
    }

    fn run(&mut self, step: Step, online: bool) {
        match step {
            Step::InitialSync(rel) => self.sync(&rel, true),
            Step::Sync(rel) => self.sync(&rel, false),
            Step::Repair(rel) => {
                let _ = std::fs::remove_file(repair_dir().join(pin::pin_id(&rel)));
                self.sync(&rel, true);
            }
            Step::StartMount(rel) => self.start_mount(&rel),
            Step::Remove(rel) => self.remove(&rel, online),
        }
    }

    /// One bisync for `rel`, then conflict resolution; updates the pin's state either way.
    fn sync(&mut self, rel: &str, resync: bool) {
        let local = pin::local_dir(rel);
        let started = now();
        {
            let p = self.pin(rel);
            p.last_attempt = started;
            if !p.initialized {
                p.phase = Phase::Downloading;
            }
        }
        self.save();
        match self.sync_once(rel, &local, resync) {
            Ok(()) => {
                self.synced_print
                    .insert(rel.to_string(), fingerprint(&local));
                let first = !self.pin(rel).initialized;
                let p = self.pin(rel);
                p.initialized = true;
                p.phase = Phase::Ready;
                p.last_sync = Some(now());
                p.failures = 0;
                p.error = None;
                if first {
                    self.start_mount(rel);
                }
            }
            Err(e) => self.failed(rel, &e, resync),
        }
    }

    fn sync_once(&mut self, rel: &str, local: &Path, resync: bool) -> Result<(), String> {
        std::fs::create_dir_all(local).map_err(|e| e.to_string())?;
        let remote = self.remote_for(rel)?;
        bisync(&self.cfg, &remote, local, resync)?;
        self.resolve_conflicts(rel, &remote, local)
    }

    fn failed(&mut self, rel: &str, err: &str, was_resync: bool) {
        let p = self.pin(rel);
        p.failures += 1;
        p.error = Some(err.to_string());
        match classify(err) {
            Failure::NeedsResync if !was_resync => {
                // Safe to do without asking: a resync only copies, never deletes.
                self.sync(rel, true);
            }
            Failure::StaleLock(lock) => {
                // This keeper is the only one using the workdir, and its own job has finished,
                // so the lock is left over from a run that was cut off.
                if lock.starts_with(pin::workdir()) {
                    let _ = std::fs::remove_file(lock);
                }
                let p = self.pin(rel);
                p.phase = if p.initialized {
                    Phase::Waiting
                } else {
                    Phase::Downloading
                };
            }
            Failure::Attention(why) => {
                let p = self.pin(rel);
                p.phase = Phase::Attention;
                p.error = Some(why.clone());
                desktop_notify(&format!("{rel}: {why}"));
            }
            Failure::Retry(_) | Failure::NeedsResync => {
                let p = self.pin(rel);
                p.phase = if p.initialized {
                    Phase::Waiting
                } else {
                    Phase::Downloading
                };
            }
        }
    }

    fn resolve_conflicts(&mut self, rel: &str, remote: &str, local: &Path) -> Result<(), String> {
        let pairs = conflict::find_pairs(local);
        if pairs.is_empty() {
            return Ok(());
        }
        let r = RcRemote {
            cfg: &self.cfg,
            remote: remote.to_string(),
            local: local.to_string_lossy().into_owned(),
        };
        let mut texts = vec![];
        for base in &pairs {
            let res = conflict::resolve(local, base, &r)?;
            texts.push(res.notice(rel));
        }
        // Both sides now match; rebuild bisync's baseline so the files aren't seen as
        // changed on both sides again.
        bisync(&self.cfg, remote, local, true)?;
        let at = now();
        for t in &texts {
            self.state.notice(at, rel, t.clone());
        }
        desktop_notify(&if texts.len() == 1 {
            texts[0].clone()
        } else {
            format!("Resolved {} conflicting edits in {rel}", texts.len())
        });
        Ok(())
    }

    fn start_mount(&mut self, rel: &str) {
        let mp = pin::mount_point(&self.cfg, rel);
        if !mp.is_dir() {
            // The cloud folder is gone; mounting over nothing would hide that.
            let p = self.pin(rel);
            if p.phase != Phase::Attention {
                p.phase = Phase::Attention;
                p.error =
                    Some("The folder wasn't found in the Drive mount (moved or deleted?)".into());
            }
            return;
        }
        if let Err(e) = units::systemctl(&["start", &units::pin_unit_name(&pin::pin_id(rel))]) {
            self.pin(rel).error = Some(format!("Couldn't mount the offline copy in place: {e}"));
        }
    }

    fn remove(&mut self, rel: &str, online: bool) {
        let local = pin::local_dir(rel);
        let initialized = self.state.pin(rel).is_some_and(|p| p.initialized);
        self.pin(rel).phase = Phase::Removing;
        if initialized && local.is_dir() {
            if !online {
                self.pin(rel).error =
                    Some("Waiting for a connection to upload the last changes".into());
                return;
            }
            let pushed = self
                .remote_for(rel)
                .and_then(|remote| bisync(&self.cfg, &remote, &local, false));
            if let Err(e) = pushed {
                self.pin(rel).error = Some(format!("Couldn't upload the last changes: {e}"));
                return;
            }
        }
        let unit = units::pin_unit_name(&pin::pin_id(rel));
        let _ = units::systemctl(&["stop", &unit]);
        if pin_is_mounted(&self.cfg, rel) {
            self.pin(rel).error = Some("Files in the folder are still open".into());
            return;
        }
        let gone = remove_local(rel);
        if let Err(e) = gone {
            self.pin(rel).error = Some(format!("Couldn't delete the local copy: {e}"));
            return;
        }
        self.synced_print.remove(rel);
        self.state.pins.retain(|p| p.rel != rel);
    }
}

/// Delete a folder's local copy, its bisync working files and its mount cache.
fn remove_local(rel: &str) -> std::io::Result<()> {
    let local = pin::local_dir(rel);
    if local.exists() {
        std::fs::remove_dir_all(&local)?;
    }
    let id = pin::pin_id(rel);
    if let Ok(rd) = std::fs::read_dir(pin::workdir()) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(&id) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let cache = pin_cache(&id);
    if cache.exists() {
        std::fs::remove_dir_all(cache)?;
    }
    Ok(())
}

fn pin_cache(id: &str) -> PathBuf {
    pin::state_dir().join("pin-cache").join(id)
}

fn pin_is_mounted(cfg: &Config, rel: &str) -> bool {
    rc::is_mounted(&pin::mount_point(cfg, rel).to_string_lossy())
}

/// `--offline-keeper`: run forever.
pub fn run() -> ! {
    let mut k = Keeper {
        cfg: Config::load(crate::APP_ID),
        state: OfflineState::load(),
        synced_print: HashMap::new(),
    };
    loop {
        k.cycle();
        std::thread::sleep(CYCLE);
    }
}

/// The pinned folder whose id is `id`.
fn pin_for_id(cfg: &Config, id: &str) -> Option<String> {
    cfg.pins.iter().find(|r| pin::pin_id(r) == id).cloned()
}

/// `--pin-mount ID`: replace this process with the rclone mount of that folder's local copy on
/// top of its place in the Drive mount (run by `<app>-pin@ID.service`).
pub fn pin_mount(id: &str) -> Result<(), String> {
    let cfg = Config::load(crate::APP_ID);
    let rel = pin_for_id(&cfg, id).ok_or_else(|| format!("No offline folder has id {id}"))?;
    let local = pin::local_dir(&rel);
    let mp = pin::mount_point(&cfg, &rel);
    if !local.is_dir() {
        return Err(format!("The local copy {} is missing", local.display()));
    }
    let err = Command::new("rclone")
        .arg("mount")
        .arg(&local)
        .arg(&mp)
        // Editors need read-write opens and seeks, which need at least the writes cache.
        .args(["--vfs-cache-mode", "writes", "--vfs-write-back", "1s"])
        .arg("--cache-dir")
        .arg(pin_cache(id))
        // The keeper changes the local copy behind this mount's back, and a local folder can't
        // announce changes, so don't trust directory listings for long.
        .args(["--dir-cache-time", "5s"])
        .exec();
    Err(format!("Couldn't start rclone: {err}"))
}

/// `--pin-unmount ID`: make sure the folder's mount is gone (after its unit stops).
pub fn pin_unmount(id: &str) {
    let cfg = Config::load(crate::APP_ID);
    if let Some(rel) = pin_for_id(&cfg, id) {
        let mp = pin::mount_point(&cfg, &rel);
        if rc::is_mounted(&mp.to_string_lossy()) {
            let _ = Command::new("fusermount3").arg("-uz").arg(&mp).status();
        }
    }
}

/// `--pin PATH` / `--unpin PATH` from the command line or the menu. Returns the relative path.
pub fn set_pinned(path: &str, pinned: bool) -> Result<String, String> {
    let cfg = Config::load(crate::APP_ID);
    if !cfg.offline_enabled {
        return Err("Offline folders are turned off; turn them on in the settings first".into());
    }
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let rel = crate::share::remote_path(&expand(&cfg.mount_path), &abs)
        .ok_or("Not inside the Drive folder")?;
    if pinned {
        let fs = rc::mount_fs(&cfg.mount_path).ok_or("The Drive mount is down")?;
        let rel = pin::normalize(&cfg.mount_path, &abs, abs.is_dir(), &cfg.pins, &fs)?;
        Config::save(crate::APP_ID, |c| c.pins.push(rel.clone()))?;
        Ok(rel)
    } else {
        if !cfg.pins.contains(&rel) {
            return Err("This folder isn't kept offline".into());
        }
        Config::save(crate::APP_ID, |c| c.pins.retain(|p| *p != rel))?;
        Ok(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real rclone: `rclone rcd --rc-no-auth --rc-addr 127.0.0.1:5611` and
    /// `RCS_IT_DIR` set to a short, empty-able directory (bisync's file names must stay short).
    /// Checks the rc calls this module makes are ones rclone accepts, end to end.
    #[test]
    #[ignore = "needs a running `rclone rcd`; see the doc comment"]
    fn real_rclone_sync_and_conflict_round_trip() {
        let base = PathBuf::from(std::env::var("RCS_IT_DIR").expect("RCS_IT_DIR"));
        let (r, l, wd) = (base.join("r"), base.join("l"), base.join("wd"));
        for d in [&r, &l, &wd] {
            std::fs::remove_dir_all(d).ok();
        }
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::create_dir_all(&l).unwrap();
        std::fs::write(r.join("sub/a.txt"), "v1").unwrap();
        let cfg = Config {
            rc_addr: std::env::var("RCS_IT_RC").unwrap_or("127.0.0.1:5611".into()),
            env_file: "/nonexistent".into(),
            ..Config::default()
        };
        let remote = r.to_string_lossy().into_owned();
        let run = |resync: bool| bisync_in(&cfg, &remote, &l, &wd, resync);
        run(true).unwrap();
        assert_eq!(std::fs::read_to_string(l.join("sub/a.txt")).unwrap(), "v1");

        // Edit both sides; the local edit is newer.
        std::fs::write(r.join("sub/a.txt"), "remote edit").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(l.join("sub/a.txt"), "local edit").unwrap();
        run(false).unwrap();
        assert_eq!(conflict::find_pairs(&l), ["sub/a.txt"]);

        let rr = RcRemote {
            cfg: &cfg,
            remote: remote.clone(),
            local: l.to_string_lossy().into_owned(),
        };
        let res = conflict::resolve(&l, "sub/a.txt", &rr).unwrap();
        assert_eq!(res.winner, conflict::Winner::Local);
        run(true).unwrap();
        for side in [&r, &l] {
            assert_eq!(
                std::fs::read_to_string(side.join("sub/a.txt")).unwrap(),
                "local edit"
            );
            assert!(!side.join("sub/a.txt..path1").exists());
            assert!(!side.join("sub/a.txt..path2").exists());
        }
        // And the next normal sync sees nothing to do.
        run(false).unwrap();
        assert_eq!(conflict::find_pairs(&l), Vec::<String>::new());
    }

    #[test]
    fn classifies_bisync_errors() {
        assert_eq!(
            classify("cannot find prior Path1 or Path2 listings, likely due to critical error"),
            Failure::NeedsResync
        );
        assert_eq!(
            classify("prior lock file found: /s/bisync/a..b.lck"),
            Failure::StaleLock(PathBuf::from("/s/bisync/a..b.lck"))
        );
        assert!(matches!(
            classify("directory not found"),
            Failure::Attention(_)
        ));
        assert!(matches!(classify("bisync aborted"), Failure::Attention(_)));
        assert!(matches!(
            classify("Post \"https://www.googleapis.com\": dial tcp: no such host"),
            Failure::Retry(_)
        ));
    }

    #[test]
    fn fingerprint_changes_with_content_and_names_only() {
        let d = std::env::temp_dir().join(format!("rcs-fp-{}", std::process::id()));
        std::fs::create_dir_all(d.join("s")).unwrap();
        std::fs::write(d.join("s/a"), "1").unwrap();
        let a = fingerprint(&d);
        assert_eq!(a, fingerprint(&d), "stable");
        std::fs::write(d.join("s/a"), "12").unwrap();
        let b = fingerprint(&d);
        assert_ne!(a, b);
        std::fs::rename(d.join("s/a"), d.join("s/b")).unwrap();
        assert_ne!(b, fingerprint(&d));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn rc_job_waits_for_the_result() {
        use crate::testutil::{ok, FakeServer};
        let s = FakeServer::start(vec![
            ok("/sync/bisync", r#"{"jobid":7}"#),
            ok(
                "/job/status",
                r#"{"finished":true,"success":false,"error":"bisync aborted"}"#,
            ),
        ]);
        let cfg = Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent".into(),
            ..Config::default()
        };
        let err = bisync(&cfg, "gdrive,skip_gdocs=true:Work", Path::new("/l"), true).unwrap_err();
        assert_eq!(err, "bisync aborted");
        let reqs = s.requests();
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["path1"], "gdrive,skip_gdocs=true:Work");
        assert_eq!(body["path2"], "/l");
        assert_eq!(body["checkSync"], "true");
        assert_eq!(body["resync"], true);
        assert_eq!(body["_async"], true);
        assert_eq!(
            serde_json::from_str::<Value>(&reqs[1].body).unwrap()["jobid"],
            7
        );
    }
}
