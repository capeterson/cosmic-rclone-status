// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod rc;

fn main() -> cosmic::iced::Result {
    // `cosmic-rclone-status --status` prints one status snapshot and exits (handy for debugging).
    if std::env::args().any(|a| a == "--status") {
        let st = rc::fetch(&config::Config::default());
        println!("{:#?}\nhealth: {:?}", st, st.health());
        return Ok(());
    }
    cosmic::applet::run::<app::AppModel>(())
}
