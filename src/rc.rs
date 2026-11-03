// SPDX-License-Identifier: GPL-3.0-only
//! Talks to rclone's remote-control API and systemd. All functions block; call them from
//! a worker thread.

use crate::config::{expand, Config};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer};
use std::{collections::HashMap, process::Command, time::Duration};

/// Treat JSON `null` like a missing field.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Transfer {
    #[serde(default)]
    pub name: String,
    /// rclone reports `-1` when the size is unknown (e.g. streaming uploads).
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub percentage: u64,
    #[serde(default)]
    pub speed: f64,
}

/// Which way a completed transfer went. Only known when rclone reports `srcFs`/`dstFs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

/// A finished transfer from `core/transferred`, with repeats of the same file merged.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedTransfer {
    /// Path relative to the remote root, e.g. `Docs/report.pdf`.
    pub name: String,
    /// `-1` when rclone doesn't know the size.
    pub size: i64,
    pub bytes: i64,
    /// When the most recent transfer of this file finished (or started, if it never finished).
    pub at: Option<jiff::Timestamp>,
    /// Set when the most recent transfer of this file failed.
    pub error: Option<String>,
    pub direction: Option<Direction>,
    /// How many raw entries were merged into this row (>= 1).
    pub times: u32,
}

/// Newest files kept in the history. rclone itself keeps about this many per stats group.
pub const HISTORY_CAP: usize = 100;

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTransferred {
    #[serde(deserialize_with = "null_as_default")]
    name: String,
    size: i64,
    bytes: i64,
    checked: bool,
    #[serde(deserialize_with = "null_as_default")]
    started_at: String,
    #[serde(deserialize_with = "null_as_default")]
    completed_at: String,
    #[serde(deserialize_with = "null_as_default")]
    error: String,
    /// Newer rclone only.
    #[serde(rename = "srcFs")]
    src_fs: Option<String>,
    #[serde(rename = "dstFs")]
    dst_fs: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TransferredResp {
    #[serde(deserialize_with = "null_as_default")]
    transferred: Vec<RawTransferred>,
}

/// Parse rclone's Go timestamps. Go's zero time (`0001-01-01…`) means "not set".
fn parse_ts(s: &str) -> Option<jiff::Timestamp> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .filter(|t| t.as_second() > 0)
}

/// A remote looks like `name:path`; local paths start with `/` (or `local:`).
fn fs_kind(fs: &str) -> Option<bool> {
    if fs.starts_with('/') || fs.starts_with("local:") || fs.starts_with("Local file system") {
        return Some(false);
    }
    let i = fs.find(':')?;
    (i > 0 && !fs[..i].contains('/')).then_some(true)
}

/// Uploads go from a local path to a remote, downloads the other way round.
fn direction(src: Option<&str>, dst: Option<&str>) -> Option<Direction> {
    match (fs_kind(src?)?, fs_kind(dst?)?) {
        (false, true) => Some(Direction::Upload),
        (true, false) => Some(Direction::Download),
        _ => None,
    }
}

/// Newest first; one row per file name, carrying the newest entry's timestamp and outcome.
fn history(raw: Vec<RawTransferred>, cap: usize) -> Vec<CompletedTransfer> {
    let mut entries: Vec<(usize, CompletedTransfer)> = raw
        .into_iter()
        .enumerate()
        .filter(|(_, r)| !r.checked && !r.name.is_empty())
        .map(|(i, r)| {
            let at = parse_ts(&r.completed_at).or_else(|| parse_ts(&r.started_at));
            let error = Some(r.error.trim().to_string()).filter(|e| !e.is_empty());
            let t = CompletedTransfer {
                direction: direction(r.src_fs.as_deref(), r.dst_fs.as_deref()),
                name: r.name,
                size: r.size,
                bytes: r.bytes,
                at,
                error,
                times: 1,
            };
            (i, t)
        })
        .collect();
    // rclone appends in completion order but sums several groups, so sort by time. Ties (and
    // entries without any timestamp) fall back to arrival order: later wins.
    entries.sort_by(|(ia, a), (ib, b)| (b.at, ib).cmp(&(a.at, ia)));

    let mut out: Vec<CompletedTransfer> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (_, t) in entries {
        if let Some(&i) = seen.get(&t.name) {
            out[i].times += 1;
        } else {
            seen.insert(t.name.clone(), out.len());
            out.push(t);
        }
    }
    out.truncate(cap);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The systemd unit is not running.
    ServiceStopped,
    /// The service runs but the folder is not mounted.
    NotMounted,
    /// Mounted, but the rc API cannot be reached.
    Unreachable,
    /// The VFS cache has no room left.
    CacheFull,
    /// rclone reports files that failed to transfer.
    FileErrors,
    Uploading,
    Downloading,
    /// Uploading and downloading at the same time.
    Transferring,
    /// Service running, folder mounted, nothing wrong, nothing moving.
    Healthy,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub service_active: bool,
    pub mounted: bool,
    pub rc_error: Option<String>,
    pub uploads_in_progress: u64,
    pub uploads_queued: u64,
    pub cache_files: u64,
    pub cache_bytes: u64,
    pub errored_files: u64,
    pub out_of_space: bool,
    pub speed: f64,
    pub last_error: Option<String>,
    pub transfers: Vec<Transfer>,
    /// Recently completed transfers, newest first, at most [`HISTORY_CAP`].
    pub recent: Vec<CompletedTransfer>,
}

