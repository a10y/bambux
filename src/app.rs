use std::{cell::Cell, collections::HashMap, fs, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use directories::ProjectDirs;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FontWeight, Hsla, IntoElement, MouseButton,
    ObjectFit, PathBuilder, Pixels, Point, Render, RenderImage, SharedString, StyledImage,
    Subscription, Task, Window, div, img, point, prelude::*, px,
};
use gpui_base::input::{DiagnosticColors, Input, InputEditorStyle, InputEvent, InputState};

use crate::{
    model::{
        ConnectionState, HistoryEntry, PrinterConfig, PrinterStatus, ServiceHealth, ServiceState,
        SessionHistory, TelemetryPoint,
    },
    model_3mf::ModelMesh,
    network::{NetworkCommand, NetworkEvent, NetworkHandle},
    printer_view::{self, PrinterOrbit},
    prints::{PrintDetails, PrintRecord, format_duration},
    theme::{self, Theme},
    timelapse::TimelapsePlayer,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overview,
    History,
    Prints,
    Settings,
}

pub struct BambuApp {
    config: PrinterConfig,
    screen: Screen,
    connection: ConnectionState,
    ever_connected: bool,
    error: Option<String>,
    status: PrinterStatus,
    history: SessionHistory,
    services: Vec<ServiceHealth>,
    network: NetworkHandle,
    ftps: crate::ftps::FtpsHandle,
    orbit: PrinterOrbit,
    config_path: Option<PathBuf>,
    host_input: Entity<InputState>,
    code_input: Entity<InputState>,
    show_access_code: bool,
    titlebar_should_move: bool,
    prints: PrintHistory,
    /// Model for the job the printer reports, shown in the Overview twin.
    job_model: Option<Arc<ModelMesh>>,
    /// Job name the current `job_model` request was made for.
    job_model_name: String,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    _poll_ftps: Task<()>,
    _poll_video: Task<()>,
}

