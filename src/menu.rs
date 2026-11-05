// SPDX-License-Identifier: GPL-3.0-only
//! The "rclone…" window opened from COSMIC Files' right-click menu (`--menu PATH...`).
//! COSMIC Files can't nest custom actions in a submenu, so it has one entry that launches this
//! window. It shows the item's sync status, who it is shared with, and the actions that apply.
//!
//! Data arrives in stages (what the item is → rclone's local view → Google Drive). Rather than
//! have every field show its own loading state and the layout shift as each lands, the window
//! shows one animated loading screen (a cloud inside a spinning pair of chasing arrows) until
//! everything has arrived, then swaps to the finished layout in one step. That layout is fixed
//! anyway: a non-resizable window, fixed-height sections and fixed column widths.

use crate::config::expand;
use crate::config::Config;
use crate::drive::{self, DriveInfo, Kind, Permission};
use crate::keeper;
use crate::mime_icon;
use crate::pin::{self, OfflineState, Phase};
use crate::share::{self, Target};
use crate::status::{self, human_bytes, Local, SyncState};
use cosmic::app::Settings;
use cosmic::iced::advanced::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::{
    time, Alignment, Length, Limits, Padding, Radians, Rotation, Size, Subscription,
};
use cosmic::prelude::*;
use cosmic::widget;
use std::path::Path;
use std::time::{Duration, Instant};

const WIDTH: f32 = 480.0;
const HEIGHT: f32 = 680.0;
const IDENTITY_H: f32 = 40.0;
const STATUS_H: f32 = 152.0;
const DETAIL_ROW_H: f32 = 24.0;
const DETAIL_LABEL_W: f32 = 132.0;
const PERM_ROW_H: f32 = 40.0;
const OFFLINE_ROW_H: f32 = 40.0;
/// How often the window re-reads the transfer counts and the keeper's progress.
const OFFLINE_REFRESH: Duration = Duration::from_secs(2);
const ROLE_W: f32 = 96.0;
const TYPE_W: f32 = 136.0;
/// Keeps column headers aligned with rows that sit beside the scrollbar.
const SCROLLBAR_GUTTER: f32 = 12.0;
/// Show the loading screen at least this long, so a fast load doesn't flash it.
const MIN_LOADING: Duration = Duration::from_millis(450);
/// One full turn of the spinner.
const SPIN_PERIOD: Duration = Duration::from_millis(1800);
const SPINNER_SIZE: u16 = 128;
const FRAME: Duration = Duration::from_millis(16);
const SKELETON_ROWS: usize = 3;
const PLACEHOLDER: &str = "…";
const NONE: &str = "—";

#[derive(Debug, Clone)]
enum Message {
    Resolved(Result<Target, String>),
    Local(Result<Local, String>),
    Drive(Result<DriveInfo, String>),
    ManageSharing,
    /// Expand "Open" into "Folder" / "File".
    ToggleOpen,
    /// Open this path with the default app (a folder opens in the file manager).
    OpenPath(String),
    /// Animation frame while loading.
    Tick(Instant),
    /// Turn "Keep offline" on or off for this folder.
    SetOffline(bool),
    OfflineSet(Result<String, String>),
    /// Time to re-read the keeper's progress.
    OfflineTick,
    OfflineRefreshed(Box<OfflineSnapshot>),
}

/// The offline-folder state as of one refresh. `cfg` is `None` if the refresh failed.
#[derive(Debug, Clone, Default)]
struct OfflineSnapshot {
    cfg: Option<Config>,
    state: OfflineState,
    /// Files and bytes in the local copy of the folder at `path` (or the one it's inside).
    usage: Option<(u64, u64)>,
}

/// Re-read the settings and the keeper's report, and measure the folder's local copy while it
/// is downloading or synced (it's cheap: a walk of local metadata).
fn offline_snapshot(path: &str) -> OfflineSnapshot {
    let cfg = Config::load(crate::APP_ID);
    let state = OfflineState::load();
    let usage = share::remote_path(&expand(&cfg.mount_path), Path::new(path))
        .and_then(|rel| pin::covering_pin(&cfg.pins, &rel).map(str::to_string))
        .filter(|p| {
            state
                .pin(p)
                .is_some_and(|s| matches!(s.phase, Phase::Downloading | Phase::Ready))
        })
        .map(|p| pin::local_usage(&p));
    OfflineSnapshot {
        cfg: Some(cfg),
        state,
        usage,
    }
}

/// A value that is on its way, arrived, or failed.
enum Load<T> {
    Pending,
    Done(Result<T, String>),
}

impl<T> Load<T> {
    fn ok(&self) -> Option<&T> {
        match self {
            Load::Done(Ok(v)) => Some(v),
            _ => None,
        }
    }

    fn err(&self) -> Option<&str> {
        match self {
            Load::Done(Err(e)) => Some(e),
            _ => None,
        }
    }
}

struct Menu {
    core: cosmic::Core,
    cfg: Config,
    name: String,
    path: String,
    is_dir: bool,
    target: Load<Target>,
    local: Load<Local>,
    drive: Load<DriveInfo>,
    /// "Open" has been clicked and shows its "Folder" / "File" choices.
    open_expanded: bool,
    /// The offline-folder keeper's last report.
    offline: OfflineState,
    /// Why the last "Keep offline" change failed.
    offline_error: Option<String>,
    /// Files and bytes downloaded into the folder's local copy so far.
    offline_usage: Option<(u64, u64)>,
    /// A refresh of `local` is in flight (so ticks don't stack them up).
    local_busy: bool,
    started: Instant,
    /// The time of the latest animation frame.
    now: Instant,
}

struct Flags {
    cfg: Config,
    paths: Vec<String>,
}

type Task = cosmic::app::Task<Message>;

fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    msg: impl Fn(T) -> Message + Send + 'static,
    fallback: impl Fn(String) -> T + Send + 'static,
) -> Task {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(f)
                .await
                .unwrap_or_else(|e| fallback(e.to_string()))
        },
        move |r| cosmic::Action::App(msg(r)),
    )
}

