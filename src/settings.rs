// SPDX-License-Identifier: GPL-3.0-only
//! The settings window (`--settings`, or "Settings…" in the panel popup).
//!
//! Its main job is offline folders: turning them on walks through the one-time setup they need
//! (a line in `/etc/fuse.conf`, granted through the system's password prompt; the keeper's
//! systemd units; one restart of the Drive mount), then lists the folders kept offline.

use crate::config::Config;
use crate::pin::{self, OfflineState, Phase};
use crate::{fuse_conf, keeper, rc, share, units};
use cosmic::app::Settings;
use cosmic::iced::{time, Alignment, Length, Size, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, settings};
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(2);

/// What's in place for offline folders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent yes/no checks, not a state machine
pub struct Checks {
    /// `user_allow_other` is in `/etc/fuse.conf`.
    pub fuse_conf: bool,
    /// The keeper/pin units and the mount drop-in are written and current.
    pub units: bool,
    pub keeper_running: bool,
    pub mounted: bool,
    /// The Drive mount is running with `allow_other` (it has been restarted since setup).
    pub mount_allows_other: bool,
}

impl Checks {
    fn probe(cfg: &Config) -> Self {
        Checks {
            fuse_conf: fuse_conf::allows_other(),
            units: units::installed(cfg),
            keeper_running: units::is_active(&units::keeper_unit_name()),
            mounted: rc::is_mounted(&cfg.mount_path),
            mount_allows_other: rc::mount_allows_other(&cfg.mount_path),
        }
    }

    pub fn complete(self) -> bool {
        self.fuse_conf && self.units && self.keeper_running && self.mount_allows_other
    }
}

/// The setup steps in order: title, explanation, done.
pub fn setup_steps(c: Checks) -> [(&'static str, &'static str, bool); 3] {
    [
        (
            "Allow mounts inside the Drive folder",
            "Adds user_allow_other to /etc/fuse.conf. Asks for an administrator password once.",
            c.fuse_conf,
        ),
        (
            "Background sync service",
            "Installs and starts a user service that keeps offline folders in sync.",
            c.units && c.keeper_running,
        ),
        (
            "Restart the Drive mount",
            "Applies --allow-other to the Drive mount. Only you can open its files. Files open in \
             the Drive folder are interrupted once.",
            c.mount_allows_other,
        ),
    ]
}

#[derive(Debug, Clone)]
enum Message {
    Refresh,
    Refreshed(Checks, OfflineState),
    Config(Config),
    ToggleOffline(bool),
    SetUp,
    Done(Result<(), String>),
    Unpin(String),
    Repair(String),
    Open(String),
    ToggleContextMenu(bool),
}

struct App {
    core: cosmic::Core,
    cfg: Config,
    checks: Checks,
    state: OfflineState,
    /// A setup or teardown is running; buttons are disabled.
    busy: bool,
    error: Option<String>,
}

type Task = cosmic::app::Task<Message>;

fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    msg: impl Fn(T) -> Message + Send + 'static,
) -> Task {
    Task::perform(
        async move { tokio::task::spawn_blocking(f).await },
        move |r| match r {
            Ok(v) => cosmic::Action::App(msg(v)),
            Err(e) => cosmic::Action::App(Message::Done(Err(e.to_string()))),
        },
    )
}

/// Run the setup steps that aren't done yet, in order.
fn set_up(cfg: &Config) -> Result<(), String> {
    let c = Checks::probe(cfg);
    if !c.fuse_conf {
        fuse_conf::enable()?;
    }
    if !c.units || !c.keeper_running {
        units::install(cfg)?;
    }
    if !c.mount_allows_other {
        units::systemctl(&["restart", &cfg.service])?;
    }
    Ok(())
}

/// Turn offline folders off: everything the setup added except the `/etc/fuse.conf` line.
fn tear_down(cfg: &Config) -> Result<(), String> {
    units::remove(cfg)?;
    Config::save(crate::APP_ID, |c| c.offline_enabled = false).map(|_| ())
}

