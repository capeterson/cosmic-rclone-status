// SPDX-License-Identifier: GPL-3.0-only
//! Talks to rclone's remote-control API and systemd. All functions block; call them from
//! a worker thread.

use crate::config::{expand, Config};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer};
use std::{process::Command, time::Duration};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Service stopped or folder not mounted.
    Down,
    /// Running, but the rc API is unreachable or rclone reports errors.
    Error,
    Uploading,
    Downloading,
    /// Uploading and downloading at the same time.
    Transferring,
    Synced,
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
}

impl Status {
    pub fn health(&self) -> Health {
        if !self.service_active || !self.mounted {
            Health::Down
        } else if self.rc_error.is_some() || self.errored_files > 0 || self.out_of_space {
            Health::Error
        } else {
            match (self.uploading(), self.downloading()) {
                (true, true) => Health::Transferring,
                (true, false) => Health::Uploading,
                (false, true) => Health::Downloading,
                (false, false) => Health::Synced,
            }
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
    let mut req = agent
        .post(&format!("http://{addr}/{method}"))
        .set("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    req.send_string("{}")
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

/// What the rc API told us: `core/stats` plus `vfs/stats`.
struct RcStats {
    core: CoreStats,
    vfs: VfsStats,
}

fn fetch_rc(addr: &str, auth: Option<&str>, timeout: Duration) -> Result<RcStats, String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let core = rc_call(&agent, addr, auth, "core/stats");
    let vfs = rc_call(&agent, addr, auth, "vfs/stats");
    match (core, vfs) {
        (Ok(core), Ok(vfs)) => Ok(RcStats { core, vfs }),
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
    };
    match rc {
        None => {}
        Some(Err(e)) => st.rc_error = Some(e),
        Some(Ok(RcStats { core, vfs })) => {
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
    fn idle_is_synced() {
        assert_eq!(status().health(), Health::Synced);
    }

    #[test]
    fn down_beats_error() {
        let mut s = status();
        s.errored_files = 3;
        s.service_active = false;
        assert_eq!(s.health(), Health::Down);
        s.service_active = true;
        s.mounted = false;
        assert_eq!(s.health(), Health::Down);
    }

    #[test]
    fn each_error_signal_alone_is_error() {
        let mut s = status();
        s.rc_error = Some("boom".into());
        assert_eq!(s.health(), Health::Error);
        let mut s = status();
        s.errored_files = 1;
        assert_eq!(s.health(), Health::Error);
        let mut s = status();
        s.out_of_space = true;
        assert_eq!(s.health(), Health::Error);
    }

    #[test]
    fn error_beats_transfers() {
        let mut s = status();
        s.uploads_queued = 2;
        s.out_of_space = true;
        assert_eq!(s.health(), Health::Error);
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
        ]);
        let rc = fetch_rc(&addr, Some("Basic dTpw"), T).unwrap();
        let st = assemble(true, true, Some(Ok(rc)));
        assert_eq!(st.transfers.len(), 1);
        assert!((st.speed - 12.5).abs() < f64::EPSILON);
        assert_eq!(st.last_error, None, "empty lastError is dropped");
        assert_eq!(st.health(), Health::Uploading);

        let reqs = h.join().unwrap();
        assert!(reqs[0].starts_with("POST /core/stats "));
        assert!(reqs[1].starts_with("POST /vfs/stats "));
        assert!(reqs[0].to_lowercase().contains("authorization: basic dtpw"));
    }

    #[test]
    fn fetch_rc_reports_http_errors() {
        let (addr, h) = serve(vec![
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
        ]);
        let st = assemble(true, true, Some(fetch_rc(&addr, None, T)));
        assert!(st.rc_error.is_some());
        assert_eq!(st.health(), Health::Error);
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
        let (addr, h) = serve(vec![("200 OK", "not json".into()), ("200 OK", "{}".into())]);
        assert!(fetch_rc(&addr, None, T).is_err());
        h.join().unwrap();
    }

    #[test]
    fn assemble_without_rc_leaves_defaults() {
        let st = assemble(false, false, None);
        assert!(st.rc_error.is_none());
        assert_eq!(st.health(), Health::Down);
    }
}
