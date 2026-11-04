// SPDX-License-Identifier: GPL-3.0-only
//! Per-item sync status for the "rclone…" window: what rclone knows locally (its transfer list
//! and VFS cache) combined with what Google Drive says (see `drive.rs`).
//!
//! rclone's VFS cache keeps one metadata file per cached item under
//! `<cache-dir>/vfsMeta/<remote>/<path>` (JSON with `ModTime`, `Size`, `Rs` = cached byte ranges
//! and `Dirty` = not yet uploaded) and the data under `<cache-dir>/vfs/...`. We read that
//! metadata directly; rclone offers no per-file query for it.

use crate::config::{expand, Config};
use crate::drive::DriveInfo;
use crate::rc;
use crate::share::Target;
use jiff::{tz::TimeZone, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How many cache entries a folder summary will look at before giving up and saying "at least".
const FOLDER_SCAN_CAP: usize = 20_000;
/// Drive and the cache can disagree by rounding; ignore differences below this.
const MTIME_SLACK_SECS: i64 = 2;

/// The sync state of a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// Being sent to Drive right now.
    Uploading,
    /// Changed locally, waiting its turn to upload.
    QueuedUpload,
    /// Being fetched from Drive right now.
    Downloading,
    /// The cached copy matches Drive.
    InSync,
    /// Only on Drive; nothing cached locally.
    OnlineOnly,
    /// Some of the file is cached.
    PartlyCached,
    /// The cached copy is newer than Drive's.
    LocalNewer,
    /// Drive has a newer version than the cached copy.
    DriveNewer,
    /// Same time but a different size.
    Differs,
}

impl SyncState {
    pub fn label(self) -> &'static str {
        match self {
            SyncState::Uploading => "Uploading",
            SyncState::QueuedUpload => "Queued for upload",
            SyncState::Downloading => "Downloading",
            SyncState::InSync => "In sync (cached)",
            SyncState::OnlineOnly => "Online only (not cached)",
            SyncState::PartlyCached => "Partly downloaded",
            SyncState::LocalNewer => "Local changes pending",
            SyncState::DriveNewer => "Drive has a newer version",
            SyncState::Differs => "Differs from Drive",
        }
    }

    /// A symbolic icon name; the label carries the meaning, so colour is never the only cue.
    pub fn icon(self) -> &'static str {
        match self {
            SyncState::Uploading => "network-transmit-symbolic",
            SyncState::QueuedUpload => "document-open-recent-symbolic",
            SyncState::Downloading => "network-receive-symbolic",
            SyncState::InSync => "emblem-ok-symbolic",
            SyncState::OnlineOnly => "weather-overcast-symbolic",
            SyncState::PartlyCached => "folder-download-symbolic",
            SyncState::LocalNewer => "emblem-synchronizing-symbolic",
            SyncState::DriveNewer => "software-update-available-symbolic",
            SyncState::Differs => "dialog-warning-symbolic",
        }
    }
}

/// What the VFS cache holds for one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cached {
    /// Size of the file.
    pub size: u64,
    /// Bytes actually present in the cache.
    pub have: u64,
    pub mtime: Option<Timestamp>,
    /// Modified locally and not yet uploaded.
    pub dirty: bool,
}

impl Cached {
    fn complete(&self) -> bool {
        self.have >= self.size
    }
}

/// A folder's summary. All counts are for items below the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderSummary {
    pub uploading: usize,
    pub downloading: usize,
    pub waiting_to_upload: usize,
    pub cached_files: usize,
    pub cached_bytes: u64,
    /// The scan stopped at [`FOLDER_SCAN_CAP`], so the cache counts are lower bounds.
    pub truncated: bool,
}

/// Everything rclone itself knows about the item (no network beyond the local rc API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Local {
    File {
        transferring: bool,
        cached: Option<Cached>,
    },
    Folder(FolderSummary),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMeta {
    #[serde(rename = "ModTime")]
    mod_time: Option<String>,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "Rs")]
    rs: Vec<RawRange>,
    #[serde(rename = "Dirty")]
    dirty: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRange {
    #[serde(rename = "Size")]
    size: i64,
}