impl cosmic::Application for Menu {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = crate::APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(mut core: cosmic::Core, flags: Flags) -> (Self, Task) {
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.set_header_title("rclone".into());
        let first = flags.paths.first().cloned().unwrap_or_default();
        let name = Path::new(&first)
            .file_name()
            .map_or_else(|| first.clone(), |n| n.to_string_lossy().into_owned());
        let is_dir = Path::new(&first).is_dir();
        let menu = Menu {
            core,
            cfg: flags.cfg.clone(),
            name,
            path: first,
            is_dir,
            target: Load::Pending,
            local: Load::Pending,
            drive: Load::Pending,
            open_expanded: false,
            offline: if flags.cfg.offline_enabled {
                OfflineState::load()
            } else {
                OfflineState::default()
            },
            offline_error: None,
            offline_usage: None,
            local_busy: false,
            started: Instant::now(),
            now: Instant::now(),
        };
        let (cfg, paths) = (flags.cfg, flags.paths);
        let task = blocking(move || share::resolve(&cfg, &paths), Message::Resolved, Err);
        (menu, task)
    }

    fn update(&mut self, message: Message) -> Task {
        match message {
            Message::Resolved(r) => {
                let task = match &r {
                    Ok(t) => {
                        let (cfg, cfg2) = (self.cfg.clone(), self.cfg.clone());
                        let (t1, t2) = (t.clone(), t.clone());
                        self.local_busy = true;
                        Task::batch([
                            blocking(move || status::local(&cfg, &t1), Message::Local, Err),
                            blocking(move || drive::fetch(&cfg2, &t2), Message::Drive, Err),
                        ])
                    }
                    Err(e) => {
                        self.local = Load::Done(Err(e.clone()));
                        self.drive = Load::Done(Err(e.clone()));
                        Task::none()
                    }
                };
                self.target = Load::Done(r);
                return task;
            }
            Message::Local(r) => {
                self.local_busy = false;
                // A refresh that fails keeps showing the last good answer.
                if r.is_ok() || self.local.ok().is_none() {
                    self.local = Load::Done(r);
                }
            }
            Message::Drive(r) => self.drive = Load::Done(r),
            Message::Tick(now) => self.now = now,
            Message::ToggleOpen => self.open_expanded = !self.open_expanded,
            Message::SetOffline(on) => {
                let path = self.path.clone();
                return blocking(
                    move || keeper::set_pinned(&path, on),
                    Message::OfflineSet,
                    Err,
                );
            }
            Message::OfflineSet(r) => match r {
                Ok(_) => {
                    self.offline_error = None;
                    self.cfg = Config::load(crate::APP_ID);
                    return self.update(Message::OfflineTick);
                }
                Err(e) => self.offline_error = Some(e),
            },
            Message::OfflineTick => {
                let mut tasks = vec![];
                if self.cfg.offline_enabled {
                    let path = self.path.clone();
                    tasks.push(blocking(
                        move || Box::new(offline_snapshot(&path)),
                        Message::OfflineRefreshed,
                        |_| Box::new(OfflineSnapshot::default()),
                    ));
                }
                // Transfers and queues change while the window is open; keep the counts live.
                if !self.local_busy {
                    if let Some(t) = self.target.ok().cloned() {
                        self.local_busy = true;
                        let cfg = self.cfg.clone();
                        tasks.push(blocking(
                            move || status::local(&cfg, &t),
                            Message::Local,
                            Err,
                        ));
                    }
                }
                return Task::batch(tasks);
            }
            Message::OfflineRefreshed(snap) => {
                if snap.cfg.is_some() {
                    let OfflineSnapshot { cfg, state, usage } = *snap;
                    self.cfg = cfg.unwrap_or_default();
                    self.offline = state;
                    self.offline_usage = usage;
                }
            }
            Message::OpenPath(p) => {
                share::open_url(&p);
                std::process::exit(0);
            }
            Message::ManageSharing => {
                if let Some(t) = self.target.ok() {
                    share::open_url(&share::sharing_url(t));
                }
                // The browser is launched detached, so there is nothing left to wait for.
                std::process::exit(0);
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.loading() {
            time::every(FRAME).map(Message::Tick)
        } else {
            // Follow transfers, the queue and the offline keeper while the window is open.
            time::every(OFFLINE_REFRESH).map(|_| Message::OfflineTick)
        }
    }

    fn view(&self) -> Element<'_, Message> {
        if self.loading() {
            return self.loading_view();
        }
        let sp = cosmic::theme::spacing();
        let body = widget::column::with_capacity(8)
            .spacing(sp.space_xs)
            .push(self.identity())
            .push(widget::divider::horizontal::default())
            .push(self.status_section())
            .push(widget::divider::horizontal::default())
            .push(self.sharing_header())
            .push(self.sharing_list())
            .push(widget::divider::horizontal::default());
        let body = match self.offline_row() {
            Some(row) => body.push(row).push(widget::divider::horizontal::default()),
            None => body,
        };
        let body = body.push(self.actions());
        widget::container(body)
            .padding(sp.space_s)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Menu {
    /// Still waiting on data, or the loading screen hasn't been up long enough yet.
    fn loading(&self) -> bool {
        let waiting = matches!(self.target, Load::Pending)
            || matches!(self.local, Load::Pending)
            || matches!(self.drive, Load::Pending);
        waiting || self.now.duration_since(self.started) < MIN_LOADING
    }

    /// A cloud with two arrows chasing each other around it, centred in the window.
    fn loading_view(&self) -> Element<'_, Message> {
        let layer = |svg: &'static [u8]| {
            let mut handle = widget::icon::from_svg_bytes(svg);
            handle.symbolic = true;
            widget::icon(handle).size(SPINNER_SIZE)
        };
        let turns = self.now.duration_since(self.started).as_secs_f32() / SPIN_PERIOD.as_secs_f32();
        let angle = turns.fract() * std::f32::consts::TAU;
        let spinner = cosmic::iced::widget::stack![
            layer(include_bytes!("../resources/loading/ring.svg"))
                .rotation(Rotation::Floating(Radians(angle))),
            layer(include_bytes!("../resources/loading/cloud.svg")),
        ]
        .width(Length::Fixed(f32::from(SPINNER_SIZE)))
        .height(Length::Fixed(f32::from(SPINNER_SIZE)));
        let col = widget::column::with_capacity(2)
            .spacing(cosmic::theme::spacing().space_s)
            .align_x(Alignment::Center)
            .push(spinner)
            .push(widget::text::body("Checking sync status…"));
        widget::container(col)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    }

    // --- 1. identity: known immediately, never changes ---

    fn identity(&self) -> Element<'_, Message> {
        let icon: Element<'_, Message> = if self.is_dir {
            widget::icon::from_name("folder-symbolic").size(32).into()
        } else {
            widget::icon(mime_icon::file_icon(&self.name, 32))
                .size(32)
                .into()
        };
        let text = widget::column::with_capacity(2)
            .width(Length::Fill)
            .push(widget::text::heading(&self.name).ellipsize(Ellipsize::Middle(lines())))
            .push(widget::text::caption(&self.path).ellipsize(Ellipsize::Start(lines())));
        let row = widget::row::with_capacity(2)
            .spacing(cosmic::theme::spacing().space_xs)
            .align_y(Alignment::Center)
            .push(icon)
            .push(text);
        widget::container(row)
            .height(Length::Fixed(IDENTITY_H))
            .align_y(Alignment::Center)
            .into()
    }

    // --- 2. sync status: always a title, a status line and exactly four detail rows ---

    /// Icon, label and right-hand caption for the status line.
    fn status_line(&self) -> (&'static str, String, String) {
        const LOADING: &str = "content-loading-symbolic";
        if let Some(e) = self.target.err() {
            return ("dialog-warning-symbolic", e.to_string(), String::new());
        }
        if let Some(e) = self.local.err() {
            return ("dialog-error-symbolic", "Error".into(), e.to_string());
        }
        // An offline folder is downloaded and synced by the keeper (a bisync job that writes
        // straight into the local copy), which the mount's own transfer counts below never see.
        if let Some(status) = self.offline_status() {
            return status;
        }
        let checking = (LOADING, "Checking…".to_string(), String::new());
        let Some(local) = self.local.ok() else {
            return checking;
        };
        match local {
            Local::File {
                transferring,
                cached,
            } => match status::file_state(*transferring, cached.as_ref(), self.drive.ok()) {
                Some(s) => (s.icon(), s.label().into(), String::new()),
                None if self.drive.err().is_some() => (
                    "dialog-warning-symbolic",
                    "Cached (can't compare with Drive)".into(),
                    String::new(),
                ),
                None => checking,
            },
            Local::Folder(f) => {
                let more = if f.truncated { " or more" } else { "" };
                if f.uploading + f.downloading > 0 {
                    let n = f.uploading + f.downloading;
                    (
                        SyncState::LocalNewer.icon(),
                        "Transferring".into(),
                        format!("{n}{more} file(s)"),
                    )
                } else if f.waiting_to_upload > 0 {
                    (
                        SyncState::QueuedUpload.icon(),
                        SyncState::QueuedUpload.label().into(),
                        format!("{}{more} file(s)", f.waiting_to_upload),
                    )
                } else {
                    (
                        SyncState::InSync.icon(),
                        "Nothing pending".into(),
                        String::new(),
                    )
                }
            }
        }
    }

    /// The status line for an item inside an offline folder whose keeper is busy (or stuck).
    fn offline_status(&self) -> Option<(&'static str, String, String)> {
        if !self.cfg.offline_enabled {
            return None;
        }
        let rel = share::remote_path(&expand(&self.cfg.mount_path), Path::new(&self.path))?;
        let p = self.offline.pin(pin::covering_pin(&self.cfg.pins, &rel)?)?;
        let so_far = || match self.offline_usage {
            Some((n, b)) if n > 0 => format!("{n} file(s), {} so far", human_bytes(b)),
            _ => String::new(),
        };
        match (p.phase, p.syncing) {
            (Phase::Downloading, _) => Some((
                SyncState::Downloading.icon(),
                "Downloading for offline use".into(),
                so_far(),
            )),
            (Phase::Removing, _) => Some((
                "content-loading-symbolic",
                "Removing offline copy".into(),
                String::new(),
            )),
            (Phase::Attention, _) => Some((
                "dialog-warning-symbolic",
                "Offline sync needs attention".into(),
                String::new(),
            )),
            (Phase::Ready | Phase::Waiting, true) => Some((
                SyncState::LocalNewer.icon(),
                "Syncing offline copy".into(),
                String::new(),
            )),
            (Phase::Waiting, false) => Some((
                "content-loading-symbolic",
                "Waiting to sync offline copy".into(),
                String::new(),
            )),
            // A file inside an idle offline folder: not in the mount's cache, but here.
            (Phase::Ready, false) if !self.is_dir => {
                let inside = rel.strip_prefix(p.rel.as_str()).unwrap_or_default();
                Some(if p.pending.iter().any(|n| n == inside.trim_matches('/')) {
                    (
                        SyncState::QueuedUpload.icon(),
                        SyncState::QueuedUpload.label().into(),
                        String::new(),
                    )
                } else {
                    (
                        SyncState::InSync.icon(),
                        "Available offline".into(),
                        String::new(),
                    )
                })
            }
            (Phase::Ready, false) => None,
        }
    }

    /// "362 file(s), 563.1 MiB" for a folder that is itself kept offline.
    fn offline_copy(&self) -> Option<String> {
        if !self.cfg.offline_enabled {
            return None;
        }
        let rel = share::remote_path(&expand(&self.cfg.mount_path), Path::new(&self.path))?;
        (pin::covering_pin(&self.cfg.pins, &rel)? == rel).then_some(())?;
        let (n, b) = self.offline_usage?;
        Some(format!("{n} file(s), {}", human_bytes(b)))
    }

    /// The four label/value pairs under the status line, with placeholders while loading.
    fn detail_rows(&self) -> [(&'static str, String); 4] {
        let wait = |l: &Load<_>| l.ok().is_none() && l.err().is_none();
        let from_drive = |f: &dyn Fn(&DriveInfo) -> Option<String>| match &self.drive {
            Load::Pending => PLACEHOLDER.to_string(),
            Load::Done(Ok(d)) => f(d).unwrap_or_else(|| NONE.into()),
            Load::Done(Err(_)) => NONE.to_string(),
        };
        if self.is_dir {
            let from_local = |f: &dyn Fn(&status::FolderSummary) -> String| match self.local.ok() {
                Some(Local::Folder(s)) => f(s),
                _ if wait(&self.local) => PLACEHOLDER.to_string(),
                _ => NONE.to_string(),
            };
            return [
                ("Uploading", from_local(&|s| s.uploading.to_string())),
                ("Queued", from_local(&|s| s.waiting_to_upload.to_string())),
                ("Downloading", from_local(&|s| s.downloading.to_string())),
                // An offline folder's files live in its local copy, not in the mount's cache.
                match self.offline_copy() {
                    Some(copy) => ("Offline copy", copy),
                    None => (
                        "Cached",
                        from_local(&|s| {
                            format!(
                                "{}{} file(s), {}",
                                s.cached_files,
                                if s.truncated { "+" } else { "" },
                                human_bytes(s.cached_bytes)
                            )
                        }),
                    ),
                },
            ];
        }
        let cached = match self.local.ok() {
            Some(Local::File { cached, .. }) => match cached {
                None => "No".to_string(),
                Some(c) if c.have >= c.size => "Yes".to_string(),
                Some(c) => format!(
                    "Partly ({} of {})",
                    human_bytes(c.have),
                    human_bytes(c.size)
                ),
            },
            _ if wait(&self.local) => PLACEHOLDER.to_string(),
            _ => NONE.to_string(),
        };
        let size = match (&self.drive, self.local.ok()) {
            (Load::Done(Ok(DriveInfo { size: Some(s), .. })), _) => human_bytes(*s),
            (
                _,
                Some(Local::File {
                    cached: Some(c), ..
                }),
            ) => human_bytes(c.size),
            (Load::Pending, _) => PLACEHOLDER.to_string(),
            _ => NONE.to_string(),
        };
        [
            (
                "Drive modified",
                from_drive(&|d| d.modified.map(status::fmt_time)),
            ),
            ("Cached locally", cached),
            ("Size", size),
            ("Version", from_drive(&|d| d.version.clone())),
        ]
    }

    fn status_section(&self) -> Element<'_, Message> {
        let (icon, label, detail) = self.status_line();
        let line = widget::row::with_capacity(3)
            .spacing(cosmic::theme::spacing().space_xxs)
            .align_y(Alignment::Center)
            .push(widget::icon::from_name(icon).size(16))
            .push(widget::text::body(label).ellipsize(Ellipsize::End(lines())))
            .push(
                widget::container(widget::text::caption(detail).ellipsize(Ellipsize::End(lines())))
                    .width(Length::Fill)
                    .align_x(Alignment::End),
            );
        let mut col = widget::column::with_capacity(6)
            .push(
                widget::container(widget::text::heading("Sync status"))
                    .height(Length::Fixed(20.0))
                    .align_y(Alignment::Center),
            )
            .push(
                widget::container(line)
                    .height(Length::Fixed(32.0))
                    .align_y(Alignment::Center),
            );
        for (label, value) in self.detail_rows() {
            col = col.push(
                widget::container(
                    widget::row::with_capacity(2)
                        .align_y(Alignment::Center)
                        .push(
                            widget::container(widget::text::caption(label))
                                .width(Length::Fixed(DETAIL_LABEL_W)),
                        )
                        .push(
                            widget::container(
                                widget::text::body(value).ellipsize(Ellipsize::End(lines())),
                            )
                            .width(Length::Fill),
                        ),
                )
                .height(Length::Fixed(DETAIL_ROW_H))
                .align_y(Alignment::Center),
            );
        }
        widget::container(col)
            .height(Length::Fixed(STATUS_H))
            .width(Length::Fill)
            .into()
    }

    // --- 3. sharing: header and column titles are fixed; the list scrolls in a fixed viewport ---

    /// The caption on the right of the "Sharing" heading.
    fn sharing_summary(&self) -> String {
        match &self.drive {
            Load::Pending => PLACEHOLDER.to_string(),
            Load::Done(Err(_)) => "Unavailable".to_string(),
            Load::Done(Ok(d)) if !d.shared => "Not shared".to_string(),
            Load::Done(Ok(d)) => {
                let others = d
                    .permissions
                    .iter()
                    .filter(|p| p.role != drive::Role::Owner)
                    .count();
                format!("Shared with {others}")
            }
        }
    }

    fn sharing_header(&self) -> Element<'_, Message> {
        let summary = self.sharing_summary();
        let title = widget::container(
            widget::row::with_capacity(2)
                .align_y(Alignment::Center)
                .push(widget::text::heading("Sharing").width(Length::Fill))
                .push(widget::text::caption(summary)),
        )
        .height(Length::Fixed(24.0))
        .align_y(Alignment::Center);
        widget::column::with_capacity(3)
            .spacing(cosmic::theme::spacing().space_xxs)
            .push(title)
            .push(
                widget::container(perm_columns(
                    widget::text::caption_heading("Name").into(),
                    widget::text::caption_heading("Role").into(),
                    widget::text::caption_heading("Type").into(),
                ))
                .height(Length::Fixed(20.0)),
            )
            .push(widget::divider::horizontal::light())
            .into()
    }

    fn sharing_list(&self) -> Element<'_, Message> {
        let mut col = widget::column::with_capacity(8).width(Length::Fill);
        match &self.drive {
            Load::Pending => {
                for _ in 0..SKELETON_ROWS {
                    col = col.push(perm_row(
                        widget::text::caption(PLACEHOLDER).into(),
                        widget::text::caption(PLACEHOLDER).into(),
                        widget::text::caption(PLACEHOLDER).into(),
                    ));
                }
            }
            Load::Done(Err(e)) => {
                col = col.push(note_row(
                    "dialog-warning-symbolic",
                    "Couldn't load sharing info",
                    Some(e),
                ));
            }
            Load::Done(Ok(d)) if !d.shared => {
                col = col.push(note_row(
                    "changes-prevent-symbolic",
                    "Only you have access",
                    None,
                ));
            }
            Load::Done(Ok(d)) if d.permissions.is_empty() => {
                col = col.push(note_row(
                    "dialog-information-symbolic",
                    "Google Drive didn't list who has access",
                    None,
                ));
            }
            Load::Done(Ok(d)) => {
                for (i, p) in d.permissions.iter().enumerate() {
                    if i > 0 {
                        col = col.push(widget::divider::horizontal::light());
                    }
                    col = col.push(permission_row(p));
                }
            }
        }
        widget::scrollable(col).height(Length::Fill).into()
    }

    // --- offline: only when offline folders are on ---

    /// The "Keep offline" switch for a folder, or a note for things inside an offline folder.
    fn offline_row(&self) -> Option<Element<'_, Message>> {
        if !self.cfg.offline_enabled {
            return None;
        }
        let rel = share::remote_path(&expand(&self.cfg.mount_path), Path::new(&self.path))?;
        let covering = pin::covering_pin(&self.cfg.pins, &rel);
        let (title, caption, toggle) = match covering {
            Some(p) if p != rel => (
                "Available offline".to_string(),
                format!(
                    "Part of the offline folder {p} · {}",
                    pin::describe(self.offline.pin(p), None)
                ),
                None,
            ),
            _ if !self.is_dir => return None,
            Some(p) => (
                "Keep offline".to_string(),
                pin::describe(self.offline.pin(p), self.offline_usage),
                Some(true),
            ),
            None => (
                "Keep offline".to_string(),
                "Download this folder and keep it in sync both ways".to_string(),
                Some(false),
            ),
        };
        let caption = self.offline_error.clone().unwrap_or(caption);
        let text = widget::column::with_capacity(2)
            .width(Length::Fill)
            .push(widget::text::body(title))
            .push(widget::text::caption(caption).ellipsize(Ellipsize::End(lines())));
        let mut row = widget::row::with_capacity(3)
            .spacing(cosmic::theme::spacing().space_xs)
            .align_y(Alignment::Center)
            .push(widget::icon::from_name("folder-download-symbolic").size(16))
            .push(text);
        if let Some(on) = toggle {
            row = row.push(widget::toggler(on).on_toggle(Message::SetOffline));
        }
        Some(
            widget::container(row)
                .height(Length::Fixed(OFFLINE_ROW_H))
                .align_y(Alignment::Center)
                .into(),
        )
    }

    // --- 4. actions: pinned last, present from the start ---

    fn actions(&self) -> Element<'_, Message> {
        let label = self.target.ok().map_or_else(
            || "Manage Sharing".to_string(),
            |t| format!("Manage Sharing ({})", t.provider.name()),
        );
        // "Open" expands in place into "Folder" / "File", so it keeps the same slot and the row
        // never changes size. A folder has nothing to choose between, so it opens directly.
        let open: Element<'_, Message> = if self.is_dir {
            widget::button::standard("Open")
                .on_press(Message::OpenPath(self.path.clone()))
                .width(Length::Fill)
                .into()
        } else if self.open_expanded {
            let parent = Path::new(&self.path)
                .parent()
                .map_or_else(String::new, |p| p.to_string_lossy().into_owned());
            widget::row::with_capacity(2)
                .spacing(cosmic::theme::spacing().space_xxs)
                .push(
                    widget::button::standard("Folder")
                        .on_press(Message::OpenPath(parent))
                        .width(Length::Fill),
                )
                .push(
                    widget::button::standard("File")
                        .on_press(Message::OpenPath(self.path.clone()))
                        .width(Length::Fill),
                )
                .into()
        } else {
            widget::button::standard("Open")
                .on_press(Message::ToggleOpen)
                .width(Length::Fill)
                .into()
        };
        widget::container(
            widget::row::with_capacity(2)
                .spacing(cosmic::theme::spacing().space_xxs)
                .push(widget::container(open).width(Length::FillPortion(2)))
                .push(
                    widget::button::suggested(label)
                        .on_press_maybe(self.target.ok().map(|_| Message::ManageSharing))
                        .width(Length::FillPortion(3)),
                ),
        )
        .height(Length::Fixed(32.0))
        .width(Length::Fill)
        .into()
    }
}

