// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config::{self, cosmic_config_derive::CosmicConfigEntry, CosmicConfigEntry};

/// User-editable settings. Defaults match the rclone Google Drive setup this was written for.
#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    /// Address of rclone's `--rc` HTTP server.
    pub rc_addr: String,
    /// File holding `RCLONE_RC_USER=` / `RCLONE_RC_PASS=` lines (`~` is expanded).
    pub env_file: String,
    /// systemd *user* unit that runs `rclone mount`.
    pub service: String,
    /// Where the remote is mounted (`~` is expanded).
    pub mount_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rc_addr: "127.0.0.1:5572".into(),
            env_file: "~/.config/rclone/gdrive-rc.env".into(),
            service: "rclone-gdrive.service".into(),
            mount_path: "~/GoogleDrive".into(),
        }
    }
}

/// Expand a leading `~/` using `$HOME`.
pub fn expand(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}
