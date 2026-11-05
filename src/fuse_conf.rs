// SPDX-License-Identifier: GPL-3.0-only
//! The one system-wide setting offline folders need: `user_allow_other` in `/etc/fuse.conf`.
//!
//! An offline folder is a second FUSE mount inside the Drive mount. Setting it up goes through
//! the setuid `fusermount3` helper, and the kernel only lets a setuid process into another
//! user's FUSE mount when that mount was made with `allow_other`. Ordinary users may only ask
//! for `allow_other` when the administrator has enabled `user_allow_other`, so that line is
//! needed once, with root. The Drive mount then keeps everyone else out through file
//! permissions (see `units.rs`).

use std::process::Command;

pub const PATH: &str = "/etc/fuse.conf";

/// The backup's name is `<file>` + this + a timestamp + `.bak`, next to the original.
pub const BACKUP_INFIX: &str = ".cosmic-rclone-ui-";

/// Run as `sh -c ENABLE_SCRIPT sh <fuse.conf path>` (through pkexec for the real file). Adds the
/// line unless it's already there, creating the file if needed. Before changing an existing file
/// it copies it to a timestamped backup beside it and prints the backup's path; if the copy
/// fails nothing is changed.
const ENABLE_SCRIPT: &str = r#"f="$1"
if [ -f "$f" ] && grep -qxE '[[:space:]]*user_allow_other[[:space:]]*' "$f"; then exit 0; fi
if [ -e "$f" ]; then
  b="$f.cosmic-rclone-ui-$(date +%Y%m%d-%H%M%S).bak"
  cp -p "$f" "$b" || exit 3
  echo "$b"
fi
printf '\n# Added by cosmic-rclone-ui for offline folders\nuser_allow_other\n' >> "$f" || exit 4
"#;

/// Run the script on `path`, through `prefix` (e.g. `pkexec`) if given. Returns the backup's
/// path when the file was changed and a backup made.
fn run_script(prefix: Option<&str>, path: &str) -> Result<Option<String>, String> {
    let mut cmd = match prefix {
        Some(p) => {
            let mut c = Command::new(p);
            c.arg("/bin/sh");
            c
        }
        None => Command::new("/bin/sh"),
    };
    let out = cmd
        .args(["-c", ENABLE_SCRIPT, "sh", path])
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("Couldn't run {}: {e}", prefix.unwrap_or("sh")))?;
    let backup = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.status.code() {
        Some(0) => Ok((!backup.is_empty()).then_some(backup)),
        // pkexec: 126 = the user dismissed the dialog or wasn't authorized, 127 = no agent.
        Some(126) if prefix.is_some() => Err("Permission was not granted".into()),
        Some(127) if prefix.is_some() => Err("No authentication agent is available".into()),
        Some(3) => Err(format!("Couldn't back up {path}, so it was left unchanged")),
        Some(4) => Err(format!("Couldn't write {path}")),
        _ => Err(format!("Updating {path} failed ({})", out.status)),
    }
}

/// Is `user_allow_other` set in this `fuse.conf` text?
pub fn allows_other_in(text: &str) -> bool {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .any(|l| l == "user_allow_other")
}

pub fn allows_other() -> bool {
    std::fs::read_to_string(PATH).is_ok_and(|t| allows_other_in(&t))
}

/// Ask for an administrator's password (polkit) and add `user_allow_other`, backing the file up
/// first. Returns the backup's path, if one was made.
pub fn enable() -> Result<Option<String>, String> {
    run_script(Some("pkexec"), PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_setting() {
        assert!(allows_other_in("user_allow_other\n"));
        assert!(allows_other_in(
            "# c\n  user_allow_other  # why\nmount_max = 1000\n"
        ));
    }

    #[test]
    fn ignores_commented_or_missing_setting() {
        let debian_default = "# mount_max = 1000\n#user_allow_other\n";
        assert!(!allows_other_in(debian_default));
        assert!(!allows_other_in(""));
        assert!(!allows_other_in("user_allow_other_not\n"));
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rcs-fuseconf-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn backups(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(BACKUP_INFIX))
            .collect()
    }

    #[test]
    fn backs_up_the_original_before_adding_the_line() {
        let d = tmp("backup");
        let f = d.join("fuse.conf");
        let original = "# mount_max = 1000\n#user_allow_other\n";
        std::fs::write(&f, original).unwrap();
        let backup = run_script(None, f.to_str().unwrap())
            .unwrap()
            .expect("a backup");
        assert!(
            backup.starts_with(&format!("{}{BACKUP_INFIX}", f.display())),
            "{backup}"
        );
        assert!(std::path::Path::new(&backup)
            .extension()
            .is_some_and(|e| e == "bak"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        let now = std::fs::read_to_string(&f).unwrap();
        assert!(now.starts_with(original), "existing lines are kept");
        assert!(allows_other_in(&now));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn already_set_means_no_change_and_no_backup() {
        let d = tmp("noop");
        let f = d.join("fuse.conf");
        std::fs::write(&f, "user_allow_other\n").unwrap();
        assert_eq!(run_script(None, f.to_str().unwrap()).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "user_allow_other\n");
        assert_eq!(backups(&d), Vec::<std::path::PathBuf>::new());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_missing_file_is_created_without_a_backup() {
        let d = tmp("missing");
        let f = d.join("fuse.conf");
        assert_eq!(run_script(None, f.to_str().unwrap()).unwrap(), None);
        assert!(allows_other_in(&std::fs::read_to_string(&f).unwrap()));
        assert_eq!(backups(&d), Vec::<std::path::PathBuf>::new());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_failed_backup_leaves_the_file_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("nobackup");
        let sub = d.join("ro");
        std::fs::create_dir_all(&sub).unwrap();
        let f = sub.join("fuse.conf");
        std::fs::write(&f, "#user_allow_other\n").unwrap();
        // The file stays writable, but its folder doesn't take new files, so the backup fails.
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = run_script(None, f.to_str().unwrap()).unwrap_err();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("Couldn't back up"), "{err}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "#user_allow_other\n");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_backup_name_in_the_script_matches_the_constant() {
        assert!(ENABLE_SCRIPT.contains(&format!("$f{BACKUP_INFIX}")));
    }
}
