//! Resources: pure data, no behaviour.

use bevy::prelude::*;
use bevy::tasks::Task;
use bevy_egui::egui;
use pcg_core::{Graph, NodeId};
use pcg_layout::Layout;
use pcg_syntax::BuildStats;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Result of the background pipeline: an immutable snapshot.
pub struct Loaded {
    pub graph: Graph,
    pub layout: Layout,
    pub stats: BuildStats,
    pub t_layout: Duration,
    /// Per-node lowercase names for search.
    pub search_names: Vec<Box<str>>,
}

#[derive(Resource, Default)]
pub struct Project {
    pub data: Option<Arc<Loaded>>,
    /// `Time::elapsed_secs_f64` when `data` arrived (drives the grow-in animation).
    pub loaded_at: f64,
}

#[derive(Resource)]
pub struct LoadRequest {
    pub path: PathBuf,
    pub pending: bool,
    /// Reload of the same project: keep camera and selection.
    pub keep_view: bool,
}

#[derive(Resource, Default)]
pub struct LoadTask {
    pub task: Option<Task<Loaded>>,
    pub keep_view: bool,
    pub started: f64,
}

/// Camera over the world-space layout. `*_target` are animated towards.
#[derive(Resource)]
pub struct View {
    pub center: egui::Pos2,
    pub zoom: f32,
    pub target_center: egui::Pos2,
    pub target_zoom: f32,
    /// Screen rect of the canvas in the last frame.
    pub canvas: egui::Rect,
    pub needs_fit: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            center: egui::Pos2::ZERO,
            zoom: 1.0,
            target_center: egui::Pos2::ZERO,
            target_zoom: 1.0,
            canvas: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0)),
            needs_fit: true,
        }
    }
}

#[derive(Resource)]
pub struct Selection {
    pub selected: NodeId,
    pub hovered: NodeId,
    /// Time of the last selection change (drives the highlight animation).
    pub changed_at: f64,
    /// After a reload, reselect by qualified name.
    pub reselect: Option<String>,
}

impl Default for Selection {
    fn default() -> Self {
        Self { selected: NodeId::NONE, hovered: NodeId::NONE, changed_at: 0.0, reselect: None }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Face {
    /// Show code at deep zoom.
    #[default]
    Code,
    /// Show `@pcg:summary` / `@pcg:intent` at deep zoom.
    Summary,
}

#[derive(Resource, Default)]
pub struct UiState {
    pub path_input: String,
    pub search: String,
    pub search_hits: Vec<NodeId>,
    pub search_for: String,
    pub show_all_edges: bool,
    pub face: Face,
    pub summary_draft: String,
    pub draft_for: NodeId,
    pub status: String,
    pub themed: bool,
}

/// Per-frame scratch buffers for the canvas (kept to avoid reallocation).
#[derive(Resource, Default)]
pub struct CanvasScratch {
    /// Nearest drawn ancestor-or-self of each node (`u32::MAX` = none).
    pub rep: Vec<u32>,
    /// Opacity each node passes to its children (semantic-zoom fade).
    pub child_alpha: Vec<f32>,
    pub drawn: usize,
    pub edge_pairs: rustc_hash::FxHashSet<(u32, u32)>,
}