/// State behind the Print History tab.
#[derive(Default)]
struct PrintHistory {
    records: Vec<PrintRecord>,
    requested: bool,
    scanning: bool,
    error: Option<String>,
    details: HashMap<String, PrintDetails>,
    plate_images: HashMap<String, Arc<gpui::Image>>,
    timelapse_thumbs: HashMap<String, Arc<gpui::Image>>,
    selected: Option<String>,
    model: Option<Arc<ModelMesh>>,
    /// Shown in place of the model while it loads or when it can't.
    model_status: Option<String>,
    orbit: PrinterOrbit,
    player: Option<TimelapsePlayer>,
    player_status: Option<String>,
    player_generation: u64,
    /// The frame painted last; replaced frames are evicted from the atlas.
    shown_frame: Option<Arc<RenderImage>>,
    retired_frames: Vec<Arc<RenderImage>>,
    /// Scrub bar bounds from the last paint, for mapping clicks to time.
    scrub_bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl PrintHistory {
    fn selected_record(&self) -> Option<&PrintRecord> {
        let key = self.selected.as_deref()?;
        self.records.iter().find(|record| record.key == key)
    }

    fn stop_player(&mut self) {
        self.player = None;
        if let Some(frame) = self.shown_frame.take() {
            self.retired_frames.push(frame);
        }
    }
}

impl BambuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::dark();
        let config_path = config_path();
        let config: PrinterConfig = config_path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|contents| toml::from_str(&contents).ok())
            .unwrap_or_default();
        let has_config = config.is_complete();

        let host_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .placeholder("192.168.1.42")
                .default_value(config.host.clone());
            input.set_editor_style(input_style(theme));
            input
        });
        let code_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .placeholder("8-digit code")
                .default_value(config.access_code.clone())
                .masked(true);
            input.set_editor_style(input_style(theme));
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(&host_input, window, |_, _, _: &InputEvent, _, cx| {
                cx.notify();
            }),
            cx.subscribe_in(&code_input, window, |_, _, _: &InputEvent, _, cx| {
                cx.notify();
            }),
        ];

        let network = NetworkHandle::spawn();
        let ftps = crate::ftps::FtpsHandle::spawn(cache_dir());
        if has_config {
            network.send(NetworkCommand::Connect(config.clone()));
        }

        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this
                    .update(cx, |app, cx| {
                        if app.drain_network() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            config,
            screen: if has_config {
                Screen::Overview
            } else {
                Screen::Settings
            },
            connection: ConnectionState::Disconnected,
            ever_connected: false,
            error: None,
            status: PrinterStatus::default(),
            history: SessionHistory::default(),
            services: ServiceHealth::defaults(),
            network,
            ftps,
            orbit: PrinterOrbit::default(),
            config_path,
            host_input,
            code_input,
            show_access_code: false,
            titlebar_should_move: false,
            prints: PrintHistory::default(),
            job_model: None,
            job_model_name: String::new(),
            _subscriptions: subscriptions,
            _poll: poll,
            _poll_video: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(15))
                        .await;
                    if this
                        .update(cx, |app, cx| {
                            let prints = &mut app.prints;
                            if let Some(player) = &prints.player {
                                let generation = player.generation();
                                if generation != prints.player_generation {
                                    prints.player_generation = generation;
                                    cx.notify();
                                }
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
            _poll_ftps: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if this
                        .update(cx, |app, cx| {
                            if app.drain_ftps() {
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
        }
    }

    fn drain_network(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.network.events.try_recv() {
            changed = true;
            match event {
                NetworkEvent::Connecting => {
                    self.connection = ConnectionState::Connecting;
                    self.error = None;
                    for service in &mut self.services {
                        service.state = ServiceState::Checking;
                    }
                }
                NetworkEvent::Connected => {
                    self.connection = ConnectionState::Online;
                    self.ever_connected = true;
                    self.error = None;
                    self.history
                        .push_event(HistoryEntry::system("Secure MQTT session established"));
                }
                NetworkEvent::Disconnected => {
                    self.connection = ConnectionState::Disconnected;
                }
                NetworkEvent::Error(message) => {
                    self.connection = if self.ever_connected {
                        ConnectionState::Reconnecting
                    } else {
                        ConnectionState::Error
                    };
                    self.error = Some(message);
                }
                NetworkEvent::Report { topic, payload } => {
                    if let Some(entry) = self.status.apply_report(&topic, payload) {
                        self.history.push_event(entry);
                    }
                    self.history.sample(&self.status);
                    self.request_job_model();
                }
                NetworkEvent::ServiceProbe {
                    port,
                    available,
                    latency_ms,
                } => {
                    if let Some(service) = self.services.iter_mut().find(|item| item.port == port) {
                        service.state = if available {
                            ServiceState::Available
                        } else {
                            ServiceState::Unavailable
                        };
                        service.latency_ms = latency_ms;
                    }
                }
            }
        }
        changed
    }

    fn drain_ftps(&mut self) -> bool {
        use crate::ftps::FtpsEvent;
        let mut changed = false;
        while let Ok(event) = self.ftps.events.try_recv() {
            changed = true;
            let prints = &mut self.prints;
            let selected = prints.selected.clone();
            let is_selected = |key: &str| selected.as_deref() == Some(key);
            match event {
                FtpsEvent::PrintsScanned(records) => {
                    prints.records = records;
                    prints.scanning = false;
                    prints.error = None;
                    if prints.selected_record().is_none()
                        && let Some(first) = prints.records.first()
                    {
                        let key = first.key.clone();
                        prints.selected = None;
                        self.select_print(&key);
                    }
                }
                FtpsEvent::ScanFailed(error) => {
                    prints.scanning = false;
                    prints.error = Some(format!("Could not read the SD card: {error}"));
                }
                FtpsEvent::PrintDetails { key, details } => {
                    if let Some(png) = &details.plate_png {
                        prints.plate_images.insert(
                            key.clone(),
                            Arc::new(gpui::Image::from_bytes(
                                gpui::ImageFormat::Png,
                                png.as_ref().clone(),
                            )),
                        );
                    }
                    prints.details.insert(key, details);
                }
                FtpsEvent::TimelapseThumbnail { key, jpeg } => {
                    prints.timelapse_thumbs.insert(
                        key,
                        Arc::new(gpui::Image::from_bytes(
                            gpui::ImageFormat::Jpeg,
                            jpeg.as_ref().clone(),
                        )),
                    );
                }
                // Results for a print the user has since clicked away from are dropped.
                FtpsEvent::PrintModelLoaded { key, model } => {
                    if is_selected(&key) {
                        prints.model = Some(Arc::new(model));
                        prints.model_status = None;
                    }
                }
                FtpsEvent::PrintModelFailed { key, error } => {
                    if is_selected(&key) {
                        prints.model_status =
                            Some(format!("Couldn't render this project: {error}"));
                    }
                }
                FtpsEvent::TimelapseReady { key, path } => {
                    if is_selected(&key) {
                        match TimelapsePlayer::open(&path) {
                            Ok(player) => {
                                prints.player = Some(player);
                                prints.player_status = None;
                            }
                            Err(error) => prints.player_status = Some(format!("{error:#}")),
                        }
                    }
                }
                FtpsEvent::TimelapseFailed { key, error } => {
                    if is_selected(&key) {
                        prints.player_status =
                            Some(format!("Couldn't download the timelapse: {error}"));
                    }
                }
                FtpsEvent::JobModelLoaded { job_name, model } => {
                    if job_name == self.job_model_name {
                        self.job_model = Some(Arc::new(model));
                    }
                }
                FtpsEvent::JobModelUnavailable { job_name, error } => {
                    tracing::warn!("no model for job {job_name}: {error}");
                }
            }
        }
        changed
    }

    fn scan_prints(&mut self) {
        self.prints.requested = true;
        self.prints.scanning = true;
        self.prints.error = None;
        self.ftps.send(crate::ftps::FtpsCommand::ScanPrints {
            config: self.config.clone(),
        });
    }

    fn select_print(&mut self, key: &str) {
        let prints = &mut self.prints;
        if prints.selected.as_deref() == Some(key) {
            return;
        }
        prints.stop_player();
        prints.selected = Some(key.to_owned());
        prints.model = None;
        prints.player_status = None;
        let Some(record) = prints.selected_record().cloned() else {
            return;
        };
        prints.model_status = Some(if record.file.is_some() {
            "Loading model…".into()
        } else {
            "The project file for this print is no longer on the SD card.".into()
        });
        if record.timelapse.is_some() {
            prints.player_status = Some("Downloading timelapse…".into());
        }
        if let Some(file) = record.file {
            self.ftps.send(crate::ftps::FtpsCommand::LoadPrintModel {
                config: self.config.clone(),
                key: record.key.clone(),
                file,
            });
        }
        if let Some(timelapse) = record.timelapse {
            self.ftps.send(crate::ftps::FtpsCommand::FetchTimelapse {
                config: self.config.clone(),
                key: record.key,
                timelapse,
            });
        }
    }

    /// Fetch the model for the printer's current job when the job changes.
    fn request_job_model(&mut self) {
        let job = self.status.job_name.trim();
        if job.is_empty() || job == self.job_model_name || !self.config.is_complete() {
            return;
        }
        self.job_model_name = job.to_owned();
        self.job_model = None;
        self.ftps.send(crate::ftps::FtpsCommand::LoadJob {
            config: self.config.clone(),
            job_name: self.job_model_name.clone(),
        });
    }

    fn draft_config(&self, cx: &App) -> PrinterConfig {
        PrinterConfig {
            host: self.host_input.read(cx).value().to_string(),
            access_code: self.code_input.read(cx).value().to_string(),
        }
    }

    fn save_and_connect(&mut self, cx: &mut Context<Self>) {
        let mut config = self.draft_config(cx);
        config.host = config.normalized_host();
        self.config = config.clone();
        self.error = None;
        // New printer credentials: rescan print history on next view.
        self.prints.stop_player();
        let retired = std::mem::take(&mut self.prints.retired_frames);
        self.prints = PrintHistory {
            retired_frames: retired,
            ..PrintHistory::default()
        };
        self.job_model_name.clear();
        self.job_model = None;
        if let Some(path) = &self.config_path {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match toml::to_string_pretty(&config)
                .map_err(anyhow::Error::from)
                .and_then(|contents| fs::write(path, contents).map_err(anyhow::Error::from))
            {
                Ok(()) => set_private_permissions(path),
                Err(error) => self.error = Some(format!("Could not save settings: {error}")),
            }
        }
        for service in &mut self.services {
            service.state = ServiceState::Checking;
            service.latency_ms = None;
        }
        self.network.send(NetworkCommand::Connect(config));
        self.screen = Screen::Overview;
        cx.notify();
    }

    fn render_titlebar(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let status_color = connection_color(self.connection, theme);
        let serial: SharedString = self
            .status
            .serial
            .clone()
            .unwrap_or_else(|| "LOCAL PRINTER".into())
            .into();
        let updated = self.status.last_update.map(|update| {
            format!(
                "UPDATED {}",
                update.with_timezone(&chrono::Local).format("%H:%M:%S")
            )
        });

        div()
            .id("titlebar-drag")
            .window_control_area(gpui::WindowControlArea::Drag)
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex_none()
            .pl(px(if cfg!(target_os = "macos") {
                82.0
            } else {
                18.0
            }))
            .pr(px(18.0))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface.opacity(0.50))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|app, _, _, _| app.titlebar_should_move = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|app, _, _, _| app.titlebar_should_move = false),
            )
            .on_mouse_move(cx.listener(|app, event: &gpui::MouseMoveEvent, window, _| {
                if app.titlebar_should_move && event.pressed_button == Some(MouseButton::Left) {
                    app.titlebar_should_move = false;
                    window.start_window_move();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(12.5))
                    .child("BAMBU")
                    .child(div().text_color(theme.accent).child("X"))
                    .child(
                        div()
                            .ml(px(4.0))
                            .text_size(px(9.0))
                            .text_color(theme.text_faint)
                            .child("STUDIO"),
                    ),
            )
            .child(
                div()
                    .mx(px(14.0))
                    .h(px(13.0))
                    .border_l_1()
                    .border_color(theme.border),
            )
            .child(
                div()
                    .font_family("Geist Mono")
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .child(serial),
            )
            .child(div().flex_1())
            .when_some(updated, |row, updated| {
                row.child(
                    div()
                        .mr(px(16.0))
                        .font_family("Geist Mono")
                        .text_size(px(9.0))
                        .text_color(theme.text_faint)
                        .child(updated),
                )
            })
            .child(status_dot(status_color, 6.0))
            .child(
                div()
                    .ml(px(6.0))
                    .text_size(px(9.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(status_color)
                    .child(self.connection.label()),
            )
            .into_any_element()
    }

    fn render_sidebar(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let available = self
            .services
            .iter()
            .filter(|service| service.state == ServiceState::Available)
            .count();
        div()
            .w(px(176.0))
            .h_full()
            .flex_none()
            .p(px(12.0))
            .pt(px(18.0))
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.surface.opacity(0.42))
            .flex()
            .flex_col()
            .child(sidebar_label(theme, "WORKSPACE"))
            .child(self.nav_item(theme, Screen::Overview, "◈", "Overview", cx))
            .child(self.nav_item(theme, Screen::Prints, "▣", "Print History", cx))
            .child(self.nav_item(theme, Screen::History, "⌁", "Telemetry", cx))
            .child(sidebar_label(theme, "DEVICE").mt(px(20.0)))
            .child(self.nav_item(theme, Screen::Settings, "⚙", "Configuration", cx))
            .child(div().flex_1())
            .child(
                theme::inset(theme)
                    .p(px(11.0))
                    .child(theme::eyebrow(theme, "NETWORK"))
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format!("{available}/4 services")),
                    ),
            )
            .into_any_element()
    }

    fn nav_item(
        &self,
        theme: Theme,
        target: Screen,
        icon: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.screen == target;
        div()
            .id(match target {
                Screen::Overview => "nav-overview",
                Screen::History => "nav-history",
                Screen::Prints => "nav-prints",
                Screen::Settings => "nav-settings",
            })
            .h(px(36.0))
            .w_full()
            .px(px(10.0))
            .mt(px(3.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .text_size(px(12.0))
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .when(selected, |row| {
                row.bg(theme.active)
                    .border_1()
                    .border_color(theme.accent.opacity(0.24))
            })
            .when(!selected, |row| {
                row.hover(move |style| style.bg(theme.hover))
            })
            .cursor_pointer()
            .child(
                div()
                    .w(px(14.0))
                    .text_color(if selected {
                        theme.accent
                    } else {
                        theme.text_faint
                    })
                    .child(icon),
            )
            .child(label)
            .on_click(cx.listener(move |app, _, _, cx| {
                app.screen = target;
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_overview(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut page = page_shell(
            theme,
            "Printer overview",
            "Live telemetry and spatial print state",
        );
        if let Some(error) = self.error.clone() {
            let config = self.config.clone();
            page = page.child(
                error_strip(theme, error).child(
                    theme::action(theme, false)
                        .id("overview-retry")
                        .ml_auto()
                        .h(px(26.0))
                        .child("Retry")
                        .on_click(cx.listener(move |app, _, _, _| {
                            app.network.send(NetworkCommand::Connect(config.clone()));
                        })),
                ),
            );
        }

        page.child(
            div()
                .mt(px(20.0))
                .w_full()
                .flex()
                .items_start()
                .gap(px(14.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(self.digital_twin(theme, cx))
                        .child(self.metrics(theme)),
                )
                .child(self.timeline_card(theme, 300.0)),
        )
        .into_any_element()
    }

    fn digital_twin(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let state_color = print_state_color(&self.status.state, theme);
        let progress = self.status.progress;
        let job: SharedString = if self.status.job_name.is_empty() {
            "No active job".into()
        } else {
            self.status.job_name.clone().into()
        };
        let remaining: SharedString = self.status.remaining_label().into();
        let status = self.status.clone();
        let yaw = self.orbit.yaw;
        let pitch = self.orbit.pitch;

        theme::card(theme)
            .p(px(16.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(theme::eyebrow(theme, "DIGITAL TWIN"))
                            .child(
                                div()
                                    .text_size(px(21.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(state_color)
                                    .child(status.state_label().to_string()),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(21.0))
                            .child(format!("{progress:03.0}%")),
                    ),
            )
            .child(
                orbit_surface(
                    "printer-scene",
                    theme,
                    |app| &mut app.orbit,
                    (-0.18, 0.72),
                    cx,
                )
                .mt(px(12.0))
                .h(px(390.0))
                .child(printer_view::scene(
                    &status,
                    self.job_model.clone(),
                    yaw,
                    pitch,
                    theme,
                ))
                .child(
                    div()
                        .absolute()
                        .left(px(12.0))
                        .bottom(px(10.0))
                        .font_family("Geist Mono")
                        .text_size(px(8.5))
                        .text_color(theme.text_faint)
                        .child("DRAG TO ORBIT"),
                ),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .h(px(3.0))
                    .w_full()
                    .rounded_full()
                    .bg(theme.wash(0.06))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(progress / 100.0))
                            .rounded_full()
                            .bg(theme.accent),
                    ),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .items_center()
                    .child(div().font_weight(FontWeight::MEDIUM).child(job))
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(remaining),
                    ),
            )
            .into_any_element()
    }

    fn metrics(&self, theme: Theme) -> AnyElement {
        div()
            .w_full()
            .flex()
            .gap(px(10.0))
            .child(metric(
                theme,
                "NOZZLE",
                temp(self.status.nozzle_temp),
                target_temp(self.status.nozzle_target),
                theme.warning,
            ))
            .child(metric(
                theme,
                "BED",
                temp(self.status.bed_temp),
                target_temp(self.status.bed_target),
                theme.accent,
            ))
            .child(metric(
                theme,
                "LAYER",
                self.status.layer_label(),
                "CURRENT / TOTAL".into(),
                theme.cyan,
            ))
            .child(metric(
                theme,
                "TIME LEFT",
                self.status.remaining_label(),
                "ESTIMATED".into(),
                theme.success,
            ))
            .into_any_element()
    }

    fn timeline_card(&self, theme: Theme, width: f32) -> AnyElement {
        let mut list = div()
            .id("overview-history-scroll")
            .mt(px(12.0))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if self.history.events.is_empty() {
            list = list.child(empty_history(theme));
        } else {
            list = list.children(
                self.history
                    .events
                    .iter()
                    .take(40)
                    .cloned()
                    .map(|event| timeline_row(theme, event)),
            );
        }
        theme::card(theme)
            .w(px(width))
            .h(px(598.0))
            .flex_none()
            .p(px(16.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(theme::eyebrow(theme, "STATUS HISTORY"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(9.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.accent)
                            .child("MQTT"),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_history(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let points: Vec<_> = self.history.telemetry.iter().cloned().collect();
        let mut events = div().mt(px(10.0)).flex().flex_col();
        if self.history.events.is_empty() {
            events = events.child(empty_history(theme));
        } else {
            events = events.children(
                self.history
                    .events
                    .iter()
                    .cloned()
                    .map(|event| timeline_row(theme, event)),
            );
        }

        page_shell(
            theme,
            "Status history",
            "Telemetry retained for the current studio session",
        )
        .child(
            theme::card(theme)
                .mt(px(20.0))
                .p(px(16.0))
                .child(theme::eyebrow(theme, "THERMAL TELEMETRY"))
                .child(
                    div()
                        .id("telemetry-chart")
                        .mt(px(10.0))
                        .h(px(235.0))
                        .w_full()
                        // The chart paints its hover readout from the live mouse
                        // position, so repaint as the pointer moves or leaves.
                        .on_mouse_move(cx.listener(|_, _, _, cx| cx.notify()))
                        .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
                        .child(telemetry_chart(points, theme)),
                )
                .child(
                    div()
                        .mt(px(8.0))
                        .flex()
                        .gap(px(16.0))
                        .child(legend(theme.warning, "Nozzle °C"))
                        .child(legend(theme.accent, "Bed °C"))
                        .child(legend(theme.cyan, "Progress %")),
                ),
        )
        .child(
            theme::card(theme)
                .mt(px(12.0))
                .p(px(16.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(theme::eyebrow(theme, "EVENT STREAM"))
                        .child(div().flex_1())
                        .child(
                            div()
                                .font_family("Geist Mono")
                                .text_size(px(9.0))
                                .text_color(theme.text_faint)
                                .child(format!("{} EVENTS", self.history.events.len())),
                        ),
                )
                .child(events),
        )
        .into_any_element()
    }

    fn render_prints(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        if !self.prints.requested && self.config.is_complete() {
            self.scan_prints();
        }

        let status: Option<(Hsla, String)> = if let Some(error) = &self.prints.error {
            Some((theme.warning, error.clone()))
        } else if self.prints.scanning {
            Some((theme.text_muted, "Reading the SD card…".into()))
        } else if !self.config.is_complete() {
            Some((
                theme.text_muted,
                "Add your printer in Configuration to see its prints.".into(),
            ))
        } else if self.prints.records.is_empty() {
            Some((theme.text_muted, "No prints found on the SD card.".into()))
        } else {
            None
        };

        let header = div()
            .mt(px(8.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .font_family("Geist Mono")
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(format!(
                        "{} PRINTS · {} TIMELAPSES",
                        self.prints.records.len(),
                        self.prints
                            .records
                            .iter()
                            .filter(|record| record.timelapse.is_some())
                            .count()
                    )),
            )
            .child(div().flex_1())
            .child(
                theme::action(theme, false)
                    .id("prints-refresh")
                    .h(px(28.0))
                    .text_size(px(11.0))
                    .child(if self.prints.scanning {
                        "Refreshing…"
                    } else {
                        "Refresh"
                    })
                    .on_click(cx.listener(|app, _, _, cx| {
                        if !app.prints.scanning && app.config.is_complete() {
                            app.scan_prints();
                            cx.notify();
                        }
                    })),
            );

        let mut list = div().flex().flex_col().gap(px(2.0));
        for (index, record) in self.prints.records.iter().enumerate() {
            list = list.child(self.print_row(theme, index, record, cx));
        }

        page_shell(
            theme,
            "Print history",
            "Every print on the SD card, with its model and timelapse",
        )
        .child(header)
        .when_some(status, |page, (color, message)| {
            page.child(
                div()
                    .mt(px(8.0))
                    .text_size(px(11.5))
                    .text_color(color)
                    .child(message),
            )
        })
        .child(
            div()
                .mt(px(12.0))
                .flex()
                .items_start()
                .gap(px(12.0))
                .child(
                    theme::card(theme)
                        .w(gpui::relative(0.34))
                        .flex_none()
                        .p(px(6.0))
                        .child(list),
                )
                .child(self.render_print_detail(theme, cx)),
        )
        .into_any_element()
    }

    fn print_row(
        &self,
        theme: Theme,
        index: usize,
        record: &PrintRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.prints.selected.as_deref() == Some(record.key.as_str());
        let thumbnail = self
            .prints
            .plate_images
            .get(&record.key)
            .or_else(|| self.prints.timelapse_thumbs.get(&record.key))
            .cloned();
        let mut subline = record
            .started_at
            .with_timezone(&chrono::Local)
            .format("%b %-d · %-I:%M %p")
            .to_string();
        if let Some(secs) = record.duration_secs() {
            subline.push_str(&format!(" · {}", format_duration(secs)));
        }
        let key = record.key.clone();

        div()
            .id(("print-row", index))
            .h(px(60.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .when(selected, |row| {
                row.bg(theme.active)
                    .border_1()
                    .border_color(theme.accent.opacity(0.24))
            })
            .when(!selected, |row| {
                row.hover(move |style| style.bg(theme.hover))
            })
            .child(
                div()
                    .w(px(58.0))
                    .h(px(44.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .overflow_hidden()
                    .bg(theme::color(0x070708))
                    .border_1()
                    .border_color(theme.border)
                    .when_some(thumbnail, |frame, image| {
                        frame.child(img(image).size_full().object_fit(ObjectFit::Cover))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .truncate()
                            .child(record.name.clone()),
                    )
                    .child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .truncate()
                            .child(subline),
                    ),
            )
            .when(record.timelapse.is_some(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.accent)
                        .child("▶"),
                )
            })
            .on_click(cx.listener(move |app, _, _, cx| {
                app.select_print(&key);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_print_detail(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(record) = self.prints.selected_record().cloned() else {
            return theme::card(theme)
                .flex_1()
                .min_w_0()
                .p(px(16.0))
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child("Select a print to see its model and timelapse.")
                .into_any_element();
        };
        let details = self.prints.details.get(&record.key).cloned();

        let mut facts: Vec<(String, Option<Hsla>)> = vec![(
            record
                .started_at
                .with_timezone(&chrono::Local)
                .format("%a %b %-d %Y, %-I:%M %p")
                .to_string(),
            None,
        )];
        if let Some(secs) = record.duration_secs() {
            facts.push((format!("Printed in {}", format_duration(secs)), None));
        }
        if let Some(details) = &details {
            if let Some(secs) = details.predicted_secs {
                facts.push((
                    format!("Estimated {}", format_duration(i64::from(secs))),
                    None,
                ));
            }
            for filament in &details.filaments {
                let label = match filament.used_g {
                    Some(grams) => format!("{} · {grams:.1} g", filament.kind),
                    None => filament.kind.clone(),
                };
                facts.push((label, Some(hex_color(&filament.color))));
            }
            if details.filaments.is_empty()
                && let Some(weight) = details.weight_g
            {
                facts.push((format!("{weight:.1} g"), None));
            }
        }
        if let Some(file) = &record.file {
            facts.push((format_bytes(file.size), None));
        }

        let summary = theme::card(theme)
            .p(px(16.0))
            .child(theme::eyebrow(theme, "PRINT"))
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(px(17.0))
                    .font_weight(FontWeight::MEDIUM)
                    .truncate()
                    .child(record.name.clone()),
            )
            .child(
                div().mt(px(10.0)).flex().flex_wrap().gap(px(6.0)).children(
                    facts
                        .into_iter()
                        .map(|(label, swatch)| fact_chip(theme, label, swatch)),
                ),
            );

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(summary)
            .child(self.render_timelapse(theme, &record, cx))
            .child(self.render_print_model(theme, cx))
            .into_any_element()
    }

    fn render_timelapse(
        &mut self,
        theme: Theme,
        record: &PrintRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prints = &mut self.prints;
        let state = prints
            .player
            .as_ref()
            .map(|player| (player.state(), player.info));

        // Track the painted frame so the one it replaces can be freed.
        let frame = state.as_ref().and_then(|(state, _)| state.frame.clone());
        let changed = match (&frame, &prints.shown_frame) {
            (Some(new), Some(old)) => !Arc::ptr_eq(new, old),
            (None, None) => false,
            _ => true,
        };
        if changed && let Some(old) = std::mem::replace(&mut prints.shown_frame, frame.clone()) {
            prints.retired_frames.push(old);
        }

        let message = if record.timelapse.is_none() {
            Some("No timelapse was recorded for this print.".to_owned())
        } else if let Some(error) = state.as_ref().and_then(|(state, _)| state.error.clone()) {
            Some(error)
        } else {
            prints.player_status.clone()
        };
        let poster = prints.timelapse_thumbs.get(&record.key).cloned();

        let mut stage = div()
            .mt(px(10.0))
            .h(px(360.0))
            .w_full()
            .relative()
            .overflow_hidden()
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme::color(0x050505));
        stage = match (frame, poster) {
            (Some(frame), _) => stage.child(img(frame).size_full().object_fit(ObjectFit::Contain)),
            (None, Some(poster)) => stage.child(
                img(poster)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .opacity(0.35),
            ),
            (None, None) => stage,
        };
        if let Some(message) = message.clone() {
            stage = stage.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(24.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(message),
            );
        }

        let controls = state.map(|(state, info)| {
            let fraction = (state.position / info.duration).clamp(0.0, 1.0);
            let scrub_bounds = prints.scrub_bounds.clone();
            let seek = move |app: &mut BambuApp, position: Point<Pixels>| {
                let bounds = app.prints.scrub_bounds.get();
                if let Some(player) = &app.prints.player
                    && bounds.size.width > px(0.0)
                {
                    player.seek_fraction((position.x - bounds.origin.x) / bounds.size.width);
                }
            };
            div()
                .mt(px(10.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    theme::action(theme, true)
                        .id("timelapse-toggle")
                        .w(px(64.0))
                        .h(px(28.0))
                        .text_size(px(11.0))
                        .child(if state.playing { "Pause" } else { "Play" })
                        .on_click(cx.listener(|app, _, _, cx| {
                            if let Some(player) = &app.prints.player {
                                player.toggle();
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("timelapse-scrub")
                        .flex_1()
                        .h(px(20.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app, event: &gpui::MouseDownEvent, _, cx| {
                                seek(app, event.position);
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            move |app, event: &gpui::MouseMoveEvent, _, _| {
                                if event.pressed_button == Some(MouseButton::Left) {
                                    seek(app, event.position);
                                }
                            },
                        ))
                        .child(scrub_bar(fraction, scrub_bounds, theme)),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family("Geist Mono")
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child(format!(
                            "{} / {}",
                            clock(state.position),
                            clock(info.duration)
                        )),
                )
        });

        theme::card(theme)
            .p(px(16.0))
            .child(theme::eyebrow(theme, "TIMELAPSE"))
            .child(stage)
            .when_some(controls, |card, controls| card.child(controls))
            .into_any_element()
    }

    fn render_print_model(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let prints = &self.prints;
        let detail = prints.model.as_ref().map(|model| {
            let size = [0, 1, 2].map(|axis| model.max[axis] - model.min[axis]);
            let kind = if model.triangles.is_empty() {
                format!("{} toolpath segments", model.toolpaths.len())
            } else {
                format!("{} triangles", model.source_triangles)
            };
            format!(
                "{:.1} × {:.1} × {:.1} mm  ·  {kind}",
                size[0], size[1], size[2]
            )
        });

        let mut stage = orbit_surface(
            "print-model",
            theme,
            |app| &mut app.prints.orbit,
            (-1.2, 1.45),
            cx,
        )
        .mt(px(10.0))
        .h(px(380.0));
        stage = match &prints.model {
            Some(model) => stage
                .child(printer_view::model_view(
                    model.clone(),
                    prints.orbit.yaw,
                    prints.orbit.pitch,
                    theme,
                ))
                .child(
                    div()
                        .absolute()
                        .left(px(12.0))
                        .bottom(px(10.0))
                        .font_family("Geist Mono")
                        .text_size(px(8.5))
                        .text_color(theme.text_faint)
                        .child("DRAG TO ORBIT"),
                ),
            None => stage.bg(theme::color(0x070708)).child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(24.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_faint)
                    .child(prints.model_status.clone().unwrap_or_default()),
            ),
        };

        theme::card(theme)
            .p(px(16.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(theme::eyebrow(theme, "MODEL"))
                    .child(div().flex_1())
                    .when_some(detail, |row, detail| {
                        row.child(
                            div()
                                .font_family("Geist Mono")
                                .text_size(px(10.0))
                                .text_color(theme.text_muted)
                                .child(detail),
                        )
                    }),
            )
            .child(stage)
            .into_any_element()
    }

    fn render_settings(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let draft = self.draft_config(cx);
        let ready = draft.is_complete();
        let show = self.show_access_code;

        page_shell(
            theme,
            "Printer configuration",
            "Direct, local and encrypted device connection",
        )
        .max_w(px(780.0))
        .child(
            theme::card(theme)
                .mt(px(20.0))
                .p(px(18.0))
                .child(section_title(theme, "Connection", "Enter the printer's local address and LAN-only access code."))
                .child(form_label(theme, "PRINTER IP OR HOSTNAME").mt(px(20.0)))
                .child(input_shell(theme, &self.host_input))
                .child(form_label(theme, "LAN-ONLY ACCESS CODE").mt(px(14.0)))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(input_shell(theme, &self.code_input).flex_1())
                        .child(
                            theme::action(theme, false)
                                .id("toggle-access-code")
                                .child(if show { "Hide" } else { "Show" })
                                .on_click(cx.listener(|app, _, window, cx| {
                                    app.show_access_code = !app.show_access_code;
                                    let masked = !app.show_access_code;
                                    app.code_input.update(cx, |input, cx| {
                                        input.set_masked(masked, window, cx);
                                    });
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .mt(px(5.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child("Find this under Settings → Network → LAN Only Mode."),
                )
                .child(
                    div()
                        .mt(px(20.0))
                        .flex()
                        .gap(px(8.0))
                        .child(
                            theme::action(theme, true)
                                .id("save-connect")
                                .opacity(if ready { 1.0 } else { 0.42 })
                                .cursor(if ready { gpui::CursorStyle::PointingHand } else { gpui::CursorStyle::Arrow })
                                .child("Save & connect")
                                .on_click(cx.listener(|app, _, _, cx| {
                                    if app.draft_config(cx).is_complete() {
                                        app.save_and_connect(cx);
                                    }
                                })),
                        )
                        .when(self.connection != ConnectionState::Disconnected, |row| {
                            row.child(
                                theme::action(theme, false)
                                    .id("disconnect")
                                    .child("Disconnect")
                                    .on_click(cx.listener(|app, _, _, _| {
                                        app.network.send(NetworkCommand::Disconnect);
                                    })),
                            )
                        }),
                ),
        )
        .child(
            theme::card(theme)
                .mt(px(12.0))
                .p(px(18.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(section_title(theme, "Service diagnostics", "Required Bambu LAN endpoints"))
                        .child(div().flex_1())
                        .child(
                            theme::action(theme, false)
                                .id("rescan-services")
                                .opacity(if ready { 1.0 } else { 0.42 })
                                .child("Rescan")
                                .on_click(cx.listener(|app, _, _, cx| {
                                    let config = app.draft_config(cx);
                                    if config.is_complete() {
                                        for service in &mut app.services {
                                            service.state = ServiceState::Checking;
                                        }
                                        app.network.send(NetworkCommand::RefreshServices(config));
                                        cx.notify();
                                    }
                                })),
                        ),
                )
                .child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .flex_col()
                        .gap(px(7.0))
                        .children(self.services.iter().map(|service| service_row(theme, service))),
                ),
        )
        .child(
            theme::inset(theme)
                .mt(px(12.0))
                .p(px(13.0))
                .flex()
                .items_start()
                .gap(px(10.0))
                .child(
                    div()
                        .font_family("Geist Mono")
                        .text_size(px(9.0))
                        .text_color(theme.accent)
                        .child("TLS"),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(10.5))
                        .line_height(px(15.0))
                        .text_color(theme.text_muted)
                        .child("Bambu devices present a self-signed certificate. Traffic and credentials are encrypted, but device identity cannot be publicly verified. Use only on a trusted private network; never expose these ports to the internet."),
                ),
        )
        .into_any_element()
    }
}

impl Render for BambuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::dark();
        // Timelapse frames are one-off images; free the ones no longer shown.
        for frame in self.prints.retired_frames.drain(..) {
            let _ = window.drop_image(frame);
        }
        let content = match self.screen {
            Screen::Overview => self.render_overview(theme, cx),
            Screen::History => self.render_history(theme, cx),
            Screen::Prints => self.render_prints(theme, cx),
            Screen::Settings => self.render_settings(theme, cx),
        };

        theme::root(theme)
            .flex()
            .flex_col()
            .child(self.render_titlebar(theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(theme, cx))
                    .child(
                        div()
                            .id("main-scroll")
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .bg(theme.bg.opacity(0.94))
                            .overflow_y_scroll()
                            .child(content),
                    ),
            )
    }
}

fn page_shell(theme: Theme, title: &'static str, subtitle: &'static str) -> gpui::Div {
    div()
        .w_full()
        .max_w(px(1180.0))
        .mx_auto()
        .p(px(24.0))
        .pb(px(42.0))
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(px(22.0))
                .line_height(px(27.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title),
        )
        .child(
            div()
                .mt(px(3.0))
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(subtitle),
        )
}

fn sidebar_label(theme: Theme, label: &'static str) -> gpui::Div {
    theme::eyebrow(theme, label)
        .h(px(27.0))
        .px(px(8.0))
        .flex()
        .items_center()
}

fn metric(
    theme: Theme,
    label: &'static str,
    value: String,
    detail: String,
    color: Hsla,
) -> AnyElement {
    theme::card(theme)
        .flex_1()
        .min_w(px(110.0))
        .h(px(100.0))
        .p(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(status_dot(color, 5.0))
                .child(theme::eyebrow(theme, label)),
        )
        .child(
            div()
                .mt(px(10.0))
                .font_family("Geist Mono")
                .text_size(px(18.0))
                .font_weight(FontWeight::MEDIUM)
                .child(value),
        )
        .child(
            div()
                .mt(px(2.0))
                .text_size(px(8.5))
                .text_color(theme.text_faint)
                .child(detail),
        )
        .into_any_element()
}

fn timeline_row(theme: Theme, event: HistoryEntry) -> AnyElement {
    let color = if event.source == "MQTT" {
        theme.accent
    } else {
        theme.cyan
    };
    div()
        .py(px(10.0))
        .border_b_1()
        .border_color(theme.border.opacity(0.7))
        .flex()
        .items_start()
        .gap(px(9.0))
        .child(div().mt(px(5.0)).child(status_dot(color, 5.0)))
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(11.0)).child(event.title))
                .child(
                    div()
                        .mt(px(3.0))
                        .font_family("Geist Mono")
                        .text_size(px(8.5))
                        .text_color(theme.text_faint)
                        .child(format!(
                            "{}  ·  {}",
                            event.at.format("%H:%M:%S"),
                            event.source
                        )),
                ),
        )
        .into_any_element()
}

fn empty_history(theme: Theme) -> AnyElement {
    div()
        .h(px(190.0))
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .child(
            div()
                .text_size(px(23.0))
                .text_color(theme.text_faint)
                .child("⌁"),
        )
        .child(
            div()
                .mt(px(8.0))
                .text_size(px(11.5))
                .child("Waiting for MQTT telemetry"),
        )
        .child(
            div()
                .mt(px(3.0))
                .text_size(px(9.5))
                .text_color(theme.text_faint)
                .child("Events appear here as printer state changes"),
        )
        .into_any_element()
}

/// A bordered canvas host that orbits the camera in `select(app)` on drag.
fn orbit_surface(
    id: &'static str,
    theme: Theme,
    select: fn(&mut BambuApp) -> &mut PrinterOrbit,
    pitch_range: (f32, f32),
    cx: &mut Context<BambuApp>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w_full()
        .relative()
        .overflow_hidden()
        .rounded(px(9.0))
        .border_1()
        .border_color(theme.border)
        .cursor(gpui::CursorStyle::Crosshair)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, event: &gpui::MouseDownEvent, _, cx| {
                select(app).dragging = Some(event.position);
                cx.stop_propagation();
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |app, _, _, _| select(app).dragging = None),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |app, _, _, _| select(app).dragging = None),
        )
        .on_mouse_move(
            cx.listener(move |app, event: &gpui::MouseMoveEvent, _, cx| {
                let orbit = select(app);
                if event.pressed_button == Some(MouseButton::Left)
                    && let Some(last) = orbit.dragging.replace(event.position)
                {
                    orbit.yaw += f32::from(event.position.x - last.x) * 0.007;
                    orbit.pitch = (orbit.pitch - f32::from(event.position.y - last.y) * 0.005)
                        .clamp(pitch_range.0, pitch_range.1);
                    cx.notify();
                }
            }),
        )
}

fn telemetry_chart(points: Vec<TelemetryPoint>, theme: Theme) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            window.paint_quad(gpui::fill(bounds, theme::color(0x090909)));
            for i in 0..=4 {
                let y = bounds.origin.y + bounds.size.height * (i as f32 / 4.0);
                paint_line(
                    window,
                    point(bounds.origin.x, y),
                    point(bounds.origin.x + bounds.size.width, y),
                    0.7,
                    theme.border,
                );
            }
            if points.len() < 2 {
                return;
            }
            let first_at = points
                .first()
                .map(|point| point.at.timestamp_millis())
                .unwrap_or(0);
            let time_span = points
                .last()
                .map(|point| point.at.timestamp_millis() - first_at)
                .unwrap_or(1)
                .max(1) as f32;
            let x_of = |sample: &TelemetryPoint| {
                let elapsed = (sample.at.timestamp_millis() - first_at) as f32;
                bounds.origin.x + bounds.size.width * (elapsed / time_span)
            };
            let y_of = |value: f32| {
                bounds.origin.y + bounds.size.height * (1.0 - (value / 280.0).clamp(0.0, 1.0))
            };
            type Series = (Hsla, fn(&TelemetryPoint) -> Option<f32>);
            let series: [Series; 3] = [
                (theme.warning, |p| p.nozzle),
                (theme.accent, |p| p.bed),
                (theme.cyan, |p| Some(p.progress)),
            ];
            for (color, value) in series {
                let mut path = PathBuilder::stroke(px(1.5));
                let mut started = false;
                for sample in &points {
                    let Some(value) = value(sample) else { continue };
                    let at = point(x_of(sample), y_of(value));
                    if started {
                        path.line_to(at);
                    } else {
                        path.move_to(at);
                        started = true;
                    }
                }
                if started && let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }

            // Hover readout: snap to the sample nearest the pointer.
            let mouse = window.mouse_position();
            if !bounds.contains(&mouse) {
                return;
            }
            let hovered_at =
                first_at + ((mouse.x - bounds.origin.x) / bounds.size.width * time_span) as i64;
            let after = points.partition_point(|p| p.at.timestamp_millis() < hovered_at);
            let sample = match (after.checked_sub(1), points.get(after)) {
                (Some(before), Some(next))
                    if hovered_at - points[before].at.timestamp_millis()
                        < next.at.timestamp_millis() - hovered_at =>
                {
                    &points[before]
                }
                (_, Some(next)) => next,
                (Some(before), None) => &points[before],
                (None, None) => return,
            };
            let x = x_of(sample);
            paint_line(
                window,
                point(x, bounds.origin.y),
                point(x, bounds.origin.y + bounds.size.height),
                1.0,
                theme.text_muted.opacity(0.5),
            );
            for (color, value) in series {
                if let Some(value) = value(sample) {
                    let center = point(x, y_of(value));
                    window.paint_quad(
                        gpui::fill(
                            gpui::Bounds::centered_at(center, gpui::size(px(7.0), px(7.0))),
                            color,
                        )
                        .corner_radii(px(3.5)),
                    );
                }
            }

            let reading = |value: Option<f32>, unit: &str| {
                value.map_or("—".to_owned(), |value| format!("{value:.1}{unit}"))
            };
            let lines = [
                (
                    theme.text_muted,
                    sample
                        .at
                        .with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string(),
                ),
                (theme.cyan, format!("Progress  {:.0}%", sample.progress)),
                (
                    theme.warning,
                    format!("Nozzle  {}", reading(sample.nozzle, " °C")),
                ),
                (theme.accent, format!("Bed  {}", reading(sample.bed, " °C"))),
            ];
            let font_size = px(10.5);
            let line_height = px(15.0);
            let padding = px(8.0);
            let base_style = window.text_style();
            let shaped: Vec<_> = lines
                .into_iter()
                .map(|(color, text)| {
                    let run = gpui::TextRun {
                        color,
                        ..base_style.to_run(text.len())
                    };
                    window
                        .text_system()
                        .shape_line(text.into(), font_size, &[run], None)
                })
                .collect();
            let width = shaped
                .iter()
                .map(|line| line.width)
                .fold(px(0.0), |a, b| a.max(b))
                + padding * 2.0;
            let height = line_height * shaped.len() as f32 + padding * 2.0;
            // Sit beside the pointer, flipping inward near the chart's edges.
            let right = bounds.origin.x + bounds.size.width;
            let bottom = bounds.origin.y + bounds.size.height;
            let left = if mouse.x + px(12.0) + width > right {
                mouse.x - px(12.0) - width
            } else {
                mouse.x + px(12.0)
            };
            let top = (mouse.y - height / 2.0)
                .max(bounds.origin.y + px(4.0))
                .min(bottom - height - px(4.0));
            let panel = gpui::Bounds::new(point(left, top), gpui::size(width, height));
            window.paint_quad(
                gpui::fill(panel, theme.surface)
                    .corner_radii(px(6.0))
                    .border_widths(px(1.0))
                    .border_color(theme.border_strong),
            );
            for (index, line) in shaped.iter().enumerate() {
                let origin = point(left + padding, top + padding + line_height * index as f32);
                let _ = line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx);
            }
        },
    )
    .size_full()
}

fn paint_line(
    window: &mut Window,
    a: gpui::Point<gpui::Pixels>,
    b: gpui::Point<gpui::Pixels>,
    width: f32,
    color: Hsla,
) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(a);
    path.line_to(b);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn legend(color: Hsla, label: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(9.0))
        .text_color(color)
        .child(status_dot(color, 5.0))
        .child(label)
        .into_any_element()
}

fn section_title(theme: Theme, title: &'static str, description: &'static str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .child(
            div()
                .mt(px(2.0))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(description),
        )
        .into_any_element()
}

fn form_label(theme: Theme, label: &'static str) -> gpui::Div {
    theme::eyebrow(theme, label).mb(px(5.0))
}

fn input_shell(theme: Theme, input: &Entity<InputState>) -> gpui::Div {
    theme::glass_plate(theme)
        .h(px(38.0))
        .w_full()
        .px(px(10.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .child(Input::new(input))
}

fn input_style(theme: Theme) -> InputEditorStyle {
    InputEditorStyle {
        foreground: theme.text,
        muted_foreground: theme.text_faint,
        background: gpui::transparent_black(),
        border: gpui::transparent_black(),
        selection: theme.accent.opacity(0.35),
        caret: theme.accent,
        diagnostics: DiagnosticColors {
            error: theme.danger,
            warning: theme.warning,
            info: theme.cyan,
            hint: theme.text_faint,
        },
        ..Default::default()
    }
}

fn service_row(theme: Theme, service: &ServiceHealth) -> AnyElement {
    let (color, state) = match service.state {
        ServiceState::Unknown => (theme.text_faint, "NOT CHECKED"),
        ServiceState::Checking => (theme.warning, "CHECKING"),
        ServiceState::Available => (theme.success, "AVAILABLE"),
        ServiceState::Unavailable => (theme.danger, "UNREACHABLE"),
    };
    theme::inset(theme)
        .h(px(48.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .child(status_dot(color, 5.0))
        .child(
            div()
                .ml(px(9.0))
                .w(px(42.0))
                .font_family("Geist Mono")
                .font_weight(FontWeight::MEDIUM)
                .child(service.port.to_string()),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_muted)
                .child(service.name),
        )
        .child(
            div()
                .mx(px(8.0))
                .text_color(theme.text_faint.opacity(0.4))
                .child("·"),
        )
        .child(
            div()
                .text_size(px(9.5))
                .text_color(theme.text_faint)
                .child(service.detail),
        )
        .child(div().flex_1())
        .when_some(service.latency_ms, |row, latency| {
            row.child(
                div()
                    .mr(px(10.0))
                    .font_family("Geist Mono")
                    .text_size(px(9.0))
                    .text_color(theme.text_faint)
                    .child(format!("{latency}ms")),
            )
        })
        .child(
            div()
                .text_size(px(9.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(state),
        )
        .into_any_element()
}

fn error_strip(theme: Theme, message: String) -> gpui::Div {
    div()
        .mt(px(16.0))
        .min_h(px(46.0))
        .px(px(14.0))
        .py(px(8.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.danger.opacity(0.22))
        .bg(theme.danger.opacity(0.065))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .text_color(theme.danger)
        .child(message)
}

fn status_dot(color: Hsla, size: f32) -> gpui::Div {
    div().size(px(size)).rounded_full().bg(color).flex_none()
}

fn connection_color(state: ConnectionState, theme: Theme) -> Hsla {
    match state {
        ConnectionState::Online => theme.success,
        ConnectionState::Connecting | ConnectionState::Reconnecting => theme.warning,
        ConnectionState::Error => theme.danger,
        ConnectionState::Disconnected => theme.text_faint,
    }
}

fn print_state_color(state: &str, theme: Theme) -> Hsla {
    match state {
        "RUNNING" => theme.success,
        "PAUSE" | "PREPARE" | "SLICING" => theme.warning,
        "FAILED" => theme.danger,
        _ => theme.text,
    }
}

fn temp(value: Option<f32>) -> String {
    value
        .map(|value| format!("{value:.0}°C"))
        .unwrap_or_else(|| "—".into())
}

fn target_temp(value: Option<f32>) -> String {
    value
        .map(|value| format!("TARGET {value:.0}°"))
        .unwrap_or_else(|| "NO TARGET".into())
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn fact_chip(theme: Theme, label: String, swatch: Option<Hsla>) -> AnyElement {
    div()
        .h(px(24.0))
        .px(px(9.0))
        .rounded(px(12.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .bg(theme.hover)
        .border_1()
        .border_color(theme.border)
        .text_size(px(10.5))
        .text_color(theme.text_muted)
        .when_some(swatch, |chip, color| {
            chip.child(
                status_dot(color, 8.0)
                    .border_1()
                    .border_color(theme.border_strong),
            )
        })
        .child(label)
        .into_any_element()
}

/// Parse a slicer `#RRGGBB` (alpha suffix ignored).
fn hex_color(hex: &str) -> Hsla {
    let digits = hex.trim_start_matches('#');
    u32::from_str_radix(digits.get(..6).unwrap_or(""), 16)
        .map(theme::color)
        .unwrap_or_else(|_| theme::color(0x888888))
}

/// `m:ss` for timelapse positions.
fn clock(secs: f32) -> String {
    let secs = secs.max(0.0) as u32;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn scrub_bar(
    fraction: f32,
    bounds_out: Rc<Cell<Bounds<Pixels>>>,
    theme: Theme,
) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            bounds_out.set(bounds);
            let track_height = px(4.0);
            let track = Bounds::new(
                point(bounds.origin.x, bounds.center().y - track_height / 2.0),
                gpui::size(bounds.size.width, track_height),
            );
            window.paint_quad(gpui::fill(track, theme.border_strong).corner_radii(px(2.0)));
            let filled = Bounds::new(
                track.origin,
                gpui::size(track.size.width * fraction, track_height),
            );
            window.paint_quad(gpui::fill(filled, theme.accent).corner_radii(px(2.0)));
            let knob = Bounds::centered_at(
                point(
                    bounds.origin.x + bounds.size.width * fraction,
                    bounds.center().y,
                ),
                gpui::size(px(12.0), px(12.0)),
            );
            window.paint_quad(gpui::fill(knob, theme.text).corner_radii(px(6.0)));
        },
    )
    .size_full()
}

fn cache_dir() -> PathBuf {
    ProjectDirs::from("dev", "bambux", "Bambu X")
        .map(|dirs| dirs.cache_dir().to_owned())
        .unwrap_or_else(|| std::env::temp_dir().join("bambux"))
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "bambux", "Bambu X").map(|dirs| dirs.config_dir().join("config.toml"))
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) {}
