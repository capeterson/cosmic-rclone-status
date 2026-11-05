// SPDX-License-Identifier: GPL-3.0-only
//! Resolve bisync conflicts as "newer wins, the older edit stays in the remote's version
//! history".
//!
//! When a file changed on both sides between syncs, bisync (asked for `--conflict-loser pathname
//! --conflict-suffix .path`; its default names changed in rclone 1.66) renames both copies and
//! puts both on both sides: `X..path1` is the remote's version and `X..path2` the local one. For
//! each such pair we:
//! 1. rename `X..path1` back to `X` on the remote, which keeps the file's identity and history;
//! 2. upload the loser over `X`, then the winner (so the loser becomes an older version);
//! 3. delete `X..path2` on the remote, and leave just the winner as `X` locally.
//!
//! Every step checks what's already done, so a failure part way is fixed by running it again.

use std::path::{Path, PathBuf};

pub const P1: &str = "..path1";
pub const P2: &str = "..path2";

/// The remote operations conflict resolution needs, so tests can run without rclone.
pub trait Remote {
    fn exists(&self, path: &str) -> Result<bool, String>;
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    /// Upload `local` (a file inside the local copy, given relative to it) over `to`.
    fn upload(&self, local: &str, to: &str) -> Result<(), String>;
    fn delete(&self, path: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    /// Both edits were byte-for-byte the same; nothing went into history.
    Same,
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The file's path inside the pinned folder.
    pub path: String,
    pub winner: Winner,
}

impl Resolved {
    pub fn notice(&self, pin: &str) -> String {
        let name = format!("{pin}/{}", self.path);
        match self.winner {
            Winner::Same => format!("{name} was changed the same way on both sides"),
            Winner::Local => format!(
                "{name} was changed in two places; kept this computer's newer version (the other \
                 is in the file's version history)"
            ),
            Winner::Remote => format!(
                "{name} was changed in two places; kept the newer version from the cloud (this \
                 computer's is in the file's version history)"
            ),
        }
    }
}

/// Conflict pairs under `root`: the base path `X` (relative, `/`-separated) of every `X..path1`
/// that has a matching `X..path2`. A lone half is left alone.
pub fn find_pairs(root: &Path) -> Vec<String> {
    let mut out = vec![];
    walk(root, "", &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), &child, out);
        } else if let Some(base) = name.strip_suffix(P1) {
            if !base.is_empty() && dir.join(format!("{base}{P2}")).is_file() {
                out.push(child.strip_suffix(P1).unwrap_or(&child).to_string());
            }
        }
    }
}

