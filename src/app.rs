// SPDX-License-Identifier: GPL-3.0-only

use crate::config::Config;
use crate::rc::{self, human, Health, Status};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{time, window::Id, Length, Limits, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use std::time::Duration;

const POLL_OPEN: Duration = Duration::from_secs(1);
const POLL_CLOSED: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    config: Config,
    status: Option<Status>,
    /// Avoid stacking requests when rclone is slow to answer.
    fetching: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    UpdateConfig(Config),
    Tick,
    StatusUpdated(Status),
    /// The background fetch panicked or was cancelled; allow the next tick to retry.
    FetchFailed,
    ServiceAction(&'static str),
    ActionDone,
    OpenFolder,
}

impl AppModel {
    fn refresh(&mut self) -> Task<cosmic::Action<Message>> {
        if self.fetching {
            return Task::none();
        }
        self.fetching = true;
        let cfg = self.config.clone();
        Task::perform(
            async move { tokio::task::spawn_blocking(move || rc::fetch(&cfg)).await },
            |res| match res {
                Ok(st) => cosmic::Action::App(Message::StatusUpdated(st)),
                Err(_) => cosmic::Action::App(Message::FetchFailed),
            },
        )
    }

    fn icon(&self) -> widget::icon::Handle {
        let svg: &'static [u8] = match self.status.as_ref().map(Status::health) {
            Some(Health::Synced) => include_bytes!("../resources/emblems/synced.svg"),
            Some(Health::Uploading) => include_bytes!("../resources/emblems/uploading.svg"),
            Some(Health::Downloading) => include_bytes!("../resources/emblems/downloading.svg"),
            Some(Health::Transferring) => include_bytes!("../resources/emblems/transferring.svg"),
            Some(Health::Error) => include_bytes!("../resources/emblems/error.svg"),
            Some(Health::Down) => include_bytes!("../resources/emblems/down.svg"),
            None => include_bytes!("../resources/emblems/loading.svg"),
        };
        let mut handle = widget::icon::from_svg_bytes(svg);
        handle.symbolic = true;
        handle
    }
}

fn headline(health: Option<Health>) -> &'static str {
    match health {
        Some(Health::Synced) => "Synced",
        Some(Health::Uploading) => "Uploading",
        Some(Health::Downloading) => "Downloading",
        Some(Health::Transferring) => "Uploading and downloading",
        Some(Health::Error) => "Error",
        Some(Health::Down) => "Mount is down",
        None => "Checking…",
    }
}

/// The body text lines of the popup (excluding per-transfer rows), in display order.
fn detail_lines(st: &Status) -> Vec<String> {
    let mut lines = vec![format!(
        "Mount: {}   Service: {}",
        if st.mounted { "up" } else { "down" },
        if st.service_active {
            "running"
        } else {
            "stopped"
        },
    )];
    if let Some(e) = &st.rc_error {
        lines.push(format!("rc unreachable: {e}"));
    }
    lines.push(format!(
        "Uploads: {} in progress, {} queued",
        st.uploads_in_progress, st.uploads_queued
    ));
    lines.push(format!(
        "Cache: {} files, {}",
        st.cache_files,
        human(st.cache_bytes as f64)
    ));
    lines.push(format!("Speed: {}/s", human(st.speed)));
    if st.errored_files > 0 || st.out_of_space {
        lines.push(format!(
            "Errored files: {}{}",
            st.errored_files,
            if st.out_of_space {
                " (cache out of space)"
            } else {
                ""
            }
        ));
    }
    lines
}

