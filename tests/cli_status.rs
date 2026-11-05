// SPDX-License-Identifier: GPL-3.0-only
//! Runs the real binary in `--status` mode. We can't assert on health here (it depends on the
//! host's systemd and /proc/mounts), only that the snapshot path runs and prints a verdict.

use std::process::Command;

#[test]
fn status_mode_prints_a_health_verdict() {
    let home = std::env::temp_dir().join(format!("rcs-cli-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cosmic-rclone-ui"))
        .args(["--status", "--rc-addr", "127.0.0.1:1"])
        // Keep the user's real cosmic-config out of the test.
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home).ok();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("health: "), "{stdout}");
}
