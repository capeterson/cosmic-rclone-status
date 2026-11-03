// SPDX-License-Identifier: GPL-3.0-only
//! "Manage Sharing": open a mounted file or folder in its cloud provider's web UI with the
//! sharing dialog open. Only Google Drive for now; add a [`Provider`] variant (and its URL in
//! [`sharing_url`]) to support another backend.

use crate::config::{expand, Config};
use crate::rc;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GoogleDrive,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "Google Drive",
        }
    }

    /// The provider for an rclone backend `type` (as in `rclone config`).
    fn from_backend(kind: &str) -> Option<Self> {
        match kind {
            "drive" => Some(Provider::GoogleDrive),
            _ => None,
        }
    }
}

/// A mounted item that can be shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub provider: Provider,
    /// The provider's own ID for the item.
    pub id: String,
    pub is_dir: bool,
    /// The rclone remote the mount shows (e.g. `gdrive:`).
    pub fs: String,
    /// The item's path relative to the mount root.
    pub rel: String,
}

/// The path of `path` relative to the mount root (no leading slash; empty for the root itself),
/// or `None` if it is outside the mount or tries to climb out of it.
pub fn remote_path(mount: &str, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(mount.trim_end_matches('/')).ok()?;
    let rel = rel.to_str()?;
    (!rel.split('/').any(|c| c == "..")).then(|| rel.trim_matches('/').to_string())
}

/// The web URL that opens `t` with the sharing dialog showing.
///
/// `sharingaction=manageaccess` is the parameter Google's own "Manage access" links use; it is
/// not a documented API, so if Google changes it the item still opens, just without the dialog.
pub fn sharing_url(t: &Target) -> String {
    match t.provider {
        Provider::GoogleDrive => {
            let id = &t.id;
            if t.is_dir {
                format!("https://drive.google.com/drive/folders/{id}?sharingaction=manageaccess")
            } else {
                format!("https://drive.google.com/file/d/{id}/view?sharingaction=manageaccess")
            }
        }
    }
}

/// Work out what the single selected item is, asking rclone for its provider ID.
pub fn resolve(cfg: &Config, paths: &[String]) -> Result<Target, String> {
    resolve_with(cfg, paths, rc::mount_fs)
}

