// SPDX-License-Identifier: GPL-3.0-only
//! Talks to rclone's remote-control API and systemd. All functions block; call them from
//! a worker thread.

use crate::config::{expand, Config};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use std::{process::Command, time::Duration};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Transfer {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
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
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_mounted(mount_path: &str) -> bool {
    let want = expand(mount_path);
    std::fs::read_to_string("/proc/mounts")
        .map(|m| {
            m.lines().any(|l| {
                let mut f = l.split_whitespace();
                let (_, mp, fstype) = (f.next(), f.next(), f.next());
                mp == Some(want.as_str()) && fstype == Some("fuse.rclone")
            })
        })
        .unwrap_or(false)
}

/// Read `RCLONE_RC_USER` / `RCLONE_RC_PASS` from the env file, as an `Authorization` value.
fn auth_header(env_file: &str) -> Option<String> {
    let text = std::fs::read_to_string(expand(env_file)).ok()?;
    let (mut user, mut pass) = (None, None);
    for line in text.lines() {
        match line.split_once('=') {
            Some(("RCLONE_RC_USER", v)) => user = Some(v.trim().to_string()),
            Some(("RCLONE_RC_PASS", v)) => pass = Some(v.trim().to_string()),
            _ => {}
        }
    }
    Some(format!("Basic {}", STANDARD.encode(format!("{}:{}", user?, pass?))))
}

fn rc_call<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    cfg: &Config,
    auth: &Option<String>,
    method: &str,
) -> Result<T, String> {
    let mut req = agent
        .post(&format!("http://{}/{}", cfg.rc_addr, method))
        .set("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    req.send_string("{}")
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

pub fn fetch(cfg: &Config) -> Status {
    let mut st = Status {
        service_active: service_active(&cfg.service),
        mounted: is_mounted(&cfg.mount_path),
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
    if !st.service_active {
        return st;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3))
        .build();
    let auth = auth_header(&cfg.env_file);
    let core: Result<CoreStats, _> = rc_call(&agent, cfg, &auth, "core/stats");
    let vfs: Result<VfsStats, _> = rc_call(&agent, cfg, &auth, "vfs/stats");
    match (core, vfs) {
        (Ok(core), Ok(vfs)) => {
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
        (Err(e), _) | (_, Err(e)) => st.rc_error = Some(e),
    }
    st
}

/// `verb` is one of `start`, `restart`, `stop`.
pub fn service_action(cfg: &Config, verb: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", verb, &cfg.service])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn open_mount(cfg: &Config) {
    let _ = Command::new("xdg-open").arg(expand(&cfg.mount_path)).spawn();
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
