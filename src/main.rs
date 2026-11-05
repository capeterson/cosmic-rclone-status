// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod conflict;
mod context_menu;
mod drive;
mod fuse_conf;
mod keeper;
mod menu;
mod mime_icon;
mod pin;
mod rc;
mod settings;
mod share;
mod status;
#[cfg(test)]
mod testutil;
mod units;
mod version;

/// Build with `APP_ID_OVERRIDE=...` to get a side-by-side dev copy with its own panel
/// identity and config (see `just install-dev`).
pub const APP_ID: &str = match option_env!("APP_ID_OVERRIDE") {
    Some(id) => id,
    None => "com.ca-peterson.CosmicRcloneUi",
};

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = offline_cli(&args) {
        std::process::exit(code);
    }
    // `--settings` opens the settings window (also reachable from the panel popup).
    if args.iter().any(|a| a == "--settings") {
        return settings::run(config::Config::load(APP_ID));
    }
    // `--menu PATH...` is the "rclone…" window COSMIC Files opens from its right-click menu.
    if let Some(i) = args.iter().position(|a| a == "--menu") {
        return menu::run(config::Config::load(APP_ID), args[i + 1..].to_vec());
    }
    // `--menu-install` adds it (the applet also does this on start-up).
    if args.iter().any(|a| a == "--menu-install") {
        context_menu::install();
        return Ok(());
    }
    // `--menu-remove` takes the applet's entry back out of the COSMIC Files right-click menu.
    if args.iter().any(|a| a == "--menu-remove") {
        context_menu::remove();
        return Ok(());
    }
    // `--share-url PATH` prints the "Manage Sharing" link for a mounted item without opening it.
    if let Some(path) = args
        .iter()
        .position(|a| a == "--share-url")
        .and_then(|i| args.get(i + 1))
    {
        let cfg = config::Config::load(APP_ID);
        match share::resolve(&cfg, std::slice::from_ref(path)) {
            Ok(t) => println!("{}", share::sharing_url(&t)),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `cosmic-rclone-ui --status [--rc-addr HOST:PORT]` prints one status snapshot and
    // exits (handy for debugging and used by the smoke test).
    if args.iter().any(|a| a == "--status") {
        let mut cfg = config::Config::load(APP_ID);
        if let Some(addr) = args
            .iter()
            .position(|a| a == "--rc-addr")
            .and_then(|i| args.get(i + 1))
        {
            cfg.rc_addr.clone_from(addr);
        }
        let mut st = rc::fetch(&cfg);
        let recent = std::mem::take(&mut st.recent);
        println!("{st:#?}\nhealth: {:?}", st.health());
        println!("recent: {} entries", recent.len());
        for t in recent.iter().take(10) {
            println!("  {t:?}");
        }
        return Ok(());
    }
    cosmic::applet::run::<app::AppModel>(())
}

/// The value after `flag`, if present.
fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Offline-folder commands that don't open a window. Returns the exit code if one ran.
fn offline_cli(args: &[String]) -> Option<i32> {
    let report = |r: Result<String, String>| match r {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    // Run by the systemd units this app installs (see `units.rs`).
    if args.iter().any(|a| a == "--offline-keeper") {
        keeper::run();
    }
    if let Some(id) = arg(args, "--pin-mount") {
        return Some(report(keeper::pin_mount(id).map(|()| String::new())));
    }
    if let Some(id) = arg(args, "--pin-unmount") {
        keeper::pin_unmount(id);
        return Some(0);
    }
    // `--pin PATH` / `--unpin PATH` keep a folder offline or stop doing so.
    if let Some(p) = arg(args, "--pin") {
        return Some(report(
            keeper::set_pinned(p, true).map(|r| format!("Keeping {r} available offline")),
        ));
    }
    if let Some(p) = arg(args, "--unpin") {
        return Some(report(
            keeper::set_pinned(p, false).map(|r| format!("No longer keeping {r} offline")),
        ));
    }
    // `--offline-repair FOLDER` asks the keeper to rebuild a folder's sync baseline.
    if let Some(rel) = arg(args, "--offline-repair") {
        return Some(report(
            keeper::request_repair(rel)
                .map(|()| format!("Repair of {rel} requested"))
                .map_err(|e| e.to_string()),
        ));
    }
    // `--offline-remove` takes out the units and mount drop-in (used when uninstalling). It
    // refuses while folders are kept offline, so local edits are never dropped unseen.
    if args.iter().any(|a| a == "--offline-remove") {
        let cfg = config::Config::load(APP_ID);
        if !cfg.pins.is_empty() {
            return Some(report(Err(format!(
                "{} folder(s) are still kept offline; remove them in Settings first",
                cfg.pins.len()
            ))));
        }
        return Some(report(
            units::remove(&cfg)
                .and_then(|()| {
                    config::Config::save(APP_ID, |c| c.offline_enabled = false).map(|_| ())
                })
                .map(|()| "Offline folder services removed".into()),
        ));
    }
    // `--pins` lists offline folders and their state.
    if args.iter().any(|a| a == "--pins") {
        let cfg = config::Config::load(APP_ID);
        let state = pin::OfflineState::load();
        let on = if cfg.offline_enabled { "on" } else { "off" };
        println!("offline folders: {on}");
        for rel in &cfg.pins {
            match state.pin(rel) {
                Some(p) => println!(
                    "  {rel}: {}{}",
                    p.phase.label(),
                    p.error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                ),
                None => println!("  {rel}: waiting for the keeper"),
            }
        }
        return Some(0);
    }
    None
}