/// [`resolve`], with the "which remote is mounted here" lookup supplied by the caller.
fn resolve_with(
    cfg: &Config,
    paths: &[String],
    mount_fs: impl Fn(&str) -> Option<String>,
) -> Result<Target, String> {
    let [path] = paths else {
        return Err("Select a single file or folder".into());
    };
    let mount = expand(&cfg.mount_path);
    let rel = remote_path(&mount, Path::new(path))
        .ok_or_else(|| format!("Not inside the rclone mount ({mount})"))?;
    if rel.is_empty() {
        return Err("Select an item inside the mount, not the mount itself".into());
    }
    let fs = mount_fs(&cfg.mount_path).ok_or("The remote is not mounted")?;
    let name = fs.split(':').next().unwrap_or_default();
    let conf = rc::rc_request(cfg, "config/get", &json!({ "name": name }))?;
    let provider = conf
        .get("type")
        .and_then(Value::as_str)
        .and_then(Provider::from_backend)
        .ok_or("Sharing is not supported for this kind of remote")?;
    let stat = rc::rc_request(cfg, "operations/stat", &json!({ "fs": fs, "remote": rel }))?;
    let item = stat.get("item").filter(|i| !i.is_null());
    let id = item
        .and_then(|i| i.get("ID"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("The remote did not report an ID for this item")?;
    let is_dir = item
        .and_then(|i| i.get("IsDir"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Target {
        provider,
        id: id.to_string(),
        is_dir,
        fs,
        rel,
    })
}

/// Open `url` with the default browser.
pub fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_maps_into_the_mount() {
        let m = "/home/u/Drive";
        assert_eq!(
            remote_path(m, Path::new("/home/u/Drive/a/b.txt")).as_deref(),
            Some("a/b.txt")
        );
        assert_eq!(
            remote_path("/home/u/Drive/", Path::new("/home/u/Drive/a")).as_deref(),
            Some("a")
        );
        assert_eq!(
            remote_path(m, Path::new("/home/u/Drive")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn remote_path_rejects_outside_paths() {
        let m = "/home/u/Drive";
        assert_eq!(remote_path(m, Path::new("/home/u/Other/x")), None);
        assert_eq!(remote_path(m, Path::new("/home/u/DriveX/x")), None);
        assert_eq!(remote_path(m, Path::new("/home/u/Drive/../x")), None);
    }

    #[test]
    fn drive_urls_differ_for_files_and_folders() {
        let t = |is_dir| Target {
            provider: Provider::GoogleDrive,
            id: "abc".into(),
            is_dir,
            fs: "gdrive:".into(),
            rel: "a".into(),
        };
        assert_eq!(
            sharing_url(&t(false)),
            "https://drive.google.com/file/d/abc/view?sharingaction=manageaccess"
        );
        assert_eq!(
            sharing_url(&t(true)),
            "https://drive.google.com/drive/folders/abc?sharingaction=manageaccess"
        );
    }

    #[test]
    fn only_drive_backends_are_supported() {
        assert_eq!(Provider::from_backend("drive"), Some(Provider::GoogleDrive));
        assert_eq!(Provider::from_backend("s3"), None);
    }

    #[test]
    fn resolve_needs_exactly_one_item() {
        let cfg = Config::default();
        assert!(resolve(&cfg, &[]).is_err());
        assert!(resolve(&cfg, &["/a".into(), "/b".into()]).is_err());
    }

    #[test]
    fn resolve_rejects_paths_outside_the_mount() {
        let cfg = Config::default();
        let err = resolve(&cfg, &["/definitely/elsewhere".into()]).unwrap_err();
        assert!(err.contains("Not inside"), "{err}");
    }

    // ---- resolve, against a fake rc API ----

    use crate::testutil::{ok, FakeServer};

    fn cfg_for(s: &FakeServer) -> Config {
        Config {
            rc_addr: s.addr.clone(),
            env_file: "/nonexistent/rc.env".into(),
            mount_path: "/m".into(),
            ..Config::default()
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn mounted(_: &str) -> Option<String> {
        Some("gdrive:".into())
    }

    fn drive_server(stat: &str) -> FakeServer {
        FakeServer::start(vec![
            ok("/config/get", r#"{"type":"drive"}"#),
            ok("/operations/stat", stat),
        ])
    }

    #[test]
    fn resolves_a_file_through_rc() {
        let s = drive_server(r#"{"item":{"ID":"F1","IsDir":false}}"#);
        let t = resolve_with(&cfg_for(&s), &["/m/a/b.txt".into()], mounted).unwrap();
        assert_eq!(
            t,
            Target {
                provider: Provider::GoogleDrive,
                id: "F1".into(),
                is_dir: false,
                fs: "gdrive:".into(),
                rel: "a/b.txt".into(),
            }
        );
        // It asked about the right remote and path.
        let reqs = s.requests();
        let stat = reqs
            .iter()
            .find(|r| r.path() == "/operations/stat")
            .unwrap();
        let body: Value = serde_json::from_str(&stat.body).unwrap();
        assert_eq!(body, json!({"fs": "gdrive:", "remote": "a/b.txt"}));
        let conf = reqs.iter().find(|r| r.path() == "/config/get").unwrap();
        let body: Value = serde_json::from_str(&conf.body).unwrap();
        assert_eq!(body, json!({"name": "gdrive"}));
    }

    #[test]
    fn resolves_a_folder() {
        let s = drive_server(r#"{"item":{"ID":"D1","IsDir":true}}"#);
        let t = resolve_with(&cfg_for(&s), &["/m/Docs".into()], mounted).unwrap();
        assert!(t.is_dir);
        assert_eq!(t.id, "D1");
    }

    #[test]
    fn remote_with_a_subpath_keeps_it_as_the_fs() {
        let s = drive_server(r#"{"item":{"ID":"F1"}}"#);
        let t = resolve_with(&cfg_for(&s), &["/m/x".into()], |_| {
            Some("gdrive:sub/dir".into())
        })
        .unwrap();
        assert_eq!(t.fs, "gdrive:sub/dir");
        // The config section is the part before the colon.
        let conf = s.requests().into_iter().find(|r| r.path() == "/config/get");
        assert!(conf.unwrap().body.contains(r#""name":"gdrive""#));
    }

    #[test]
    fn other_backends_are_not_supported() {
        let s = FakeServer::start(vec![ok("/config/get", r#"{"type":"s3"}"#)]);
        let err = resolve_with(&cfg_for(&s), &["/m/x".into()], mounted).unwrap_err();
        assert!(err.contains("not supported"), "{err}");
        // Never asked for an ID it can't use.
        assert!(!s.paths().contains(&"/operations/stat".to_string()));
    }

    #[test]
    fn missing_or_empty_ids_are_reported() {
        for stat in [
            r#"{"item":null}"#,
            "{}",
            r#"{"item":{"ID":""}}"#,
            r#"{"item":{"Name":"x"}}"#,
        ] {
            let s = drive_server(stat);
            let err = resolve_with(&cfg_for(&s), &["/m/x".into()], mounted).unwrap_err();
            assert!(err.contains("did not report an ID"), "{stat}: {err}");
        }
    }

    #[test]
    fn unmounted_remote_fails_before_any_request() {
        let s = drive_server("{}");
        let err = resolve_with(&cfg_for(&s), &["/m/x".into()], |_| None).unwrap_err();
        assert!(err.contains("not mounted"), "{err}");
        assert_eq!(s.paths(), Vec::<String>::new());
    }

    #[test]
    fn the_mount_root_itself_is_not_shareable() {
        let s = drive_server("{}");
        let err = resolve_with(&cfg_for(&s), &["/m".into()], mounted).unwrap_err();
        assert!(err.contains("not the mount itself"), "{err}");
    }

    #[test]
    fn rc_being_down_is_an_error_not_a_panic() {
        let cfg = Config {
            rc_addr: "127.0.0.1:1".into(),
            env_file: "/nonexistent/rc.env".into(),
            mount_path: "/m".into(),
            ..Config::default()
        };
        assert!(resolve_with(&cfg, &["/m/x".into()], mounted).is_err());
    }
}
