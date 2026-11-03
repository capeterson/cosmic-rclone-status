// SPDX-License-Identifier: GPL-3.0-only

use crate::config::Config;
use crate::mime_icon;
use crate::pin::{self, OfflineState};
use crate::rc::{self, human, CompletedTransfer, Direction, Health, Status};
use cosmic::iced::advanced::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{time, window::Id, Alignment, Length, Limits, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use std::time::Duration;

const POLL_OPEN: Duration = Duration::from_secs(1);
const POLL_CLOSED: Duration = Duration::from_secs(5);
/// How long the file lists wait before showing new history, so rows don't jump every poll.
const HISTORY_DEBOUNCE: Duration = Duration::from_secs(2);
/// Rows shown on the main page; the full history lives on its own page.
const RECENT_IN_POPUP: usize = 10;
const ICON_SIZE: u16 = 24;

/// Which view the popup is showing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Main,
    History,
}

#[derive(Default)]
pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    config: Config,
    status: Option<Status>,
    /// Avoid stacking requests when rclone is slow to answer.
    fetching: bool,
    page: Page,
    /// The history the lists display. It trails `status.recent` by up to `HISTORY_DEBOUNCE`.
    shown: Vec<CompletedTransfer>,
    /// A `HistoryDebounced` message is already scheduled.
    history_pending: bool,
    /// What the offline-folder keeper last reported.
    offline: OfflineState,
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
    /// Open the details window for this remote path.
    OpenDetails(String),
    ShowHistory,
    ShowMain,
    /// The debounce delay elapsed: show the newest history.
    HistoryDebounced,
    OfflineLoaded(OfflineState),
    OpenSettings,
}

impl AppModel {
    fn refresh(&mut self) -> Task<cosmic::Action<Message>> {
        if self.fetching {
            return Task::none();
        }
        self.fetching = true;
        let cfg = self.config.clone();
        let status = Task::perform(
            async move { tokio::task::spawn_blocking(move || rc::fetch(&cfg)).await },
            |res| match res {
                Ok(st) => cosmic::Action::App(Message::StatusUpdated(st)),
                Err(_) => cosmic::Action::App(Message::FetchFailed),
            },
        );
        if !self.config.offline_enabled {
            return status;
        }
        let offline = Task::perform(
            async { tokio::task::spawn_blocking(OfflineState::load).await },
            |res| cosmic::Action::App(Message::OfflineLoaded(res.unwrap_or_default())),
        );
        Task::batch([status, offline])
    }

    /// Decide when the displayed history catches up with the latest status. The first fill is
    /// immediate; after that changes show up `HISTORY_DEBOUNCE` after the first unseen one.
    fn sync_history(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(st) = &self.status else {
            return Task::none();
        };
        if st.recent == self.shown || self.history_pending {
            return Task::none();
        }
        if self.shown.is_empty() {
            self.shown.clone_from(&st.recent);
            return Task::none();
        }
        self.history_pending = true;
        // Build the sleep inside the future: `update` runs outside the tokio runtime.
        Task::perform(async { tokio::time::sleep(HISTORY_DEBOUNCE).await }, |()| {
            cosmic::Action::App(Message::HistoryDebounced)
        })
    }

    fn icon(&self) -> widget::icon::Handle {
        let svg = self
            .status
            .as_ref()
            .map_or(LOADING_SVG, |st| health_svg(st.health()));
        let mut handle = widget::icon::from_svg_bytes(svg);
        handle.symbolic = true;
        handle
    }
}

const LOADING_SVG: &[u8] = include_bytes!("../resources/emblems/loading.svg");

/// The emblem for each health state.
fn health_svg(h: Health) -> &'static [u8] {
    match h {
        Health::Healthy => include_bytes!("../resources/emblems/synced.svg"),
        Health::Uploading => include_bytes!("../resources/emblems/uploading.svg"),
        Health::Downloading => include_bytes!("../resources/emblems/downloading.svg"),
        Health::Transferring => include_bytes!("../resources/emblems/transferring.svg"),
        Health::FileErrors => include_bytes!("../resources/emblems/error.svg"),
        Health::CacheFull => include_bytes!("../resources/emblems/full.svg"),
        Health::Unreachable => include_bytes!("../resources/emblems/unreachable.svg"),
        Health::NotMounted => include_bytes!("../resources/emblems/unmounted.svg"),
        Health::ServiceStopped => include_bytes!("../resources/emblems/stopped.svg"),
    }
}