impl cosmic::Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = Config;
    type Message = Message;
    const APP_ID: &'static str = crate::APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(mut core: cosmic::Core, cfg: Config) -> (Self, Task) {
        core.window.show_maximize = false;
        core.set_header_title("rclone Status settings".into());
        let app = App {
            core,
            cfg,
            checks: Checks::default(),
            state: OfflineState::default(),
            busy: false,
            error: None,
        };
        (app, Task::done(cosmic::Action::App(Message::Refresh)))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch(vec![
            time::every(REFRESH).map(|_| Message::Refresh),
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|u| Message::Config(u.config)),
        ])
    }

    fn update(&mut self, message: Message) -> Task {
        match message {
            Message::Refresh => {
                let cfg = self.cfg.clone();
                return blocking(
                    move || (Checks::probe(&cfg), OfflineState::load()),
                    |(c, s)| Message::Refreshed(c, s),
                );
            }
            Message::Refreshed(c, s) => {
                self.checks = c;
                self.state = s;
            }
            Message::Config(c) => self.cfg = c,
            Message::ToggleOffline(on) => {
                self.error = None;
                if on {
                    if let Err(e) = Config::save(crate::APP_ID, |c| c.offline_enabled = true) {
                        self.error = Some(e);
                    }
                    self.cfg.offline_enabled = true;
                } else {
                    self.busy = true;
                    let cfg = self.cfg.clone();
                    return blocking(move || tear_down(&cfg), Message::Done);
                }
            }
            Message::SetUp => {
                self.busy = true;
                self.error = None;
                let cfg = self.cfg.clone();
                return blocking(move || set_up(&cfg), Message::Done);
            }
            Message::Done(r) => {
                self.busy = false;
                self.error = r.err();
                return self.update(Message::Refresh);
            }
            Message::Unpin(rel) => {
                if let Err(e) = Config::save(crate::APP_ID, |c| c.pins.retain(|p| *p != rel)) {
                    self.error = Some(e);
                }
            }
            Message::Repair(rel) => {
                if let Err(e) = keeper::request_repair(&rel) {
                    self.error = Some(e.to_string());
                }
            }
            Message::Open(rel) => {
                share::open_url(&pin::mount_point(&self.cfg, &rel).to_string_lossy());
            }
            Message::ToggleContextMenu(on) => {
                match Config::save(crate::APP_ID, |c| c.context_menu = on) {
                    Ok(_) if on => crate::context_menu::install(),
                    Ok(_) => crate::context_menu::remove(),
                    Err(e) => self.error = Some(e),
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let mut sections: Vec<Element<'_, Message>> = vec![self.offline_section()];
        if self.cfg.offline_enabled && !self.state.notices.is_empty() {
            sections.push(self.notices_section());
        }
        sections.push(
            settings::section()
                .title("General")
                .add(
                    settings::item::builder("Show “Rclone…” in the Files right-click menu")
                        .toggler(self.cfg.context_menu, Message::ToggleContextMenu),
                )
                .into(),
        );
        if let Some(e) = &self.error {
            sections.push(
                widget::row::with_capacity(2)
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .push(widget::icon::from_name("dialog-error-symbolic").size(16))
                    .push(widget::text::body(e.as_str()))
                    .into(),
            );
        }
        let sp = cosmic::theme::spacing();
        widget::scrollable(
            widget::container(settings::view_column(sections))
                .padding(sp.space_m)
                .width(Length::Fill),
        )
        .into()
    }
}

impl App {
    fn offline_section(&self) -> Element<'_, Message> {
        let can_turn_off = self.cfg.pins.is_empty() && !self.busy;
        let description = if self.cfg.offline_enabled && !self.cfg.pins.is_empty() {
            "Stop keeping every folder offline before turning this off."
        } else {
            "Folders you choose are fully downloaded, kept in sync both ways, and work without a \
             connection, right where they are in the Drive folder."
        };
        let toggle_allowed = !self.cfg.offline_enabled || can_turn_off;
        let mut sec = settings::section().title("Offline folders").add(
            settings::item::builder("Keep folders available offline")
                .description(description)
                .toggler_maybe(
                    self.cfg.offline_enabled,
                    toggle_allowed.then_some(Message::ToggleOffline),
                ),
        );
        if !self.cfg.offline_enabled {
            return sec.into();
        }
        if !self.checks.complete() {
            for (title, desc, done) in setup_steps(self.checks) {
                let icon = if done {
                    "emblem-ok-symbolic"
                } else {
                    "dialog-information-symbolic"
                };
                sec = sec.add(
                    settings::item::builder(title)
                        .description(desc)
                        .icon(widget::icon::from_name(icon).size(16))
                        .control(widget::text::caption(if done { "Done" } else { "To do" })),
                );
            }
            let label = if self.busy { "Setting up…" } else { "Set up" };
            sec = sec.add(
                settings::item::builder("Finish setup")
                    .description(if self.checks.mounted {
                        "Runs the remaining steps above."
                    } else {
                        "The Drive mount isn't running; setup will start it."
                    })
                    .control(
                        widget::button::suggested(label)
                            .on_press_maybe((!self.busy).then_some(Message::SetUp)),
                    ),
            );
            return sec.into();
        }
        if self.cfg.pins.is_empty() {
            sec = sec.add(settings::item::builder("No folders are kept offline yet").description(
                "In Files, right-click a folder in the Drive folder, choose “Rclone…”, then \
                 “Keep offline”.",
            ).control(widget::Space::new()));
        }
        let now = jiff::Timestamp::now();
        for rel in &self.cfg.pins {
            let p = self.state.pin(rel);
            let phase = p.map_or(Phase::Downloading, |p| p.phase);
            let mut desc = phase.label().to_string();
            if let Some(t) = p.and_then(|p| p.last_sync) {
                let when = crate::app::ago(now, jiff::Timestamp::from_second(t).ok());
                desc = format!("{desc} · synced {when}");
            }
            if let Some(e) = p.and_then(|p| p.error.as_deref()) {
                desc = format!("{desc}\n{e}");
            }
            let mut buttons = widget::row::with_capacity(3).spacing(8);
            if phase == Phase::Attention {
                buttons = buttons.push(
                    widget::button::standard("Repair").on_press(Message::Repair(rel.clone())),
                );
            }
            buttons = buttons
                .push(widget::button::standard("Open").on_press(Message::Open(rel.clone())))
                .push(widget::button::destructive("Remove").on_press(Message::Unpin(rel.clone())));
            sec = sec.add(
                settings::item::builder(rel.as_str())
                    .description(desc)
                    .icon(widget::icon::from_name(phase.icon()).size(16))
                    .control(buttons),
            );
        }
        // Folders that were removed but are still being cleaned up.
        for p in self
            .state
            .pins
            .iter()
            .filter(|p| !self.cfg.pins.contains(&p.rel))
        {
            let desc = p.error.as_deref().map_or_else(
                || Phase::Removing.label().to_string(),
                |e| format!("{}\n{e}", Phase::Removing.label()),
            );
            sec = sec.add(
                settings::item::builder(p.rel.as_str())
                    .description(desc)
                    .icon(widget::icon::from_name(Phase::Removing.icon()).size(16))
                    .control(widget::Space::new()),
            );
        }
        sec.into()
    }

    fn notices_section(&self) -> Element<'_, Message> {
        let now = jiff::Timestamp::now();
        let mut sec = settings::section().title("Recent offline activity");
        for n in self.state.notices.iter().rev().take(5) {
            let when = crate::app::ago(now, jiff::Timestamp::from_second(n.at).ok());
            sec = sec.add(
                settings::item::builder(n.text.as_str())
                    .description(when)
                    .control(widget::Space::new()),
            );
        }
        sec.into()
    }
}

/// Open the window and block until it closes.
pub fn run(cfg: Config) -> cosmic::iced::Result {
    let settings = Settings::default().size(Size::new(600.0, 680.0));
    cosmic::app::run::<App>(settings, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_is_complete_only_when_every_piece_is_in_place() {
        let all = Checks {
            fuse_conf: true,
            units: true,
            keeper_running: true,
            mounted: true,
            mount_allows_other: true,
        };
        assert!(all.complete());
        for broken in [
            Checks {
                fuse_conf: false,
                ..all
            },
            Checks {
                units: false,
                ..all
            },
            Checks {
                keeper_running: false,
                ..all
            },
            Checks {
                mount_allows_other: false,
                ..all
            },
        ] {
            assert!(!broken.complete(), "{broken:?}");
        }
    }

    #[test]
    fn setup_steps_track_the_checks_in_order() {
        let c = Checks {
            fuse_conf: true,
            units: true,
            keeper_running: false,
            ..Checks::default()
        };
        let steps = setup_steps(c);
        assert_eq!(steps.map(|s| s.2), [true, false, false]);
        assert!(steps[0].1.contains("/etc/fuse.conf"));
        assert!(steps[2].1.contains("Only you can open"));
    }
}
