// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod mime_icon;
mod rc;

/// Build with `APP_ID_OVERRIDE=...` to get a side-by-side dev copy with its own panel
/// identity and config (see `just install-dev`).
pub const APP_ID: &str = match option_env!("APP_ID_OVERRIDE") {
    Some(id) => id,
    None => "com.ca-peterson.CosmicRcloneStatus",
};

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
