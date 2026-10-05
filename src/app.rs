use std::{cell::Cell, collections::HashMap, fs, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use directories::ProjectDirs;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FontWeight, Hsla, IntoElement, MouseButton,
    ObjectFit, PathBuilder, Pixels, Point, Render, RenderImage, SharedString, StyledImage,
    Subscription, Task, Window, div, img, point, prelude::*, px,
};
use gpui_base::input::{DiagnosticColors, Input, InputEditorStyle, InputEvent, InputState};

use crate::{
    ams::{ActiveSpool, AmsState, Spool},
    discovery::DiscoveredPrinter,
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
    Printers,
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
    discovery: DiscoveryState,
    discovery_code_input: Entity<InputState>,
    /// Model for the job the printer reports, shown in the Overview twin.
    job_model: Option<Arc<ModelMesh>>,
    /// Job name the current `job_model` request was made for.
    job_model_name: String,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    _poll_ftps: Task<()>,
    _poll_video: Task<()>,
}

/// State behind the Printers (discovery) screen.
#[derive(Default)]
struct DiscoveryState {
    printers: Vec<DiscoveredPrinter>,
    scanning: bool,
    /// Whether any search has finished, so the screen searches once on open.
    scanned: bool,
    error: Option<String>,
    /// Serial of the printer whose access-code prompt is open.
    selected: Option<String>,
    results: Option<std::sync::mpsc::Receiver<anyhow::Result<Vec<DiscoveredPrinter>>>>,
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
        let discovery_code_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("8-digit code");
            input.set_editor_style(input_style(theme));
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &discovery_code_input,
                window,
                |app, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        app.connect_discovered(window, cx);
                    }
                    cx.notify();
                },
            ),
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
                        if app.drain_network() | app.drain_discovery() {
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
                Screen::Printers
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
            discovery: DiscoveryState::default(),
            discovery_code_input,
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
        let host = self.host_input.read(cx).value().to_string();
        // Keep the discovered identity while the address is unchanged.
        let identity = if host.trim() == self.config.host {
            self.config.clone()
        } else {
            PrinterConfig::default()
        };
        PrinterConfig {
            host,
            access_code: self.code_input.read(cx).value().to_string(),
            ..identity
        }
    }

    fn start_discovery(&mut self) {
        if self.discovery.scanning {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("bambux-discovery".into())
            .spawn(move || {
                let _ = sender.send(crate::discovery::discover(Duration::from_secs(3)));
            })
            .expect("spawn discovery thread");
        self.discovery.scanning = true;
        self.discovery.error = None;
        self.discovery.results = Some(receiver);
    }

    fn drain_discovery(&mut self) -> bool {
        let Some(receiver) = &self.discovery.results else {
            return false;
        };
        let Ok(result) = receiver.try_recv() else {
            return false;
        };
        let discovery = &mut self.discovery;
        discovery.results = None;
        discovery.scanning = false;
        discovery.scanned = true;
        match result {
            Ok(printers) => discovery.printers = printers,
            Err(error) => discovery.error = Some(format!("Discovery failed: {error:#}")),
        }
        if let Some(selected) = &discovery.selected
            && !discovery.printers.iter().any(|p| &p.serial == selected)
        {
            discovery.selected = None;
        }
        true
    }

    fn select_discovered(&mut self, serial: String, window: &mut Window, cx: &mut Context<Self>) {
        self.discovery.selected = Some(serial);
        self.discovery_code_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn connect_discovered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(printer) = self
            .discovery
            .selected
            .as_ref()
            .and_then(|serial| self.discovery.printers.iter().find(|p| &p.serial == serial))
            .cloned()
        else {
            return;
        };
        let code = self.discovery_code_input.read(cx).value().trim().to_owned();
        if code.is_empty() {
            return;
        }
        let config = PrinterConfig {
            host: printer.host,
            access_code: code,
            name: printer.name,
            model: printer.model_code,
            serial: printer.serial,
        };
        // Keep the manual Configuration form in step with what was saved.
        let (host, code) = (config.host.clone(), config.access_code.clone());
        self.host_input
            .update(cx, |input, cx| input.set_value(host, window, cx));
        self.code_input
            .update(cx, |input, cx| input.set_value(code, window, cx));
        self.discovery.selected = None;
        self.discovery_code_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.apply_config(config, cx);
    }

    fn render_printers(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        if !self.discovery.scanned && !self.discovery.scanning {
            self.start_discovery();
        }
        let discovery = &self.discovery;
        let status: Option<(Hsla, String)> = if let Some(error) = &discovery.error {
            Some((theme.warning, error.clone()))
        } else if discovery.scanning {
            Some((theme.text_muted, "Searching your network…".into()))
        } else if discovery.printers.is_empty() {
            Some((
                theme.text_muted,
                "No printers answered. Check that the printer is on and on the same network as this computer."
                    .into(),
            ))
        } else {
            None
        };

        let mut list = div().flex().flex_col().gap(px(6.0));
        for printer in &discovery.printers {
            let selected = discovery.selected.as_deref() == Some(printer.serial.as_str());
            let current = self.config.host == printer.host
                || (!self.config.serial.is_empty() && self.config.serial == printer.serial);
            let serial = printer.serial.clone();
            let row = div()
                .id(SharedString::from(format!("discovered-{}", printer.serial)))
                .p(px(12.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if selected { theme.accent } else { theme.border })
                .when(selected, |row| row.bg(theme.active))
                .when(!selected, |row| {
                    row.hover(move |style| style.bg(theme.hover))
                })
                .cursor_pointer()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(printer.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(printer.model_name().to_owned()),
                                ),
                        )
                        .child(
                            div()
                                .font_family("Geist Mono")
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(
                                    [
                                        Some(printer.host.clone()),
                                        printer.firmware.as_ref().map(|v| format!("FW {v}")),
                                        printer.connection.as_ref().map(|mode| {
                                            if mode == "lan" {
                                                "LAN-ONLY".to_owned()
                                            } else {
                                                mode.to_ascii_uppercase()
                                            }
                                        }),
                                    ]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join("  ·  "),
                                ),
                        ),
                )
                .when(current, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .font_family("Geist Mono")
                            .text_size(px(9.0))
                            .text_color(connection_color(self.connection, theme))
                            .child(if self.connection == ConnectionState::Online {
                                "CONNECTED"
                            } else {
                                "CONFIGURED"
                            }),
                    )
                })
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.select_discovered(serial.clone(), window, cx);
                }));
            list = list.child(row);
            if selected {
                list = list.child(self.access_code_prompt(theme, printer, cx));
            }
        }

        page_shell(
            theme,
            "Printers",
            "Find Bambu printers on your network and connect",
        )
        .max_w(px(780.0))
        .child(
            div()
                .mt(px(16.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .font_family("Geist Mono")
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(format!("{} FOUND", discovery.printers.len())),
                )
                .child(div().flex_1())
                .child(
                    theme::action(theme, false)
                        .id("discovery-manual")
                        .h(px(28.0))
                        .text_size(px(11.0))
                        .child("Enter manually")
                        .on_click(cx.listener(|app, _, _, cx| {
                            app.screen = Screen::Settings;
                            cx.notify();
                        })),
                )
                .child(
                    theme::action(theme, true)
                        .id("discovery-scan")
                        .h(px(28.0))
                        .text_size(px(11.0))
                        .opacity(if discovery.scanning { 0.5 } else { 1.0 })
                        .child(if discovery.scanning {
                            "Searching…"
                        } else {
                            "Search again"
                        })
                        .on_click(cx.listener(|app, _, _, cx| {
                            app.start_discovery();
                            cx.notify();
                        })),
                ),
        )
        .when_some(status, |page, (color, message)| {
            page.child(
                div()
                    .mt(px(10.0))
                    .text_size(px(11.5))
                    .text_color(color)
                    .child(message),
            )
        })
        .child(theme::card(theme).mt(px(12.0)).p(px(8.0)).child(list))
        .into_any_element()
    }

    fn access_code_prompt(
        &self,
        theme: Theme,
        printer: &crate::discovery::DiscoveredPrinter,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ready = !self.discovery_code_input.read(cx).value().trim().is_empty();
        div()
            .mb(px(6.0))
            .p(px(14.0))
            .rounded(px(8.0))
            .bg(theme.hover)
            .border_1()
            .border_color(theme.border)
            .child(form_label(theme, "LAN ACCESS CODE"))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(input_shell(theme, &self.discovery_code_input).flex_1())
                    .child(
                        theme::action(theme, true)
                            .id("discovery-connect")
                            .opacity(if ready { 1.0 } else { 0.42 })
                            .child("Connect")
                            .on_click(cx.listener(|app, _, window, cx| {
                                app.connect_discovered(window, cx);
                            })),
                    )
                    .child(
                        theme::action(theme, false)
                            .id("discovery-cancel")
                            .child("Cancel")
                            .on_click(cx.listener(|app, _, _, cx| {
                                app.discovery.selected = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(format!(
                        "Shown on {} under Settings → Network → LAN Only Mode.",
                        printer.name
                    )),
            )
            .into_any_element()
    }

    fn save_and_connect(&mut self, cx: &mut Context<Self>) {
        let config = self.draft_config(cx);
        self.apply_config(config, cx);
    }

    /// Save `config` as the configured printer and connect to it.
    fn apply_config(&mut self, mut config: PrinterConfig, cx: &mut Context<Self>) {
        config.host = config.normalized_host();
        if config.host != self.config.host {
            // A different printer: drop the previous one's live state.
            self.status = PrinterStatus::default();
            self.history = SessionHistory::default();
        }
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
        let printer_label: SharedString = if !self.config.name.is_empty() {
            match crate::discovery::model_name(&self.config.model) {
                "" => self.config.name.clone(),
                model => format!("{} · {model}", self.config.name),
            }
        } else {
            self.status
                .serial
                .clone()
                .unwrap_or_else(|| "LOCAL PRINTER".into())
        }
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
                    .id("titlebar-printer")
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(5.0))
                    .font_family("Geist Mono")
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(theme.hover).text_color(theme.text))
                    // Don't start a window drag from the printer switcher.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.screen = Screen::Printers;
                        cx.notify();
                    }))
                    .child(format!("{printer_label}  ▾")),
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
            .child(self.nav_item(theme, Screen::Printers, "⌕", "Printers", cx))
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
                Screen::Printers => "nav-printers",
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
                        .child(self.metrics(theme))
                        .when(!self.status.ams.is_empty(), |column| {
                            column.child(self.ams_overview(theme))
                        }),
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

    /// Overview card: one tile per AMS slot plus the external spool.
    fn ams_overview(&self, theme: Theme) -> AnyElement {
        let ams = &self.status.ams;
        let mut card = theme::card(theme).p(px(16.0));
        for unit in &ams.units {
            let tiles = unit.slots.iter().enumerate().map(|(slot, spool)| {
                let active = ams.active
                    == Some(ActiveSpool::Slot {
                        unit: unit.id,
                        slot: slot as u32,
                    });
                spool_tile(theme, slot_label(unit.id, slot as u32), spool, active)
            });
            card = card.child(
                div()
                    .when(unit.id > 0, |section| section.mt(px(14.0)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(theme::eyebrow(
                                theme,
                                if ams.units.len() > 1 {
                                    ["AMS A", "AMS B", "AMS C", "AMS D"]
                                        .get(unit.id as usize)
                                        .copied()
                                        .unwrap_or("AMS")
                                } else {
                                    "AMS"
                                },
                            ))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .font_family("Geist Mono")
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted)
                                    .child(ams_environment_label(unit)),
                            ),
                    )
                    .child(div().mt(px(10.0)).flex().gap(px(8.0)).children(tiles)),
            );
        }
        if let Some(spool) = &ams.external {
            let active = ams.active == Some(ActiveSpool::External);
            card = card.child(
                div()
                    .mt(px(14.0))
                    .child(theme::eyebrow(theme, "EXTERNAL SPOOL"))
                    .child(
                        div()
                            .mt(px(10.0))
                            .flex()
                            .gap(px(8.0))
                            .child(spool_tile(theme, "EXT".into(), spool, active))
                            .children((0..3).map(|_| div().flex_1())),
                    ),
            );
        }
        card.into_any_element()
    }

    /// Telemetry card: remaining filament per spool and what this session used.
    fn filament_usage(&self, theme: Theme) -> AnyElement {
        let ams = &self.status.ams;
        let mut rows = div().mt(px(8.0)).flex().flex_col();
        let mut spools: Vec<(String, u32, u32, &Spool, bool)> = Vec::new();
        for unit in &ams.units {
            for (slot, spool) in unit.slots.iter().enumerate() {
                let active = ams.active
                    == Some(ActiveSpool::Slot {
                        unit: unit.id,
                        slot: slot as u32,
                    });
                spools.push((
                    slot_label(unit.id, slot as u32),
                    unit.id,
                    slot as u32,
                    spool,
                    active,
                ));
            }
        }
        if let Some(spool) = &ams.external {
            spools.push((
                "EXT".into(),
                u32::MAX,
                0,
                spool,
                ams.active == Some(ActiveSpool::External),
            ));
        }

        for (label, unit, slot, spool, active) in spools {
            let used = self.history.spool_used(unit, slot, spool);
            let (title, detail) = if spool.loaded {
                let title = if spool.name.is_empty() {
                    spool.kind.clone()
                } else {
                    spool.name.clone()
                };
                let detail = match (spool.remain, used) {
                    (Some(_), Some(used)) if used > 0 => format!("−{used}% this session"),
                    (Some(_), _) => "No change this session".into(),
                    (None, _) => "Remaining not tracked (no RFID tag)".into(),
                };
                (title, detail)
            } else {
                ("Empty".into(), String::new())
            };
            rows = rows.child(
                div()
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w(px(28.0))
                            .flex_none()
                            .font_family("Geist Mono")
                            .text_size(px(10.0))
                            .text_color(if active {
                                theme.accent
                            } else {
                                theme.text_faint
                            })
                            .child(label),
                    )
                    .child(spool_swatch(theme, spool, 14.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(if spool.loaded {
                                        theme.text
                                    } else {
                                        theme.text_faint
                                    })
                                    .truncate()
                                    .child(title),
                            )
                            .when(!detail.is_empty(), |column| {
                                column.child(
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(theme.text_faint)
                                        .truncate()
                                        .child(detail),
                                )
                            }),
                    )
                    .when(spool.loaded, |row| {
                        row.child(
                            div()
                                .w(px(84.0))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .items_end()
                                .gap(px(4.0))
                                .child(
                                    div()
                                        .font_family("Geist Mono")
                                        .text_size(px(12.0))
                                        .child(remain_label(spool.remain)),
                                )
                                .child(remain_bar(theme, spool.remain)),
                        )
                    }),
            );
        }

        theme::card(theme)
            .w(px(340.0))
            .flex_none()
            .p(px(16.0))
            .child(theme::eyebrow(theme, "FILAMENT"))
            .child(rows)
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
        let thermal_series = vec![
            ChartSeries::new(theme.warning, "Nozzle °C", " °C", 1, |p| p.nozzle),
            ChartSeries::new(theme.accent, "Bed °C", " °C", 1, |p| p.bed),
            ChartSeries::new(theme.cyan, "Progress %", "%", 0, |p| Some(p.progress)),
        ];
        let ams_series = ams_chart_series(&self.status.ams, theme);
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
                        .child(telemetry_chart(
                            points.clone(),
                            thermal_series.clone(),
                            280.0,
                            theme,
                        )),
                )
                .child(
                    div().mt(px(8.0)).flex().gap(px(16.0)).children(
                        thermal_series
                            .iter()
                            .map(|line| legend(line.color, line.label.clone())),
                    ),
                ),
        )
        .when(!self.status.ams.is_empty(), |page| {
            page.child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .items_start()
                    .gap(px(12.0))
                    .child(
                        theme::card(theme)
                            .flex_1()
                            .min_w_0()
                            .p(px(16.0))
                            .child(theme::eyebrow(theme, "AMS ENVIRONMENT"))
                            .child(
                                div()
                                    .id("ams-chart")
                                    .mt(px(10.0))
                                    .h(px(190.0))
                                    .w_full()
                                    .on_mouse_move(cx.listener(|_, _, _, cx| cx.notify()))
                                    .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
                                    .child(telemetry_chart(
                                        points,
                                        ams_series.clone(),
                                        100.0,
                                        theme,
                                    )),
                            )
                            .child(
                                div().mt(px(8.0)).flex().flex_wrap().gap(px(16.0)).children(
                                    ams_series
                                        .iter()
                                        .map(|line| legend(line.color, line.label.clone())),
                                ),
                            ),
                    )
                    .child(self.filament_usage(theme)),
            )
        })
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
            div()
                .mt(px(16.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child("Printer on this network? Find it instead of typing its address."),
                )
                .child(div().flex_1())
                .child(
                    theme::action(theme, false)
                        .id("settings-discover")
                        .h(px(28.0))
                        .text_size(px(11.0))
                        .child("Find printers")
                        .on_click(cx.listener(|app, _, _, cx| {
                            app.screen = Screen::Printers;
                            app.start_discovery();
                            cx.notify();
                        })),
                ),
        )
        .child(
            theme::card(theme)
                .mt(px(12.0))
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
            Screen::Printers => self.render_printers(theme, cx),
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

type SeriesValue = Rc<dyn Fn(&TelemetryPoint) -> Option<f32>>;

/// One line on a telemetry chart.
#[derive(Clone)]
struct ChartSeries {
    color: Hsla,
    label: SharedString,
    unit: &'static str,
    decimals: usize,
    value: SeriesValue,
}

impl ChartSeries {
    fn new(
        color: Hsla,
        label: impl Into<SharedString>,
        unit: &'static str,
        decimals: usize,
        value: impl Fn(&TelemetryPoint) -> Option<f32> + 'static,
    ) -> Self {
        Self {
            color,
            label: label.into(),
            unit,
            decimals,
            value: Rc::new(value),
        }
    }
}

/// Session line chart on a fixed 0..`max` scale, with a hover readout.
fn telemetry_chart(
    points: Vec<TelemetryPoint>,
    series: Vec<ChartSeries>,
    max: f32,
    theme: Theme,
) -> impl IntoElement {
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
                bounds.origin.y + bounds.size.height * (1.0 - (value / max).clamp(0.0, 1.0))
            };
            for line in &series {
                let mut path = PathBuilder::stroke(px(1.5));
                let mut started = false;
                for sample in &points {
                    let Some(value) = (line.value)(sample) else {
                        continue;
                    };
                    let at = point(x_of(sample), y_of(value));
                    if started {
                        path.line_to(at);
                    } else {
                        path.move_to(at);
                        started = true;
                    }
                }
                if started && let Ok(path) = path.build() {
                    window.paint_path(path, line.color);
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
            for line in &series {
                if let Some(value) = (line.value)(sample) {
                    let center = point(x, y_of(value));
                    window.paint_quad(
                        gpui::fill(
                            gpui::Bounds::centered_at(center, gpui::size(px(7.0), px(7.0))),
                            line.color,
                        )
                        .corner_radii(px(3.5)),
                    );
                }
            }

            let time = sample
                .at
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string();
            let lines =
                std::iter::once((theme.text_muted, time)).chain(series.iter().map(|line| {
                    let reading = (line.value)(sample).map_or("—".to_owned(), |value| {
                        format!("{value:.*}{}", line.decimals, line.unit)
                    });
                    (line.color, format!("{}  {reading}", line.label))
                }));
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

fn legend(color: Hsla, label: impl Into<SharedString>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(9.0))
        .text_color(color)
        .child(status_dot(color, 5.0))
        .child(label.into())
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

/// Bambu's slot naming: AMS A slots are A1–A4, AMS B slots B1–B4, and so on.
fn slot_label(unit: u32, slot: u32) -> String {
    let letter = char::from(b'A' + (unit.min(25) as u8));
    format!("{letter}{}", slot + 1)
}

fn ams_environment_label(unit: &crate::ams::AmsUnit) -> String {
    let humidity = match (unit.humidity, unit.humidity_level) {
        (Some(percent), _) => format!("{percent:.0}% RH"),
        (None, Some(level)) => format!("Humidity {level}/5"),
        (None, None) => "Humidity —".into(),
    };
    match unit.temperature {
        Some(temperature) => format!("{humidity} · {temperature:.1} °C"),
        None => humidity,
    }
}

/// Humidity and temperature lines for each AMS unit, on a shared 0–100 scale.
fn ams_chart_series(ams: &AmsState, theme: Theme) -> Vec<ChartSeries> {
    let humidity_colors = [theme.cyan, theme.success];
    let temperature_colors = [theme.warning, theme.accent];
    let multiple = ams.units.len() > 1;
    ams.units
        .iter()
        .enumerate()
        .flat_map(|(index, unit)| {
            let id = unit.id;
            let prefix = if multiple {
                format!("AMS {} ", char::from(b'A' + (id.min(25) as u8)))
            } else {
                String::new()
            };
            let reading =
                move |p: &TelemetryPoint| p.ams.iter().find(|env| env.unit == id).copied();
            [
                ChartSeries::new(
                    humidity_colors[index % 2],
                    format!("{prefix}Humidity %"),
                    "%",
                    0,
                    move |p| reading(p)?.humidity,
                ),
                ChartSeries::new(
                    temperature_colors[index % 2],
                    format!("{prefix}Temperature °C"),
                    " °C",
                    1,
                    move |p| reading(p)?.temperature,
                ),
            ]
        })
        .collect()
}

fn spool_swatch(theme: Theme, spool: &Spool, size: f32) -> gpui::Div {
    let fill = match (spool.loaded, spool.color) {
        (true, Some(rgb)) => theme::color(rgb),
        (true, None) => theme.text_faint,
        (false, _) => theme.hover,
    };
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(fill)
        .border_1()
        .border_color(theme.border_strong)
}

fn remain_label(remain: Option<u8>) -> String {
    remain.map_or("—".into(), |remain| format!("{remain}%"))
}

fn remain_bar(theme: Theme, remain: Option<u8>) -> gpui::Div {
    let fraction = remain.map_or(0.0, |remain| f32::from(remain) / 100.0);
    let color = match remain {
        Some(remain) if remain <= 15 => theme.warning,
        _ => theme.accent,
    };
    div()
        .w_full()
        .h(px(4.0))
        .rounded(px(2.0))
        .bg(theme.border_strong)
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction))
                .rounded(px(2.0))
                .bg(color),
        )
}

