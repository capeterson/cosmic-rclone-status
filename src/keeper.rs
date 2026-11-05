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
use crate::rc::{self, ChangeKind, Direction};
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
/// timeout. Returns the job's status reply.
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
                Ok(st)
            } else {
                Err(failure_text(&st))
            };
        }
    }
}

/// Remove ANSI colour codes (rclone colours bisync's messages, even in rc replies).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // `ESC [ params letter`
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Why a finished rc job failed. For bisync's aborts the job's `error` is only "bisync aborted";
/// the reason (listings missing, folder not found, ...) is in the captured output.
fn failure_text(st: &Value) -> String {
    let err = strip_ansi(st.get("error").and_then(Value::as_str).unwrap_or("failed"));
    let output = st
        .pointer("/output/output")
        .and_then(Value::as_str)
        .map(strip_ansi)
        .unwrap_or_default();
    match abort_reason(&output) {
        Some(reason) if err.trim() == "bisync aborted" => reason,
        _ => err,
    }
}

/// The reason in bisync's output: the last "Bisync critical error:" or "Safety abort:" line.
fn abort_reason(output: &str) -> Option<String> {
    output.lines().rev().find_map(|l| {
        let (_, reason) = l
            .split_once("Bisync critical error: ")
            .or_else(|| l.split_once("Safety abort: "))?;
        Some(reason.trim().to_string())
    })
}

// ---- reading bisync's listings ----
//
// bisync keeps a listing per side from the last sync (`<session>.path1.lst` for the remote,
// `.path2.lst` for the local copy) and writes the current state next to it (`.lst-new`) before
// its safety checks. Comparing the two says exactly what was deleted, whatever rclone's log
// level (the mount logs at NOTICE, so bisync's own INFO lines never arrive).

/// A listing as path → its line (size, hash, time), or `None` if it isn't there.
fn read_listing(file: &Path) -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(file).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| {
                // `- size hash - modtime "path"`; the path is the quoted tail.
                let start = l.find('"')?;
                let path = l[start + 1..].strip_suffix('"')?;
                Some((path.to_string(), l.to_string()))
            })
            .collect(),
    )
}

/// The listing file for one side of `local`'s session (`suffix` is e.g. `.path1.lst-new`).
fn listing_file(workdir: &Path, local: &Path, suffix: &str) -> Option<PathBuf> {
    let flat = local
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "_");
    std::fs::read_dir(workdir).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        (n.contains(&flat) && n.ends_with(suffix)).then(|| e.path())
    })
}

/// What a sync is about to delete, from an aborted run's listings.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pending {
    /// Deleted in the cloud since the last sync, and how many files the cloud side had.
    gone_remote: Vec<String>,
    had_remote: usize,
    /// Deleted here since the last sync, and how many files this side had.
    gone_local: Vec<String>,
    had_local: usize,
}

impl Pending {
    fn read(workdir: &Path, local: &Path) -> Option<Self> {
        let lst = |s: &str| listing_file(workdir, local, s).and_then(|f| read_listing(&f));
        let (old1, new1) = (lst(".path1.lst")?, lst(".path1.lst-new")?);
        let (old2, new2) = (lst(".path2.lst")?, lst(".path2.lst-new")?);
        Some(Self::from_listings(&old1, &new1, &old2, &new2))
    }

    /// A file counts as deleted on a side when it left that side's listing while the other
    /// side's copy stayed exactly as it was (otherwise bisync restores it, not deletes it).
    fn from_listings(
        old1: &HashMap<String, String>,
        new1: &HashMap<String, String>,
        old2: &HashMap<String, String>,
        new2: &HashMap<String, String>,
    ) -> Self {
        let gone = |old: &HashMap<String, String>,
                    new: &HashMap<String, String>,
                    other_old: &HashMap<String, String>,
                    other_new: &HashMap<String, String>| {
            let mut v: Vec<String> = old
                .keys()
                .filter(|p| !new.contains_key(*p))
                .filter(|p| other_old.get(*p).is_some() && other_old.get(*p) == other_new.get(*p))
                .cloned()
                .collect();
            v.sort();
            v
        };
        Pending {
            gone_remote: gone(old1, new1, old2, new2),
            had_remote: old1.len(),
            gone_local: gone(old2, new2, old1, new1),
            had_local: old2.len(),
        }
    }

    /// Within bisync's usual limit on both sides.
    fn within_limit(&self) -> bool {
        deletes_ok(self.gone_remote.len(), self.had_remote)
            && deletes_ok(self.gone_local.len(), self.had_local)
    }
}

