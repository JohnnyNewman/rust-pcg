//! Background loading: the static pipeline runs on the async compute pool and
//! hands back an immutable [`Loaded`] snapshot. Reloads of the same project
//! reuse the [`Cache`] (only changed files are re-parsed) and carry a diff
//! against the previous snapshot, which drives the [`Transition`]. Unsaved
//! editor text ([`Editing`]) enters the build as an overlay.

use crate::model::*;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, futures::check_ready};
use pcg_core::NodeId;
use pcg_syntax::{Overlays, ParseCache};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub fn build(
    path: std::path::PathBuf,
    cache: Arc<Mutex<ParseCache>>,
    overlays: Overlays,
    prev: Option<Arc<Loaded>>,
) -> Loaded {
    let (graph, stats) = {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        pcg_syntax::build_graph_overlaid(&path, &mut c, &overlays)
    };
    let t = Instant::now();
    let name_len: Vec<u32> =
        (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).chars().count() as u32).collect();
    let layout = pcg_layout::layout(&graph.nodes, &name_len, &graph.edges.src, &graph.edges.dst, &Default::default());
    let t_layout = t.elapsed();
    let search_names = (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).to_lowercase().into()).collect();
    let t = Instant::now();
    let diff = prev.map(|p| pcg_syntax::diff(&p.graph.nodes, &graph.nodes));
    let t_diff = t.elapsed();
    Loaded { graph, layout, stats, t_layout, search_names, diff, t_diff }
}

pub fn start(
    mut req: ResMut<LoadRequest>,
    mut task: ResMut<LoadTask>,
    mut cache: ResMut<Cache>,
    project: Res<Project>,
    mut editing: ResMut<Editing>,
    time: Res<Time>,
) {
    if !req.pending || task.task.is_some() {
        return;
    }
    req.pending = false;
    let path = req.path.clone();
    // A different project gets a fresh cache and no diff.
    let same_project = cache.root == path && project.data.is_some();
    if !same_project {
        *cache = Cache { root: path.clone(), ..default() };
        editing.docs.clear();
    }
    let overlays = crate::edit::overlays(&editing);
    let prev = if same_project { project.data.clone() } else { None };
    let c = cache.cache.clone();
    task.keep_view = req.keep_view && same_project;
    task.path = path.clone();
    task.started = time.elapsed_secs_f64();
    task.task = Some(AsyncComputeTaskPool::get().spawn(async move { build(path, c, overlays, prev) }));
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
    if selected != sel.selected {
        sel.changed_at = now;
    }
    sel.selected = selected;
    sel.hovered = NodeId::NONE;
    ui.search_for.clear(); // invalidate search hits (ids changed)
    let closed = crate::edit::rebind(&mut editing, &loaded);
    if closed > 0 {
        ui.status += &format!("\n{closed} editor(s) closed: their code is gone from disk");
    }

    if task.keep_view {
        // Animate only if something actually changed.
        let changed = loaded.diff.as_ref().is_some_and(|d| !d.is_empty());
        tr.prev = if changed { project.data.take() } else { None };
        tr.started = now;
    } else {
        tr.prev = None;
        view.needs_fit = true;
        project.loaded_at = now;
    }
    if watch.root != task.path {
        crate::watch::watch(&mut watch, &task.path);
    }
    project.data = Some(Arc::new(loaded));
}
