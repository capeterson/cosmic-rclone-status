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

/// Adds the line unless it's already there (creating the file if needed). Run through pkexec.
const ENABLE_SCRIPT: &str = "grep -qxE '[[:space:]]*user_allow_other[[:space:]]*' /etc/fuse.conf \
     || printf '\\n# Added by cosmic-rclone-status for offline folders\\nuser_allow_other\\n' \
     >> /etc/fuse.conf";

/// Is `user_allow_other` set in this `fuse.conf` text?
pub fn allows_other_in(text: &str) -> bool {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .any(|l| l == "user_allow_other")
}

pub fn allows_other() -> bool {
    std::fs::read_to_string(PATH).is_ok_and(|t| allows_other_in(&t))
}

/// Ask for an administrator's password (polkit) and add `user_allow_other`.
pub fn enable() -> Result<(), String> {
    let status = Command::new("pkexec")
        .args(["/bin/sh", "-c", ENABLE_SCRIPT])
        .status()
        .map_err(|e| format!("Couldn't run pkexec: {e}"))?;
    match status.code() {
        Some(0) => Ok(()),
        // pkexec: 126 = the user dismissed the dialog or wasn't authorized, 127 = no agent.
        Some(126) => Err("Permission was not granted".into()),
        Some(127) => Err("No authentication agent is available".into()),
        _ => Err(format!("Updating {PATH} failed ({status})")),
    }
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

    #[test]
    fn enable_script_is_idempotent_and_only_touches_fuse_conf() {
        assert!(ENABLE_SCRIPT.starts_with("grep -qxE"));
        assert!(ENABLE_SCRIPT.contains("|| printf"));
        assert_eq!(ENABLE_SCRIPT.matches("/etc/fuse.conf").count(), 2);
    }
}