/// bisync's own default limit: refuse when more than this share of a side was deleted.
const MAX_DELETE_PERCENT_USIZE: usize = MAX_DELETE_PERCENT as usize;
/// ...unless it's only this many files, so emptying a small folder isn't treated as a disaster.
const DELETE_FLOOR: usize = 3;

fn deletes_ok(deleted: usize, had: usize) -> bool {
    deleted <= DELETE_FLOOR || deleted * 100 <= had * MAX_DELETE_PERCENT_USIZE
}

// ---- what a sync changed, for recent activity ----

/// Files as path → (size, modified time in ms).
type Files = HashMap<String, (i64, i64)>;

/// A listing file's entries with their size and time (`- size hash - time "path"`).
fn listing_files(file: &Path) -> Option<Files> {
    let parse = |line: &str| {
        let mut f = line.split_whitespace();
        let size = f.nth(1)?.parse().ok()?;
        let time = f.nth(2)?;
        let ms = jiff::fmt::strtime::parse("%Y-%m-%dT%H:%M:%S%.f%z", time)
            .and_then(|t| t.to_timestamp())
            .ok()?
            .as_millisecond();
        Some((size, ms))
    };
    Some(
        read_listing(file)?
            .into_iter()
            .filter_map(|(path, line)| Some((path, parse(&line)?)))
            .collect(),
    )
}

/// What's in the local copy right now.
fn local_files(root: &Path) -> Files {
    fn walk(dir: &Path, rel: &str, out: &mut Files) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&e.path(), &child, out);
            } else {
                let ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                out.insert(child, (i64::try_from(md.len()).unwrap_or(-1), ms));
            }
        }
    }
    let mut out = Files::new();
    walk(root, "", &mut out);
    out
}

/// The same file content as far as a listing can tell (size, and time to the millisecond).
fn same(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 == b.0 && (a.1 - b.1).abs() <= 1
}

/// Bisync's conflict copies; conflicts get their own notices instead.
fn is_conflict_copy(path: &str) -> bool {
    path.ends_with(conflict::P1) || path.ends_with(conflict::P2)
}

/// What a sync did, from three views of the local copy: as of the last sync (`synced`, bisync's
/// listing), just before this sync (`before`), and after it (`after`). Changes between `synced`
/// and `before` were made here and went up; changes between `before` and `after` came down.
/// Returns (kind, direction, path, size), sorted by path.
fn classify_changes(
    synced: &Files,
    before: &Files,
    after: &Files,
) -> Vec<(ChangeKind, Direction, String, i64)> {
    let mut out = vec![];
    let mut diff = |old: &Files, new: &Files, dir: Direction| {
        for (p, &now) in new {
            match old.get(p) {
                None => out.push((ChangeKind::Created, dir, p.clone(), now.0)),
                Some(&was) if !same(was, now) => {
                    out.push((ChangeKind::Updated, dir, p.clone(), now.0));
                }
                Some(_) => {}
            }
        }
        for p in old.keys().filter(|p| !new.contains_key(*p)) {
            out.push((ChangeKind::Deleted, dir, p.clone(), -1));
        }
    };
    diff(synced, before, Direction::Upload);
    diff(before, after, Direction::Download);
    // A file in conflict shows up as gone and as two renamed copies; leave all that to the
    // conflict notice.
    let conflicted: HashSet<String> = after
        .keys()
        .filter_map(|p| p.strip_suffix(conflict::P1).map(str::to_string))
        .collect();
    out.retain(|(_, _, p, _)| !is_conflict_copy(p) && !conflicted.contains(p));
    out.sort_by(|a, b| (&a.2, a.1 as u8).cmp(&(&b.2, b.1 as u8)));
    out
}

/// Run bisync for one folder.
fn bisync(cfg: &Config, remote: &str, local: &Path, resync: bool) -> Result<(), String> {
    // Jobs run inside rclone, so one started by an earlier keeper (before a restart or an
    // upgrade) can still be going. Never run a second one alongside it.
    if running_jobs(cfg) > 0 {
        return Err(EARLIER_SYNC_RUNNING.into());
    }
    bisync_in(cfg, remote, local, &pin::workdir(), resync)
}

const EARLIER_SYNC_RUNNING: &str = "An earlier sync is still finishing";

/// How many rc jobs rclone is still running. This keeper is what starts async jobs on the
/// mount's rclone, so any running job is a sync it started. Unknown counts as none: if rclone
/// can't be asked, the sync itself fails anyway.
fn running_jobs(cfg: &Config) -> usize {
    let Ok(list) = rc::rc_request(cfg, "job/list", &json!({})) else {
        return 0;
    };
    list.get("jobids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .filter(|id| {
            rc::rc_request(cfg, "job/status", &json!({ "jobid": id }))
                .is_ok_and(|s| s.get("finished").and_then(Value::as_bool) == Some(false))
        })
        .count()
}

