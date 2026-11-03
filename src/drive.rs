// SPDX-License-Identifier: GPL-3.0-only
//! Google Drive API access using the OAuth token rclone already holds.
//!
//! rclone's remote-control API hands out the current access token (`config/get`), so the applet
//! never reads `rclone.conf` and never refreshes or stores credentials itself. rclone only
//! refreshes an expired token when it makes a call, so we make a cheap one first.

use crate::config::Config;
use crate::rc;
use crate::share::Target;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Owner,
    Editor,
    Commenter,
    Viewer,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Owner => "Owner",
            Role::Editor => "Editor",
            Role::Commenter => "Commenter",
            Role::Viewer => "Viewer",
        }
    }

    fn from_api(role: &str) -> Self {
        match role {
            "owner" => Role::Owner,
            "organizer" | "fileOrganizer" | "writer" => Role::Editor,
            "commenter" => Role::Commenter,
            _ => Role::Viewer,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Group,
    Domain,
    Anyone,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::User => "User",
            Kind::Group => "Group",
            Kind::Domain => "Domain",
            Kind::Anyone => "Anyone with link",
        }
    }
}

/// One row of "who has access".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    /// Display name, or the email/domain when Drive gives no name.
    pub name: String,
    /// Email address (users and groups) or domain; empty for "anyone".
    pub address: String,
    pub role: Role,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriveInfo {
    pub modified: Option<Timestamp>,
    pub size: Option<u64>,
    pub version: Option<String>,
    pub shared: bool,
    pub owned_by_me: bool,
    /// Sorted owner first, then "anyone with the link", then by name. Empty when Drive hides it.
    pub permissions: Vec<Permission>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFile {
    size: Option<String>,
    version: Option<String>,
    #[serde(rename = "modifiedTime")]
    modified_time: Option<String>,
    shared: bool,
    #[serde(rename = "ownedByMe")]
    owned_by_me: bool,
    permissions: Vec<RawPermission>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPermission {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    #[serde(rename = "emailAddress")]
    email: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    domain: Option<String>,
    deleted: bool,
}

fn permission(p: RawPermission) -> Option<Permission> {
    if p.deleted {
        return None;
    }
    let kind = match p.kind.as_str() {
        "user" => Kind::User,
        "group" => Kind::Group,
        "domain" => Kind::Domain,
        _ => Kind::Anyone,
    };
    let address = match kind {
        Kind::User | Kind::Group => p.email.unwrap_or_default(),
        Kind::Domain => p.domain.unwrap_or_default(),
        Kind::Anyone => String::new(),
    };
    let name = match kind {
        Kind::Anyone => "Anyone with the link".to_string(),
        _ => p
            .display_name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| address.clone()),
    };
    Some(Permission {
        name,
        address,
        role: Role::from_api(&p.role),
        kind,
    })
}

/// Parse a `files.get` reply (see [`FIELDS`]).
pub fn parse(json: &Value) -> Result<DriveInfo, String> {
    let raw: RawFile = serde_json::from_value(json.clone()).map_err(|e| e.to_string())?;
    let mut permissions: Vec<Permission> =
        raw.permissions.into_iter().filter_map(permission).collect();
    // Owner first, then "anyone with the link", then everyone else by name.
    permissions.sort_by_key(|p| {
        let rank = match (p.role, p.kind) {
            (Role::Owner, _) => 0,
            (_, Kind::Anyone) => 1,
            _ => 2,
        };
        (rank, p.name.to_lowercase())
    });
    Ok(DriveInfo {
        modified: raw.modified_time.and_then(|t| t.parse().ok()),
        size: raw.size.and_then(|s| s.parse().ok()),
        version: raw.version,
        shared: raw.shared,
        owned_by_me: raw.owned_by_me,
        permissions,
    })
}

const FIELDS: &str = "id,size,version,modifiedTime,shared,ownedByMe,\
permissions(type,role,emailAddress,displayName,domain,deleted)";

/// A current access token for the mounted remote, via rclone.
fn access_token(cfg: &Config, target: &Target) -> Result<String, String> {
    // Any call that touches the remote makes rclone refresh an expired token.
    let _ = rc::rc_request(
        cfg,
        "operations/stat",
        &json!({ "fs": target.fs, "remote": target.rel }),
    );
    let name = target.fs.split(':').next().unwrap_or_default();
    let conf = rc::rc_request(cfg, "config/get", &json!({ "name": name }))?;
    let token = conf
        .get("token")
        .and_then(Value::as_str)
        .ok_or("rclone has no token for this remote")?;
    serde_json::from_str::<Value>(token)
        .ok()
        .and_then(|t| t.get("access_token")?.as_str().map(str::to_string))
        .ok_or_else(|| "rclone's token is unreadable".to_string())
}

/// Ask Google Drive about `target`: modified time, sharing state and who has access.
pub fn fetch(cfg: &Config, target: &Target) -> Result<DriveInfo, String> {
    fetch_from(API_BASE, cfg, target)
}

const API_BASE: &str = "https://www.googleapis.com";

/// [`fetch`] against another API host (tests point this at a local fake).
fn fetch_from(base: &str, cfg: &Config, target: &Target) -> Result<DriveInfo, String> {
    let token = access_token(cfg, target)?;
    let reply = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
        .get(&format!("{base}/drive/v3/files/{}", target.id))
        .query("fields", FIELDS)
        .query("supportsAllDrives", "true")
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    match reply {
        Ok(r) => parse(&r.into_json::<Value>().map_err(|e| e.to_string())?),
        Err(ureq::Error::Status(403, _)) => Err("Google Drive denied access to this item".into()),
        Err(ureq::Error::Status(404, _)) => Err("Google Drive can't find this item".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "id": "abc",
            "size": "190",
            "version": "10",
            "modifiedTime": "2026-05-10T02:33:55.578Z",
            "shared": true,
            "ownedByMe": true,
            "permissions": [
                {"type": "user", "role": "reader", "emailAddress": "zed@example.com", "displayName": "Zed"},
                {"type": "anyone", "role": "reader"},
                {"type": "user", "role": "owner", "emailAddress": "me@example.com", "displayName": "Me"},
                {"type": "user", "role": "writer", "emailAddress": "amy@example.com"},
                {"type": "user", "role": "reader", "emailAddress": "gone@example.com", "deleted": true},
                {"type": "domain", "role": "commenter", "domain": "example.com"}
            ]
        })
    }

    #[test]
    fn parses_basic_fields() {
        let d = parse(&sample()).unwrap();
        assert_eq!(d.size, Some(190));
        assert_eq!(d.version.as_deref(), Some("10"));
        assert!(d.shared && d.owned_by_me);
        assert_eq!(
            d.modified.map(|t| t.to_string()).as_deref(),
            Some("2026-05-10T02:33:55.578Z")
        );
    }

    #[test]
    fn permissions_sorted_owner_first_and_deleted_dropped() {
        let d = parse(&sample()).unwrap();
        let names: Vec<_> = d.permissions.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Me",
                "Anyone with the link",
                "amy@example.com",
                "example.com",
                "Zed"
            ]
        );
        assert_eq!(d.permissions[0].role, Role::Owner);
    }

    #[test]
    fn anyone_has_no_address() {
        let d = parse(&sample()).unwrap();
        let any = d
            .permissions
            .iter()
            .find(|p| p.kind == Kind::Anyone)
            .unwrap();
        assert_eq!(any.address, "");
        assert_eq!(any.role, Role::Viewer);
    }

    #[test]
    fn unshared_item_parses_with_no_permissions() {
        let d = parse(&json!({"id": "x", "shared": false})).unwrap();
        assert!(!d.shared);
        assert_eq!(d.permissions.len(), 0);
        assert_eq!(d.size, None);
    }

    #[test]
    fn role_mapping() {
        assert_eq!(Role::from_api("organizer"), Role::Editor);
        assert_eq!(Role::from_api("fileOrganizer"), Role::Editor);
        assert_eq!(Role::from_api("whatever"), Role::Viewer);
    }

    // ---- fetch, against fake rc and Drive servers ----

    use crate::testutil::{ok, status, FakeServer};

    const TOKEN_CONF: &str = r#"{"type":"drive","token":"{\"access_token\":\"TOK\",\"expiry\":\"2030-01-01T00:00:00Z\"}"}"#;

    fn target() -> Target {
        Target {
            provider: crate::share::Provider::GoogleDrive,
            id: "F1".into(),
            is_dir: false,
            fs: "gdrive:".into(),
            rel: "a.txt".into(),
        }
    }

    fn cfg_for(rc: &FakeServer) -> Config {
        Config {
            rc_addr: rc.addr.clone(),
            env_file: "/nonexistent/rc.env".into(),
            ..Config::default()
        }
    }

    fn rc_server(conf: &str) -> FakeServer {
        FakeServer::start(vec![
            ok("/operations/stat", r#"{"item":{"ID":"F1"}}"#),
            ok("/config/get", conf),
        ])
    }

    fn api_server(code: u16, body: &str) -> FakeServer {
        FakeServer::start(vec![crate::testutil::Route {
            path: "/drive/v3/files/F1",
            status: code,
            body: body.to_string(),
        }])
    }

    fn base(s: &FakeServer) -> String {
        format!("http://{}", s.addr)
    }

    #[test]
    fn fetch_uses_rclones_token_and_parses_the_reply() {
        let rc = rc_server(TOKEN_CONF);
        let api = api_server(200, &sample().to_string());
        let info = fetch_from(&base(&api), &cfg_for(&rc), &target()).unwrap();
        assert!(info.shared);
        assert_eq!(info.permissions.len(), 5);

        let req = &api.requests()[0];
        assert_eq!(req.authorization.as_deref(), Some("Bearer TOK"));
        assert!(
            req.target.contains("supportsAllDrives=true"),
            "{}",
            req.target
        );
        assert!(req.target.contains("permissions"), "{}", req.target);
    }

    #[test]
    fn rclone_is_touched_before_its_token_is_read() {
        // rclone only refreshes an expired token when it makes a call, so the stat has to
        // come first.
        let rc = rc_server(TOKEN_CONF);
        let api = api_server(200, "{}");
        fetch_from(&base(&api), &cfg_for(&rc), &target()).unwrap();
        assert_eq!(rc.paths(), ["/operations/stat", "/config/get"]);
    }

    #[test]
    fn a_failing_stat_does_not_block_the_lookup() {
        let rc = FakeServer::start(vec![
            status("/operations/stat", 500),
            ok("/config/get", TOKEN_CONF),
        ]);
        let api = api_server(200, "{}");
        assert!(fetch_from(&base(&api), &cfg_for(&rc), &target()).is_ok());
    }

    #[test]
    fn drive_errors_are_turned_into_readable_messages() {
        let rc = rc_server(TOKEN_CONF);
        let denied = fetch_from(&base(&api_server(403, "{}")), &cfg_for(&rc), &target());
        assert!(denied.unwrap_err().contains("denied"));
        let gone = fetch_from(&base(&api_server(404, "{}")), &cfg_for(&rc), &target());
        assert!(gone.unwrap_err().contains("can't find"));
        let broken = fetch_from(&base(&api_server(500, "{}")), &cfg_for(&rc), &target());
        assert!(broken.is_err());
    }

    #[test]
    fn missing_or_unreadable_tokens_are_reported() {
        let api = api_server(200, "{}");
        for (conf, want) in [
            (r#"{"type":"drive"}"#, "no token"),
            (r#"{"token":"not json"}"#, "unreadable"),
            (r#"{"token":"{\"refresh_token\":\"r\"}"}"#, "unreadable"),
        ] {
            let rc = rc_server(conf);
            let err = fetch_from(&base(&api), &cfg_for(&rc), &target()).unwrap_err();
            assert!(err.contains(want), "{conf}: {err}");
        }
        // Nothing was sent to Google without a token.
        assert!(api.requests().is_empty());
    }

    #[test]
    fn rc_being_down_is_an_error() {
        let cfg = Config {
            rc_addr: "127.0.0.1:1".into(),
            env_file: "/nonexistent/rc.env".into(),
            ..Config::default()
        };
        assert!(fetch_from("http://127.0.0.1:1", &cfg, &target()).is_err());
    }
}
