//! A focused port of Zeron's dark semantic palette and surface recipes.
//!
//! Bambu X intentionally uses the same GPUI fork, gpui-base foundation,
//! Geist typography, spacing ladder, radii, frosted shell treatment and glass
//! control recipe as Zeron. This module keeps only the roles this app needs.

use std::borrow::Cow;

use gpui::prelude::*;
use gpui::{
    App, Background, BoxShadow, Div, FontWeight, Hsla, Styled, div, hsla, linear_color_stop,
    linear_gradient, point, px, rgb,
};

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Hsla,
    pub surface: Hsla,
    pub card: Hsla,
    pub overlay: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub accent: Hsla,
    pub accent_strong: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub cyan: Hsla,
    pub solid: Hsla,
    pub on_solid: Hsla,
}

impl Theme {
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    pub const PANEL_RADIUS: f32 = 10.0;
    pub fn dark() -> Self {
        Self {
            bg: color(0x060606),
            surface: color(0x0d0d0d),
            card: color(0x0e0e0e),
            overlay: color(0x161616),
            hover: hsla(0.0, 0.0, 0.92, 0.11),
            active: hsla(0.0, 0.0, 0.92, 0.16),
            border: hsla(0.0, 0.0, 1.0, 0.08),
            border_strong: hsla(0.0, 0.0, 1.0, 0.14),
            text: color(0xe8e8ea),
            text_muted: color(0xa9a9ae),
            text_faint: color(0x85858a),
            accent: color(0x8b7cf6),
            accent_strong: color(0x6d5ce8),
            success: color(0x34d399),
            warning: color(0xfacc15),
            danger: color(0xf87171),
            cyan: color(0x22d3ee),
            solid: color(0xebebef),
            on_solid: color(0x0e0e0e),
        }
    }

    /// Zeron's macOS/Windows frosted shell tint.
    pub fn glass(self) -> Hsla {
        self.surface
            .opacity(if cfg!(any(target_os = "macos", target_os = "windows")) {
                0.80
            } else {
                1.0
            })
    }

    pub fn wash(self, alpha: f32) -> Hsla {
        hsla(0.0, 0.0, 0.92, alpha)
    }

    pub fn input_bg(self) -> Hsla {
        hsla(0.0, 0.0, 1.0, 0.035)
    }
}

pub fn register_fonts(cx: &App) {
    const FONTS: [&[u8]; 4] = [
        include_bytes!("../assets/fonts/Geist.ttf"),
        include_bytes!("../assets/fonts/Geist-Medium.ttf"),
        include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
        include_bytes!("../assets/fonts/GeistMono.ttf"),
    ];
    if let Err(error) = cx
        .text_system()
        .add_fonts(FONTS.into_iter().map(Cow::Borrowed).collect())
    {
        tracing::warn!(%error, "failed to register bundled Geist fonts");
    }
}

pub fn root(theme: Theme) -> Div {
    div()
        .size_full()
        .bg(theme.glass())
        .text_color(theme.text)
        .font_family("Geist")
        .text_size(px(13.0))
}

pub fn card(theme: Theme) -> Div {
    div()
        .rounded(px(Theme::PANEL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme
            .card
            .opacity(if cfg!(target_os = "macos") { 0.76 } else { 1.0 }))
        .shadow(vec![BoxShadow {
            color: hsla(0.0, 0.0, 0.0, 0.22),
            offset: point(px(0.0), px(8.0)),
            blur_radius: px(30.0),
            spread_radius: px(-12.0),
            inset: false,
        }])
}

pub fn inset(theme: Theme) -> Div {
    div()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.overlay.opacity(0.58))
}

pub fn eyebrow(theme: Theme, text: impl Into<gpui::SharedString>) -> Div {
    div()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(text.into())
}

pub fn action(theme: Theme, solid: bool) -> Div {
    let base = div()
        .h(px(32.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .text_size(px(12.5))
        .cursor_pointer();
    if solid {
        base.bg(theme.solid)
            .text_color(theme.on_solid)
            .font_weight(FontWeight::MEDIUM)
            .hover(|style| style.opacity(0.90))
    } else {
        base.bg(theme.input_bg())
            .border_1()
            .border_color(theme.border)
            .text_color(theme.text)
            .hover(move |style| style.bg(theme.hover).border_color(theme.border_strong))
    }
}

/// Zeron's neutral glass plate: top-lit gradient, faint rim, inner highlight,
/// and tight drop shadow.
pub fn glass_plate(theme: Theme) -> Div {
    div()
        .bg(vertical(
            hsla(0.0, 0.0, 1.0, 0.08),
            hsla(0.0, 0.0, 1.0, 0.05),
        ))
        .border_1()
        .border_color(hsla(0.0, 0.0, 1.0, 0.09))
        .shadow(vec![
            shadow(hsla(0.0, 0.0, 1.0, 0.07), 1.0, 0.0, 0.0, true),
            shadow(hsla(0.0, 0.0, 0.0, 0.16), 1.0, 2.0, 0.0, false),
        ])
        .text_color(theme.text)
}

fn vertical(top: Hsla, bottom: Hsla) -> Background {
    linear_gradient(
        180.0,
        linear_color_stop(top, 0.0),
        linear_color_stop(bottom, 1.0),
    )
}

fn shadow(color: Hsla, y: f32, blur: f32, spread: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.0), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset,
    }
}

pub fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}