impl Status {
    pub fn health(&self) -> Health {
        if !self.service_active {
            Health::ServiceStopped
        } else if !self.mounted {
            Health::NotMounted
        } else if self.rc_error.is_some() {
            Health::Unreachable
        } else if self.out_of_space {
            Health::CacheFull
        } else if self.errored_files > 0 {
            Health::FileErrors
        } else {
            match (self.uploading(), self.downloading()) {
                (true, true) => Health::Transferring,
                (true, false) => Health::Uploading,
                (false, true) => Health::Downloading,
                (false, false) => Health::Healthy,
            }
        }
    }

    /// Something is actually moving data right now.
    pub fn active(&self) -> bool {
        !self.transfers.is_empty() || self.uploads_in_progress > 0
    }

    /// The transfer speed to display. rclone reports a smoothed average that keeps decaying
    /// toward zero long after the last transfer ends, so report zero when nothing is moving.
    pub fn display_speed(&self) -> f64 {
        if self.active() {
            self.speed
        } else {
            0.0
        }
    }

    /// Files are queued or being sent to the remote.
    pub fn uploading(&self) -> bool {
        self.uploads_in_progress + self.uploads_queued > 0
    }

    /// rclone lists uploads and downloads together in `core/stats`, without a direction.
    /// The VFS tells us how many of those are uploads, so any surplus must be downloads.
    pub fn downloading(&self) -> bool {
        self.transfers.len() as u64 > self.uploads_in_progress
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CoreStats {
    speed: f64,
    #[serde(rename = "lastError")]
    last_error: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    transferring: Vec<Transfer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VfsStats {
    #[serde(rename = "diskCache")]
    disk_cache: DiskCache,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DiskCache {
    files: u64,
    #[serde(rename = "bytesUsed")]
    bytes_used: u64,
    #[serde(rename = "erroredFiles")]
    errored_files: u64,
    #[serde(rename = "outOfSpace")]
    out_of_space: bool,
    #[serde(rename = "uploadsInProgress")]
    uploads_in_progress: u64,
    #[serde(rename = "uploadsQueued")]
    uploads_queued: u64,
}

fn service_active(service: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", service])
        .status()
        .is_ok_and(|s| s.success())
}

/// Undo the octal escapes (`\040` for space, etc.) the kernel applies in `/proc/mounts`.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (u32::from(b[i + 1] - b'0') << 6)
                | (u32::from(b[i + 2] - b'0') << 3)
                | u32::from(b[i + 3] - b'0');
            out.push(u8::try_from(v & 0xff).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is `want` a `fuse.rclone` mount point according to `/proc/mounts`-formatted `mounts`?
fn parse_mounts(mounts: &str, want: &str) -> bool {
    let want = want.trim_end_matches('/');
    mounts.lines().any(|l| {
        let mut f = l.split_whitespace();
        let (_, mp, fstype) = (f.next(), f.next(), f.next());
        fstype == Some("fuse.rclone")
            && mp.is_some_and(|mp| unescape_mount(mp).trim_end_matches('/') == want)
    })
}

fn is_mounted(mount_path: &str) -> bool {
    std::fs::read_to_string("/proc/mounts").is_ok_and(|m| parse_mounts(&m, &expand(mount_path)))
}

/// Parse `KEY=VALUE` env-file text the way systemd's `EnvironmentFile` does (comments, optional
/// `export`, surrounding quotes), returning the rc user and password.
fn parse_credentials(text: &str) -> Option<(String, String)> {
    let (mut user, mut pass) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            match k.trim() {
                "RCLONE_RC_USER" => user = Some(v.to_string()),
                "RCLONE_RC_PASS" => pass = Some(v.to_string()),
                _ => {}
            }
        }
    }
    Some((user?, pass?))
}

/// Read `RCLONE_RC_USER` / `RCLONE_RC_PASS` from the env file, as an `Authorization` value.
fn auth_header(env_file: &str) -> Option<String> {
    let text = std::fs::read_to_string(expand(env_file)).ok()?;
    let (user, pass) = parse_credentials(&text)?;
    Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{user}:{pass}"))
    ))
}

fn rc_call<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    addr: &str,
    auth: Option<&str>,
    method: &str,
) -> Result<T, String> {
    rc_post(agent, addr, auth, method, "{}")
}

