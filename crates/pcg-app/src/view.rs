//! Camera math and animation.

use crate::model::View;
use bevy::prelude::*;
use bevy_egui::egui::{Pos2, Rect, Vec2};
use pcg_core::NodeId;
use pcg_layout::Layout;

pub const MAX_ZOOM: f32 = 12.0;

impl View {
    #[inline]
    pub fn w2s(&self, p: Pos2) -> Pos2 {
        self.canvas.center() + (p - self.center) * self.zoom
    }
    #[inline]
    pub fn s2w(&self, p: Pos2) -> Pos2 {
        self.center + (p - self.canvas.center()) / self.zoom
    }
    #[inline]
    pub fn world_rect_to_screen(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.w2s(Pos2::new(x, y)), Vec2::new(w, h) * self.zoom)
    }

    /// Animate the camera so `world` fills the canvas.
    pub fn fly_to_rect(&mut self, world: Rect, margin: f32) {
        let s = self.canvas.size();
        let z = (s.x / (world.width() * margin)).min(s.y / (world.height() * margin));
        self.target_zoom = z.clamp(1e-4, MAX_ZOOM);
        self.target_center = world.center();
    }

    pub fn fly_to_node(&mut self, l: &Layout, n: NodeId) {
        let i = n.idx();
        let r = Rect::from_min_size(Pos2::new(l.x[i], l.y[i]), Vec2::new(l.w[i], l.h[i]));
        self.fly_to_rect(r, 1.3);
    }

    /// Zoom by `factor` keeping the world point under `anchor` (screen) fixed.
    /// Immediate (no animation) — wheel input is already smooth.
    pub fn zoom_at(&mut self, anchor: Pos2, factor: f32, min_zoom: f32) {
        let wp = self.s2w(anchor);
        self.zoom = (self.zoom * factor).clamp(min_zoom, MAX_ZOOM);
        self.center = wp - (anchor - self.canvas.center()) / self.zoom;
        self.target_zoom = self.zoom;
        self.target_center = self.center;
    }

    pub fn pan(&mut self, screen_delta: Vec2) {
        self.center -= screen_delta / self.zoom;
        self.target_center = self.center;
    }
}

/// Exponential approach to the targets; zoom interpolates in log space so a
/// fly-to feels uniform across scales.
pub fn animate(mut view: ResMut<View>, time: Res<Time>) {
    let k = 1.0 - (-time.delta_secs() * 9.0).exp();
    let (lz, lt) = (view.zoom.ln(), view.target_zoom.ln());
    view.zoom = (lz + (lt - lz) * k).exp();
    // Move the center in screen-space-proportional steps so zooming out and
    // panning feel balanced.
    let c = view.center + (view.target_center - view.center) * k;
    view.center = c;
    if (view.zoom - view.target_zoom).abs() < 1e-5 * view.target_zoom {
        view.zoom = view.target_zoom;
    }
}
