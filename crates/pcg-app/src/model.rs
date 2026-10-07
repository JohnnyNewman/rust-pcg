//! Resources: pure data, no behaviour.

use bevy::prelude::*;
use bevy::tasks::Task;
use bevy_egui::egui;
use pcg_core::{Graph, NodeId};
use pcg_layout::Layout;
use pcg_syntax::{BuildStats, GraphDiff, ParseCache};
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
    /// Diff against the snapshot this one replaced (same project only).
    pub diff: Option<GraphDiff>,
    pub t_diff: Duration,
}

/// The animated hand-over from one snapshot to the next. While active, the
/// canvas tweens matched nodes from their old to their new rect, fades
/// entered nodes in, exited ones out, and flashes changed ones.
#[derive(Resource, Default)]
pub struct Transition {
    /// Previous snapshot; dropped when the transition ends (frees its memory).
    pub prev: Option<Arc<Loaded>>,
    pub started: f64,
}

/// Parse cache shared with the background build task. Reset when another
/// project is opened.
#[derive(Resource, Default)]
pub struct Cache {
    pub root: PathBuf,
    pub cache: Arc<std::sync::Mutex<ParseCache>>,
}

/// File watcher. The notify callback only bumps an atomic counter (no locks,
/// no allocation on the hot path); [`crate::watch::poll`] debounces it into a
/// reload request.
#[derive(Resource, Default)]
pub struct Watch {
    pub root: PathBuf,
    pub watcher: Option<notify::RecommendedWatcher>,
    pub events: Arc<std::sync::atomic::AtomicU64>,
    /// Counter value already turned into a reload.
    pub seen: u64,
    /// Counter value at the last observed change (for debouncing).
    pub burst: u64,
    /// Last time the counter moved.
    pub last_change: f64,
    pub error: Option<String>,
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
    pub path: PathBuf,
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
}

impl Default for Selection {
    fn default() -> Self {
        Self { selected: NodeId::NONE, hovered: NodeId::NONE, changed_at: 0.0 }
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
