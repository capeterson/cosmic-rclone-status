// SPDX-License-Identifier: GPL-3.0-only
//! Runs the real binary for the COSMIC Files integration flags, with `HOME` and `XDG_CONFIG_HOME`
//! pointed at a scratch directory so the user's real config is never touched.

use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Sandbox {
    home: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("rcs-cli-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        Self { home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cosmic-rclone-status"))
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .output()
            .unwrap()
    }

    /// COSMIC Files' `context_actions` file.
    fn actions_file(&self) -> PathBuf {
        self.home
            .join("config/cosmic/com.system76.CosmicFiles/v1/context_actions")
    }

    fn actions(&self) -> String {
        std::fs::read_to_string(self.actions_file()).unwrap_or_default()
    }

    fn seed_actions(&self, ron: &str) {
        let file = self.actions_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, ron).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.home).ok();
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const USER_ENTRY: &str =
    r#"[(name: "Compress", confirm: false, selection: Folders, steps: ["tar czf %f.tgz %f"])]"#;

#[test]
fn menu_install_adds_one_entry_that_runs_this_binary() {
    let sb = Sandbox::new("install");
    let out = sb.run(&["--menu-install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = sb.actions();
    assert_eq!(text.matches("name:").count(), 1, "{text}");
    assert!(text.contains("Rclone…"), "{text}");
    assert!(text.contains("--menu %F"), "{text}");
    assert!(text.contains("cosmic-rclone-status"), "{text}");
}

#[test]
fn menu_install_twice_does_not_duplicate() {
    let sb = Sandbox::new("twice");
    sb.run(&["--menu-install"]);
    let first = sb.actions();
    sb.run(&["--menu-install"]);
    assert_eq!(sb.actions(), first);
}

#[test]
fn menu_install_and_remove_leave_the_users_own_entries_alone() {
    let sb = Sandbox::new("keep");
    sb.seed_actions(USER_ENTRY);

    sb.run(&["--menu-install"]);
    let text = sb.actions();
    assert!(
        text.contains("Compress") && text.contains("Rclone…"),
        "{text}"
    );

    let out = sb.run(&["--menu-remove"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = sb.actions();
    assert!(text.contains("Compress"), "{text}");
    assert!(!text.contains("Rclone"), "{text}");
}

#[test]
fn menu_install_replaces_prototype_entries() {
    let sb = Sandbox::new("legacy");
    sb.seed_actions(
        r#"[(name: "rclone: Synced", confirm: false, selection: Any, steps: ["old --ctl status"])]"#,
    );
    sb.run(&["--menu-install"]);
    let text = sb.actions();
    assert!(
        !text.contains("rclone: Synced") && !text.contains("--ctl"),
        "{text}"
    );
    assert!(text.contains("Rclone…"), "{text}");
}

#[test]
fn menu_remove_with_nothing_installed_is_harmless() {
    let sb = Sandbox::new("noop");
    assert!(sb.run(&["--menu-remove"]).status.success());
}

#[test]
fn share_url_refuses_paths_outside_the_mount() {
    let sb = Sandbox::new("outside");
    let out = sb.run(&["--share-url", "/definitely/not/in/the/mount"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Not inside the rclone mount"),
        "{}",
        stderr(&out)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn share_url_reports_an_unmounted_remote() {
    let sb = Sandbox::new("unmounted");
    // Inside the default mount path (~/GoogleDrive), which is not an rclone mount here.
    let path = sb.home.join("GoogleDrive/file.txt");
    let out = sb.run(&["--share-url", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not mounted"), "{}", stderr(&out));
}
