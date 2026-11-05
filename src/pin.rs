// SPDX-License-Identifier: GPL-3.0-only
//! Offline folders ("pins"): where their pieces live, what is allowed to be pinned, the state the
//! keeper shares with the UI, and the plan the keeper follows each cycle.
//!
//! A pinned folder `Work/Taxes` has:
//! - a local copy at `<data>/offline/<id>`, kept in two-way sync with the remote by `rclone
//!   bisync` (run inside the main mount's rclone process through its rc API);
//! - a mount of that copy on top of `<mount>/Work/Taxes`, so it shows up in place.
//!
//! `<id>` is a short hash of the path. bisync names its lock and listing files after both full
//! paths and rclone 1.60 can't cope when that name is longer than a file name may be, so the
//! local side is kept short.

use crate::config::{expand, Config};
use crate::rc::{ChangeKind, Direction};
use crate::share;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// bisync's session name (both paths, `/` turned into `_`) must fit in a file name, along with
/// suffixes like `.path1.lst-new`.
const SESSION_NAME_MAX: usize = 230;
/// After a local change, wait this long for more before syncing.
pub const LOCAL_DEBOUNCE_SECS: i64 = 10;
/// First retry delay after a failed sync; doubles per failure, capped at the sync interval.
const RETRY_BASE_SECS: i64 = 30;

/// The folder name used for this build's data and state (the dev build keeps its own).
pub fn app_dir_name() -> &'static str {
    if crate::APP_ID.ends_with("Dev") {
        "cosmic-rclone-ui-dev"
    } else {
        "cosmic-rclone-ui"
    }
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(expand(fallback)), PathBuf::from)
        .join(app_dir_name())
}

/// `$XDG_DATA_HOME/cosmic-rclone-ui`: the local copies live under here.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", "~/.local/share")
}

/// `$XDG_STATE_HOME/cosmic-rclone-ui`: bisync's working files and `offline.json`.
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", "~/.local/state")
}

pub fn state_file() -> PathBuf {
    state_dir().join("offline.json")
}

pub fn workdir() -> PathBuf {
    state_dir().join("bisync")
}

/// A stable 16-hex-digit name for a pinned path (FNV-1a, which unlike `DefaultHasher` never
/// changes between Rust releases).
pub fn pin_id(rel: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rel.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Where the local copy of `rel` lives.
pub fn local_dir(rel: &str) -> PathBuf {
    data_dir().join("offline").join(pin_id(rel))
}

/// Where `rel` shows up inside the mount.
pub fn mount_point(cfg: &Config, rel: &str) -> PathBuf {
    Path::new(&expand(&cfg.mount_path)).join(rel)
}

/// The remote side of `rel` for bisync, given the mounted remote `fs` (e.g. `gdrive:` or
/// `gdrive:sub`). Native Google Docs are skipped: rclone can only download them as exported
/// copies, which can't be synced back.
pub fn remote(fs: &str, rel: &str) -> String {
    let (name, root) = fs.split_once(':').unwrap_or((fs, ""));
    let path = [root.trim_matches('/'), rel]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    format!("{name},skip_gdocs=true:{path}")
}

/// The pinned folder that `rel` is, or is inside of.
pub fn covering_pin<'a>(pins: &'a [String], rel: &str) -> Option<&'a str> {
    pins.iter()
        .map(String::as_str)
        .find(|p| rel == *p || rel.starts_with(&format!("{p}/")))
}