fn lines() -> EllipsizeHeightLimit {
    EllipsizeHeightLimit::Lines(1)
}

/// Name / Role / Type with the shared column widths.
fn perm_columns<'a>(
    name: Element<'a, Message>,
    role: Element<'a, Message>,
    kind: Element<'a, Message>,
) -> Element<'a, Message> {
    widget::row::with_capacity(3)
        .spacing(cosmic::theme::spacing().space_xxs)
        .align_y(Alignment::Center)
        .padding(Padding {
            right: SCROLLBAR_GUTTER,
            ..Padding::ZERO
        })
        .push(widget::container(name).width(Length::Fill))
        .push(widget::container(role).width(Length::Fixed(ROLE_W)))
        .push(widget::container(kind).width(Length::Fixed(TYPE_W)))
        .into()
}

fn perm_row<'a>(
    name: Element<'a, Message>,
    role: Element<'a, Message>,
    kind: Element<'a, Message>,
) -> Element<'a, Message> {
    widget::container(perm_columns(name, role, kind))
        .height(Length::Fixed(PERM_ROW_H))
        .align_y(Alignment::Center)
        .into()
}

fn permission_row(p: &Permission) -> Element<'_, Message> {
    let mut name = widget::column::with_capacity(2)
        .push(widget::text::body(&p.name).ellipsize(Ellipsize::End(lines())));
    if !p.address.is_empty() && p.address != p.name {
        name = name.push(widget::text::caption(&p.address).ellipsize(Ellipsize::Middle(lines())));
    }
    let full = if p.address.is_empty() || p.address == p.name {
        p.name.clone()
    } else {
        format!("{} <{}>", p.name, p.address)
    };
    let name = widget::tooltip(
        name,
        widget::text::body(full),
        widget::tooltip::Position::Bottom,
    );
    let icon = match p.kind {
        Kind::User => "avatar-default-symbolic",
        Kind::Group => "system-users-symbolic",
        Kind::Domain => "network-workgroup-symbolic",
        Kind::Anyone => "insert-link-symbolic",
    };
    let kind = widget::row::with_capacity(2)
        .spacing(cosmic::theme::spacing().space_xxs)
        .align_y(Alignment::Center)
        .push(widget::icon::from_name(icon).size(16))
        .push(widget::text::caption(p.kind.label()).ellipsize(Ellipsize::End(lines())));
    perm_row(
        name.into(),
        widget::text::body(p.role.label()).into(),
        kind.into(),
    )
}

