//! Resources: pure data, no behaviour.

use bevy::prelude::*;
use bevy::tasks::Task;
use bevy_egui::egui;
use pcg_core::{Graph, NodeId};
use pcg_layout::Layout;
use pcg_syntax::{Buffer, BuildStats, GraphDiff, ItemPath, ParseCache, Precise};
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
    /// Some file's text is not what the language server was last asked about.
    pub lsp_stale: bool,
}

/// The language server (rust-analyzer) behind the precise call edges.
#[derive(Resource, Default)]
pub struct Lsp {
    /// Off with `--no-lsp`.
    pub enabled: bool,
    /// Project the worker thread was started for.
    pub root: PathBuf,
    /// Snapshots to resolve, to the worker.
    pub jobs: Option<std::sync::mpsc::Sender<Arc<Loaded>>>,
    pub events: Option<std::sync::Mutex<std::sync::mpsc::Receiver<crate::lsp::Event>>>,
    /// Latest answers; every build applies what still matches the text.
    pub precise: Option<Arc<Precise>>,
    /// What the server is doing, for the side panel.
    pub status: String,
    /// A snapshot is with the worker.
    pub busy: bool,
    pub failed: bool,
    /// Identity of the snapshot last sent.
    pub asked: usize,
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

/// Open files and the in-node editors looking into them.
#[derive(Resource, Default)]
pub struct Editing {
    pub docs: Vec<Doc>,
    /// Source of [`Editor::id`]s.
    pub next_id: u64,
}

/// A file open for editing.
pub struct Doc {
    pub path: PathBuf,
    /// The whole file, with its syntax tree.
    pub buffer: Buffer,
    /// On-disk text the buffer started from / was last saved as (save guard).
    pub base: Arc<str>,
    pub crlf: bool,
    /// Buffer differs from `base`.
    pub dirty: bool,
    /// The file changed on disk under unsaved text.
    pub conflict: bool,
    /// Time of the last keystroke (debounces the live rebuild).
    pub changed_at: f64,
    /// Buffer changed since the last rebuild was requested.
    pub graph_stale: bool,
    /// Views into the buffer; their spans never overlap.
    pub editors: Vec<Editor>,
}

/// One node's source, open on the canvas.
pub struct Editor {
    /// Stable id for egui state (focus, cursor, scroll).
    pub id: u64,
    /// Caption: the edited node, as last seen in a snapshot.
    pub title: String,
    /// The edited node's bytes in the doc's buffer.
    pub span: std::ops::Range<usize>,
    /// `span`'s text as shown in the editor (LF line endings).
    pub draft: String,
    /// `draft` as last applied to the buffer.
    pub synced: String,
    /// `synced`, syntax-highlighted.
    pub job: egui::text::LayoutJob,
    /// The edited item's identity in its file (to re-find it in new disk text).
    pub item: ItemPath,
    /// Editing the file module: the span is the whole file.
    pub whole_file: bool,
    /// The edited node in the current snapshot (`NONE` while it does not parse).
    pub node: NodeId,
    /// World rect the editor sits on (the node's, as last seen).
    pub anchor: [f32; 4],
    pub want_focus: bool,
    /// The text field had keyboard focus last frame.
    pub focused: bool,
    /// Esc was pressed once on unsaved text.
    pub discard_armed: bool,
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