/// Check that `path` (an absolute path) can be pinned and return it relative to the mount.
pub fn normalize(
    mount: &str,
    path: &Path,
    is_dir: bool,
    pins: &[String],
    fs: &str,
) -> Result<String, String> {
    let rel = share::remote_path(&expand(mount), path)
        .ok_or_else(|| "Only items inside the Drive folder can be kept offline".to_string())?;
    if rel.is_empty() {
        return Err("The whole Drive can't be kept offline; pick a folder inside it".into());
    }
    if !is_dir {
        return Err("Only folders can be kept offline".into());
    }
    if let Some(p) = covering_pin(pins, &rel) {
        return Err(if p == rel {
            "Already kept offline".into()
        } else {
            format!("Already kept offline as part of {p}")
        });
    }
    if let Some(p) = pins.iter().find(|p| p.starts_with(&format!("{rel}/"))) {
        return Err(format!(
            "Contains {p}, which is already kept offline; remove that first"
        ));
    }
    let session = session_name(&remote(fs, &rel), &local_dir(&rel).to_string_lossy());
    if session.len() > SESSION_NAME_MAX {
        return Err("The folder's path is too long for rclone bisync".into());
    }
    Ok(rel)
}

/// Roughly how bisync names its working files: both paths with separators flattened.
fn session_name(path1: &str, path2: &str) -> String {
    let flat = |p: &str| p.trim_start_matches('/').replace(['/', ':'], "_");
    format!("{}..{}", flat(path1), flat(path2))
}

// ---- state shared between the keeper and the UI (`offline.json`) ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// First download in progress; the folder isn't mounted in place yet.
    #[default]
    Downloading,
    /// Synced at `last_sync`.
    Ready,
    /// Last sync failed for a reason that may fix itself (offline, mount down); retrying.
    Waiting,
    /// Stopped until someone looks: bisync refused for safety, or the folder is gone remotely.
    Attention,
    /// Unpinned; pushing last changes and cleaning up.
    Removing,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Downloading => "Downloading for offline use",
            Phase::Ready => "Available offline",
            Phase::Waiting => "Waiting to sync",
            Phase::Attention => "Needs attention",
            Phase::Removing => "Removing offline copy",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Phase::Downloading => "folder-download-symbolic",
            Phase::Ready => "emblem-ok-symbolic",
            Phase::Waiting => "network-offline-symbolic",
            Phase::Attention => "dialog-warning-symbolic",
            Phase::Removing => "edit-delete-symbolic",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PinState {
    pub rel: String,
    pub phase: Phase,
    /// The first full sync has completed, so bisync has listings to compare against.
    pub initialized: bool,
    /// Unix seconds of the last successful sync.
    pub last_sync: Option<i64>,
    /// Unix seconds of the last attempt, successful or not.
    pub last_attempt: i64,
    /// Failed attempts in a row.
    pub failures: u32,
    pub error: Option<String>,
    /// A sync is running right now.
    pub syncing: bool,
}

/// How much of a folder's local copy is on disk: (files, bytes).
pub fn local_usage(rel: &str) -> (u64, u64) {
    fn walk(dir: &Path, acc: &mut (u64, u64)) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&e.path(), acc);
            } else {
                acc.0 += 1;
                acc.1 += md.len();
            }
        }
    }
    let mut acc = (0, 0);
    walk(&local_dir(rel), &mut acc);
    acc
}

/// One line describing a pinned folder's state for the UI, e.g. "Downloading for offline use ·
/// 12 files (3.0 MiB) so far" or "Syncing…". `usage` is from [`local_usage`], when measured.
pub fn describe(p: Option<&PinState>, usage: Option<(u64, u64)>) -> String {
    let Some(p) = p else {
        return "Waiting to start…".into();
    };
    let files = |(n, b): (u64, u64)| {
        let s = if n == 1 { "" } else { "s" };
        format!("{n} file{s} ({})", crate::status::human_bytes(b))
    };
    let mut line = match (p.phase, p.syncing) {
        (Phase::Downloading, _) => match usage {
            Some(u) if u.0 > 0 => format!("{} · {} so far", p.phase.label(), files(u)),
            _ => p.phase.label().to_string(),
        },
        (Phase::Ready | Phase::Waiting, true) => "Syncing…".to_string(),
        (Phase::Ready, false) => match usage {
            Some(u) => format!("{} · {}", p.phase.label(), files(u)),
            None => p.phase.label().to_string(),
        },
        (phase, _) => phase.label().to_string(),
    };
    // While a sync runs, the error is the last attempt's and may no longer apply.
    if let Some(e) = p.error.as_deref().filter(|_| !p.syncing) {
        line = format!("{line}: {e}");
    }
    line
}

