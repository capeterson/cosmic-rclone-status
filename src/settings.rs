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

/// What setup is about to change, for the consent dialog: one (title, detail) per change still
/// to be made, plus what stays true afterwards. Built from `c` so already-done steps aren't
/// listed.
pub fn consent_items(c: Checks, cfg: &Config) -> Vec<(String, String)> {
    let mut items = vec![];
    if !c.fuse_conf {
        items.push((
            format!("Change a system setting: {}", fuse_conf::PATH),
            format!(
                "Adds the line “user_allow_other”, which lets your own programs mount with \
                 --allow-other. This needs an administrator password, which you'll be asked for \
                 next. The current file is first copied to {p}{infix}<date-time>.bak (if that \
                 copy fails, the file isn't touched). The line stays in place if you turn \
                 offline folders off later.",
                p = fuse_conf::PATH,
                infix = fuse_conf::BACKUP_INFIX,
            ),
        ));
    }
    if !c.units || !c.keeper_running {
        items.push((
            "Install background services".to_string(),
            format!(
                "Adds {keeper} and {pin} to {dir}, enables the first, and ties both to {svc} \
                 so they start and stop with the Drive mount.",
                keeper = units::keeper_unit_name(),
                pin = units::pin_unit_name("<folder>"),
                dir = units::unit_dir().display(),
                svc = cfg.service,
            ),
        ));
        items.push((
            "Change the Drive mount's settings".to_string(),
            format!(
                "Adds {} so the mount runs with --allow-other, --default-permissions and umask 077. \
                 Files in the Drive folder then show as owner-only (0600 / 0700), which keeps \
                 other users on this computer out.",
                units::dropin_path(&cfg.service).display()
            ),
        ));
    }
    if !c.mount_allows_other {
        items.push((
            format!("Restart {}", cfg.service),
            format!(
                "Happens once, now. Anything open in {} is interrupted; changes waiting to upload \
                 are kept and resume after the restart.",
                cfg.mount_path
            ),
        ));
    }
    items.push((
        "Afterwards".to_string(),
        format!(
            "Folders you choose to keep offline are downloaded to {} and use disk space there. \
             Turning offline folders off removes the services and mount settings again.",
            pin::data_dir().join("offline").display()
        ),
    ));
    items
}

