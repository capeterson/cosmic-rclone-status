// SPDX-License-Identifier: GPL-3.0-only
//! The oldest rclone this app supports, and the check the popup uses to warn about older ones.
//!
//! **Keep this current.** Whenever code starts relying on a newer rclone feature (or drops a
//! workaround for an old one), raise [`MIN_RCLONE`] and the "Requirements" line in the README
//! (a test fails if they disagree), and verify the change against that exact version.

/// Minimum supported rclone, as `MAJOR.MINOR.PATCH`. Last verified: 1.75.2.
pub const MIN_RCLONE: &str = "1.75.0";

/// `v1.75.2`, `1.75.2` and `v1.75.2-beta.123.abcdef` all parse to `(1, 75, 2)`.
pub fn parse(version: &str) -> Option<(u32, u32, u32)> {
    let v = version.trim().trim_start_matches('v');
    let core = v.split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // Some builds report only MAJOR.MINOR.
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

/// Is this rclone older than [`MIN_RCLONE`]? An unreadable version is not reported as old: the
/// alert is for versions we know are too old, not for odd builds.
pub fn is_too_old(version: &str) -> bool {
    match (parse(version), parse(MIN_RCLONE)) {
        (Some(have), Some(min)) => have < min,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_beta_names() {
        assert_eq!(parse("v1.75.2"), Some((1, 75, 2)));
        assert_eq!(parse("1.60.1"), Some((1, 60, 1)));
        assert_eq!(parse("v1.76.0-beta.9012.abc123"), Some((1, 76, 0)));
        assert_eq!(parse("v1.75"), Some((1, 75, 0)));
        assert_eq!(parse("garbage"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn compares_numerically_not_as_text() {
        assert!(is_too_old("v1.60.1"));
        assert!(is_too_old("v1.9.0"));
        assert!(!is_too_old("v1.75.0"));
        assert!(!is_too_old("v1.75.2"));
        assert!(!is_too_old("v1.100.0"));
        assert!(!is_too_old("v2.0.0"));
    }

    #[test]
    fn unreadable_versions_are_not_flagged() {
        assert!(!is_too_old("custom-build"));
    }

    #[test]
    fn minimum_parses() {
        assert!(parse(MIN_RCLONE).is_some());
    }

    /// The README must state the same minimum the code enforces.
    #[test]
    fn readme_states_the_minimum() {
        let readme = include_str!("../README.md");
        assert!(
            readme.contains(&format!("rclone {MIN_RCLONE} or newer")),
            "README.md must say \"rclone {MIN_RCLONE} or newer\" (see src/version.rs)"
        );
    }
}