fn headline(health: Option<Health>) -> &'static str {
    match health {
        Some(Health::Healthy) => "Healthy",
        Some(Health::Uploading) => "Uploading",
        Some(Health::Downloading) => "Downloading",
        Some(Health::Transferring) => "Uploading and downloading",
        Some(Health::FileErrors) => "Some files failed",
        Some(Health::CacheFull) => "Cache is full",
        Some(Health::Unreachable) => "Can't reach rclone",
        Some(Health::NotMounted) => "Not mounted",
        Some(Health::ServiceStopped) => "Service stopped",
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

/// "Offline: 2 folders · all available offline", or a nudge to finish setup.
fn offline_line(pins: &[String], state: &OfflineState) -> String {
    if state.updated == 0 {
        "Offline folders: finish setup in Settings".to_string()
    } else {
        format!("Offline: {}", pin::summary(pins, state))
    }
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

/// "just now", "5m ago", "3h ago", "2d ago". Empty when the time is unknown.
pub(crate) fn ago(now: jiff::Timestamp, then: Option<jiff::Timestamp>) -> String {
    let Some(then) = then else {
        return String::new();
    };
    let secs = now.as_second() - then.as_second();
    match secs {
        i64::MIN..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// Split a remote path into its folder (possibly empty) and file name.
fn split_path(name: &str) -> (&str, &str) {
    let trimmed = name.trim_end_matches('/');
    trimmed.rsplit_once('/').unwrap_or(("", trimmed))
}

/// Second line of a history row: `Folder · 1.2 MiB · 5m ago · ×3`.
fn recent_caption(t: &CompletedTransfer, now: jiff::Timestamp) -> String {
    let (dir, _) = split_path(&t.name);
    let mut parts: Vec<String> = vec![];
    if !dir.is_empty() {
        parts.push(dir.to_string());
    }
    if t.size >= 0 {
        parts.push(human(t.size as f64));
    }
    let when = ago(now, t.at);
    if !when.is_empty() {
        parts.push(when);
    }
    if t.times > 1 {
        parts.push(format!("×{}", t.times));
    }
    parts.join(" · ")
}

/// Why the history list is empty, in words.
fn empty_history_text(st: &Status) -> &'static str {
    if !st.service_active || st.rc_error.is_some() {
        "History unavailable"
    } else {
        "Nothing synced since rclone started"
    }
}

/// A small symbolic emblem from the bundled SVGs.
fn emblem<'a>(svg: &'static [u8]) -> Element<'a, Message> {
    let mut handle = widget::icon::from_svg_bytes(svg);
    handle.symbolic = true;
    widget::icon(handle).size(16).into()
}

fn recent_row(t: &CompletedTransfer, now: jiff::Timestamp, detailed: bool) -> Element<'_, Message> {
    let (_, file) = split_path(&t.name);
    let mut text = widget::Column::new()
        .width(Length::Fill)
        .push(widget::text::body(file).ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1))))
        .push(
            widget::text::caption(recent_caption(t, now))
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
        );
    if let (true, Some(e)) = (detailed, &t.error) {
        text = text.push(
            widget::text::caption(format!("Failed: {e}"))
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
        );
    }
    let mut row = widget::Row::new()
        .spacing(8)
        .align_y(Alignment::Center)
        .push(widget::icon(mime_icon::file_icon(&t.name, ICON_SIZE)).size(ICON_SIZE))
        .push(text);
    match t.direction {
        Some(Direction::Upload) => {
            row = row.push(emblem(include_bytes!("../resources/emblems/uploading.svg")));
        }
        Some(Direction::Download) => {
            row = row.push(emblem(include_bytes!(
                "../resources/emblems/downloading.svg"
            )));
        }
        None => {}
    }
    if t.error.is_some() {
        row = row.push(emblem(include_bytes!("../resources/emblems/error.svg")));
    }
    widget::button::custom(row)
        .on_press(Message::OpenDetails(t.name.clone()))
        .class(cosmic::theme::Button::MenuItem)
        .width(Length::Fill)
        .padding([4, 8])
        .into()
}

