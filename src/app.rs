use std::{fs, path::PathBuf, time::Duration};

use directories::ProjectDirs;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, MouseButton, PathBuilder,
    Render, SharedString, Subscription, Task, Window, div, point, prelude::*, px,
};
use gpui_base::input::{DiagnosticColors, Input, InputEditorStyle, InputEvent, InputState};

use crate::{
    model::{
        ConnectionState, HistoryEntry, PrinterConfig, PrinterStatus, ServiceHealth, ServiceState,
        SessionHistory, TelemetryPoint,
    },
    network::{NetworkCommand, NetworkEvent, NetworkHandle},
    printer_view::{self, PrinterOrbit},
    theme::{self, Theme},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overview,
    History,
    Files,
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
    ftps_entries: Vec<crate::ftps::FtpsEntry>,
    ftps_path: String,
    ftps_message: Option<String>,
    /// Set once the Files page has issued its initial listing, so renders don't re-request it.
    ftps_requested: bool,
    model_mesh: Option<std::sync::Arc<crate::model_3mf::ModelMesh>>,
    model_path: String,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    _poll_ftps: Task<()>,
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
        let ftps = crate::ftps::FtpsHandle::spawn();
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
            ftps_entries: Vec::new(),
            ftps_path: "/".into(),
            ftps_message: None,
            ftps_requested: false,
            model_mesh: None,
            model_path: String::new(),
            _subscriptions: subscriptions,
            _poll: poll,
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
        let mut changed = false;
        while let Ok(event) = self.ftps.events.try_recv() {
            changed = true;
            match event {
                crate::ftps::FtpsEvent::LoadingDirectory(path) => {
                    self.ftps_message = Some(format!("Listing {path}…"));
                }
                crate::ftps::FtpsEvent::Listing { path, entries } => {
                    self.ftps_path = path;
                    self.ftps_entries = entries;
                    self.ftps_message = None;
                }
                crate::ftps::FtpsEvent::LoadingModel(path) => {
                    self.ftps_message = Some(format!("Parsing {path}…"));
                }
                crate::ftps::FtpsEvent::ModelLoaded { remote_path, model } => {
                    self.model_path = remote_path;
                    self.model_mesh = Some(std::sync::Arc::new(model));
                    self.ftps_message = None;
                    self.screen = Screen::Overview;
                }
                crate::ftps::FtpsEvent::Error(message) => {
                    self.ftps_message = Some(message);
                }
            }
        }
        changed
    }

    fn ftps_list(&mut self, path: &str) {
        self.ftps_requested = true;
        self.ftps_message = Some(format!("Listing {path}…"));
        self.ftps.send(crate::ftps::FtpsCommand::List {
            config: self.config.clone(),
            path: path.to_owned(),
        });
    }

    fn ftps_load_model(&self, path: &str) {
        self.ftps.send(crate::ftps::FtpsCommand::LoadModel {
            config: self.config.clone(),
            remote_path: path.to_owned(),
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
        // New printer credentials: refresh the Files page from the root on next view.
        self.ftps_requested = false;
        self.ftps_entries.clear();
        self.ftps_path = "/".into();
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
            .child(self.nav_item(theme, Screen::History, "⌁", "History", cx))
            .child(self.nav_item(theme, Screen::Files, "◉", "Files", cx))
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
                Screen::Files => "nav-files",
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
                div()
                    .id("printer-scene")
                    .mt(px(12.0))
                    .h(px(390.0))
                    .w_full()
                    .relative()
                    .overflow_hidden()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(theme.border)
                    .cursor(gpui::CursorStyle::Crosshair)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|app, event: &gpui::MouseDownEvent, _, cx| {
                            app.orbit.dragging = Some(event.position);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|app, _, _, _| app.orbit.dragging = None),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|app, _, _, _| app.orbit.dragging = None),
                    )
                    .on_mouse_move(cx.listener(|app, event: &gpui::MouseMoveEvent, _, cx| {
                        if event.pressed_button == Some(MouseButton::Left)
                            && let Some(last) = app.orbit.dragging.replace(event.position)
                        {
                            app.orbit.yaw += f32::from(event.position.x - last.x) * 0.007;
                            app.orbit.pitch = (app.orbit.pitch
                                - f32::from(event.position.y - last.y) * 0.005)
                                .clamp(-0.18, 0.72);
                            cx.notify();
                        }
                    }))
                    .child(printer_view::scene(
                        &status,
                        self.model_mesh.clone(),
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

    fn render_history(&self, theme: Theme) -> AnyElement {
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
                        .mt(px(10.0))
                        .h(px(235.0))
                        .w_full()
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

    fn render_files(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        if !self.ftps_requested && self.config.is_complete() {
            let path = self.ftps_path.clone();
            self.ftps_list(&path);
        }
        let mut rows = div().mt(px(14.0)).flex().flex_col().gap(px(2.0));
        rows = rows.child(
            theme::action(theme, false)
                .id("ftps-up")
                .h(px(28.0))
                .child(format!(
                    "..  ·  {}",
                    crate::ftps::parent_path(&self.ftps_path)
                ))
                .on_click(cx.listener(|app, _, _, _| {
                    let parent = crate::ftps::parent_path(&app.ftps_path);
                    app.ftps_list(&parent);
                })),
        );
        for (index, entry) in self.ftps_entries.clone().into_iter().enumerate() {
            let icon = if entry.is_directory { "▸" } else { "·" };
            let size = if entry.is_directory {
                "".into()
            } else {
                format_bytes(entry.size)
            };
            let name = entry.name.clone();
            let is_model = entry.name.to_ascii_lowercase().ends_with(".3mf");
            let path = crate::ftps::join_path(&self.ftps_path, &entry.name);
            let is_dir = entry.is_directory;
            rows = rows.child(
                div()
                    .id(format!("ftps-row-{index}"))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(if is_dir { theme.text } else { theme.text_muted })
                    .hover(move |style| style.bg(theme.hover))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(16.0))
                            .text_color(if is_dir {
                                theme.accent
                            } else {
                                theme.text_faint
                            })
                            .child(icon),
                    )
                    .child(div().flex_1().min_w_0().child(name.clone()))
                    .child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(size),
                    )
                    .on_click(cx.listener(move |app, _, _, _| {
                        if is_dir {
                            app.ftps_list(&path);
                        } else if is_model {
                            app.ftps_load_model(&path);
                        }
                    })),
            );
        }
        page_shell(theme, "Files", "Browse printer storage over FTPS")
            .child(
                div()
                    .mt(px(6.0))
                    .font_family("Geist Mono")
                    .text_size(px(11.0))
                    .text_color(theme.accent)
                    .child(self.ftps_path.clone()),
            )
            .when_some(self.ftps_message.clone(), |page, message| {
                page.child(
                    div()
                        .mt(px(8.0))
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(theme::card(theme).mt(px(14.0)).p(px(8.0)).child(rows))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::dark();
        let content = match self.screen {
            Screen::Overview => self.render_overview(theme, cx),
            Screen::History => self.render_history(theme),
            Screen::Files => self.render_files(theme, cx),
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

fn telemetry_chart(points: Vec<TelemetryPoint>, theme: Theme) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
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
            let draw = |window: &mut Window,
                        color: Hsla,
                        value: &dyn Fn(&TelemetryPoint) -> Option<f32>| {
                let mut path = PathBuilder::stroke(px(1.5));
                let mut started = false;
                for sample in &points {
                    let Some(value) = value(sample) else { continue };
                    let elapsed = (sample.at.timestamp_millis() - first_at) as f32;
                    let x = bounds.origin.x + bounds.size.width * (elapsed / time_span);
                    let y = bounds.origin.y
                        + bounds.size.height * (1.0 - (value / 280.0).clamp(0.0, 1.0));
                    if started {
                        path.line_to(point(x, y));
                    } else {
                        path.move_to(point(x, y));
                        started = true;
                    }
                }
                if started && let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            draw(window, theme.warning, &|p| p.nozzle);
            draw(window, theme.accent, &|p| p.bed);
            draw(window, theme.cyan, &|p| Some(p.progress));
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