/// Parse one `vfsMeta` file.
fn parse_meta(text: &str) -> Option<Cached> {
    let m: RawMeta = serde_json::from_str(text).ok()?;
    let size = u64::try_from(m.size).unwrap_or(0);
    let have =
        m.rs.iter()
            .map(|r| u64::try_from(r.size).unwrap_or(0))
            .sum::<u64>()
            .min(size);
    Some(Cached {
        size,
        have,
        mtime: m.mod_time.and_then(|t| t.parse().ok()),
        dirty: m.dirty,
    })
}

/// `<cache>/vfsMeta/<remote name>` for the remote shown by the mount.
fn meta_root(cfg: &Config, fs: &str) -> PathBuf {
    // `gdrive:` -> `gdrive`; `gdrive:sub/dir` -> `gdrive/sub/dir`.
    let name = fs.replacen(':', "/", 1);
    Path::new(&expand(&cfg.cache_dir))
        .join("vfsMeta")
        .join(name.trim_end_matches('/'))
}

fn read_meta(path: &Path) -> Option<Cached> {
    parse_meta(&std::fs::read_to_string(path).ok()?)
}

/// Names of the files rclone is transferring right now.
fn transferring_names(cfg: &Config) -> Result<Vec<String>, String> {
    let stats = rc::rc_request(cfg, "core/stats", &json!({}))?;
    Ok(stats
        .get("transferring")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Walk `dir` (a `vfsMeta` folder), calling `f` with each file's parsed metadata and its path
/// relative to `dir`. Stops after [`FOLDER_SCAN_CAP`] entries; returns whether it did.
fn walk(dir: &Path, rel: &str, seen: &mut usize, f: &mut impl FnMut(&str, Cached)) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        if *seen >= FOLDER_SCAN_CAP {
            return true;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let path = e.path();
        if path.is_dir() {
            if walk(&path, &child, seen, f) {
                return true;
            }
        } else {
            *seen += 1;
            if let Some(c) = read_meta(&path) {
                f(&child, c);
            }
        }
    }
    false
}

/// Gather the local half of the status for `target`.
pub fn local(cfg: &Config, target: &Target) -> Result<Local, String> {
    let root = meta_root(cfg, &target.fs);
    let busy = transferring_names(cfg)?;
    if !target.is_dir {
        let cached = read_meta(&root.join(&target.rel));
        let transferring = busy.contains(&target.rel);
        return Ok(Local::File {
            transferring,
            cached,
        });
    }
    let prefix = format!("{}/", target.rel);
    let mut sum = FolderSummary::default();
    let mut seen = 0;
    let mut dirty_paths = Vec::new();
    sum.truncated = walk(
        &root.join(&target.rel),
        &target.rel,
        &mut seen,
        &mut |path, c| {
            sum.cached_files += 1;
            sum.cached_bytes += c.have;
            if c.dirty {
                dirty_paths.push(path.to_string());
            }
        },
    );
    // `walk` reports paths relative to the folder's parent because we passed `target.rel` as the
    // starting prefix, which is the same form `core/stats` uses.
    for n in busy.iter().filter(|n| n.starts_with(&prefix)) {
        if dirty_paths.iter().any(|p| p == n) {
            sum.uploading += 1;
        } else {
            sum.downloading += 1;
        }
    }
    sum.waiting_to_upload = dirty_paths
        .iter()
        .filter(|p| !busy.iter().any(|n| n == *p))
        .count();
    Ok(Local::Folder(sum))
}

/// The state of a file. `None` means "can't tell yet": the file is cached and we're still
/// waiting for Drive's side to compare against.
pub fn file_state(
    transferring: bool,
    cached: Option<&Cached>,
    drive: Option<&DriveInfo>,
) -> Option<SyncState> {
    let Some(c) = cached else {
        return Some(if transferring {
            SyncState::Downloading
        } else {
            SyncState::OnlineOnly
        });
    };
    if c.dirty {
        return Some(if transferring {
            SyncState::Uploading
        } else {
            SyncState::QueuedUpload
        });
    }
    if transferring {
        return Some(SyncState::Downloading);
    }
    if !c.complete() {
        return Some(SyncState::PartlyCached);
    }
    let d = drive?;
    Some(match (c.mtime, d.modified) {
        (Some(local), Some(remote)) => {
            let diff = local.as_second() - remote.as_second();
            if diff > MTIME_SLACK_SECS {
                SyncState::LocalNewer
            } else if diff < -MTIME_SLACK_SECS {
                SyncState::DriveNewer
            } else if d.size.is_some_and(|s| s != c.size) {
                SyncState::Differs
            } else {
                SyncState::InSync
            }
        }
        // Without both times the best available check is the size.
        _ if d.size.is_some_and(|s| s != c.size) => SyncState::Differs,
        _ => SyncState::InSync,
    })
}

/// A byte count for display, e.g. `1.5 MiB`.
pub fn human_bytes(n: u64) -> String {
    rc::human(n as f64)
}

/// A timestamp in the user's time zone, e.g. `May 9, 2026 7:33 PM`.
pub fn fmt_time(ts: Timestamp) -> String {
    ts.to_zoned(TimeZone::system())
        .strftime("%b %-d, %Y %-I:%M %p")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn cached(size: u64, have: u64, mtime: &str, dirty: bool) -> Cached {
        Cached {
            size,
            have,
            mtime: Some(ts(mtime)),
            dirty,
        }
    }

    fn drive(size: u64, mtime: &str) -> DriveInfo {
        DriveInfo {
            size: Some(size),
            modified: Some(ts(mtime)),
            ..DriveInfo::default()
        }
    }

    const T: &str = "2026-05-10T02:33:55Z";

    #[test]
    fn parses_vfs_meta() {
        let c = parse_meta(
            r#"{"ModTime":"2026-05-10T02:33:55.578Z","ATime":"2026-05-11T00:00:00Z",
                "Size":100,"Rs":[{"Pos":0,"Size":40},{"Pos":60,"Size":10}],
                "Fingerprint":"x","Dirty":true}"#,
        )
        .unwrap();
        assert_eq!((c.size, c.have, c.dirty), (100, 50, true));
        assert!(c.mtime.is_some());
        assert!(!c.complete());
    }

    #[test]
    fn meta_with_missing_fields_still_parses() {
        let c = parse_meta("{}").unwrap();
        assert_eq!((c.size, c.have, c.dirty), (0, 0, false));
        assert!(parse_meta("not json").is_none());
    }

    #[test]
    fn have_never_exceeds_size() {
        let c = parse_meta(r#"{"Size":10,"Rs":[{"Pos":0,"Size":99}]}"#).unwrap();
        assert_eq!(c.have, 10);
    }

    #[test]
    fn uncached_is_online_only_without_asking_drive() {
        assert_eq!(file_state(false, None, None), Some(SyncState::OnlineOnly));
        assert_eq!(file_state(true, None, None), Some(SyncState::Downloading));
    }

    #[test]
    fn dirty_means_uploading_or_waiting() {
        let c = cached(10, 10, T, true);
        assert_eq!(file_state(true, Some(&c), None), Some(SyncState::Uploading));
        assert_eq!(
            file_state(false, Some(&c), None),
            Some(SyncState::QueuedUpload)
        );
    }

    #[test]
    fn partial_cache_is_decided_locally() {
        let c = cached(10, 4, T, false);
        assert_eq!(
            file_state(false, Some(&c), None),
            Some(SyncState::PartlyCached)
        );
    }

    #[test]
    fn complete_cache_waits_for_drive() {
        let c = cached(10, 10, T, false);
        assert_eq!(file_state(false, Some(&c), None), None);
    }

    #[test]
    fn compares_against_drive() {
        let c = cached(10, 10, T, false);
        assert_eq!(
            file_state(false, Some(&c), Some(&drive(10, T))),
            Some(SyncState::InSync)
        );
        // within the slack
        assert_eq!(
            file_state(false, Some(&c), Some(&drive(10, "2026-05-10T02:33:56Z"))),
            Some(SyncState::InSync)
        );
        assert_eq!(
            file_state(false, Some(&c), Some(&drive(10, "2026-05-10T03:00:00Z"))),
            Some(SyncState::DriveNewer)
        );
        assert_eq!(
            file_state(false, Some(&c), Some(&drive(10, "2026-05-10T02:00:00Z"))),
            Some(SyncState::LocalNewer)
        );
        assert_eq!(
            file_state(false, Some(&c), Some(&drive(11, T))),
            Some(SyncState::Differs)
        );
    }

    #[test]
    fn every_state_has_label_and_icon() {
        for s in [
            SyncState::Uploading,
            SyncState::QueuedUpload,
            SyncState::Downloading,
            SyncState::InSync,
            SyncState::OnlineOnly,
            SyncState::PartlyCached,
            SyncState::LocalNewer,
            SyncState::DriveNewer,
            SyncState::Differs,
        ] {
            assert!(!s.label().is_empty() && s.icon().ends_with("-symbolic"));
        }
    }

    #[test]
    fn meta_root_maps_remote_to_cache_path() {
        let cfg = Config {
            cache_dir: "/c".into(),
            ..Config::default()
        };
        assert_eq!(meta_root(&cfg, "gdrive:"), Path::new("/c/vfsMeta/gdrive"));
        assert_eq!(
            meta_root(&cfg, "gdrive:sub/dir"),
            Path::new("/c/vfsMeta/gdrive/sub/dir")
        );
    }

    #[test]
    fn folder_scan_counts_cached_and_dirty() {
        let dir = std::env::temp_dir().join(format!("rcs-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), r#"{"Size":5,"Rs":[{"Pos":0,"Size":5}]}"#).unwrap();
        std::fs::write(
            dir.join("sub/b"),
            r#"{"Size":7,"Rs":[{"Pos":0,"Size":7}],"Dirty":true}"#,
        )
        .unwrap();
        let (mut files, mut bytes, mut dirty) = (0, 0, vec![]);
        let mut seen = 0;
        let cut = walk(&dir, "d", &mut seen, &mut |p, c| {
            files += 1;
            bytes += c.have;
            if c.dirty {
                dirty.push(p.to_string());
            }
        });
        std::fs::remove_dir_all(&dir).ok();
        assert!(!cut);
        assert_eq!((files, bytes), (2, 12));
        assert_eq!(dirty, ["d/sub/b"]);
    }

    // ---- local(): rc transfer list + on-disk cache metadata ----

    use crate::share::Provider;
    use crate::testutil::{ok, FakeServer};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rcs-status-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(cache: &Path, rel: &str, json: &str) {
        let path = cache.join("vfsMeta/gdrive").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }

    fn cfg_for(s: &FakeServer, cache: &Path) -> Config {
        Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent/rc.env".into(),
            cache_dir: cache.to_string_lossy().into_owned(),
            ..Config::default()
        }
    }

    fn target(rel: &str, is_dir: bool) -> Target {
        Target {
            provider: Provider::GoogleDrive,
            id: "id".into(),
            is_dir,
            fs: "gdrive:".into(),
            rel: rel.into(),
        }
    }

    fn stats(names: &[&str]) -> FakeServer {
        let transferring: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        FakeServer::start(vec![ok(
            "/core/stats",
            &serde_json::json!({ "transferring": transferring }).to_string(),
        )])
    }

    const CLEAN: &str =
        r#"{"ModTime":"2026-05-10T02:33:55Z","Size":10,"Rs":[{"Pos":0,"Size":10}]}"#;
    const DIRTY: &str =
        r#"{"ModTime":"2026-05-10T02:33:55Z","Size":10,"Rs":[{"Pos":0,"Size":10}],"Dirty":true}"#;

    #[test]
    fn a_cached_file_is_read_from_the_vfs_metadata() {
        let cache = scratch("file-clean");
        put(&cache, "d/a.txt", CLEAN);
        let s = stats(&[]);
        let l = local(&cfg_for(&s, &cache), &target("d/a.txt", false)).unwrap();
        let Local::File {
            transferring,
            cached,
        } = l
        else {
            panic!("expected a file");
        };
        assert!(!transferring);
        let c = cached.unwrap();
        assert_eq!((c.size, c.have, c.dirty), (10, 10, false));
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn an_uploading_file_is_dirty_and_listed_by_rclone() {
        let cache = scratch("file-dirty");
        put(&cache, "a.txt", DIRTY);
        let s = stats(&["a.txt"]);
        let l = local(&cfg_for(&s, &cache), &target("a.txt", false)).unwrap();
        let Local::File {
            transferring,
            cached,
        } = l
        else {
            panic!("expected a file");
        };
        assert!(transferring && cached.unwrap().dirty);
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn an_uncached_file_has_no_cache_entry() {
        let cache = scratch("file-none");
        let s = stats(&[]);
        let l = local(&cfg_for(&s, &cache), &target("nope.txt", false)).unwrap();
        assert_eq!(
            l,
            Local::File {
                transferring: false,
                cached: None
            }
        );
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn only_exact_names_count_as_transferring() {
        let cache = scratch("file-prefix");
        let s = stats(&["a.txt.bak", "dir/a.txt"]);
        let l = local(&cfg_for(&s, &cache), &target("a.txt", false)).unwrap();
        assert!(matches!(
            l,
            Local::File {
                transferring: false,
                ..
            }
        ));
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn a_folder_summarises_everything_below_it() {
        let cache = scratch("folder");
        put(&cache, "d/a", CLEAN);
        put(&cache, "d/x.bin", DIRTY); // being uploaded
        put(&cache, "d/y", DIRTY); // waiting its turn
        put(&cache, "d/sub/z", CLEAN);
        put(&cache, "other/w", DIRTY); // outside the folder
                                       // d/q.bin has no cache entry yet: a download in progress. "dd/x" shares the prefix
                                       // text but is a different folder.
        let s = stats(&["d/x.bin", "d/q.bin", "dd/x", "other/w"]);
        let l = local(&cfg_for(&s, &cache), &target("d", true)).unwrap();
        let Local::Folder(f) = l else {
            panic!("expected a folder");
        };
        assert_eq!(f.uploading, 1);
        assert_eq!(f.downloading, 1);
        assert_eq!(f.waiting_to_upload, 1);
        assert_eq!(f.cached_files, 4);
        assert_eq!(f.cached_bytes, 40);
        assert!(!f.truncated);
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn an_empty_or_missing_cache_folder_is_an_empty_summary() {
        let cache = scratch("folder-empty");
        let s = stats(&[]);
        let l = local(&cfg_for(&s, &cache), &target("nowhere", true)).unwrap();
        assert_eq!(l, Local::Folder(FolderSummary::default()));
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn rc_being_down_is_an_error() {
        let cfg = Config {
            rc_addr: "127.0.0.1:1".into(),
            env_file: "/nonexistent/rc.env".into(),
            ..Config::default()
        };
        assert!(local(&cfg, &target("a", false)).is_err());
    }

    #[test]
    fn unreadable_metadata_files_are_skipped() {
        let cache = scratch("folder-junk");
        put(&cache, "d/ok", CLEAN);
        put(&cache, "d/junk", "not json");
        let s = stats(&[]);
        let l = local(&cfg_for(&s, &cache), &target("d", true)).unwrap();
        let Local::Folder(f) = l else {
            panic!("expected a folder");
        };
        assert_eq!(f.cached_files, 1);
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn times_format_in_a_readable_way() {
        let s = fmt_time("2026-05-10T02:33:55Z".parse().unwrap());
        assert!(s.contains("2026") && s.contains(':'), "{s}");
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(human_bytes(0), "0.0 B");
        assert!(human_bytes(1536).contains("KiB"));
    }
}
