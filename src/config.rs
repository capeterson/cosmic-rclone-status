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
    /// rclone's `--cache-dir` (`~` is expanded); read for per-file cache status.
    pub cache_dir: String,
    /// Add an "Rclone…" entry to the COSMIC Files right-click menu.
    pub context_menu: bool,
    /// Offline folders are switched on (set up from the settings window).
    pub offline_enabled: bool,
    /// Folders kept available offline, relative to the mount root (no leading slash).
    pub pins: Vec<String>,
    /// How often each offline folder is synced with the remote when nothing changes locally.
    pub sync_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rc_addr: "127.0.0.1:5572".into(),
            env_file: "~/.config/rclone/gdrive-rc.env".into(),
            service: "rclone-gdrive.service".into(),
            mount_path: "~/GoogleDrive".into(),
            cache_dir: "~/.cache/rclone".into(),
            context_menu: true,
            offline_enabled: false,
            pins: Vec::new(),
            sync_interval_secs: 300,
        }
    }
}

impl Config {
    /// Load the user's saved settings, falling back to defaults for anything unreadable.
    pub fn load(app_id: &str) -> Self {
        cosmic_config::Config::new(app_id, Self::VERSION)
            .map(|ctx| match Self::get_entry(&ctx) {
                Ok(c) | Err((_, c)) => c,
            })
            .unwrap_or_default()
    }

    /// Change the saved settings: `f` edits a fresh copy, which is written back. Running apps
    /// pick the change up through their config watch.
    pub fn save(app_id: &str, f: impl FnOnce(&mut Config)) -> Result<Config, String> {
        let ctx = cosmic_config::Config::new(app_id, Self::VERSION).map_err(|e| e.to_string())?;
        let mut cfg = match Self::get_entry(&ctx) {
            Ok(c) | Err((_, c)) => c,
        };
        f(&mut cfg);
        cfg.write_entry(&ctx).map_err(|e| e.to_string())?;
        Ok(cfg)
    }
}

/// Expand a leading `~/` (or a bare `~`) using `$HOME`.
pub fn expand(path: &str) -> String {
    expand_with(path, std::env::var("HOME").ok().as_deref())
}

fn expand_with(path: &str, home: Option<&str>) -> String {
    match (home, path) {
        (Some(home), "~") => home.to_string(),
        (Some(home), p) => p
            .strip_prefix("~/")
            .map_or_else(|| p.to_string(), |rest| format!("{home}/{rest}")),
        (None, p) => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_prefix() {
        assert_eq!(expand_with("~/Drive", Some("/home/u")), "/home/u/Drive");
        assert_eq!(expand_with("~", Some("/home/u")), "/home/u");
    }

    #[test]
    fn leaves_other_paths_alone() {
        assert_eq!(expand_with("/mnt/x", Some("/home/u")), "/mnt/x");
        assert_eq!(expand_with("~other/x", Some("/home/u")), "~other/x");
        assert_eq!(expand_with("a/~/b", Some("/home/u")), "a/~/b");
    }

    #[test]
    fn no_home_means_no_expansion() {
        assert_eq!(expand_with("~/Drive", None), "~/Drive");
    }

    #[test]
    fn defaults_are_stable() {
        let c = Config::default();
        assert_eq!(c.rc_addr, "127.0.0.1:5572");
        assert_eq!(c.service, "rclone-gdrive.service");
    }
}