/// One line describing a transfer. `size` is negative when rclone doesn't know it.
fn transfer_line(t: &rc::Transfer) -> String {
    let total = if t.size < 0 {
        "?".to_string()
    } else {
        human(t.size as f64)
    };
    format!(
        "{} — {} / {} at {}/s",
        t.name,
        human(t.bytes as f64),
        total,
        human(t.speed)
    )
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = crate::APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let config = Config::load(Self::APP_ID);
        let mut app = AppModel {
            core,
            config,
            ..Default::default()
        };
        let task = app.refresh();
        (app, task)
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn view(&self) -> Element<'_, Message> {
        self.core
            .applet
            .icon_button_from_handle(self.icon())
            .on_press(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        let mut col = widget::Column::new()
            .spacing(8)
            .padding(12)
            .width(Length::Fill);
        col = col.push(widget::text::heading(headline(
            self.status.as_ref().map(Status::health),
        )));

        match &self.status {
            None => {}
            Some(st) => {
                for line in detail_lines(st) {
                    col = col.push(widget::text::body(line));
                }
                if let Some(e) = &st.last_error {
                    col = col.push(widget::text::caption(format!("Last error: {e}")));
                }
                if !st.transfers.is_empty() {
                    col = col.push(widget::divider::horizontal::default());
                    for t in st.transfers.iter().take(8) {
                        col = col.push(widget::text::caption(transfer_line(t)));
                        col = col.push(widget::determinate_linear(t.percentage as f32 / 100.0));
                    }
                    if st.transfers.len() > 8 {
                        col = col.push(widget::text::caption(format!(
                            "…and {} more",
                            st.transfers.len() - 8
                        )));
                    }
                }
            }
        }

        let running = self.status.as_ref().is_some_and(|s| s.service_active);
        let actions = widget::Row::new()
            .spacing(8)
            .push(widget::button::standard("Open folder").on_press(Message::OpenFolder))
            .push(if running {
                widget::button::standard("Restart").on_press(Message::ServiceAction("restart"))
            } else {
                widget::button::suggested("Start").on_press(Message::ServiceAction("start"))
            })
            .push(
                widget::button::destructive("Stop")
                    .on_press_maybe(running.then_some(Message::ServiceAction("stop"))),
            );
        col = col
            .push(widget::divider::horizontal::default())
            .push(actions);

        self.core.applet.popup_container(col).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let every = if self.popup.is_some() {
            POLL_OPEN
        } else {
            POLL_CLOSED
        };
        Subscription::batch(vec![
            time::every(every).map(|_| Message::Tick),
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
        ])
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::UpdateConfig(c) => self.config = c,
            Message::StatusUpdated(st) => {
                self.status = Some(st);
                self.fetching = false;
            }
            Message::FetchFailed => self.fetching = false,
            // Don't clear `fetching`: a poll may still be in flight, and stacking a second
            // request is what the flag prevents. The next tick picks up any change.
            Message::Tick | Message::ActionDone => return self.refresh(),
            Message::OpenFolder => rc::open_mount(&self.config),
            Message::ServiceAction(verb) => {
                let cfg = self.config.clone();
                return Task::perform(
                    async move {
                        let _ = tokio::task::spawn_blocking(move || rc::service_action(&cfg, verb))
                            .await;
                    },
                    |()| cosmic::Action::App(Message::ActionDone),
                );
            }
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    settings.positioner.size_limits = Limits::NONE
                        .max_width(420.0)
                        .min_width(320.0)
                        .min_height(120.0)
                        .max_height(800.0);
                    Task::batch([get_popup(settings), self.refresh()])
                };
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::Application as _;

    fn status() -> Status {
        Status {
            service_active: true,
            mounted: true,
            rc_error: None,
            uploads_in_progress: 0,
            uploads_queued: 0,
            cache_files: 0,
            cache_bytes: 0,
            errored_files: 0,
            out_of_space: false,
            speed: 0.0,
            last_error: None,
            transfers: vec![],
        }
    }

    #[test]
    fn headline_covers_every_state() {
        assert_eq!(headline(None), "Checking…");
        assert_eq!(headline(Some(Health::Synced)), "Synced");
        assert_eq!(headline(Some(Health::Down)), "Mount is down");
        assert_eq!(
            headline(Some(Health::Transferring)),
            "Uploading and downloading"
        );
    }

    #[test]
    fn detail_lines_idle() {
        let l = detail_lines(&status());
        assert_eq!(l[0], "Mount: up   Service: running");
        assert_eq!(l[1], "Uploads: 0 in progress, 0 queued");
        assert_eq!(l.len(), 4, "no error lines when healthy: {l:?}");
    }

    #[test]
    fn detail_lines_report_problems() {
        let mut s = status();
        s.mounted = false;
        s.service_active = false;
        s.rc_error = Some("timeout".into());
        s.errored_files = 2;
        s.out_of_space = true;
        let l = detail_lines(&s);
        assert_eq!(l[0], "Mount: down   Service: stopped");
        assert!(l.contains(&"rc unreachable: timeout".to_string()));
        assert!(l.contains(&"Errored files: 2 (cache out of space)".to_string()));
    }

    #[test]
    fn transfer_line_handles_unknown_size() {
        let t = rc::Transfer {
            name: "a.bin".into(),
            size: -1,
            bytes: 1024,
            speed: 0.0,
            ..Default::default()
        };
        assert_eq!(transfer_line(&t), "a.bin — 1.0 KiB / ? at 0.0 B/s");
    }

    #[test]
    fn status_update_stores_status_and_clears_fetching() {
        let mut app = AppModel {
            fetching: true,
            ..Default::default()
        };
        let _ = app.update(Message::StatusUpdated(status()));
        assert!(!app.fetching);
        assert!(app.status.is_some());
    }

    #[test]
    fn tick_does_not_stack_fetches() {
        let mut app = AppModel::default();
        let _ = app.update(Message::Tick);
        assert!(app.fetching);
        let _ = app.update(Message::Tick); // still fetching: must be a no-op
        assert!(app.fetching);
    }

    #[test]
    fn action_done_keeps_in_flight_fetch_flag() {
        let mut app = AppModel {
            fetching: true,
            ..Default::default()
        };
        let _ = app.update(Message::ActionDone);
        assert!(app.fetching, "must not start a second concurrent fetch");
    }

    #[test]
    fn fetch_failure_allows_retry_on_next_tick_only() {
        let mut app = AppModel {
            fetching: true,
            ..Default::default()
        };
        let _ = app.update(Message::FetchFailed);
        assert!(!app.fetching, "no immediate retry");
        let _ = app.update(Message::Tick);
        assert!(app.fetching);
    }

    #[test]
    fn config_update_replaces_config() {
        let mut app = AppModel::default();
        let c = Config {
            rc_addr: "127.0.0.1:1".into(),
            ..Config::default()
        };
        let _ = app.update(Message::UpdateConfig(c.clone()));
        assert_eq!(app.config, c);
    }

    #[test]
    fn popup_closed_only_clears_matching_id() {
        let mut app = AppModel::default();
        let (a, b) = (Id::unique(), Id::unique());
        app.popup = Some(a);
        let _ = app.update(Message::PopupClosed(b));
        assert_eq!(app.popup, Some(a));
        let _ = app.update(Message::PopupClosed(a));
        assert_eq!(app.popup, None);
    }
}