/// Overview tile for one AMS slot or the external spool.
fn spool_tile(theme: Theme, label: String, spool: &Spool, active: bool) -> AnyElement {
    let (title, subtitle) = if spool.loaded {
        (
            spool.kind.clone(),
            if spool.name.is_empty() || spool.name == spool.kind {
                if spool.bambu {
                    "Bambu".into()
                } else {
                    "Generic".into()
                }
            } else {
                spool.name.clone()
            },
        )
    } else {
        ("Empty".to_owned(), String::new())
    };
    div()
        .flex_1()
        .min_w_0()
        .p(px(10.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .when(active, |tile| tile.bg(theme.active))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .font_family("Geist Mono")
                        .text_size(px(9.5))
                        .text_color(if active {
                            theme.accent
                        } else {
                            theme.text_faint
                        })
                        .child(label),
                )
                .child(div().flex_1())
                .when(active, |row| {
                    row.child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(8.5))
                            .text_color(theme.accent)
                            .child("FEEDING"),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(spool_swatch(theme, spool, 22.0))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(if spool.loaded {
                                    theme.text
                                } else {
                                    theme.text_faint
                                })
                                .truncate()
                                .child(title),
                        )
                        .when(!subtitle.is_empty(), |column| {
                            column.child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(theme.text_faint)
                                    .truncate()
                                    .child(subtitle),
                            )
                        }),
                ),
        )
        .when(spool.loaded, |tile| {
            tile.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .font_family("Geist Mono")
                            .text_size(px(15.0))
                            .child(remain_label(spool.remain)),
                    )
                    .child(remain_bar(theme, spool.remain)),
            )
        })
        .into_any_element()
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