impl AppModel {
    /// The default popup page: status, active transfers, recent files, actions.
    /// One line on offline folders, when they're switched on.
    fn offline_row(&self) -> Option<Element<'_, Message>> {
        self.config.offline_enabled.then(|| {
            widget::Row::new()
                .spacing(8)
                .align_y(Alignment::Center)
                .push(widget::icon::from_name("folder-download-symbolic").size(16))
                .push(widget::text::caption(offline_line(
                    &self.config.pins,
                    &self.offline,
                )))
                .into()
        })
    }

    fn main_page(&self) -> Element<'_, Message> {
        let mut col = widget::Column::new()
            .spacing(8)
            .padding(12)
            .width(Length::Fill);
        let health = self.status.as_ref().map(Status::health);
        let title = widget::Row::new()
            .spacing(8)
            .align_y(Alignment::Center)
            .push(emblem(health.map_or(LOADING_SVG, health_svg)))
            .push(widget::text::heading(headline(health)));
        // The details live in a tooltip to keep the popup compact.
        if let Some(st) = &self.status {
            let mut tip = widget::Column::new().spacing(2);
            for line in detail_lines(st) {
                tip = tip.push(widget::text::body(line));
            }
            if let Some(e) = &st.last_error {
                tip = tip.push(widget::text::caption(format!("Last error: {e}")));
            }
            let mut header = widget::Row::new()
                .align_y(Alignment::Center)
                .push(widget::tooltip(
                    title,
                    tip,
                    widget::tooltip::Position::Bottom,
                ));
            // Speed sits beside the status, and only while data is moving.
            let speed = st.display_speed();
            if speed > 0.0 {
                header = header
                    .push(widget::Space::new().width(Length::Fill))
                    .push(widget::text::body(format!("{}/s", human(speed))));
            }
            col = col.push(header);
        } else {
            col = col.push(title);
        }

        if let Some(row) = self.offline_row() {
            col = col.push(row);
        }

        if let Some(st) = &self.status {
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
            col = col.push(widget::divider::horizontal::default());
            let mut header = widget::Row::new()
                .align_y(Alignment::Center)
                .push(widget::text::heading("Recently synced").width(Length::Fill));
            if !self.shown.is_empty() {
                header = header.push(
                    widget::button::link(format!("Show all ({})", self.shown.len()))
                        .on_press(Message::ShowHistory),
                );
            }
            col = col.push(header);
            if self.shown.is_empty() {
                col = col.push(widget::text::caption(empty_history_text(st)));
            } else {
                let now = jiff::Timestamp::now();
                let rows = self
                    .shown
                    .iter()
                    .take(RECENT_IN_POPUP)
                    .fold(widget::Column::new().width(Length::Fill), |c, t| {
                        c.push(recent_row(t, now, false))
                    });
                col = col.push(widget::container(widget::scrollable(rows)).max_height(320.0));
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
            )
            .push(widget::Space::new().width(Length::Fill))
            .push(widget::tooltip(
                widget::button::icon(widget::icon::from_name("emblem-system-symbolic"))
                    .on_press(Message::OpenSettings),
                widget::text::body("Settings"),
                widget::tooltip::Position::Top,
            ));
        col.push(widget::divider::horizontal::default())
            .push(actions)
            .into()
    }

    /// Every file in the history, newest first.
    fn history_page(&self) -> Element<'_, Message> {
        let header = widget::Row::new()
            .spacing(8)
            .align_y(Alignment::Center)
            .push(
                widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                    .on_press(Message::ShowMain),
            )
            .push(widget::text::heading("Sync history"));
        let mut col = widget::Column::new()
            .spacing(8)
            .padding(12)
            .width(Length::Fill)
            .push(header);
        if self.shown.is_empty() {
            let text = self.status.as_ref().map_or("Checking…", empty_history_text);
            col = col.push(widget::text::body(text));
        } else {
            col = col.push(widget::text::caption(format!(
                "Last {} files since rclone started · newest first",
                self.shown.len()
            )));
            let now = jiff::Timestamp::now();
            let rows = self
                .shown
                .iter()
                .fold(widget::Column::new().width(Length::Fill), |c, t| {
                    c.push(recent_row(t, now, true))
                });
            col = col.push(widget::container(widget::scrollable(rows)).max_height(520.0));
        }
        col.into()
    }
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
        if config.context_menu {
            crate::context_menu::install();
        }
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
        let body = match self.page {
            Page::Main => self.main_page(),
            Page::History => self.history_page(),
        };
        self.core.applet.popup_container(body).into()
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
                return self.sync_history();
            }
            Message::HistoryDebounced => {
                self.history_pending = false;
                if let Some(st) = &self.status {
                    self.shown.clone_from(&st.recent);
                }
            }
            Message::ShowHistory => self.page = Page::History,
            Message::ShowMain => self.page = Page::Main,
            Message::OpenDetails(name) => rc::open_details(&self.config, &name),
            Message::FetchFailed => self.fetching = false,
            // Don't clear `fetching`: a poll may still be in flight, and stacking a second
            // request is what the flag prevents. The next tick picks up any change.
            Message::Tick | Message::ActionDone => return self.refresh(),
            Message::OpenFolder => rc::open_mount(&self.config),
            Message::OfflineLoaded(s) => self.offline = s,
            Message::OpenSettings => {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--settings").spawn();
                }
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p);
                }
            }
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
                    // Open on the main page with the freshest history.
                    self.page = Page::Main;
                    if let Some(st) = &self.status {
                        self.shown.clone_from(&st.recent);
                    }
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
                    self.page = Page::Main;
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
            recent: vec![],
        }
    }

    #[test]
    fn headline_covers_every_state() {
        assert_eq!(headline(None), "Checking…");
        assert_eq!(headline(Some(Health::Healthy)), "Healthy");
        assert_eq!(headline(Some(Health::NotMounted)), "Not mounted");
        assert_eq!(headline(Some(Health::ServiceStopped)), "Service stopped");
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
        assert_eq!(l.len(), 3, "no error lines when healthy: {l:?}");
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

    fn ts(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    fn done(name: &str, size: i64, at: Option<&str>) -> CompletedTransfer {
        CompletedTransfer {
            name: name.into(),
            size,
            bytes: size.max(0),
            at: at.map(ts),
            error: None,
            direction: None,
            times: 1,
        }
    }

    #[test]
    fn ago_covers_every_boundary() {
        let now = ts("2026-11-02T12:00:00Z");
        let at = |secs: i64| Some(now - jiff::SignedDuration::from_secs(secs));
        assert_eq!(ago(now, None), "");
        assert_eq!(ago(now, at(0)), "just now");
        assert_eq!(ago(now, at(59)), "just now");
        assert_eq!(ago(now, at(60)), "1m ago");
        assert_eq!(ago(now, at(3599)), "59m ago");
        assert_eq!(ago(now, at(3600)), "1h ago");
        assert_eq!(ago(now, at(86399)), "23h ago");
        assert_eq!(ago(now, at(86400)), "1d ago");
        assert_eq!(ago(now, at(-30)), "just now", "clock skew");
    }

    #[test]
    fn split_path_separates_folder_and_file() {
        assert_eq!(split_path("a/b/c.txt"), ("a/b", "c.txt"));
        assert_eq!(split_path("c.txt"), ("", "c.txt"));
        assert_eq!(split_path("a/b/"), ("a", "b"));
    }

    #[test]
    fn recent_caption_joins_the_known_parts() {
        let now = ts("2026-11-02T12:00:00Z");
        let mut t = done("Docs/a.bin", 2048, Some("2026-11-02T11:55:00Z"));
        assert_eq!(recent_caption(&t, now), "Docs · 2.0 KiB · 5m ago");
        t.times = 3;
        assert_eq!(recent_caption(&t, now), "Docs · 2.0 KiB · 5m ago · ×3");
        let t = done("top.bin", -1, None);
        assert_eq!(recent_caption(&t, now), "", "nothing known, nothing shown");
    }

    #[test]
    fn empty_history_text_explains_itself() {
        let mut s = status();
        assert_eq!(
            empty_history_text(&s),
            "Nothing synced since rclone started"
        );
        s.rc_error = Some("down".into());
        assert_eq!(empty_history_text(&s), "History unavailable");
        s.rc_error = None;
        s.service_active = false;
        assert_eq!(empty_history_text(&s), "History unavailable");
    }

    #[test]
    fn first_history_is_shown_immediately() {
        let mut app = AppModel::default();
        let mut s = status();
        s.recent = vec![done("a", 1, None)];
        let _ = app.update(Message::StatusUpdated(s));
        assert_eq!(app.shown.len(), 1);
        assert!(!app.history_pending);
    }

    #[test]
    fn later_history_changes_wait_for_the_debounce() {
        let mut app = AppModel {
            shown: vec![done("a", 1, None)],
            ..Default::default()
        };
        let mut s = status();
        s.recent = vec![done("b", 1, None), done("a", 1, None)];
        let _ = app.update(Message::StatusUpdated(s.clone()));
        assert_eq!(app.shown.len(), 1, "not yet");
        assert!(app.history_pending);

        // Further updates while waiting neither show anything nor schedule a second timer.
        s.recent.insert(0, done("c", 1, None));
        let _ = app.update(Message::StatusUpdated(s));
        assert_eq!(app.shown.len(), 1);
        assert!(app.history_pending);

        let _ = app.update(Message::HistoryDebounced);
        assert!(!app.history_pending);
        assert_eq!(app.shown.len(), 3, "catches up to the newest status");
    }

    #[test]
    fn unchanged_history_schedules_nothing() {
        let mut app = AppModel {
            shown: vec![done("a", 1, None)],
            ..Default::default()
        };
        let mut s = status();
        s.recent = vec![done("a", 1, None)];
        let _ = app.update(Message::StatusUpdated(s));
        assert!(!app.history_pending);
    }

    #[test]
    fn history_page_navigation_round_trips() {
        let mut app = AppModel::default();
        assert_eq!(app.page, Page::Main);
        let _ = app.update(Message::ShowHistory);
        assert_eq!(app.page, Page::History);
        let _ = app.update(Message::ShowMain);
        assert_eq!(app.page, Page::Main);
    }

    #[test]
    fn closing_the_popup_returns_to_the_main_page() {
        let mut app = AppModel::default();
        let id = Id::unique();
        app.popup = Some(id);
        app.page = Page::History;
        let _ = app.update(Message::PopupClosed(id));
        assert_eq!(app.page, Page::Main);
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