fn same_contents(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Resolve the conflict for `base` (from [`find_pairs`]) in the local copy at `root`.
pub fn resolve(root: &Path, base: &str, remote: &dyn Remote) -> Result<Resolved, String> {
    let (r1, r2) = (format!("{base}{P1}"), format!("{base}{P2}"));
    let (l1, l2): (PathBuf, PathBuf) = (root.join(&r1), root.join(&r2));
    let same = same_contents(&l1, &l2).map_err(|e| e.to_string())?;
    let winner = if same {
        Winner::Same
    } else {
        let (m1, m2) = (
            std::fs::metadata(&l1)
                .and_then(|m| m.modified())
                .map_err(|e| e.to_string())?,
            std::fs::metadata(&l2)
                .and_then(|m| m.modified())
                .map_err(|e| e.to_string())?,
        );
        // Ties go to the cloud copy: it's the one other devices already see.
        if m2 > m1 {
            Winner::Local
        } else {
            Winner::Remote
        }
    };

    // 1. Give the remote's file its name back (a no-op if a previous attempt did it).
    if remote.exists(&r1)? {
        remote.rename(&r1, base)?;
    }
    // 2. Loser first, winner last, so the winner is current and the loser is history.
    match winner {
        Winner::Same => {}
        Winner::Local => remote.upload(&r2, base)?,
        Winner::Remote => {
            remote.upload(&r2, base)?;
            remote.upload(&r1, base)?;
        }
    }
    // 3. Tidy both sides.
    if remote.exists(&r2)? {
        remote.delete(&r2)?;
    }
    let (keep, drop) = if winner == Winner::Local {
        (&l2, &l1)
    } else {
        (&l1, &l2)
    };
    std::fs::rename(keep, root.join(base)).map_err(|e| e.to_string())?;
    std::fs::remove_file(drop).map_err(|e| e.to_string())?;
    Ok(Resolved {
        path: base.to_string(),
        winner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::time::{Duration, SystemTime};

    /// An in-memory remote: a set of paths plus a log of operations.
    #[derive(Default)]
    struct Fake {
        files: RefCell<BTreeSet<String>>,
        log: RefCell<Vec<String>>,
        fail_on: Option<&'static str>,
    }

    impl Fake {
        fn with(paths: &[&str]) -> Self {
            Self {
                files: RefCell::new(paths.iter().map(|s| (*s).to_string()).collect()),
                ..Self::default()
            }
        }
        fn op(&self, s: String) -> Result<(), String> {
            if self.fail_on.is_some_and(|f| s.starts_with(f)) {
                return Err(format!("boom: {s}"));
            }
            self.log.borrow_mut().push(s);
            Ok(())
        }
    }

    impl Remote for Fake {
        fn exists(&self, p: &str) -> Result<bool, String> {
            Ok(self.files.borrow().contains(p))
        }
        fn rename(&self, from: &str, to: &str) -> Result<(), String> {
            self.op(format!("rename {from} -> {to}"))?;
            self.files.borrow_mut().remove(from);
            self.files.borrow_mut().insert(to.into());
            Ok(())
        }
        fn upload(&self, local: &str, to: &str) -> Result<(), String> {
            self.op(format!("upload {local} -> {to}"))?;
            self.files.borrow_mut().insert(to.into());
            Ok(())
        }
        fn delete(&self, p: &str) -> Result<(), String> {
            self.op(format!("delete {p}"))?;
            self.files.borrow_mut().remove(p);
            Ok(())
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rcs-conflict-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str, age_secs: u64) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn pair(
        root: &Path,
        base: &str,
        remote_body: &str,
        remote_age: u64,
        local_body: &str,
        local_age: u64,
    ) {
        write(root, &format!("{base}{P1}"), remote_body, remote_age);
        write(root, &format!("{base}{P2}"), local_body, local_age);
    }

    fn remote_pair(base: &str) -> Fake {
        Fake::with(&[&format!("{base}{P1}"), &format!("{base}{P2}")])
    }

    #[test]
    fn finds_pairs_in_nested_dirs_and_ignores_lone_halves() {
        let d = tmp("find");
        pair(&d, "a.txt", "r", 0, "l", 0);
        pair(&d, "sub/deeper/b..c.txt", "r", 0, "l", 0);
        write(&d, "lonely.txt..path1", "x", 0);
        write(&d, "plain.txt", "x", 0);
        assert_eq!(find_pairs(&d), ["a.txt", "sub/deeper/b..c.txt"]);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn newer_local_wins_and_remote_version_goes_to_history() {
        let d = tmp("local");
        pair(&d, "a.txt", "remote", 100, "local", 10);
        let r = remote_pair("a.txt");
        let res = resolve(&d, "a.txt", &r).unwrap();
        assert_eq!(res.winner, Winner::Local);
        assert_eq!(
            *r.log.borrow(),
            [
                "rename a.txt..path1 -> a.txt",
                "upload a.txt..path2 -> a.txt",
                "delete a.txt..path2"
            ]
        );
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "local");
        assert_eq!(find_pairs(&d), Vec::<String>::new());
        assert!(!d.join("a.txt..path1").exists());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn newer_remote_wins_and_local_edit_goes_to_history() {
        let d = tmp("remote");
        pair(&d, "s/a.txt", "remote", 10, "local", 100);
        let r = remote_pair("s/a.txt");
        let res = resolve(&d, "s/a.txt", &r).unwrap();
        assert_eq!(res.winner, Winner::Remote);
        assert_eq!(
            *r.log.borrow(),
            [
                "rename s/a.txt..path1 -> s/a.txt",
                "upload s/a.txt..path2 -> s/a.txt",
                "upload s/a.txt..path1 -> s/a.txt",
                "delete s/a.txt..path2"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(d.join("s/a.txt")).unwrap(),
            "remote"
        );
        assert!(!d.join("s/a.txt..path2").exists());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn identical_edits_collapse_without_uploading() {
        let d = tmp("same");
        pair(&d, "a.txt", "same", 10, "same", 100);
        let r = remote_pair("a.txt");
        let res = resolve(&d, "a.txt", &r).unwrap();
        assert_eq!(res.winner, Winner::Same);
        assert!(r.log.borrow().iter().all(|l| !l.starts_with("upload")));
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "same");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_failed_attempt_can_simply_be_retried() {
        let d = tmp("retry");
        pair(&d, "a.txt", "remote", 100, "local", 10);
        let mut r = remote_pair("a.txt");
        r.fail_on = Some("delete");
        assert!(resolve(&d, "a.txt", &r).is_err());
        // Nothing local was touched, so the pair is still found.
        assert_eq!(find_pairs(&d), ["a.txt"]);
        r.fail_on = None;
        resolve(&d, "a.txt", &r).unwrap();
        // The rename isn't repeated: the remote file already has its name back.
        let log = r.log.borrow();
        assert_eq!(log.iter().filter(|l| l.starts_with("rename")).count(), 1);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "local");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn notices_name_the_winner() {
        let r = Resolved {
            path: "a.txt".into(),
            winner: Winner::Local,
        };
        assert!(r
            .notice("Work")
            .starts_with("Work/a.txt was changed in two places"));
        assert!(r.notice("Work").contains("this computer's newer version"));
    }
}