/// Something the user should hear about once, e.g. a resolved conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notice {
    pub at: i64,
    pub rel: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OfflineState {
    /// Unix seconds when the keeper last wrote this.
    pub updated: i64,
    pub pins: Vec<PinState>,
    /// Newest last; trimmed to [`NOTICES_KEPT`].
    pub notices: Vec<Notice>,
    /// What offline-folder syncs did, newest last; trimmed to [`ACTIVITY_KEPT`]. rclone's own
    /// history can't say whether a file was new, changed or deleted, or which way it went.
    pub changes: Vec<Change>,
}

pub const NOTICES_KEPT: usize = 20;
pub const ACTIVITY_KEPT: usize = 100;

/// One file a sync of an offline folder created, updated or deleted on the other side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Unix seconds when the sync applied it.
    pub at: i64,
    /// Path relative to the mount root, e.g. `Work/Taxes/2024.pdf`.
    pub path: String,
    /// Bytes, or `-1` for a deletion.
    pub size: i64,
    pub kind: ChangeKind,
    /// `Upload`: changed on this computer and sent to the cloud; `Download`: the reverse.
    pub direction: Direction,
}

impl OfflineState {
    pub fn record_change(&mut self, change: Change) {
        self.changes.push(change);
        let extra = self.changes.len().saturating_sub(ACTIVITY_KEPT);
        self.changes.drain(..extra);
    }

    pub fn pin(&self, rel: &str) -> Option<&PinState> {
        self.pins.iter().find(|p| p.rel == rel)
    }

    pub fn pin_mut(&mut self, rel: &str) -> Option<&mut PinState> {
        self.pins.iter_mut().find(|p| p.rel == rel)
    }

    pub fn notice(&mut self, at: i64, rel: &str, text: String) {
        self.notices.push(Notice {
            at,
            rel: rel.to_string(),
            text,
        });
        let extra = self.notices.len().saturating_sub(NOTICES_KEPT);
        self.notices.drain(..extra);
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn load() -> Self {
        Self::load_from(&state_file())
    }

    /// Write atomically (temp file + rename) so readers never see half a file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }
}

/// One-line summary for the panel popup, e.g. "3 folders · all available offline".
pub fn summary(pins: &[String], state: &OfflineState) -> String {
    let n = pins.len();
    let folders = if n == 1 {
        "1 folder".to_string()
    } else {
        format!("{n} folders")
    };
    let worst = pins
        .iter()
        .map(|r| state.pin(r).map_or(Phase::Downloading, |p| p.phase))
        .max_by_key(|p| match p {
            Phase::Ready => 0,
            Phase::Removing => 1,
            Phase::Downloading => 2,
            Phase::Waiting => 3,
            Phase::Attention => 4,
        });
    match worst {
        None => "No folders kept offline".into(),
        Some(Phase::Ready) => format!("{folders} · all available offline"),
        Some(p) => format!("{folders} · {}", p.label().to_lowercase()),
    }
}

// ---- the keeper's plan ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create the local copy and do the first full download (bisync `--resync`).
    InitialSync(String),
    /// Mount the local copy in place.
    StartMount(String),
    /// A normal two-way sync.
    Sync(String),
    /// Re-establish bisync's baseline after it refused to continue (the user asked for it).
    Repair(String),
    /// Unpinned: push last changes, unmount, delete the local copy.
    Remove(String),
}

