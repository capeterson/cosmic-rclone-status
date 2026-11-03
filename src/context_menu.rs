// SPDX-License-Identifier: GPL-3.0-only
//! Registers the single "Rclone…" entry in COSMIC Files' right-click menu.
//!
//! COSMIC Files has no extension API, only a `context_actions` list in its own config, rendered
//! flat. We add one entry that runs `cosmic-rclone-status --menu %F` (see `menu.rs`); everything
//! else lives in that window. The user's other entries are left alone.

use cosmic::cosmic_config::{Config as CosmicConfig, ConfigGet, ConfigSet};
use serde::{Deserialize, Serialize};

const FILES_APP_ID: &str = "com.system76.CosmicFiles";
const FILES_CONFIG_VERSION: u64 = 1;
const KEY: &str = "context_actions";

/// Mirrors cosmic-files' `ContextActionSelection`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selection {
    #[default]
    #[serde(alias = "any")]
    Any,
    #[serde(alias = "files")]
    Files,
    #[serde(alias = "folders")]
    Folders,
}

/// Mirrors cosmic-files' `ContextActionPreset`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub name: String,
    pub confirm: bool,
    pub selection: Selection,
    pub steps: Vec<String>,
}

/// The menu label. The dev copy gets its own so it doesn't replace the real applet's entry.
pub fn entry_name() -> &'static str {
    if crate::APP_ID.ends_with("Dev") {
        "Rclone (dev)…"
    } else {
        "Rclone…"
    }
}

/// Is `name` one of ours, including the labels earlier prototypes used?
fn is_ours(name: &str) -> bool {
    let legacy = if crate::APP_ID.ends_with("Dev") {
        "rclone (dev): "
    } else {
        "rclone: "
    };
    name == entry_name() || name.starts_with(legacy)
}

