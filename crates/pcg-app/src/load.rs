//! Background loading: the static pipeline runs on the async compute pool and
//! hands back an immutable [`Loaded`] snapshot. Reloads of the same project
//! reuse the [`Cache`] (only changed files are re-parsed) and carry a diff
//! against the previous snapshot, which drives the [`Transition`]. Unsaved
//! editor text ([`Editing`]) enters the build as an overlay.
//!
//! A change of view state only ([`ViewState`]: folds, focus) takes the short
//! way: [`relayout`] shares the graph and runs just the layout.

use crate::model::*;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, futures::check_ready};
use pcg_core::{Graph, NodeId};
use pcg_layout::Hints;
use pcg_syntax::{BuildStats, Overlays, ParseCache, Precise};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub fn build(
    path: std::path::PathBuf,
    cache: Arc<Mutex<ParseCache>>,
    overlays: Overlays,
    precise: Option<Arc<Precise>>,
    prev: Option<Arc<Loaded>>,
    folds: Folds,
    view_rev: u64,
) -> Loaded {
    let (graph, stats) = {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        pcg_syntax::build_graph_with(&path, &mut c, &overlays, precise.as_deref())
    };
    let search_names = (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).to_lowercase().into()).collect();
    let lsp_stale = (0..graph.files.len()).any(|f| {
        let known = precise.as_ref().and_then(|p| p.files.get(&graph.files.path[f]));
        known != Some(&pcg_syntax::text_hash(&graph.files.source[f]))
    });
    finish(Arc::new(graph), stats, search_names, lsp_stale, prev, &folds, view_rev)
}

/// `prev`'s graph, laid out for other folds.
pub fn relayout(prev: Arc<Loaded>, folds: Folds, view_rev: u64) -> Loaded {
    let (graph, stats, names) = (prev.graph.clone(), prev.stats.clone(), prev.search_names.clone());
    finish(graph, stats, names, prev.lsp_stale, Some(prev), &folds, view_rev)
}

/// Diff against `prev`, then the layout — which is told where things were, so
/// that it moves them as little as possible.
fn finish(
    graph: Arc<Graph>,
    stats: BuildStats,
    search_names: Arc<[Box<str>]>,
    lsp_stale: bool,
    prev: Option<Arc<Loaded>>,
    folds: &Folds,
    view_rev: u64,
) -> Loaded {
    let t = Instant::now();
    let diff = prev.as_ref().map(|p| pcg_syntax::diff(&p.graph.nodes, &graph.nodes));
    let t_diff = t.elapsed();

    let t = Instant::now();
    let name_len: Vec<u32> =
        (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).chars().count() as u32).collect();
    let (collapsed, dim) = crate::viewstate::columns(&graph, folds);
    let (mut rank, mut flow) = (Vec::new(), Vec::new());
    if let (Some(p), Some(d)) = (&prev, &diff) {
        let old = |o: &NodeId| o.is_some().then(|| o.idx());
        rank = d.old_of_new.iter().map(|o| old(o).map_or(u32::MAX, |o| p.layout.rank[o])).collect();
        flow = d.old_of_new.iter().map(|o| old(o).map_or([0, 0], |o| p.layout.flow[o])).collect();
    }
    let hints = Hints { collapsed: &collapsed, rank: &rank, flow: &flow };
    let (src, dst) = (&graph.edges.src, &graph.edges.dst);
    let layout = pcg_layout::layout_with(&graph.nodes, &name_len, src, dst, &Default::default(), &hints);
    let t_layout = t.elapsed();
    let view_changed = prev.is_some_and(|p| p.view_rev != view_rev);
    Loaded { graph, layout, stats, t_layout, search_names, dim, view_rev, view_changed, diff, t_diff, lsp_stale }
}