/// What the keeper knows at the start of a cycle besides config and state.
pub struct Facts<'a> {
    pub now: i64,
    pub interval: i64,
    /// The main mount and its rc API are up; nothing can be synced otherwise.
    pub online: bool,
    /// Pins whose in-place mount is active.
    pub mounted: &'a HashSet<String>,
    /// Pins whose local copy changed since their last attempt.
    pub changed: &'a HashSet<String>,
    /// Pins the user asked to repair.
    pub repair: &'a HashSet<String>,
}

fn retry_delay(failures: u32, interval: i64) -> i64 {
    if failures == 0 {
        interval
    } else {
        (RETRY_BASE_SECS << failures.min(6).saturating_sub(1)).min(interval)
    }
}

/// Decide what to do this cycle. Idempotent: running it again before acting yields the same
/// steps, so a crash between cycles loses nothing.
pub fn plan(wanted: &[String], state: &OfflineState, f: &Facts) -> Vec<Step> {
    let mut steps = vec![];
    for rel in wanted {
        let Some(p) = state.pin(rel) else {
            if f.online {
                steps.push(Step::InitialSync(rel.clone()));
            }
            continue;
        };
        // A local copy that has been fully downloaded is usable even with no network.
        if p.initialized && !f.mounted.contains(rel) {
            steps.push(Step::StartMount(rel.clone()));
        }
        if !f.online {
            continue;
        }
        if p.phase == Phase::Attention {
            if f.repair.contains(rel) {
                steps.push(Step::Repair(rel.clone()));
            }
            continue;
        }
        if !p.initialized {
            if f.now - p.last_attempt >= retry_delay(p.failures, f.interval) || p.failures == 0 {
                steps.push(Step::InitialSync(rel.clone()));
            }
            continue;
        }
        let since = f.now - p.last_attempt;
        let due = since >= retry_delay(p.failures, f.interval);
        let changed = f.changed.contains(rel) && since >= LOCAL_DEBOUNCE_SECS;
        if due || changed {
            steps.push(Step::Sync(rel.clone()));
        }
    }
    for p in &state.pins {
        if !wanted.contains(&p.rel) {
            steps.push(Step::Remove(p.rel.clone()));
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "/home/u/Drive";

    fn norm(path: &str, is_dir: bool, pins: &[&str]) -> Result<String, String> {
        let pins: Vec<String> = pins.iter().map(|s| (*s).to_string()).collect();
        normalize(M, Path::new(path), is_dir, &pins, "gdrive:")
    }

    #[test]
    fn normalizes_folders_inside_the_mount() {
        assert_eq!(
            norm("/home/u/Drive/Work/Taxes", true, &[]).unwrap(),
            "Work/Taxes"
        );
        assert_eq!(
            norm("/home/u/Drive/Work/Taxes/", true, &[]).unwrap(),
            "Work/Taxes"
        );
        assert_eq!(
            norm("/home/u/Drive/My Stuff", true, &[]).unwrap(),
            "My Stuff"
        );
    }

    #[test]
    fn rejects_root_outside_files_and_climbing() {
        assert!(norm("/home/u/Drive", true, &[])
            .unwrap_err()
            .contains("whole Drive"));
        assert!(norm("/home/u/Other", true, &[])
            .unwrap_err()
            .contains("inside the Drive"));
        assert!(norm("/home/u/Drive/../x", true, &[]).is_err());
        assert!(norm("/home/u/Drive/a.txt", false, &[])
            .unwrap_err()
            .contains("folders"));
    }

    #[test]
    fn rejects_overlapping_pins() {
        assert_eq!(
            norm("/home/u/Drive/Work", true, &["Work"]).unwrap_err(),
            "Already kept offline"
        );
        assert!(norm("/home/u/Drive/Work/Taxes", true, &["Work"])
            .unwrap_err()
            .contains("part of Work"));
        assert!(norm("/home/u/Drive/Work", true, &["Work/Taxes"])
            .unwrap_err()
            .contains("Contains Work/Taxes"));
        // A shared prefix that isn't a parent is fine.
        assert_eq!(
            norm("/home/u/Drive/Workshop", true, &["Work"]).unwrap(),
            "Workshop"
        );
    }

    #[test]
    fn rejects_paths_too_long_for_bisync() {
        let long = format!("/home/u/Drive/{}", "x".repeat(200));
        assert!(norm(&long, true, &[]).unwrap_err().contains("too long"));
    }

    #[test]
    fn covering_pin_matches_self_and_children_only() {
        let pins = vec!["Work".to_string()];
        assert_eq!(covering_pin(&pins, "Work"), Some("Work"));
        assert_eq!(covering_pin(&pins, "Work/a/b"), Some("Work"));
        assert_eq!(covering_pin(&pins, "Workshop"), None);
    }

    #[test]
    fn pin_ids_are_stable_and_short() {
        assert_eq!(pin_id(""), "cbf29ce484222325");
        assert_eq!(pin_id("Work/Taxes").len(), 16);
        assert_ne!(pin_id("a"), pin_id("b"));
    }

    #[test]
    fn remote_skips_google_docs_and_keeps_the_mount_root() {
        assert_eq!(remote("gdrive:", "Work"), "gdrive,skip_gdocs=true:Work");
        assert_eq!(
            remote("gdrive:sub/", "Work"),
            "gdrive,skip_gdocs=true:sub/Work"
        );
    }

    #[test]
    fn state_round_trips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("rcs-pinstate-{}", std::process::id()));
        let f = dir.join("offline.json");
        let mut s = OfflineState::default();
        s.pins.push(PinState {
            rel: "Work".into(),
            phase: Phase::Ready,
            initialized: true,
            last_sync: Some(5),
            ..PinState::default()
        });
        s.notice(1, "Work", "hi".into());
        s.save_to(&f).unwrap();
        assert_eq!(OfflineState::load_from(&f), s);
        std::fs::write(&f, "not json").unwrap();
        assert_eq!(OfflineState::load_from(&f), OfflineState::default());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn notices_are_capped() {
        let mut s = OfflineState::default();
        for i in 0..(NOTICES_KEPT + 5) {
            s.notice(i64::try_from(i).unwrap(), "x", format!("{i}"));
        }
        assert_eq!(s.notices.len(), NOTICES_KEPT);
        assert_eq!(s.notices[0].text, "5");
    }

    #[test]
    fn describe_follows_the_keeper() {
        assert_eq!(describe(None, None), "Waiting to start…");
        let mut p = PinState {
            rel: "a".into(),
            ..PinState::default()
        };
        assert_eq!(describe(Some(&p), None), "Downloading for offline use");
        assert_eq!(
            describe(Some(&p), Some((12, 3 * 1024 * 1024))),
            "Downloading for offline use · 12 files (3.0 MiB) so far"
        );
        p.phase = Phase::Ready;
        p.syncing = true;
        assert_eq!(describe(Some(&p), Some((1, 5))), "Syncing…");
        p.syncing = false;
        assert_eq!(
            describe(Some(&p), Some((1, 5))),
            "Available offline · 1 file (5.0 B)"
        );
        p.phase = Phase::Waiting;
        p.error = Some("no network".into());
        assert_eq!(describe(Some(&p), None), "Waiting to sync: no network");
    }

    #[test]
    fn summary_reports_the_worst_state() {
        let pins = vec!["a".to_string(), "b".to_string()];
        let mut s = OfflineState::default();
        assert_eq!(summary(&[], &s), "No folders kept offline");
        for r in &pins {
            s.pins.push(PinState {
                rel: r.clone(),
                phase: Phase::Ready,
                ..PinState::default()
            });
        }
        assert_eq!(summary(&pins, &s), "2 folders · all available offline");
        s.pin_mut("b").unwrap().phase = Phase::Attention;
        assert_eq!(summary(&pins, &s), "2 folders · needs attention");
    }

    // ---- plan ----

    struct World {
        mounted: HashSet<String>,
        changed: HashSet<String>,
        repair: HashSet<String>,
    }

    impl World {
        fn new() -> Self {
            Self {
                mounted: HashSet::new(),
                changed: HashSet::new(),
                repair: HashSet::new(),
            }
        }
        fn facts(&self, now: i64, online: bool) -> Facts<'_> {
            Facts {
                now,
                interval: 300,
                online,
                mounted: &self.mounted,
                changed: &self.changed,
                repair: &self.repair,
            }
        }
    }

    fn ready(rel: &str, last: i64) -> PinState {
        PinState {
            rel: rel.into(),
            phase: Phase::Ready,
            initialized: true,
            last_sync: Some(last),
            last_attempt: last,
            ..PinState::default()
        }
    }

    fn state(pins: Vec<PinState>) -> OfflineState {
        OfflineState {
            pins,
            ..OfflineState::default()
        }
    }

    fn wanted(r: &[&str]) -> Vec<String> {
        r.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn new_pin_downloads_first_and_only_online() {
        let w = World::new();
        let s = OfflineState::default();
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(0, true)),
            [Step::InitialSync("a".into())]
        );
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(0, false)),
            Vec::<Step>::new()
        );
    }

    #[test]
    fn half_downloaded_pin_is_not_mounted() {
        let w = World::new();
        let s = state(vec![PinState {
            rel: "a".into(),
            last_attempt: 0,
            ..PinState::default()
        }]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1, true)),
            [Step::InitialSync("a".into())]
        );
    }

    #[test]
    fn downloaded_pin_is_mounted_even_offline() {
        let w = World::new();
        let s = state(vec![ready("a", 0)]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(10, false)),
            [Step::StartMount("a".into())]
        );
    }

    #[test]
    fn steady_state_syncs_on_interval_or_local_change() {
        let mut w = World::new();
        w.mounted.insert("a".into());
        let s = state(vec![ready("a", 1000)]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1100, true)),
            Vec::<Step>::new()
        );
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1300, true)),
            [Step::Sync("a".into())]
        );
        w.changed.insert("a".into());
        assert!(
            plan(&wanted(&["a"]), &s, &w.facts(1005, true)).is_empty(),
            "debounced"
        );
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1010, true)),
            [Step::Sync("a".into())]
        );
    }

    #[test]
    fn failures_retry_sooner_than_the_interval_with_backoff() {
        let mut w = World::new();
        w.mounted.insert("a".into());
        let mut p = ready("a", 1000);
        p.phase = Phase::Waiting;
        p.failures = 1;
        let s = state(vec![p.clone()]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1029, true)),
            Vec::<Step>::new()
        );
        assert_eq!(plan(&wanted(&["a"]), &s, &w.facts(1030, true)).len(), 1);
        p.failures = 3; // 120s
        let s = state(vec![p]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(1100, true)),
            Vec::<Step>::new()
        );
        assert_eq!(plan(&wanted(&["a"]), &s, &w.facts(1120, true)).len(), 1);
    }

    #[test]
    fn attention_waits_for_a_repair_request() {
        let mut w = World::new();
        w.mounted.insert("a".into());
        let mut p = ready("a", 0);
        p.phase = Phase::Attention;
        let s = state(vec![p]);
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(10_000, true)),
            Vec::<Step>::new()
        );
        w.repair.insert("a".into());
        assert_eq!(
            plan(&wanted(&["a"]), &s, &w.facts(10_000, true)),
            [Step::Repair("a".into())]
        );
    }

    #[test]
    fn unpinned_folders_are_removed_even_offline() {
        // Removal itself checks it can push changes first; planning it is always right.
        let w = World::new();
        let s = state(vec![ready("gone", 0)]);
        assert_eq!(
            plan(&[], &s, &w.facts(1, false)),
            [Step::Remove("gone".into())]
        );
    }
}