/// Quote the executable for an exec-style command line.
fn quote(exe: &str) -> String {
    if exe.contains([' ', '"', '\\']) {
        format!("\"{}\"", exe.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        exe.to_string()
    }
}

pub fn entry(exe: &str) -> Preset {
    Preset {
        name: entry_name().to_string(),
        confirm: false,
        selection: Selection::Any,
        steps: vec![format!("{} --menu %F", quote(exe))],
    }
}

/// `existing` with our entries replaced by `ours`; everything else is kept as is.
pub fn merge(existing: Vec<Preset>, ours: Vec<Preset>) -> Vec<Preset> {
    let mut out: Vec<Preset> = existing.into_iter().filter(|p| !is_ours(&p.name)).collect();
    out.extend(ours);
    out
}

/// The list to write, or `None` when `existing` already has exactly our entries.
fn apply(existing: &[Preset], ours: Vec<Preset>) -> Option<Vec<Preset>> {
    let merged = merge(existing.to_vec(), ours);
    (merged != existing).then_some(merged)
}

fn write_merged_to(ctx: &CosmicConfig, ours: Vec<Preset>) {
    let existing: Vec<Preset> = ctx.get(KEY).unwrap_or_default();
    if let Some(merged) = apply(&existing, ours) {
        let _ = ctx.set(KEY, merged);
    }
}

fn write_merged(ours: Vec<Preset>) {
    if let Ok(ctx) = CosmicConfig::new(FILES_APP_ID, FILES_CONFIG_VERSION) {
        write_merged_to(&ctx, ours);
    }
}

/// Make sure the entry exists and points at this binary. Cheap when nothing changed.
pub fn install() {
    if let Ok(exe) = std::env::current_exe() {
        write_merged(vec![entry(&exe.to_string_lossy())]);
    }
}

/// Remove our entry (`--menu-remove`).
pub fn remove() {
    write_merged(vec![]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(n: &str) -> Preset {
        Preset {
            name: n.into(),
            ..Preset::default()
        }
    }

    #[test]
    fn entry_runs_the_menu_with_the_selection() {
        assert_eq!(entry("/x/rcs").steps, ["/x/rcs --menu %F"]);
        assert_eq!(entry("/my bin/rcs").steps, ["\"/my bin/rcs\" --menu %F"]);
    }

    #[test]
    fn merge_replaces_ours_and_keeps_the_users() {
        let theirs = named("Compress");
        let out = merge(vec![named(entry_name()), theirs.clone()], vec![entry("/x")]);
        assert_eq!(out, vec![theirs, entry("/x")]);
    }

    #[test]
    fn merge_drops_prototype_entries() {
        let legacy = if crate::APP_ID.ends_with("Dev") {
            "rclone (dev): Synced"
        } else {
            "rclone: Synced"
        };
        assert_eq!(merge(vec![named(legacy)], vec![]), vec![]);
    }

    #[test]
    fn merge_is_idempotent() {
        let once = merge(vec![], vec![entry("/x")]);
        assert_eq!(merge(once.clone(), vec![entry("/x")]), once);
    }

    #[test]
    fn apply_does_nothing_when_already_installed() {
        let existing = vec![named("Compress"), entry("/x")];
        assert_eq!(apply(&existing, vec![entry("/x")]), None);
    }

    #[test]
    fn apply_updates_a_stale_path_and_installs_when_missing() {
        let stale = vec![entry("/old/bin")];
        assert_eq!(
            apply(&stale, vec![entry("/new/bin")]),
            Some(vec![entry("/new/bin")])
        );
        assert_eq!(apply(&[], vec![entry("/x")]), Some(vec![entry("/x")]));
    }

    #[test]
    fn apply_removes_only_ours() {
        let existing = vec![named("Compress"), entry("/x")];
        assert_eq!(apply(&existing, vec![]), Some(vec![named("Compress")]));
        assert_eq!(apply(&[named("Compress")], vec![]), None);
    }

    fn temp_config(name: &str) -> (std::path::PathBuf, CosmicConfig) {
        let dir = std::env::temp_dir().join(format!("rcs-ctxcfg-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let ctx = CosmicConfig::with_custom_path(FILES_APP_ID, FILES_CONFIG_VERSION, dir.clone())
            .unwrap();
        (dir, ctx)
    }

    #[test]
    fn writes_the_entry_to_disk_in_files_config_format() {
        let (dir, ctx) = temp_config("write");
        write_merged_to(&ctx, vec![entry("/x/rcs")]);
        let on_disk: Vec<Preset> = ctx.get(KEY).unwrap();
        assert_eq!(on_disk, vec![entry("/x/rcs")]);
        // COSMIC Files reads this file with its own struct, so the field names and the enum
        // spelling must be exactly these.
        let file = dir.join("cosmic").join(FILES_APP_ID).join("v1").join(KEY);
        let text = std::fs::read_to_string(file).unwrap();
        for want in ["name:", "confirm:", "selection: Any", "steps:", "Rclone"] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disk_round_trip_keeps_the_users_entries_and_drops_prototypes() {
        let (dir, ctx) = temp_config("roundtrip");
        let theirs = Preset {
            name: "Compress".into(),
            confirm: true,
            selection: Selection::Folders,
            steps: vec!["tar czf %f.tgz %f".into()],
        };
        let legacy = if crate::APP_ID.ends_with("Dev") {
            "rclone (dev): Synced"
        } else {
            "rclone: Synced"
        };
        ctx.set(KEY, vec![named(legacy), theirs.clone()]).unwrap();

        write_merged_to(&ctx, vec![entry("/x")]);
        let after: Vec<Preset> = ctx.get(KEY).unwrap();
        assert_eq!(after, vec![theirs.clone(), entry("/x")]);

        // Installing again changes nothing; removing takes only ours.
        write_merged_to(&ctx, vec![entry("/x")]);
        assert_eq!(ctx.get::<Vec<Preset>>(KEY).unwrap(), after);
        write_merged_to(&ctx, vec![]);
        assert_eq!(ctx.get::<Vec<Preset>>(KEY).unwrap(), vec![theirs]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