#[allow(clippy::too_many_arguments)]
pub fn start(
    mut req: ResMut<LoadRequest>,
    mut task: ResMut<LoadTask>,
    mut cache: ResMut<Cache>,
    project: Res<Project>,
    mut editing: ResMut<Editing>,
    mut vs: ResMut<ViewState>,
    lsp: Res<Lsp>,
    time: Res<Time>,
) {
    if !(req.pending || req.relayout) || task.task.is_some() {
        return;
    }
    let layout_only = std::mem::take(&mut req.relayout) && !req.pending;
    if layout_only {
        // The graph is what it was: only the layout runs again.
        let Some(prev) = project.data.clone() else { return };
        let (folds, rev) = (vs.folds.clone(), vs.rev);
        vs.laid_out = rev;
        task.keep_view = true;
        task.path = cache.root.clone();
        task.started = time.elapsed_secs_f64();
        task.task = Some(AsyncComputeTaskPool::get().spawn(async move { relayout(prev, folds, rev) }));
        return;
    }
    req.pending = false;
    let path = req.path.clone();
    // A different project gets a fresh cache and no diff.
    let same_project = cache.root == path && project.data.is_some();
    if !same_project {
        *cache = Cache { root: path.clone(), ..default() };
        editing.docs.clear();
        *vs = crate::viewstate::load(&path);
    }
    let (folds, rev) = (vs.folds.clone(), vs.rev);
    vs.laid_out = rev;
    let overlays = crate::edit::overlays(&editing);
    // Answers for another project's files would simply not apply.
    let precise = lsp.precise.clone();
    let prev = if same_project { project.data.clone() } else { None };
    let c = cache.cache.clone();
    task.keep_view = req.keep_view && same_project;
    task.path = path.clone();
    task.started = time.elapsed_secs_f64();
    task.task =
        Some(AsyncComputeTaskPool::get().spawn(async move { build(path, c, overlays, precise, prev, folds, rev) }));
}

#[allow(clippy::too_many_arguments)]
pub fn poll(
    mut task: ResMut<LoadTask>,
    mut project: ResMut<Project>,
    mut view: ResMut<View>,
    mut sel: ResMut<Selection>,
    mut ui: ResMut<UiState>,
    mut tr: ResMut<Transition>,
    mut watch: ResMut<Watch>,
    mut editing: ResMut<Editing>,
    mut vs: ResMut<ViewState>,
    time: Res<Time>,
) {
    let Some(t) = task.task.as_mut() else { return };
    let Some(loaded) = check_ready(t) else { return };
    task.task = None;
    let now = time.elapsed_secs_f64();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    ui.status = format!("{}\nlayout {:.1} ms", loaded.stats, ms(loaded.t_layout));
    if let Some(d) = &loaded.diff {
        ui.status += &format!(" | diff {:.1} ms: +{} −{} ~{}", ms(loaded.t_diff), d.entered, d.exited, d.changed);
    }
    info!("{}", ui.status);

    // Carry selection over by identity, not by position.
    let map = |n: NodeId| match &loaded.diff {
        Some(d) if n.is_some() && n.idx() < d.new_of_old.len() => d.new_of_old[n.idx()],
        _ => NodeId::NONE,
    };
    let selected = map(sel.selected);
    // Folding moves everything around: keep what the user is looking at —
    // the selection, or the box it is now folded into — where it is on screen.
    if loaded.view_changed
        && selected.is_some()
        && let Some(prev) = &project.data
    {
        let centre = |l: &pcg_layout::Layout, n: NodeId| {
            let i = l.shown[n.idx()] as usize;
            bevy_egui::egui::vec2(l.x[i] + l.w[i] * 0.5, l.y[i] + l.h[i] * 0.5)
        };
        let shift = centre(&loaded.layout, selected) - centre(&prev.layout, sel.selected);
        view.target_center += shift;
    }
    if selected != sel.selected {
        sel.changed_at = now;
    }
    sel.selected = selected;
    if std::mem::take(&mut vs.fly_to_selection) && selected.is_some() {
        view.fly_to_node(&loaded.layout, NodeId(loaded.layout.shown[selected.idx()]));
    }
    sel.hovered = NodeId::NONE;
    ui.search_for.clear(); // invalidate search hits (ids changed)
    let closed = crate::edit::rebind(&mut editing, &loaded);
    if closed > 0 {
        ui.status += &format!("\n{closed} editor(s) closed: their code is gone from disk");
    }

    if task.keep_view {
        // Animate only if something actually changed.
        let changed = loaded.view_changed || loaded.diff.as_ref().is_some_and(|d| !d.is_empty());
        tr.prev = if changed { project.data.take() } else { None };
        tr.started = now;
    } else {
        tr.prev = None;
        project.loaded_at = now;
        match vs.camera.take() {
            // Back where the user left off.
            Some([x, y, zoom]) => {
                view.center = bevy_egui::egui::pos2(x, y);
                view.zoom = zoom;
                (view.target_center, view.target_zoom) = (view.center, zoom);
                view.needs_fit = false;
            }
            None => view.needs_fit = true,
        }
    }
    if watch.root != task.path {
        crate::watch::watch(&mut watch, &task.path);
    }
    project.data = Some(Arc::new(loaded));
}