#[derive(Debug, Clone)]
enum Message {
    Refresh,
    Refreshed(Checks, OfflineState),
    Config(Config),
    ToggleOffline(bool),
    SetUp,
    /// The consent dialog's answer.
    Consent(bool),
    /// A setup or teardown finished, with a note to show on success.
    Done(Result<Option<String>, String>),
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
    /// Something worth telling after a successful setup (where the backup went).
    note: Option<String>,
    /// The consent dialog is showing; nothing has been changed yet.
    consent: bool,
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

/// Run the setup steps that aren't done yet, in order. Returns a note for the user (where the
/// `/etc/fuse.conf` backup went), if there is one.
fn set_up(cfg: &Config) -> Result<Option<String>, String> {
    let c = Checks::probe(cfg);
    let mut note = None;
    if !c.fuse_conf {
        note = fuse_conf::enable()?
            .map(|b| format!("The previous {} was saved as {b}", fuse_conf::PATH));
    }
    if !c.units || !c.keeper_running {
        units::install(cfg)?;
    }
    if !c.mount_allows_other {
        units::systemctl(&["restart", &cfg.service])?;
    }
    Ok(note)
}

/// Switch offline folders on and run the setup. If it can't finish (say the password prompt was
/// dismissed), undo it so the switch goes back off with the reason shown.
fn turn_on(cfg: &Config) -> Result<Option<String>, String> {
    Config::save(crate::APP_ID, |c| c.offline_enabled = true)?;
    set_up(cfg).map_err(|e| {
        let _ = tear_down(cfg);
        format!("Offline folders weren't turned on: {e}")
    })
}

/// Turn offline folders off: everything the setup added except the `/etc/fuse.conf` line.
fn tear_down(cfg: &Config) -> Result<Option<String>, String> {
    units::remove(cfg)?;
    Config::save(crate::APP_ID, |c| c.offline_enabled = false).map(|_| None)
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
            note: None,
            consent: false,
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
                    // Explain first; nothing is changed until the user continues.
                    self.consent = true;
                } else {
                    self.busy = true;
                    self.cfg.offline_enabled = false;
                    let cfg = self.cfg.clone();
                    return blocking(move || tear_down(&cfg), Message::Done);
                }
            }
            Message::SetUp => {
                self.error = None;
                self.consent = true;
            }
            Message::Consent(go) => {
                self.consent = false;
                if !go {
                    // Nothing was saved or changed, so the switch simply stays off.
                    return Task::none();
                }
                self.busy = true;
                let cfg = self.cfg.clone();
                return if cfg.offline_enabled {
                    blocking(move || set_up(&cfg), Message::Done)
                } else {
                    self.cfg.offline_enabled = true;
                    blocking(move || turn_on(&cfg), Message::Done)
                };
            }
            Message::Done(r) => {
                self.busy = false;
                (self.note, self.error) = match r {
                    Ok(note) => (note, None),
                    Err(e) => (None, Some(e)),
                };
                // A failed turn-on switches itself back off; show that now, not on the next
                // config notification.
                self.cfg = Config::load(crate::APP_ID);
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

    /// Informed consent before setup changes anything: what will change, then Continue (on to
    /// the password prompt) or Cancel (nothing is changed and the switch stays off).
    fn dialog(&self) -> Option<Element<'_, Message>> {
        if !self.consent {
            return None;
        }
        let items = consent_items(self.checks, &self.cfg);
        let list = items.into_iter().fold(
            widget::column::with_capacity(8).spacing(cosmic::theme::spacing().space_s),
            |col, (title, detail)| {
                col.push(
                    widget::column::with_capacity(2)
                        .spacing(2)
                        .push(widget::text::heading(title))
                        .push(widget::text::body(detail)),
                )
            },
        );
        let needs_password = !self.checks.fuse_conf;
        let go = if needs_password {
            "Continue to password prompt"
        } else {
            "Continue"
        };
        Some(
            widget::dialog()
                .title("Turn on offline folders?")
                .icon(widget::icon::from_name("folder-download-symbolic").size(64))
                .body("Keeping folders offline needs these changes. Nothing has been changed yet.")
                .control(widget::container(widget::scrollable(list)).max_height(360.0))
                .primary_action(widget::button::suggested(go).on_press(Message::Consent(true)))
                .secondary_action(
                    widget::button::standard("Cancel").on_press(Message::Consent(false)),
                )
                .into(),
        )
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
        for (icon, text) in [
            ("dialog-information-symbolic", &self.note),
            ("dialog-error-symbolic", &self.error),
        ] {
            if let Some(t) = text {
                sections.push(
                    widget::row::with_capacity(2)
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .push(widget::icon::from_name(icon).size(16))
                        .push(widget::text::body(t.as_str()))
                        .into(),
                );
            }
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
    /// The setup checklist, shown while setup is running or incomplete.
    fn setup_rows<'a>(
        &self,
        mut sec: settings::Section<'a, Message>,
    ) -> settings::Section<'a, Message> {
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
        // Setup starts from the switch; this button only picks up after something undid part
        // of it (say the drop-in was deleted). Both go through the consent dialog.
        sec.add(if self.busy {
            settings::item::builder("Setting up…")
                .description("Enter your password if asked. The Drive mount restarts once.")
                .control(widget::Space::new())
        } else {
            settings::item::builder("Setup isn't complete")
                .description(if self.checks.mounted {
                    "Runs the remaining steps above."
                } else {
                    "The Drive mount isn't running; setup will start it."
                })
                .control(widget::button::suggested("Set up").on_press(Message::SetUp))
        })
    }

    fn offline_section(&self) -> Element<'_, Message> {
        let can_turn_off = self.cfg.pins.is_empty() && !self.busy;
        let description = if self.cfg.offline_enabled && !self.cfg.pins.is_empty() {
            "Stop keeping every folder offline before turning this off."
        } else {
            "Folders you choose are fully downloaded, kept in sync both ways, and work without a \
             connection, right where they are in the Drive folder."
        };
        let toggle_allowed =
            !self.busy && !self.consent && (!self.cfg.offline_enabled || can_turn_off);
        let mut sec = settings::section().title("Offline folders").add(
            settings::item::builder("Keep folders available offline")
                .description(description)
                .toggler_maybe(
                    self.cfg.offline_enabled || self.consent,
                    toggle_allowed.then_some(Message::ToggleOffline),
                ),
        );
        if !self.cfg.offline_enabled {
            return sec.into();
        }
        if !self.checks.complete() {
            return self.setup_rows(sec).into();
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

    #[test]
    fn consent_lists_every_change_on_a_fresh_system() {
        let cfg = Config::default();
        let items = consent_items(Checks::default(), &cfg);
        let titles: Vec<&str> = items.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Change a system setting: /etc/fuse.conf",
                "Install background services",
                "Change the Drive mount's settings",
                "Restart rclone-gdrive.service",
                "Afterwards",
            ]
        );
        let fuse = &items[0].1;
        assert!(fuse.contains("administrator password"), "{fuse}");
        assert!(
            fuse.contains("/etc/fuse.conf.cosmic-rclone-status-<date-time>.bak"),
            "{fuse}"
        );
        assert!(items[2]
            .1
            .contains("--allow-other, --default-permissions and umask 077"));
        assert!(items[3].1.contains("~/GoogleDrive"));
    }

    #[test]
    fn consent_leaves_out_steps_already_done() {
        let c = Checks {
            fuse_conf: true,
            units: true,
            keeper_running: true,
            mounted: true,
            mount_allows_other: false,
        };
        let items = consent_items(c, &Config::default());
        let titles: Vec<&str> = items.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, ["Restart rclone-gdrive.service", "Afterwards"]);
    }
}