fn rc_post<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    addr: &str,
    auth: Option<&str>,
    method: &str,
    body: &str,
) -> Result<T, String> {
    let mut req = agent
        .post(&format!("http://{addr}/{method}"))
        .set("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    req.send_string(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

/// Call an rc method with a JSON body and return the JSON reply.
pub fn rc_request(
    cfg: &Config,
    method: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let auth = auth_header(&cfg.env_file);
    rc_post(
        &agent,
        &cfg.rc_addr,
        auth.as_deref(),
        method,
        &body.to_string(),
    )
}

/// The rclone remote (e.g. `gdrive:`) mounted at `want`, from `/proc/mounts`-formatted text.
fn parse_mount_fs(mounts: &str, want: &str) -> Option<String> {
    let want = want.trim_end_matches('/');
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (fs, mp, fstype) = (f.next()?, f.next()?, f.next()?);
        (fstype == "fuse.rclone" && unescape_mount(mp).trim_end_matches('/') == want)
            .then(|| unescape_mount(fs))
    })
}

/// Which rclone remote is mounted at `mount_path`.
pub fn mount_fs(mount_path: &str) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    parse_mount_fs(&mounts, &expand(mount_path))
}

/// What the rc API told us: `core/stats`, `vfs/stats` and `core/transferred`.
struct RcStats {
    core: CoreStats,
    vfs: VfsStats,
    transferred: Vec<RawTransferred>,
}