/// bisync's "all files were changed" safety abort.
const ALL_CHANGED: &str = "all files were changed";

/// The cloud folder at a pinned path has a different Drive id than the one the copy came from.
const FOLDER_REPLACED: &str = "cloud folder was replaced";
const FOLDER_REPLACED_WHY: &str = "The folder in the cloud was deleted and recreated, so this \
     offline copy no longer matches it. Repair merges both sides without deleting anything.";

/// bisync's "too many deletes" safety abort.
const TOO_MANY_DELETES: &str = "too many deletes";

/// bisync's own delete limit, as a percentage (rc passes it as `maxDelete`).
const MAX_DELETE_PERCENT: u32 = 50;

/// Never sync these: rclone's temporary download files, which a killed sync can leave in the
/// local copy and which would otherwise be uploaded as if they were the user's files.
const FILTERS: &str = "- *.partial\n";

/// Run bisync.
///
/// Every call asks for the self-healing options (`resilient`, `recover`, `maxLock`) so an
/// interrupted run recovers on its own, tracks renames so renaming a file or folder moves it in
/// the cloud instead of uploading it again, and names conflict copies the way `conflict.rs`
/// expects. A resync keeps the newer file when both sides differ (the default would let the
/// cloud copy overwrite a newer local one).
fn bisync_in(
    cfg: &Config,
    remote: &str,
    local: &Path,
    workdir: &Path,
    resync: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(workdir).map_err(|e| e.to_string())?;
    let filters = workdir.join("keeper-filters.txt");
    if std::fs::read_to_string(&filters).ok().as_deref() != Some(FILTERS) {
        std::fs::write(&filters, FILTERS).map_err(|e| e.to_string())?;
    }
    let run = |force: bool| {
        let mut p = json!({
            "path1": remote,
            "path2": local.to_string_lossy(),
            "workdir": workdir.to_string_lossy(),
            "filtersFile": filters.to_string_lossy(),
            "maxDelete": MAX_DELETE_PERCENT,
            "resilient": true,
            "recover": true,
            "maxLock": "2m",
            "conflictLoser": "pathname",
            "conflictSuffix": ".path",
            "_config": { "TrackRenames": true },
        });
        if resync {
            p["resync"] = json!(true);
            p["resyncMode"] = json!("newer");
        }
        if force {
            p["force"] = json!(true);
        }
        rc_job(cfg, "sync/bisync", p).map(|_| ())
    };
    match run(false) {
        Ok(()) => Ok(()),
        // bisync refuses to delete more than its percentage, which on a small folder is a
        // handful of files. Allow up to `DELETE_FLOOR` files regardless. The abort comes before
        // anything is applied and leaves both sides' listings, so count what's pending here.
        Err(e) if e.contains(TOO_MANY_DELETES) => {
            let pending = Pending::read(workdir, local).ok_or_else(|| {
                format!("bisync aborted: {TOO_MANY_DELETES} (can't check how many)")
            })?;
            if !pending.within_limit() {
                return Err(format!(
                    "bisync aborted: {TOO_MANY_DELETES} ({} of {} in the cloud, {} of {} here)",
                    pending.gone_remote.len(),
                    pending.had_remote,
                    pending.gone_local.len(),
                    pending.had_local,
                ));
            }
            run(true)
        }
        // This check guards against every timestamp shifting at once (a FAT disk after a DST
        // change); here it mostly fires on small folders where the one file was edited. It
        // aborts before anything is applied and only after the delete check has passed (so
        // nothing was deleted), and repeating with `force` deletes nothing more.
        Err(e) if e.contains(ALL_CHANGED) => run(true),
        Err(e) => Err(e),
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
    } else if e.contains("empty current path") {
        Failure::Attention(
            "One side of the folder is empty, and syncing would empty the other side too. \
             Repair brings the files back."
                .into(),
        )
    } else if e.contains("filters") && e.contains("resync") {
        // The filters changed since the baseline was made.
        Failure::NeedsResync
    } else if let Some(lock) = err.split("prior lock file found: ").nth(1) {
        // The message goes on with tips after the path.
        Failure::StaleLock(PathBuf::from(
            lock.lines().next().unwrap_or_default().trim(),
        ))
    } else if e.contains(FOLDER_REPLACED) {
        Failure::Attention(FOLDER_REPLACED_WHY.into())
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
            "--app-name=rclone UI",
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

    /// The cloud side to use when only flushing a copy that already syncs (unpinning): by the
    /// Drive id it has been synced with, if any.
    fn remote_for(&self, rel: &str) -> Result<String, String> {
        let fs = rc::mount_fs(&self.cfg.mount_path).ok_or("The Drive mount is down")?;
        Ok(
            match self.state.pin(rel).and_then(|p| p.folder_id.as_deref()) {
                Some(id) => pin::remote_by_id(&fs, id),
                None => pin::remote(&fs, rel),
            },
        )
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
            p.syncing = true;
            if !p.initialized {
                p.phase = Phase::Downloading;
            }
        }
        // Written now so the UI can show the sync while it runs.
        self.save();
        let result = self.sync_once(rel, &local, resync);
        self.pin(rel).syncing = false;
        match result {
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
        let fs = rc::mount_fs(&self.cfg.mount_path).ok_or("The Drive mount is down")?;
        let path_remote = pin::remote(&fs, rel);
        // Address the cloud folder by its Drive id (see `pin::remote_by_id`), when it has one.
        let known = self.state.pin(rel).and_then(|p| p.folder_id.clone());
        let id = match cloud_folder_id(&self.cfg, &fs, rel) {
            Lookup::Id(id) => Some(id),
            // Once a copy syncs by id there is no baseline for the path, and a resync (which the
            // keeper starts by itself when a baseline is missing) would recreate a deleted
            // folder from the local copy. So never fall back to the path from here.
            Lookup::Missing if known.is_some() => {
                return Err("directory not found: the folder is gone from the cloud".into())
            }
            Lookup::Failed(e) if known.is_some() => {
                return Err(format!("Couldn't look up the cloud folder: {e}"))
            }
            Lookup::NotDrive | Lookup::Missing | Lookup::Failed(_) => None,
        };
        let (remote, resync) = if let Some(id) = &id {
            if known.as_deref().is_some_and(|k| k != id) {
                return Err(FOLDER_REPLACED.into());
            }
            let mut resync = resync;
            if known.is_none() {
                // First time on this id: bisync's working files for it don't exist yet. An
                // existing copy first syncs the old way so changes made since its last sync
                // aren't lost, then the old baseline is dropped and the new one is made.
                if self.state.pin(rel).is_some_and(|p| p.initialized) && !resync {
                    self.bisync_recorded(rel, &path_remote, local, false)?;
                    resync = true;
                }
                clear_workdir(rel);
            }
            (pin::remote_by_id(&fs, id), resync)
        } else {
            if !pin::session_fits(&path_remote, &local.to_string_lossy()) {
                return Err("The folder's path is too long for rclone bisync".into());
            }
            (path_remote, resync)
        };
        self.bisync_recorded(rel, &remote, local, resync)?;
        if id.is_some() {
            self.pin(rel).folder_id = id;
        }
        self.resolve_conflicts(rel, &remote, local)
    }

    /// bisync, noting what it changed for the recent-activity list. A resync (the first download,
    /// or a repair) isn't recorded: it would list every file in the folder.
    fn bisync_recorded(
        &mut self,
        rel: &str,
        remote: &str,
        local: &Path,
        resync: bool,
    ) -> Result<(), String> {
        let synced = (!resync)
            .then(|| listing_file(&pin::workdir(), local, ".path2.lst"))
            .flatten()
            .and_then(|f| listing_files(&f));
        let before = synced.as_ref().map(|_| local_files(local));
        bisync(&self.cfg, remote, local, resync)?;
        if let (Some(synced), Some(before)) = (synced, before) {
            let at = now();
            let after = local_files(local);
            for (kind, direction, path, size) in classify_changes(&synced, &before, &after) {
                self.state.record_change(pin::Change {
                    at,
                    path: format!("{rel}/{path}"),
                    size,
                    kind,
                    direction,
                });
            }
        }
        Ok(())
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
                // Only this keeper uses the workdir and its own job has finished, so unless rclone
                // is still running a job (one started before the keeper restarted), the lock is
                // left over from a run that was cut off.
                if lock.starts_with(pin::workdir()) && running_jobs(&self.cfg) == 0 {
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
            let pushed = match self.remote_for(rel) {
                Ok(remote) => self.bisync_recorded(rel, &remote, &local, false),
                Err(e) => Err(e),
            };
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

/// What looking up a cloud folder's Drive id found.
#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    Id(String),
    /// The remote answered and there is no such folder.
    Missing,
    /// The remote isn't Google Drive, so there are no folder ids to use.
    NotDrive,
    Failed(String),
}

/// The Drive id of the cloud folder `rel` on the mounted remote `fs`.
fn cloud_folder_id(cfg: &Config, fs: &str, rel: &str) -> Lookup {
    let name = fs.split(':').next().unwrap_or_default();
    let conf = match rc::rc_request(cfg, "config/get", &json!({ "name": name })) {
        Ok(c) => c,
        Err(e) => return Lookup::Failed(e),
    };
    if conf.get("type").and_then(Value::as_str) != Some("drive") {
        return Lookup::NotDrive;
    }
    let st = match rc::rc_request(cfg, "operations/stat", &json!({ "fs": fs, "remote": rel })) {
        Ok(st) => st,
        Err(e) => return Lookup::Failed(e),
    };
    let Some(item) = st.get("item").filter(|i| !i.is_null()) else {
        return Lookup::Missing;
    };
    match item.get("ID").and_then(Value::as_str) {
        Some(id) if !id.is_empty() && item.get("IsDir").and_then(Value::as_bool) == Some(true) => {
            Lookup::Id(id.to_string())
        }
        _ => Lookup::Missing,
    }
}

/// Delete a folder's bisync working files (its baseline).
fn clear_workdir(rel: &str) {
    let id = pin::pin_id(rel);
    if let Ok(rd) = std::fs::read_dir(pin::workdir()) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(&id) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Delete a folder's local copy, its bisync working files and its mount cache.
fn remove_local(rel: &str) -> std::io::Result<()> {
    let local = pin::local_dir(rel);
    if local.exists() {
        std::fs::remove_dir_all(&local)?;
    }
    clear_workdir(rel);
    let id = pin::pin_id(rel);
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
    // A sync that was running when the keeper last stopped isn't running any more.
    for p in &mut k.state.pins {
        p.syncing = false;
    }
    k.save();
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
        // The folder is mounted over its own place in the Drive mount, which lists the cloud copy.
        // (rclone checks this properly since 1.62 and refuses without the flag.)
        .arg("--allow-non-empty")
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

        // A single deletion goes through (it is under bisync's delete limit), and the log
        // says what was deleted.
        std::fs::write(r.join("sub/keep.txt"), "k").unwrap();
        run(true).unwrap();
        std::fs::remove_file(l.join("sub/a.txt")).unwrap();
        std::fs::write(l.join("sub/new.txt"), "n").unwrap();
        std::fs::write(r.join("sub/keep.txt"), "k2").unwrap();
        let synced = listing_files(&listing_file(&wd, &l, ".path2.lst").unwrap()).unwrap();
        let before = local_files(&l);
        run(false).unwrap();
        assert!(!r.join("sub/a.txt").exists());
        let changes = classify_changes(&synced, &before, &local_files(&l));
        assert_eq!(
            changes,
            [
                (
                    ChangeKind::Deleted,
                    Direction::Upload,
                    "sub/a.txt".to_string(),
                    -1
                ),
                (
                    ChangeKind::Updated,
                    Direction::Download,
                    "sub/keep.txt".to_string(),
                    2
                ),
                (
                    ChangeKind::Created,
                    Direction::Upload,
                    "sub/new.txt".to_string(),
                    1
                ),
            ]
        );
    }

    /// Same setup as `real_rclone_sync_and_conflict_round_trip`. Checks the behaviours the keeper
    /// relies on from rclone itself, so an rclone upgrade that changes them fails here first:
    /// renames become moves, `*.partial` files stay out, a small folder may lose a few files, and
    /// failures come back in a form `classify` understands.
    #[test]
    #[ignore = "needs a running `rclone rcd`; see real_rclone_sync_and_conflict_round_trip"]
    fn real_rclone_renames_filters_floor_and_failures() {
        use std::os::unix::fs::MetadataExt;
        let base = PathBuf::from(std::env::var("RCS_IT_DIR").expect("RCS_IT_DIR")).join("b");
        let (r, l, wd) = (base.join("r"), base.join("l"), base.join("wd"));
        std::fs::remove_dir_all(&base).ok();
        for d in [&r, &l] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cfg = Config {
            rc_addr: std::env::var("RCS_IT_RC").unwrap_or("127.0.0.1:5611".into()),
            env_file: "/nonexistent".into(),
            ..Config::default()
        };
        let remote = r.to_string_lossy().into_owned();
        let run = |resync: bool| bisync_in(&cfg, &remote, &l, &wd, resync);

        std::fs::write(r.join("big.bin"), vec![7u8; 200_000]).unwrap();
        for i in 1..=4 {
            std::fs::write(r.join(format!("s{i}.txt")), format!("{i}")).unwrap();
        }
        run(true).unwrap();

        // A rename in the cloud arrives as a move, not a second copy of the data.
        let inode = l.join("big.bin").metadata().unwrap().ino();
        std::fs::rename(r.join("big.bin"), r.join("big2.bin")).unwrap();
        run(false).unwrap();
        assert_eq!(l.join("big2.bin").metadata().unwrap().ino(), inode);
        assert!(!l.join("big.bin").exists());

        // ...and one made here becomes a move in the cloud.
        let inode = r.join("big2.bin").metadata().unwrap().ino();
        std::fs::rename(l.join("big2.bin"), l.join("big3.bin")).unwrap();
        run(false).unwrap();
        assert_eq!(r.join("big3.bin").metadata().unwrap().ino(), inode);

        // Debris from a killed download never goes up.
        std::fs::write(l.join("x.bin.1a2b3c4d.partial"), "half").unwrap();
        run(false).unwrap();
        assert!(!r.join("x.bin.1a2b3c4d.partial").exists());

        // 3 of the 5 files deleted is over bisync's 50%, but a few files are always allowed.
        for f in ["s1.txt", "s2.txt", "s3.txt"] {
            std::fs::remove_file(l.join(f)).unwrap();
        }
        run(false).unwrap();
        assert!(!r.join("s1.txt").exists() && r.join("s4.txt").exists());

        // Deleting more than that from a bigger folder is not.
        for i in 10..18 {
            std::fs::write(l.join(format!("m{i}.txt")), "m").unwrap();
        }
        run(false).unwrap();
        // 6 of the 10 files (a bare 50% would still be allowed).
        for i in 10..15 {
            std::fs::remove_file(l.join(format!("m{i}.txt"))).unwrap();
        }
        std::fs::remove_file(l.join("s4.txt")).unwrap();
        let err = run(false).unwrap_err();
        assert!(err.contains("too many deletes"), "{err}");
        assert!(r.join("m10.txt").exists(), "nothing was deleted");
        for i in 10..15 {
            std::fs::write(l.join(format!("m{i}.txt")), "m").unwrap();
        }
        std::fs::write(l.join("s4.txt"), "4").unwrap();
        run(false).unwrap();

        // Emptying a side entirely is refused outright.
        for e in std::fs::read_dir(&l).unwrap().flatten() {
            std::fs::remove_file(e.path()).unwrap();
        }
        let err = run(false).unwrap_err();
        assert!(matches!(classify(&err), Failure::Attention(_)), "{err}");
        assert!(r.join("m10.txt").exists(), "nothing was deleted");
        // Repair brings everything back.
        run(true).unwrap();
        assert!(l.join("m10.txt").exists());

        // Failures arrive with a reason `classify` understands, without colour codes.
        let wd2 = base.join("wd2");
        let err = bisync_in(&cfg, &remote, &l, &wd2, false).unwrap_err();
        assert_eq!(classify(&err), Failure::NeedsResync, "{err}");
        // A cloud folder that vanished stops the sync. (A *resync* would recreate it from the
        // local copy, which is why the keeper never resyncs a missing folder by itself.)
        let away = base.join("r.away");
        std::fs::rename(&r, &away).unwrap();
        let err = run(false).unwrap_err();
        assert!(matches!(classify(&err), Failure::Attention(_)), "{err}");
        assert!(!err.contains('\u{1b}'));
        assert!(!r.exists(), "nothing was recreated");
        std::fs::rename(&away, &r).unwrap();
    }

    /// A listing file in bisync's format.
    fn listing(entries: &[(&str, &str)]) -> String {
        let head = "# bisync listing v1 from 2026-11-04T05:13:09+0000\n".to_string();
        entries.iter().fold(head, |s, (path, hash)| {
            s + &format!("- 5 md5:{hash} - 2026-10-30T02:46:00.190000000+0000 \"{path}\"\n")
        })
    }

    fn write_session(wd: &Path, files: [(&str, String); 4]) {
        std::fs::create_dir_all(wd).unwrap();
        let name = "gdrive{X}_Games..home_u_.local_share_x_offline_abc";
        for (suffix, text) in files {
            std::fs::write(wd.join(format!("{name}{suffix}")), text).unwrap();
        }
    }

    const LOCAL: &str = "/home/u/.local/share/x/offline/abc";

    #[test]
    fn listings_say_what_each_side_deleted() {
        let wd = std::env::temp_dir().join(format!("rcs-lst-{}", std::process::id()));
        let all = [("a", "1"), ("b", "1"), ("sub/c d.txt", "1"), ("e", "1")];
        write_session(
            &wd,
            [
                (".path1.lst", listing(&all)),
                // The cloud lost "a"; "e" changed there.
                (
                    ".path1.lst-new",
                    listing(&[("b", "1"), ("sub/c d.txt", "1"), ("e", "2")]),
                ),
                (".path2.lst", listing(&all)),
                // This side lost "sub/c d.txt" and "e" (but "e" changed in the cloud, so bisync
                // brings it back rather than deleting it there).
                (".path2.lst-new", listing(&[("a", "1"), ("b", "1")])),
            ],
        );
        let p = Pending::read(&wd, Path::new(LOCAL)).unwrap();
        assert_eq!(p.gone_remote, ["a"]);
        assert_eq!(p.gone_local, ["sub/c d.txt"]);
        assert_eq!((p.had_remote, p.had_local), (4, 4));
        assert!(p.within_limit());
        assert_eq!(Pending::read(&wd, Path::new("/elsewhere")), None);
        std::fs::remove_dir_all(wd).ok();
    }

    #[test]
    fn classifies_what_each_side_changed() {
        let files = |e: &[(&str, i64, i64)]| -> Files {
            e.iter()
                .map(|(p, s, t)| ((*p).to_string(), (*s, *t)))
                .collect()
        };
        // As of the last sync.
        let synced = files(&[("a", 1, 100), ("b", 1, 100), ("c", 1, 100), ("d", 1, 100)]);
        // Here, before this sync: "a" edited, "b" deleted, "n" new.
        let before = files(&[("a", 2, 200), ("c", 1, 100), ("d", 1, 100), ("n", 1, 300)]);
        // After: the cloud's "c" came down changed, "d" was deleted in the cloud, "m" is new
        // from the cloud, and "x" was in conflict.
        let mut after = files(&[("a", 2, 200), ("c", 3, 400), ("n", 1, 300), ("m", 5, 500)]);
        after.insert("x..path1".into(), (1, 1));
        after.insert("x..path2".into(), (1, 1));
        let got = classify_changes(&synced, &before, &after);
        let want = [
            (ChangeKind::Updated, Direction::Upload, "a", 2),
            (ChangeKind::Deleted, Direction::Upload, "b", -1),
            (ChangeKind::Updated, Direction::Download, "c", 3),
            (ChangeKind::Deleted, Direction::Download, "d", -1),
            (ChangeKind::Created, Direction::Download, "m", 5),
            (ChangeKind::Created, Direction::Upload, "n", 1),
        ];
        let want: Vec<_> = want
            .iter()
            .map(|(k, d, p, s)| (*k, *d, (*p).to_string(), *s))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn reads_size_and_time_from_listings() {
        let wd = std::env::temp_dir().join(format!("rcs-lstf-{}", std::process::id()));
        std::fs::create_dir_all(&wd).unwrap();
        let f = wd.join("x.path2.lst");
        std::fs::write(
            &f,
            "# bisync listing v1 from 2026-11-04T05:13:09+0000\n\
             -  4637802 md5:0d74 - 2026-10-30T02:46:00.251000000+0000 \"One Page/a b.pdf\"\n",
        )
        .unwrap();
        let got = listing_files(&f).unwrap();
        let want = jiff::Timestamp::from_second(1_793_328_360)
            .unwrap()
            .as_millisecond()
            + 251;
        assert_eq!(got["One Page/a b.pdf"], (4_637_802, want));
        std::fs::remove_dir_all(wd).ok();
    }

    #[test]
    fn delete_limit_is_half_the_folder_but_never_below_a_few_files() {
        assert!(deletes_ok(3, 1068), "the 3 GCS files in 1068");
        assert!(deletes_ok(3, 3), "a few files are always fine");
        assert!(deletes_ok(500, 1000));
        assert!(!deletes_ok(501, 1000));
        assert!(!deletes_ok(10, 10), "emptying a folder of 10 needs a look");
    }

    #[test]
    fn colour_codes_are_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[31mprior \u{1b}[93m/x.lck\u{1b}[0m \n"),
            "prior /x.lck \n"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn a_bare_abort_takes_its_reason_from_the_output() {
        let st = json!({
            "error": "bisync aborted",
            "output": { "output": "2026/11/04 ERROR : Bisync critical error: cannot find prior \
                Path1 or Path2 listings, likely due to critical error on prior run \nTip: x\n\
                2026/11/04 ERROR : Bisync aborted. Must run --resync to recover.\n" }
        });
        let text = failure_text(&st);
        assert!(
            text.starts_with("cannot find prior Path1 or Path2 listings"),
            "{text}"
        );
        assert_eq!(classify(&text), Failure::NeedsResync);

        let st = json!({
            "error": "bisync aborted",
            "output": { "output": "ERROR : Local file system at /x: error reading source root \
                directory: directory not found\nERROR : Bisync critical error: directory not \
                found\nERROR : Bisync aborted. Must run --resync to recover.\n" }
        });
        assert!(matches!(
            classify(&failure_text(&st)),
            Failure::Attention(_)
        ));

        // Specific errors keep their own text, colours removed.
        let st = json!({ "error": "\u{1b}[31mtoo many deletes\u{1b}[0m" });
        assert_eq!(failure_text(&st), "too many deletes");
        // And with nothing to go on, the bare text is kept.
        assert_eq!(
            failure_text(&json!({ "error": "bisync aborted" })),
            "bisync aborted"
        );
    }

    #[test]
    fn a_coloured_lock_message_yields_just_the_path() {
        let st = json!({ "error": "\u{1b}[31mprior lock file found: \u{1b}[93m/s/bisync/a..b.lck\u{1b}[0m \n\
            \u{1b}[0m\u{1b}[35mTip: this indicates that another bisync run\n" });
        assert_eq!(
            classify(&failure_text(&st)),
            Failure::StaleLock(PathBuf::from("/s/bisync/a..b.lck"))
        );
    }

    #[test]
    fn looking_up_a_cloud_folder_tells_missing_from_failed() {
        use crate::testutil::{ok, status, FakeServer};
        let cfg = |s: &FakeServer| Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent".into(),
            ..Config::default()
        };
        let drive = || ok("/config/get", r#"{"type":"drive"}"#);
        let s = FakeServer::start(vec![
            drive(),
            ok("/operations/stat", r#"{"item":{"ID":"F1","IsDir":true}}"#),
        ]);
        assert_eq!(
            cloud_folder_id(&cfg(&s), "gd:", "Work"),
            Lookup::Id("F1".into())
        );
        let s = FakeServer::start(vec![drive(), ok("/operations/stat", r#"{"item":null}"#)]);
        assert_eq!(cloud_folder_id(&cfg(&s), "gd:", "Work"), Lookup::Missing);
        let s = FakeServer::start(vec![
            drive(),
            ok("/operations/stat", r#"{"item":{"ID":"F2","IsDir":false}}"#),
        ]);
        assert_eq!(cloud_folder_id(&cfg(&s), "gd:", "Work"), Lookup::Missing);
        let s = FakeServer::start(vec![ok("/config/get", r#"{"type":"s3"}"#)]);
        assert_eq!(cloud_folder_id(&cfg(&s), "s3:", "Work"), Lookup::NotDrive);
        let s = FakeServer::start(vec![drive(), status("/operations/stat", 500)]);
        assert!(matches!(
            cloud_folder_id(&cfg(&s), "gd:", "Work"),
            Lookup::Failed(_)
        ));
    }

    #[test]
    fn a_replaced_cloud_folder_needs_attention() {
        assert!(matches!(classify(FOLDER_REPLACED), Failure::Attention(_)));
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
        let wd = std::env::temp_dir().join(format!("rcs-job-{}", std::process::id()));
        let err = bisync_in(
            &cfg,
            "gdrive,skip_gdocs=true:Work",
            Path::new("/l"),
            &wd,
            true,
        )
        .unwrap_err();
        assert_eq!(err, "bisync aborted");
        let reqs = s.requests();
        let start = reqs.iter().find(|r| r.path() == "/sync/bisync").unwrap();
        let body: Value = serde_json::from_str(&start.body).unwrap();
        assert_eq!(body["path1"], "gdrive,skip_gdocs=true:Work");
        assert_eq!(body["path2"], "/l");
        assert!(body.get("checkSync").is_none(), "no longer needed");
        assert_eq!(body["resync"], true);
        assert_eq!(body["resyncMode"], "newer");
        assert_eq!(body["maxDelete"], 50);
        assert_eq!(body["resilient"], true);
        assert_eq!(body["recover"], true);
        assert_eq!(body["maxLock"], "2m");
        assert_eq!(body["conflictLoser"], "pathname");
        assert_eq!(body["conflictSuffix"], ".path");
        assert_eq!(body["_config"]["TrackRenames"], true);
        assert_eq!(body["_async"], true);
        assert_eq!(
            std::fs::read_to_string(body["filtersFile"].as_str().unwrap()).unwrap(),
            "- *.partial\n"
        );
        std::fs::remove_dir_all(wd).ok();
        let status = reqs.iter().rfind(|r| r.path() == "/job/status").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&status.body).unwrap()["jobid"],
            7
        );
    }

    #[test]
    fn a_job_still_running_in_rclone_blocks_a_second_sync() {
        use crate::testutil::{ok, FakeServer};
        let s = FakeServer::start(vec![
            ok("/job/list", r#"{"jobids":[3]}"#),
            ok("/job/status", r#"{"finished":false}"#),
        ]);
        let cfg = Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent".into(),
            ..Config::default()
        };
        assert_eq!(running_jobs(&cfg), 1);
        let err = bisync(&cfg, "r:x", Path::new("/l"), false).unwrap_err();
        assert_eq!(err, EARLIER_SYNC_RUNNING);
        assert!(!s.paths().contains(&"/sync/bisync".to_string()));
    }
}
