// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod rc;

pub const APP_ID: &str = "com.ca-peterson.CosmicRcloneStatus";

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
        let st = rc::fetch(&cfg);
        println!("{st:#?}\nhealth: {:?}", st.health());
        return Ok(());
    }
    cosmic::applet::run::<app::AppModel>(())
}