fn fetch_rc(addr: &str, auth: Option<&str>, timeout: Duration) -> Result<RcStats, String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let core = rc_call(&agent, addr, auth, "core/stats");
    let vfs = rc_call(&agent, addr, auth, "vfs/stats");
    // The history is a nicety: if it fails, the rest of the snapshot is still good.
    let transferred = rc_call::<TransferredResp>(&agent, addr, auth, "core/transferred")
        .map(|r| r.transferred)
        .unwrap_or_default();
    match (core, vfs) {
        (Ok(core), Ok(vfs)) => Ok(RcStats {
            core,
            vfs,
            transferred,
        }),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

/// Combine the independent probes into a `Status`. `rc` is `None` when it wasn't queried.
fn assemble(service_active: bool, mounted: bool, rc: Option<Result<RcStats, String>>) -> Status {
    let mut st = Status {
        service_active,
        mounted,
        rc_error: None,
        uploads_in_progress: 0,
        uploads_queued: 0,
        cache_files: 0,
        cache_bytes: 0,
        errored_files: 0,
        out_of_space: false,
        speed: 0.0,
        last_error: None,
        transfers: vec![],
        recent: vec![],
    };
    match rc {
        None => {}
        Some(Err(e)) => st.rc_error = Some(e),
        Some(Ok(RcStats {
            core,
            vfs,
            transferred,
        })) => {
            let d = vfs.disk_cache;
            st.uploads_in_progress = d.uploads_in_progress;
            st.uploads_queued = d.uploads_queued;
            st.cache_files = d.files;
            st.cache_bytes = d.bytes_used;
            st.errored_files = d.errored_files;
            st.out_of_space = d.out_of_space;
            st.speed = core.speed;
            st.last_error = core.last_error.filter(|e| !e.is_empty());
            st.transfers = core.transferring;
            st.recent = history(transferred, HISTORY_CAP);
        }
    }
    st
}

pub fn fetch(cfg: &Config) -> Status {
    let active = service_active(&cfg.service);
    let mounted = is_mounted(&cfg.mount_path);
    let rc = active.then(|| {
        let auth = auth_header(&cfg.env_file);
        fetch_rc(&cfg.rc_addr, auth.as_deref(), Duration::from_secs(3))
    });
    assemble(active, mounted, rc)
}

/// `verb` is one of `start`, `restart`, `stop`.
pub fn service_action(cfg: &Config, verb: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", verb, &cfg.service])
        .status()
        .is_ok_and(|s| s.success())
}

pub fn open_mount(cfg: &Config) {
    let _ = Command::new("xdg-open")
        .arg(expand(&cfg.mount_path))
        .spawn();
}

/// Where the remote path `name` appears in the mount, or `None` for paths that try to climb out
/// of it with `..`.
fn item_path(mount_path: &str, name: &str) -> Option<String> {
    let mount = expand(mount_path);
    let mount = mount.trim_end_matches('/');
    let name = name.trim_matches('/');
    if name.is_empty() || name.split('/').any(|c| c == "..") {
        return None;
    }
    Some(format!("{mount}/{name}"))
}

/// Open the "Rclone…" details window for the remote path `name`, as a separate process (the
/// same window COSMIC Files' right-click entry opens). Nothing is downloaded.
pub fn open_details(cfg: &Config, name: &str) {
    let (Some(path), Ok(exe)) = (item_path(&cfg.mount_path, name), std::env::current_exe()) else {
        return;
    };
    let _ = Command::new(exe).arg("--menu").arg(path).spawn();
}

pub fn human(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n, 0);
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn status() -> Status {
        assemble(true, true, None)
    }

    // ---- Status::health -------------------------------------------------

    #[test]
    fn idle_is_healthy() {
        assert_eq!(status().health(), Health::Healthy);
    }

    #[test]
    fn speed_is_zero_once_transfers_stop() {
        let mut s = status();
        s.speed = 123.0; // rclone's decaying average, still non-zero after the transfer ended
        assert!((s.display_speed()).abs() < f64::EPSILON);
        s.uploads_in_progress = 1;
        assert!((s.display_speed() - 123.0).abs() < f64::EPSILON);
        s.uploads_in_progress = 0;
        s.transfers = transfers(1);
        assert!((s.display_speed() - 123.0).abs() < f64::EPSILON);
        // Queued but not started is not moving data yet.
        s.transfers = vec![];
        s.uploads_queued = 2;
        assert!((s.display_speed()).abs() < f64::EPSILON);
    }

    #[test]
    fn down_beats_error() {
        let mut s = status();
        s.errored_files = 3;
        s.service_active = false;
        assert_eq!(s.health(), Health::ServiceStopped);
        s.service_active = true;
        s.mounted = false;
        assert_eq!(s.health(), Health::NotMounted);
    }

    #[test]
    fn each_error_signal_alone_has_its_own_state() {
        let mut s = status();
        s.rc_error = Some("boom".into());
        assert_eq!(s.health(), Health::Unreachable);
        let mut s = status();
        s.errored_files = 1;
        assert_eq!(s.health(), Health::FileErrors);
        let mut s = status();
        s.out_of_space = true;
        assert_eq!(s.health(), Health::CacheFull);
    }

    #[test]
    fn error_priority_is_unreachable_then_full_then_file_errors() {
        let mut s = status();
        s.errored_files = 1;
        s.out_of_space = true;
        assert_eq!(s.health(), Health::CacheFull);
        s.rc_error = Some("boom".into());
        assert_eq!(s.health(), Health::Unreachable);
    }

    #[test]
    fn error_beats_transfers() {
        let mut s = status();
        s.uploads_queued = 2;
        s.out_of_space = true;
        assert_eq!(s.health(), Health::CacheFull);
    }

    // ---- direction heuristic -------------------------------------------

    fn transfers(n: usize) -> Vec<Transfer> {
        vec![Transfer::default(); n]
    }

    #[test]
    fn queued_uploads_with_no_transfers_is_uploading() {
        let mut s = status();
        s.uploads_queued = 4;
        assert_eq!(s.health(), Health::Uploading);
    }

    #[test]
    fn surplus_transfers_are_downloads() {
        let mut s = status();
        s.transfers = transfers(2);
        assert_eq!(s.health(), Health::Downloading);
    }

    #[test]
    fn mixed_is_transferring() {
        let mut s = status();
        s.transfers = transfers(3);
        s.uploads_in_progress = 1;
        assert_eq!(s.health(), Health::Transferring);
    }

    #[test]
    fn more_uploads_than_transfers_is_not_a_download() {
        // core and vfs stats are sampled at different moments, so this can happen.
        let mut s = status();
        s.transfers = transfers(1);
        s.uploads_in_progress = 2;
        assert!(!s.downloading());
        assert_eq!(s.health(), Health::Uploading);
    }

    // ---- human ---------------------------------------------------------

    #[test]
    fn human_formats_units() {
        assert_eq!(human(0.0), "0.0 B");
        assert_eq!(human(1023.0), "1023.0 B");
        assert_eq!(human(1024.0), "1.0 KiB");
        assert_eq!(human(1536.0), "1.5 KiB");
        assert_eq!(human(1024.0 * 1024.0 * 5.0), "5.0 MiB");
    }

    #[test]
    fn human_caps_at_tib() {
        assert_eq!(human(1024f64.powi(5)), "1024.0 TiB");
    }

    // ---- parse_mounts --------------------------------------------------

    const MOUNTS: &str = "\
sysfs /sys sysfs rw 0 0
gdrive: /home/u/GoogleDrive fuse.rclone rw,nosuid 0 0
gdrive: /home/u/My\\040Drive fuse.rclone rw 0 0
other: /home/u/Other fuse.sshfs rw 0 0
";

    #[test]
    fn finds_rclone_mount() {
        assert!(parse_mounts(MOUNTS, "/home/u/GoogleDrive"));
    }

    #[test]
    fn tolerates_trailing_slash_in_config() {
        assert!(parse_mounts(MOUNTS, "/home/u/GoogleDrive/"));
    }

    #[test]
    fn handles_escaped_spaces() {
        assert!(parse_mounts(MOUNTS, "/home/u/My Drive"));
    }

    #[test]
    fn ignores_other_filesystems_and_paths() {
        assert!(!parse_mounts(MOUNTS, "/home/u/Other"));
        assert!(!parse_mounts(MOUNTS, "/home/u/Missing"));
        assert!(!parse_mounts("", "/x"));
    }

    // ---- credentials ---------------------------------------------------

    #[test]
    fn parses_plain_credentials() {
        let t = "RCLONE_RC_USER=me\nRCLONE_RC_PASS=s3=cret\n";
        assert_eq!(parse_credentials(t), Some(("me".into(), "s3=cret".into())));
    }

    #[test]
    fn strips_quotes_export_and_comments() {
        let t = "# comment\nexport RCLONE_RC_USER=\"me\"\n RCLONE_RC_PASS = 'pw' \n";
        assert_eq!(parse_credentials(t), Some(("me".into(), "pw".into())));
    }

    #[test]
    fn last_duplicate_wins() {
        let t = "RCLONE_RC_USER=a\nRCLONE_RC_USER=b\nRCLONE_RC_PASS=p\n";
        assert_eq!(parse_credentials(t), Some(("b".into(), "p".into())));
    }

    #[test]
    fn missing_half_means_no_auth() {
        assert_eq!(parse_credentials("RCLONE_RC_USER=me\n"), None);
        assert_eq!(parse_credentials("RCLONE_RC_PASS=pw\n"), None);
        assert_eq!(parse_credentials(""), None);
    }

    #[test]
    fn auth_header_is_basic_base64() {
        let dir = std::env::temp_dir().join(format!("rcs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rc.env");
        std::fs::write(&f, "RCLONE_RC_USER=u\nRCLONE_RC_PASS=p\n").unwrap();
        assert_eq!(
            auth_header(f.to_str().unwrap()).as_deref(),
            Some("Basic dTpw")
        );
        assert_eq!(auth_header(dir.join("nope").to_str().unwrap()), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ---- JSON parsing --------------------------------------------------

    #[test]
    fn unknown_transfer_size_does_not_break_parsing() {
        let c: CoreStats =
            serde_json::from_str(r#"{"transferring":[{"name":"a","size":-1,"bytes":5}]}"#).unwrap();
        assert_eq!(c.transferring[0].size, -1);
    }

    #[test]
    fn null_or_missing_transferring_is_empty() {
        let c: CoreStats = serde_json::from_str(r#"{"transferring":null}"#).unwrap();
        assert!(c.transferring.is_empty());
        let c: CoreStats = serde_json::from_str("{}").unwrap();
        assert!(c.transferring.is_empty());
    }

    #[test]
    fn parses_vfs_disk_cache() {
        let v: VfsStats = serde_json::from_str(
            r#"{"diskCache":{"files":2,"bytesUsed":10,"erroredFiles":1,"outOfSpace":true,
                "uploadsInProgress":3,"uploadsQueued":4}}"#,
        )
        .unwrap();
        let d = v.disk_cache;
        assert_eq!((d.files, d.bytes_used, d.errored_files), (2, 10, 1));
        assert!(d.out_of_space);
        assert_eq!((d.uploads_in_progress, d.uploads_queued), (3, 4));
    }

    // ---- fake rc server ------------------------------------------------

    /// Serve one canned `(status line, body)` per request, in order, then stop.
    /// Returns the address and a handle yielding the raw requests received.
    fn serve(
        responses: Vec<(&'static str, String)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let mut seen = vec![];
            for (status, body) in responses {
                let (mut sock, _) = l.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (addr, h)
    }

    const T: Duration = Duration::from_secs(2);

    #[test]
    fn fetch_rc_merges_core_and_vfs() {
        let (addr, h) = serve(vec![
            ("200 OK", r#"{"speed":12.5,"lastError":"","transferring":[{"name":"f","size":10,"bytes":5,"percentage":50}]}"#.into()),
            ("200 OK", r#"{"diskCache":{"uploadsInProgress":1}}"#.into()),
            (
                "200 OK",
                r#"{"transferred":[
                    {"name":"old.txt","size":1,"bytes":1,"checked":false,
                     "completed_at":"2026-11-02T10:00:00Z","error":""},
                    {"name":"Docs/new.pdf","size":2048,"bytes":2048,"checked":false,
                     "completed_at":"2026-11-02T11:00:00.5-08:00","error":""}]}"#
                    .into(),
            ),
        ]);
        let rc = fetch_rc(&addr, Some("Basic dTpw"), T).unwrap();
        let st = assemble(true, true, Some(Ok(rc)));
        assert_eq!(st.transfers.len(), 1);
        assert!((st.speed - 12.5).abs() < f64::EPSILON);
        assert_eq!(st.last_error, None, "empty lastError is dropped");
        assert_eq!(st.health(), Health::Uploading);
        let names: Vec<_> = st.recent.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Docs/new.pdf", "old.txt"], "newest first");

        let reqs = h.join().unwrap();
        assert!(reqs[0].starts_with("POST /core/stats "));
        assert!(reqs[1].starts_with("POST /vfs/stats "));
        assert!(reqs[2].starts_with("POST /core/transferred "));
        assert!(reqs[0].to_lowercase().contains("authorization: basic dtpw"));
    }

    #[test]
    fn fetch_rc_reports_http_errors() {
        let (addr, h) = serve(vec![
            ("401 Unauthorized", "{}".into()),
            ("401 Unauthorized", "{}".into()),
            ("401 Unauthorized", "{}".into()),
        ]);
        let err = fetch_rc(&addr, None, T).err().unwrap();
        assert!(err.contains("401"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn vfs_failure_discards_core_data() {
        // Records current behaviour: one failed call turns the whole snapshot into an error.
        let (addr, h) = serve(vec![
            ("200 OK", r#"{"speed":1.0}"#.into()),
            ("500 Internal Server Error", r#"{"error":"no vfs"}"#.into()),
            ("200 OK", r#"{"transferred":[]}"#.into()),
        ]);
        let st = assemble(true, true, Some(fetch_rc(&addr, None, T)));
        assert!(st.rc_error.is_some());
        assert_eq!(st.health(), Health::Unreachable);
        h.join().unwrap();
    }

    #[test]
    fn unreachable_rc_is_an_error() {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        }; // listener dropped, port closed
        assert!(fetch_rc(&addr, None, T).is_err());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let (addr, h) = serve(vec![
            ("200 OK", "not json".into()),
            ("200 OK", "{}".into()),
            ("200 OK", "{}".into()),
        ]);
        assert!(fetch_rc(&addr, None, T).is_err());
        h.join().unwrap();
    }

    #[test]
    fn history_failure_is_not_fatal() {
        let (addr, h) = serve(vec![
            ("200 OK", r#"{"speed":1.0}"#.into()),
            ("200 OK", "{}".into()),
            ("500 Internal Server Error", r#"{"error":"nope"}"#.into()),
        ]);
        let st = assemble(true, true, Some(fetch_rc(&addr, None, T)));
        assert!(st.rc_error.is_none());
        assert_eq!(st.recent.len(), 0);
        assert_eq!(st.health(), Health::Healthy);
        h.join().unwrap();
    }

    // ---- completed-transfer history -------------------------------------

    fn raw(json: &str) -> Vec<RawTransferred> {
        serde_json::from_str::<TransferredResp>(&format!(r#"{{"transferred":{json}}}"#))
            .unwrap()
            .transferred
    }

    fn entry(name: &str, done: &str, error: &str) -> String {
        format!(
            r#"{{"name":"{name}","size":10,"bytes":10,"completed_at":"{done}","error":"{error}"}}"#
        )
    }

    #[test]
    fn transferred_parses_a_full_entry() {
        let r = raw(
            r#"[{"name":"Docs/a.txt","size":-1,"bytes":7,"checked":false,
                "started_at":"2026-11-02T10:00:00-08:00","completed_at":"2026-11-02T10:00:01.123456789-08:00",
                "error":"boom","group":"job/1","srcFs":"/home/u/.cache/rclone/vfs/gd","dstFs":"gd:"}]"#,
        );
        let h = history(r, 10);
        assert_eq!(h.len(), 1);
        let t = &h[0];
        assert_eq!((t.name.as_str(), t.size, t.bytes), ("Docs/a.txt", -1, 7));
        assert_eq!(t.error.as_deref(), Some("boom"));
        assert_eq!(t.direction, Some(Direction::Upload));
        assert_eq!(t.times, 1);
        assert_eq!(
            t.at,
            Some("2026-11-02T18:00:01.123456789Z".parse().unwrap()),
            "offset is normalised, nanoseconds kept"
        );
    }

    #[test]
    fn null_or_missing_fields_do_not_break_parsing() {
        assert!(raw("null").is_empty());
        let h = history(
            raw(
                r#"[{"name":"a","error":null,"completed_at":null,"started_at":null}, {"name":"b"}]"#,
            ),
            10,
        );
        assert_eq!(h.len(), 2);
        assert!(h.iter().all(|t| t.error.is_none() && t.at.is_none()));
        let none: TransferredResp = serde_json::from_str("{}").unwrap();
        assert!(none.transferred.is_empty());
    }

    #[test]
    fn timestamps_zero_and_garbage_are_unset() {
        assert!(parse_ts("").is_none());
        assert!(parse_ts("nope").is_none());
        assert!(parse_ts("0001-01-01T00:00:00Z").is_none(), "Go zero time");
        assert_eq!(
            parse_ts("2026-11-02T10:00:00-08:00"),
            parse_ts("2026-11-02T18:00:00Z")
        );
    }

    #[test]
    fn history_sorts_newest_first_regardless_of_arrival_order() {
        let j = format!(
            "[{},{},{}]",
            entry("mid", "2026-11-02T10:00:00Z", ""),
            entry("new", "2026-11-02T12:00:00Z", ""),
            entry("old", "2026-11-02T08:00:00Z", ""),
        );
        let names: Vec<_> = history(raw(&j), 10).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["new", "mid", "old"]);
    }

    #[test]
    fn repeats_merge_into_one_row_at_the_latest_time() {
        let j = format!(
            "[{},{},{},{}]",
            entry("f", "2026-11-02T09:00:00Z", "first try failed"),
            entry("other", "2026-11-02T10:00:00Z", ""),
            entry("f", "2026-11-02T11:00:00Z", ""),
            entry("f", "2026-11-02T08:00:00Z", ""),
        );
        let h = history(raw(&j), 10);
        let names: Vec<_> = h.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["f", "other"], "f sorts by its newest timestamp");
        assert_eq!(h[0].times, 3);
        assert_eq!(h[0].at, parse_ts("2026-11-02T11:00:00Z"));
        assert_eq!(h[0].error, None, "the newest attempt's outcome wins");
    }

    #[test]
    fn a_newer_failure_overrides_an_older_success() {
        let j = format!(
            "[{},{}]",
            entry("f", "2026-11-02T09:00:00Z", ""),
            entry("f", "2026-11-02T10:00:00Z", "later failure"),
        );
        let h = history(raw(&j), 10);
        assert_eq!(h[0].error.as_deref(), Some("later failure"));
    }

    #[test]
    fn checks_and_nameless_entries_are_dropped() {
        let h = history(
            raw(r#"[{"name":"c","checked":true},{"name":""},{"name":"ok"}]"#),
            10,
        );
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].name, "ok");
    }

    #[test]
    fn entries_without_timestamps_sort_last_in_arrival_order() {
        let j = format!(
            r#"[{{"name":"u1"}},{},{{"name":"u2"}}]"#,
            entry("dated", "2026-11-02T10:00:00Z", "")
        );
        let names: Vec<_> = history(raw(&j), 10).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["dated", "u2", "u1"]);
    }

    #[test]
    fn history_is_capped() {
        let items: Vec<String> = (0..150)
            .map(|i| entry(&format!("f{i}"), "2026-11-02T10:00:00Z", ""))
            .collect();
        let h = history(raw(&format!("[{}]", items.join(","))), HISTORY_CAP);
        assert_eq!(h.len(), HISTORY_CAP);
    }

    #[test]
    fn blank_error_strings_mean_no_error() {
        let h = history(raw(r#"[{"name":"a","error":"  "}]"#), 10);
        assert_eq!(h[0].error, None);
    }

    #[test]
    fn direction_needs_one_local_and_one_remote_side() {
        use Direction::{Download, Upload};
        assert_eq!(direction(Some("/cache/vfs"), Some("gd:")), Some(Upload));
        assert_eq!(
            direction(Some("local:/cache"), Some("gd:dir")),
            Some(Upload)
        );
        assert_eq!(direction(Some("gd:"), Some("/cache/vfs")), Some(Download));
        assert_eq!(
            direction(Some("gd:"), Some("other:")),
            None,
            "remote to remote"
        );
        assert_eq!(direction(Some("/a"), Some("/b")), None, "local to local");
        assert_eq!(direction(None, None), None, "older rclone sends neither");
        assert_eq!(direction(Some("/a"), None), None);
        assert_eq!(
            direction(Some("/a:b/c"), Some("gd:")),
            Some(Upload),
            "colon inside a path"
        );
    }

    // ---- item path -------------------------------------------------------

    #[test]
    fn item_path_is_the_name_inside_the_mount() {
        assert_eq!(
            item_path("/mnt/gd", "Docs/Sub/a.txt").as_deref(),
            Some("/mnt/gd/Docs/Sub/a.txt")
        );
        assert_eq!(
            item_path("/mnt/gd/", "top.txt").as_deref(),
            Some("/mnt/gd/top.txt")
        );
        assert_eq!(item_path("/", "top.txt").as_deref(), Some("/top.txt"));
        assert_eq!(
            item_path("/mnt/gd", "Docs/").as_deref(),
            Some("/mnt/gd/Docs")
        );
    }

    #[test]
    fn item_path_refuses_to_climb_out_or_be_empty() {
        assert_eq!(item_path("/mnt/gd", "../etc/passwd"), None);
        assert_eq!(item_path("/mnt/gd", "a/../../b/c"), None);
        assert_eq!(item_path("/mnt/gd", ""), None);
    }

    #[test]
    fn assemble_without_rc_leaves_defaults() {
        let st = assemble(false, false, None);
        assert!(st.rc_error.is_none());
        assert_eq!(st.health(), Health::ServiceStopped);
    }

    // ---- rc_request and mount_fs ----

    use crate::testutil::{ok, status as http_status, FakeServer};

    fn cfg_for(s: &FakeServer) -> Config {
        Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent/rc.env".into(),
            ..Config::default()
        }
    }

    #[test]
    fn rc_request_posts_the_json_body_and_returns_the_reply() {
        let s = FakeServer::start(vec![ok("/vfs/refresh", r#"{"result":{"a/":"OK"}}"#)]);
        let body = serde_json::json!({"dir": "a", "recursive": "false"});
        let reply = rc_request(&cfg_for(&s), "vfs/refresh", &body).unwrap();
        assert_eq!(reply["result"]["a/"], "OK");
        let req = &s.requests()[0];
        assert_eq!(req.path(), "/vfs/refresh");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&req.body).unwrap(),
            body
        );
        assert_eq!(req.authorization, None);
    }

    #[test]
    fn rc_request_sends_basic_auth_from_the_env_file() {
        let dir = std::env::temp_dir().join(format!("rcs-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join("rc.env");
        std::fs::write(&env, "RCLONE_RC_USER=u\nRCLONE_RC_PASS=p\n").unwrap();
        let s = FakeServer::start(vec![ok("/x", "{}")]);
        let cfg = Config {
            env_file: env.to_string_lossy().into_owned(),
            ..cfg_for(&s)
        };
        rc_request(&cfg, "x", &serde_json::json!({})).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(s.requests()[0].authorization.as_deref(), Some("Basic dTpw"));
    }

    #[test]
    fn rc_request_reports_http_errors_and_unreachable_servers() {
        let s = FakeServer::start(vec![http_status("/boom", 500)]);
        assert!(rc_request(&cfg_for(&s), "boom", &serde_json::json!({})).is_err());
        assert!(rc_request(&cfg_for(&s), "missing", &serde_json::json!({})).is_err());
        let down = Config {
            rc_addr: "127.0.0.1:1".into(),
            ..cfg_for(&s)
        };
        assert!(rc_request(&down, "x", &serde_json::json!({})).is_err());
    }

    const FS_MOUNTS: &str = "\
proc /proc proc rw 0 0
gdrive: /home/u/GoogleDrive fuse.rclone rw,nosuid 0 0
work:sub/dir /home/u/My\\040Work fuse.rclone rw 0 0
/dev/sda1 /home/u/Other ext4 rw 0 0
";

    #[test]
    fn parse_mount_fs_finds_the_remote_for_a_mount_point() {
        assert_eq!(
            parse_mount_fs(FS_MOUNTS, "/home/u/GoogleDrive").as_deref(),
            Some("gdrive:")
        );
        assert_eq!(
            parse_mount_fs(FS_MOUNTS, "/home/u/GoogleDrive/").as_deref(),
            Some("gdrive:")
        );
        // Escaped spaces and sub-path remotes.
        assert_eq!(
            parse_mount_fs(FS_MOUNTS, "/home/u/My Work").as_deref(),
            Some("work:sub/dir")
        );
    }

    #[test]
    fn parse_mount_fs_ignores_other_filesystems_and_unknown_paths() {
        assert_eq!(parse_mount_fs(FS_MOUNTS, "/home/u/Other"), None);
        assert_eq!(parse_mount_fs(FS_MOUNTS, "/home/u/Missing"), None);
        assert_eq!(parse_mount_fs("", "/x"), None);
        assert_eq!(parse_mount_fs("garbage", "/x"), None);
    }
}