/// A single full-width row for empty and error states, same height as a data row.
fn note_row<'a>(
    icon: &'static str,
    text: &'a str,
    tooltip: Option<&'a str>,
) -> Element<'a, Message> {
    let label = widget::text::body(text).ellipsize(Ellipsize::End(lines()));
    let label: Element<'a, Message> = match tooltip {
        Some(t) => widget::tooltip(
            label,
            widget::text::body(t),
            widget::tooltip::Position::Bottom,
        )
        .into(),
        None => label.into(),
    };
    widget::container(
        widget::row::with_capacity(2)
            .spacing(cosmic::theme::spacing().space_xxs)
            .align_y(Alignment::Center)
            .push(widget::icon::from_name(icon).size(16))
            .push(label),
    )
    .height(Length::Fixed(PERM_ROW_H))
    .width(Length::Fill)
    .align_y(Alignment::Center)
    .into()
}

/// Open the window and block until it closes.
pub fn run(cfg: Config, paths: Vec<String>) -> cosmic::iced::Result {
    let settings = Settings::default()
        .size(Size::new(WIDTH, HEIGHT))
        // `resizable(None)` alone doesn't stop resizing; pinning min = max does.
        .size_limits(
            Limits::NONE
                .min_width(WIDTH)
                .max_width(WIDTH)
                .min_height(HEIGHT)
                .max_height(HEIGHT),
        )
        .resizable(None);
    cosmic::app::run::<Menu>(settings, Flags { cfg, paths })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Permission, Role};
    use crate::share::Provider;
    use crate::status::{Cached, FolderSummary};
    use cosmic::Application as _;

    fn menu(is_dir: bool) -> Menu {
        let now = Instant::now();
        Menu {
            core: cosmic::Core::default(),
            cfg: Config::default(),
            name: "a.txt".into(),
            path: "/m/d/a.txt".into(),
            is_dir,
            target: Load::Pending,
            local: Load::Pending,
            drive: Load::Pending,
            open_expanded: false,
            offline: OfflineState::default(),
            offline_error: None,
            offline_usage: None,
            local_busy: false,
            started: now,
            now,
        }
    }

    fn target(is_dir: bool) -> Target {
        Target {
            provider: Provider::GoogleDrive,
            id: "id".into(),
            is_dir,
            fs: "gdrive:".into(),
            rel: "d/a.txt".into(),
        }
    }

    fn cached(size: u64, have: u64, dirty: bool) -> Cached {
        Cached {
            size,
            have,
            mtime: Some("2026-05-10T02:33:55Z".parse().unwrap()),
            dirty,
        }
    }

    fn file_local(transferring: bool, cached: Option<Cached>) -> Local {
        Local::File {
            transferring,
            cached,
        }
    }

    fn drive_info() -> DriveInfo {
        DriveInfo {
            modified: Some("2026-05-10T02:33:55Z".parse().unwrap()),
            size: Some(10),
            version: Some("7".into()),
            ..DriveInfo::default()
        }
    }

    /// A menu whose three loads have all finished and whose loading screen has timed out.
    fn loaded(is_dir: bool, local: Local, drive: Result<DriveInfo, String>) -> Menu {
        let mut m = menu(is_dir);
        m.target = Load::Done(Ok(target(is_dir)));
        m.local = Load::Done(Ok(local));
        m.drive = Load::Done(drive);
        m.now = m.started + MIN_LOADING;
        m
    }

    fn perm(name: &str, role: Role) -> Permission {
        Permission {
            name: name.into(),
            address: format!("{name}@example.com"),
            role,
            kind: Kind::User,
        }
    }

    // ---- loading screen ----

    #[test]
    fn loading_until_every_stage_has_answered() {
        let mut m = menu(false);
        m.now = m.started + MIN_LOADING * 4; // plenty of time has passed
        assert!(m.loading());
        m.target = Load::Done(Ok(target(false)));
        assert!(m.loading(), "local and drive still pending");
        m.local = Load::Done(Ok(file_local(false, None)));
        assert!(m.loading(), "drive still pending");
        m.drive = Load::Done(Ok(drive_info()));
        assert!(!m.loading());
    }

    #[test]
    fn a_fast_load_still_shows_the_spinner_for_the_minimum_time() {
        let mut m = loaded(false, file_local(false, None), Ok(drive_info()));
        m.now = m.started + MIN_LOADING.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(m.loading());
        m.now = m.started + MIN_LOADING;
        assert!(!m.loading());
    }

    #[test]
    fn ticks_advance_the_animation_clock() {
        let mut m = menu(false);
        let later = m.started + Duration::from_millis(300);
        let _ = m.update(Message::Tick(later));
        assert_eq!(m.now, later);
    }

    #[test]
    fn a_failed_lookup_fills_every_stage_so_the_spinner_can_end() {
        let mut m = menu(false);
        let _ = m.update(Message::Resolved(Err("Not inside the mount".into())));
        assert_eq!(m.target.err(), Some("Not inside the mount"));
        assert_eq!(m.local.err(), Some("Not inside the mount"));
        assert_eq!(m.drive.err(), Some("Not inside the mount"));
        m.now = m.started + MIN_LOADING;
        assert!(!m.loading());
        // The reason replaces the status label.
        let (icon, label, _) = m.status_line();
        assert_eq!(
            (icon, label.as_str()),
            ("dialog-warning-symbolic", "Not inside the mount")
        );
    }

    #[test]
    fn resolving_starts_the_other_lookups_without_finishing_them() {
        let mut m = menu(false);
        let _ = m.update(Message::Resolved(Ok(target(false))));
        assert!(m.target.ok().is_some());
        assert!(matches!(m.local, Load::Pending));
        assert!(matches!(m.drive, Load::Pending));
        assert!(m.loading());
    }

    #[test]
    fn results_land_in_the_right_slot() {
        let mut m = menu(false);
        let _ = m.update(Message::Local(Ok(file_local(false, None))));
        let _ = m.update(Message::Drive(Err("boom".into())));
        assert!(m.local.ok().is_some());
        assert_eq!(m.drive.err(), Some("boom"));
    }

    // ---- status line ----

    #[test]
    fn status_says_checking_until_it_can_decide() {
        let (icon, label, _) = menu(false).status_line();
        assert_eq!(
            (icon, label.as_str()),
            ("content-loading-symbolic", "Checking…")
        );

        // Cached and complete: needs Drive before it can say "in sync".
        let mut m = menu(false);
        m.target = Load::Done(Ok(target(false)));
        m.local = Load::Done(Ok(file_local(false, Some(cached(10, 10, false)))));
        assert_eq!(m.status_line().1, "Checking…");
    }

    #[test]
    fn uncached_files_do_not_wait_for_drive() {
        let mut m = menu(false);
        m.target = Load::Done(Ok(target(false)));
        m.local = Load::Done(Ok(file_local(false, None)));
        assert_eq!(m.status_line().1, "Online only (not cached)");
    }

    #[test]
    fn cached_files_compare_against_drive() {
        let m = loaded(
            false,
            file_local(false, Some(cached(10, 10, false))),
            Ok(drive_info()),
        );
        let (icon, label, _) = m.status_line();
        assert_eq!(
            (icon, label.as_str()),
            ("emblem-ok-symbolic", "In sync (cached)")
        );
    }

    #[test]
    fn upload_states_come_from_the_local_view() {
        let queued = loaded(
            false,
            file_local(false, Some(cached(10, 10, true))),
            Err("x".into()),
        );
        assert_eq!(queued.status_line().1, "Queued for upload");
        let active = loaded(
            false,
            file_local(true, Some(cached(10, 10, true))),
            Err("x".into()),
        );
        assert_eq!(active.status_line().1, "Uploading");
    }

    #[test]
    fn a_drive_failure_does_not_hide_what_is_known_locally() {
        // Cached and complete but Drive unreachable: say so, don't spin forever.
        let m = loaded(
            false,
            file_local(false, Some(cached(10, 10, false))),
            Err("denied".into()),
        );
        let (icon, label, _) = m.status_line();
        assert_eq!(icon, "dialog-warning-symbolic");
        assert!(label.contains("can't compare"), "{label}");
        // Online-only files don't need Drive at all.
        let m = loaded(false, file_local(false, None), Err("denied".into()));
        assert_eq!(m.status_line().1, "Online only (not cached)");
    }

    #[test]
    fn a_local_failure_is_an_error_with_the_reason() {
        let mut m = loaded(false, file_local(false, None), Ok(drive_info()));
        m.local = Load::Done(Err("rclone not running".into()));
        let (icon, label, detail) = m.status_line();
        assert_eq!(
            (icon, label.as_str(), detail.as_str()),
            ("dialog-error-symbolic", "Error", "rclone not running")
        );
    }

    #[test]
    fn folders_summarise_transfers() {
        let folder =
            |f: FolderSummary| loaded(true, Local::Folder(f), Ok(drive_info())).status_line();

        let (_, label, detail) = folder(FolderSummary {
            uploading: 2,
            downloading: 1,
            ..FolderSummary::default()
        });
        assert_eq!(
            (label.as_str(), detail.as_str()),
            ("Transferring", "3 file(s)")
        );

        let (_, label, detail) = folder(FolderSummary {
            waiting_to_upload: 4,
            ..FolderSummary::default()
        });
        assert_eq!(
            (label.as_str(), detail.as_str()),
            ("Queued for upload", "4 file(s)")
        );

        let (icon, label, _) = folder(FolderSummary::default());
        assert_eq!(
            (icon, label.as_str()),
            ("emblem-ok-symbolic", "Nothing pending")
        );
    }

    /// A menu for an item inside the offline folder `d`, whose keeper is in `phase`.
    fn in_offline_folder(phase: Phase, syncing: bool) -> Menu {
        let mut m = menu(true);
        m.cfg.mount_path = "/m".into();
        m.cfg.offline_enabled = true;
        m.cfg.pins = vec!["d".into()];
        m.path = "/m/d".into();
        m.target = Load::Done(Ok(target(true)));
        m.local = Load::Done(Ok(Local::Folder(FolderSummary::default())));
        m.offline.pins.push(pin::PinState {
            rel: "d".into(),
            phase,
            syncing,
            ..pin::PinState::default()
        });
        m
    }

    #[test]
    fn the_initial_offline_download_is_not_nothing_pending() {
        let mut m = in_offline_folder(Phase::Downloading, true);
        m.offline_usage = Some((12, 3 * 1024 * 1024));
        let (icon, label, detail) = m.status_line();
        assert_eq!(icon, SyncState::Downloading.icon());
        assert_eq!(label, "Downloading for offline use");
        assert_eq!(detail, "12 file(s), 3.0 MiB so far");
        // Even before the first file has landed.
        m.offline_usage = None;
        assert_eq!(m.status_line().2, "");
    }

    #[test]
    fn an_offline_folder_shows_its_copy_instead_of_an_empty_cache() {
        let mut m = in_offline_folder(Phase::Ready, false);
        m.offline_usage = Some((362, 563 * 1024 * 1024));
        let rows = m.detail_rows();
        assert_eq!(rows[3].0, "Offline copy");
        assert!(rows[3].1.starts_with("362 file(s), 563"), "{}", rows[3].1);
        // A folder inside it isn't the pin itself, so it keeps the usual cache row.
        m.path = "/m/d/sub".into();
        assert_eq!(m.detail_rows()[3].0, "Cached");
    }

    #[test]
    fn a_file_in_an_idle_offline_folder_is_available_offline() {
        let mut m = in_offline_folder(Phase::Ready, false);
        m.is_dir = false;
        m.path = "/m/d/sub/a.txt".into();
        assert_eq!(m.status_line().1, "Available offline");
        // ...unless it has a change waiting to upload.
        m.offline.pins[0].pending = vec!["sub/a.txt".into()];
        assert_eq!(m.status_line().1, "Queued for upload");
        m.offline.pins[0].pending = vec!["sub/other.txt".into()];
        assert_eq!(m.status_line().1, "Available offline");
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_counts() {
        let mut m = in_offline_folder(Phase::Ready, false);
        let good = Local::Folder(FolderSummary {
            uploading: 2,
            ..FolderSummary::default()
        });
        let _ = m.update(Message::Local(Ok(good.clone())));
        assert!(!m.local_busy);
        let _ = m.update(Message::Local(Err("rc down".into())));
        assert_eq!(m.local.ok(), Some(&good));
        // But a first failure is shown.
        m.local = Load::Pending;
        let _ = m.update(Message::Local(Err("rc down".into())));
        assert_eq!(m.local.err(), Some("rc down"));
    }

    #[test]
    fn offline_syncs_and_problems_show_in_the_status_line() {
        let label = |phase, syncing| in_offline_folder(phase, syncing).status_line().1;
        assert_eq!(label(Phase::Ready, true), "Syncing offline copy");
        assert_eq!(label(Phase::Waiting, true), "Syncing offline copy");
        assert_eq!(label(Phase::Waiting, false), "Waiting to sync offline copy");
        assert_eq!(
            label(Phase::Attention, false),
            "Offline sync needs attention"
        );
        assert_eq!(label(Phase::Removing, false), "Removing offline copy");
        // A quiet, synced offline folder falls through to the usual status.
        assert_eq!(label(Phase::Ready, false), "Nothing pending");
    }

    #[test]
    fn items_inside_the_offline_folder_follow_its_state_and_others_do_not() {
        let mut m = in_offline_folder(Phase::Downloading, true);
        m.path = "/m/d/sub/a.txt".into();
        assert_eq!(m.status_line().1, "Downloading for offline use");
        m.path = "/m/other".into();
        assert_eq!(m.status_line().1, "Nothing pending");
        m.path = "/m/d".into();
        m.cfg.offline_enabled = false;
        assert_eq!(m.status_line().1, "Nothing pending");
    }

    #[test]
    fn truncated_folder_counts_say_or_more() {
        let (_, _, detail) = loaded(
            true,
            Local::Folder(FolderSummary {
                uploading: 5,
                truncated: true,
                ..FolderSummary::default()
            }),
            Ok(drive_info()),
        )
        .status_line();
        assert_eq!(detail, "5 or more file(s)");
    }

    // ---- detail rows ----

    fn labels(rows: &[(&'static str, String); 4]) -> Vec<&'static str> {
        rows.iter().map(|(l, _)| *l).collect()
    }

    fn value<'a>(rows: &'a [(&'static str, String); 4], label: &str) -> &'a str {
        rows.iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn there_are_always_exactly_four_rows_with_stable_labels() {
        let file = ["Drive modified", "Cached locally", "Size", "Version"];
        let folder = ["Uploading", "Queued", "Downloading", "Cached"];
        assert_eq!(labels(&menu(false).detail_rows()), file);
        assert_eq!(labels(&menu(true).detail_rows()), folder);
        let done = loaded(false, file_local(false, None), Ok(drive_info()));
        assert_eq!(labels(&done.detail_rows()), file);
    }

    #[test]
    fn rows_show_placeholders_while_pending_and_dashes_on_failure() {
        let rows = menu(false).detail_rows();
        assert!(rows.iter().all(|(_, v)| v == PLACEHOLDER));

        let mut m = menu(false);
        m.local = Load::Done(Err("x".into()));
        m.drive = Load::Done(Err("x".into()));
        let rows = m.detail_rows();
        assert!(rows.iter().all(|(_, v)| v == NONE), "{rows:?}");
    }

    #[test]
    fn file_rows_combine_drive_and_cache() {
        let rows = loaded(
            false,
            file_local(false, Some(cached(10, 4, false))),
            Ok(drive_info()),
        )
        .detail_rows();
        assert_eq!(value(&rows, "Cached locally"), "Partly (4.0 B of 10.0 B)");
        assert_eq!(value(&rows, "Size"), "10.0 B");
        assert_eq!(value(&rows, "Version"), "7");
        assert!(value(&rows, "Drive modified").contains("2026"));

        let rows = loaded(
            false,
            file_local(false, Some(cached(10, 10, false))),
            Ok(drive_info()),
        )
        .detail_rows();
        assert_eq!(value(&rows, "Cached locally"), "Yes");
        let rows = loaded(false, file_local(false, None), Ok(drive_info())).detail_rows();
        assert_eq!(value(&rows, "Cached locally"), "No");
    }

    #[test]
    fn size_falls_back_to_the_cache_when_drive_has_none() {
        let rows = loaded(
            false,
            file_local(false, Some(cached(2048, 2048, false))),
            Err("x".into()),
        )
        .detail_rows();
        assert_eq!(value(&rows, "Size"), "2.0 KiB");
        assert_eq!(value(&rows, "Version"), NONE);
    }

    #[test]
    fn folder_rows_show_the_counts() {
        let rows = loaded(
            true,
            Local::Folder(FolderSummary {
                uploading: 1,
                downloading: 2,
                waiting_to_upload: 3,
                cached_files: 4,
                cached_bytes: 2048,
                truncated: true,
            }),
            Ok(drive_info()),
        )
        .detail_rows();
        assert_eq!(value(&rows, "Uploading"), "1");
        assert_eq!(value(&rows, "Downloading"), "2");
        assert_eq!(value(&rows, "Queued"), "3");
        assert_eq!(value(&rows, "Cached"), "4+ file(s), 2.0 KiB");
    }

    // ---- sharing ----

    #[test]
    fn sharing_summary_covers_every_state() {
        let mut m = menu(false);
        assert_eq!(m.sharing_summary(), PLACEHOLDER);
        m.drive = Load::Done(Err("x".into()));
        assert_eq!(m.sharing_summary(), "Unavailable");
        m.drive = Load::Done(Ok(DriveInfo {
            shared: false,
            ..DriveInfo::default()
        }));
        assert_eq!(m.sharing_summary(), "Not shared");
        m.drive = Load::Done(Ok(DriveInfo {
            shared: true,
            permissions: vec![
                perm("me", Role::Owner),
                perm("amy", Role::Editor),
                perm("bob", Role::Viewer),
            ],
            ..DriveInfo::default()
        }));
        // The owner is not "shared with".
        assert_eq!(m.sharing_summary(), "Shared with 2");
    }

    // ---- actions ----

    #[test]
    fn open_expands_and_collapses() {
        let mut m = menu(false);
        assert!(!m.open_expanded);
        let _ = m.update(Message::ToggleOpen);
        assert!(m.open_expanded);
        let _ = m.update(Message::ToggleOpen);
        assert!(!m.open_expanded);
    }

    #[test]
    fn every_state_builds_a_view_without_panicking() {
        let states = [
            menu(false),
            menu(true),
            loaded(
                false,
                file_local(false, Some(cached(10, 10, false))),
                Ok(drive_info()),
            ),
            loaded(
                true,
                Local::Folder(FolderSummary::default()),
                Ok(drive_info()),
            ),
            loaded(
                false,
                file_local(false, None),
                Ok(DriveInfo {
                    shared: true,
                    permissions: (0..60)
                        .map(|i| perm(&format!("user{i}"), Role::Viewer))
                        .collect(),
                    ..drive_info()
                }),
            ),
            loaded(false, file_local(false, None), Err("denied".into())),
        ];
        for mut m in states {
            let _ = m.view();
            m.open_expanded = true;
            let _ = m.view();
        }
    }
}
