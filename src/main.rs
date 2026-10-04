mod app;
mod ftps;
mod model;
mod model_3mf;
mod network;
mod printer_view;
mod prints;
mod theme;
mod timelapse;

use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    gpui_platform::application().run(|cx: &mut App| {
        gpui_base::init(cx);
        theme::register_fonts(cx);

        let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.0), px(640.0))),
                titlebar: Some(TitlebarOptions {
                    title: cfg!(target_os = "windows").then(|| "Bambu X".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.0), px(14.0))),
                }),
                app_owns_titlebar_drag: true,
                window_decorations: cfg!(target_os = "linux")
                    .then_some(gpui::WindowDecorations::Client),
                window_background: if cfg!(any(target_os = "macos", target_os = "windows")) {
                    gpui::WindowBackgroundAppearance::Blurred
                } else {
                    gpui::WindowBackgroundAppearance::Transparent
                },
                app_id: Some("bambux".into()),
                ..Default::default()
            },
            |window, cx| {
                window.set_rem_size(px(16.0));
                cx.new(|cx| app::BambuApp::new(window, cx))
            },
        )
        .expect("failed to open Bambu X window");
        cx.activate(true);
    });
}
