// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod context_menu;
mod drive;
mod menu;
mod mime_icon;
mod rc;
mod share;
mod status;
#[cfg(test)]
mod testutil;

/// Build with `APP_ID_OVERRIDE=...` to get a side-by-side dev copy with its own panel
/// identity and config (see `just install-dev`).
pub const APP_ID: &str = match option_env!("APP_ID_OVERRIDE") {
    Some(id) => id,
    None => "com.ca-peterson.CosmicRcloneStatus",
};

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--menu PATH...` is the "Rclone…" window COSMIC Files opens from its right-click menu.
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
    // `cosmic-rclone-status --status [--rc-addr HOST:PORT]` prints one status snapshot and
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
