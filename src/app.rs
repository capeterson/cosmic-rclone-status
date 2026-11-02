// SPDX-License-Identifier: GPL-3.0-only

use crate::config::Config;
use crate::rc::{self, human, Health, Status};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
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
                Err(_) => cosmic::Action::App(Message::ActionDone),
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

    fn headline(&self) -> String {
        match self.status.as_ref().map(Status::health) {
            Some(Health::Synced) => "Synced".into(),
            Some(Health::Uploading) => "Uploading".into(),
            Some(Health::Downloading) => "Downloading".into(),
            Some(Health::Transferring) => "Uploading and downloading".into(),
            Some(Health::Error) => "Error".into(),
            Some(Health::Down) => "Mount is down".into(),
            None => "Checking…".into(),
        }
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "com.ca-peterson.CosmicRcloneStatus";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let config = cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
            .map(|ctx| match Config::get_entry(&ctx) {
                Ok(c) => c,
                Err((_errs, c)) => c,
            })
            .unwrap_or_default();
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
        let mut col = widget::Column::new().spacing(8).padding(12).width(Length::Fill);
        col = col.push(widget::text::heading(self.headline()));

        match &self.status {
            None => {}
            Some(st) => {
                col = col.push(widget::text::body(format!(
                    "Mount: {}   Service: {}",
                    if st.mounted { "up" } else { "down" },
                    if st.service_active { "running" } else { "stopped" },
                )));
                if let Some(e) = &st.rc_error {
                    col = col.push(widget::text::body(format!("rc unreachable: {e}")));
                }
                col = col.push(widget::text::body(format!(
                    "Uploads: {} in progress, {} queued",
                    st.uploads_in_progress, st.uploads_queued
                )));
                col = col.push(widget::text::body(format!(
                    "Cache: {} files, {}",
                    st.cache_files,
                    human(st.cache_bytes as f64)
                )));
                col = col.push(widget::text::body(format!(
                    "Speed: {}/s",
                    human(st.speed)
                )));
                if st.errored_files > 0 || st.out_of_space {
                    col = col.push(widget::text::body(format!(
                        "Errored files: {}{}",
                        st.errored_files,
                        if st.out_of_space { " (cache out of space)" } else { "" }
                    )));
                }
                if let Some(e) = &st.last_error {
                    col = col.push(widget::text::caption(format!("Last error: {e}")));
                }
                if !st.transfers.is_empty() {
                    col = col.push(widget::divider::horizontal::default());
                    for t in st.transfers.iter().take(8) {
                        col = col.push(widget::text::caption(format!(
                            "{} — {} / {} at {}/s",
                            t.name,
                            human(t.bytes as f64),
                            human(t.size as f64),
                            human(t.speed)
                        )));
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
        col = col.push(widget::divider::horizontal::default()).push(actions);

        self.core.applet.popup_container(col).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let every = if self.popup.is_some() { POLL_OPEN } else { POLL_CLOSED };
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
            Message::Tick => return self.refresh(),
            Message::StatusUpdated(st) => {
                self.status = Some(st);
                self.fetching = false;
            }
            Message::ActionDone => {
                self.fetching = false;
                return self.refresh();
            }
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
