//! Colours and egui styling.

use bevy_egui::egui::{self, Color32};
use pcg_core::{NodeKind, SummaryState};

pub const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x1b);
pub const PANEL: Color32 = Color32::from_rgb(0x18, 0x19, 0x26);
pub const TEXT: Color32 = Color32::from_rgb(0xca, 0xd3, 0xf5);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x80, 0x87, 0xa2);
pub const ACCENT: Color32 = Color32::from_rgb(0xf4, 0xdb, 0xd6);
pub const EDGE_OUT: Color32 = Color32::from_rgb(0x8a, 0xad, 0xf4);
pub const EDGE_IN: Color32 = Color32::from_rgb(0xf5, 0xa9, 0x7f);
pub const GRID: Color32 = Color32::from_rgb(0x2a, 0x2c, 0x3d);

pub fn bevy_clear() -> bevy::color::Color {
    bevy::color::Color::srgb_u8(BG.r(), BG.g(), BG.b())
}

pub fn kind(k: NodeKind) -> Color32 {
    use NodeKind::*;
    match k {
        Workspace => Color32::from_rgb(0x6e, 0x73, 0x8d),
        Crate => Color32::from_rgb(0x7a, 0xa2, 0xf7),
        FileModule | InlineModule => Color32::from_rgb(0x8b, 0xd5, 0xca),
        Struct | Enum | Union | TypeAlias => Color32::from_rgb(0xee, 0xd4, 0x9f),
        Trait => Color32::from_rgb(0xc6, 0xa0, 0xf6),
        Impl => Color32::from_rgb(0x91, 0xd7, 0xe3),
        Fn => Color32::from_rgb(0xa6, 0xda, 0x95),
        Const | Static => Color32::from_rgb(0xf5, 0xa9, 0x7f),
        Macro => Color32::from_rgb(0xf0, 0xc6, 0xc6),
    }
}

pub fn summary_state(s: SummaryState) -> (Color32, &'static str) {
    match s {
        SummaryState::Missing => (TEXT_DIM, "no summary"),
        SummaryState::Fresh => (Color32::from_rgb(0xa6, 0xda, 0x95), "summary fresh"),
        SummaryState::Stale => (Color32::from_rgb(0xf5, 0xa9, 0x7f), "summary STALE"),
        SummaryState::Unhashed => (Color32::from_rgb(0xee, 0xd4, 0x9f), "summary unhashed"),
    }
}

pub const INTENT: Color32 = Color32::from_rgb(0xc6, 0xa0, 0xf6);

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(0x36, 0x3a, 0x4f);
    v.hyperlink_color = EDGE_OUT;
    ctx.set_visuals(v);
}

/// Smoothstep from 0 at `a` to 1 at `b`.
#[inline]
pub fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
