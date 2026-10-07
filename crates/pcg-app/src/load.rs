//! Background loading: the whole static pipeline runs on the async compute pool
//! and hands back an immutable [`Loaded`] snapshot.

use crate::model::*;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, futures::check_ready};
use pcg_core::NodeId;
use std::sync::Arc;
use std::time::Instant;

pub fn build(path: std::path::PathBuf) -> Loaded {
    let (graph, stats) = pcg_syntax::build_graph(&path);
    let t = Instant::now();
    let name_len: Vec<u32> =
        (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).chars().count() as u32).collect();
    let layout = pcg_layout::layout(&graph.nodes, &name_len, &Default::default());
    let t_layout = t.elapsed();
    let search_names = (0..graph.nodes.len()).map(|i| graph.name(NodeId::from_idx(i)).to_lowercase().into()).collect();
    Loaded { graph, layout, stats, t_layout, search_names }
}

pub fn start(mut req: ResMut<LoadRequest>, mut task: ResMut<LoadTask>, time: Res<Time>) {
    if !req.pending || task.task.is_some() {
        return;
    }
    req.pending = false;
    let path = req.path.clone();
    task.keep_view = req.keep_view;
    task.started = time.elapsed_secs_f64();
    task.task = Some(AsyncComputeTaskPool::get().spawn(async move { build(path) }));
}

pub fn poll(
    mut task: ResMut<LoadTask>,
    mut project: ResMut<Project>,
    mut view: ResMut<View>,
    mut sel: ResMut<Selection>,
    mut ui: ResMut<UiState>,
    time: Res<Time>,
) {
    let Some(t) = task.task.as_mut() else { return };
    let Some(loaded) = check_ready(t) else { return };
    task.task = None;
    let now = time.elapsed_secs_f64();
    ui.status = format!("{}\nlayout {:.1} ms", loaded.stats, loaded.t_layout.as_secs_f64() * 1e3);
    info!("{}", ui.status);

    let g = &loaded.graph;
    sel.hovered = NodeId::NONE;
    sel.selected = NodeId::NONE;
    if let Some(q) = sel.reselect.take() {
        sel.selected =
            (0..g.nodes.len()).map(NodeId::from_idx).find(|&n| g.qualified_name(n) == q).unwrap_or(NodeId::NONE);
    }
    ui.search_for.clear(); // invalidate search hits (ids changed)
    if !task.keep_view {
        view.needs_fit = true;
        project.loaded_at = now;
    }
    project.data = Some(Arc::new(loaded));
}
