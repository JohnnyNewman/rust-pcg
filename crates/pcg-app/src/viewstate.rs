//! View state: what is not code, and so not in the source files — which
//! containers are folded, what the view is focused on, where the camera is.
//!
//! Folds are kept by `stable_key`, so they survive edits and restarts. They
//! become per-node columns ([`columns`]) for the layout, which runs again
//! whenever they change ([`poll`]); the canvas animates from the old layout
//! to the new one like after any reload.
//!
//! The collapsed set and the camera persist in a sidecar,
//! `<project>/.pcg/view.json`. It is first written when the user folds
//! something — just looking at a project leaves no file behind.

use crate::model::*;
use bevy::prelude::*;
use pcg_core::{Graph, NodeId};
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};

/// Quiet time before the sidecar is written.
const SAVE_AFTER: f64 = 1.0;

/// `(collapsed, dim)` per node for `folds`; empty columns where nothing applies.
///
/// With a focus, everything that is not the focus, inside it, or a caller /
/// callee of it is dimmed, and containers of only such nodes are closed.
pub fn columns(g: &Graph, folds: &Folds) -> (Vec<bool>, Vec<bool>) {
    let keys = &g.nodes.stable_key;
    let focus = folds.focus.and_then(|k| keys.iter().position(|&x| x == k));
    let keep = focus.map(|f| pcg_layout::related(&g.nodes, &g.edges.src, &g.edges.dst, NodeId::from_idx(f)));
    if folds.collapsed.is_empty() && keep.is_none() {
        return (Vec::new(), Vec::new());
    }
    let collapsed = (0..g.nodes.len())
        .map(|i| {
            let folded = keep.as_ref().is_some_and(|k| !k[i]) && !folds.opened.contains(&keys[i]);
            folded || folds.collapsed.contains(&keys[i])
        })
        .collect();
    let dim = keep.map(|k| k.iter().map(|kept| !kept).collect()).unwrap_or_default();
    (collapsed, dim)
}

fn touch(vs: &mut ViewState, persist: bool) {
    vs.rev += 1;
    if persist {
        vs.persist_rev += 1;
    }
}

/// Close container `n`, or open it again.
pub fn toggle(vs: &mut ViewState, p: &Loaded, n: NodeId) {
    let key = p.graph.nodes.stable_key[n.idx()];
    if p.layout.collapsed[n.idx()] {
        let persist = vs.folds.collapsed.remove(&key);
        // Closed by the focus, not (only) by the user.
        if vs.folds.focus.is_some() {
            vs.folds.opened.insert(key);
        }
        touch(vs, persist);
    } else if !p.graph.nodes.descendants(n).is_empty() {
        vs.folds.opened.remove(&key);
        vs.folds.collapsed.insert(key);
        touch(vs, true);
    }
}

/// Open every closed container around `n`. Returns whether there was one.
pub fn reveal(vs: &mut ViewState, p: &Loaded, n: NodeId) -> bool {
    if p.layout.shown[n.idx()] == n.0 {
        return false;
    }
    let mut persist = false;
    for a in p.graph.nodes.ancestors(n) {
        let key = p.graph.nodes.stable_key[a.idx()];
        persist |= vs.folds.collapsed.remove(&key);
        if vs.folds.focus.is_some() {
            vs.folds.opened.insert(key);
        }
    }
    touch(vs, persist);
    true
}

pub fn expand_all(vs: &mut ViewState) {
    let persist = !vs.folds.collapsed.is_empty();
    vs.folds = Folds::default();
    touch(vs, persist);
}

/// Focus on `n` (`NONE`: leave focus mode).
pub fn focus(vs: &mut ViewState, p: &Loaded, n: NodeId) {
    let key = n.is_some().then(|| p.graph.nodes.stable_key[n.idx()]);
    if vs.folds.focus != key {
        vs.folds.focus = key;
        vs.folds.opened.clear();
        touch(vs, false);
    }
}

pub fn sidecar(root: &Path) -> PathBuf {
    root.join(".pcg").join("view.json")
}

fn to_json(collapsed: &FxHashSet<u64>, camera: [f32; 3]) -> String {
    let mut keys: Vec<u64> = collapsed.iter().copied().collect();
    keys.sort_unstable();
    let keys: Vec<String> = keys.iter().map(|k| format!("\"{k:016x}\"")).collect();
    let [x, y, zoom] = camera;
    format!(
        "{{\n  \"version\": 1,\n  \"camera\": {{ \"x\": {x}, \"y\": {y}, \"zoom\": {zoom} }},\n  \"collapsed\": [{}]\n}}\n",
        keys.join(", ")
    )
}

fn from_json(text: &str) -> Option<(FxHashSet<u64>, Option<[f32; 3]>)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? != 1 {
        return None;
    }
    let collapsed = v
        .get("collapsed")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|k| u64::from_str_radix(k.as_str()?, 16).ok()).collect())
        .unwrap_or_default();
    let cam = v.get("camera").and_then(|c| {
        let f = |k: &str| c.get(k).and_then(|x| x.as_f64()).map(|x| x as f32).filter(|x| x.is_finite());
        Some([f("x")?, f("y")?, f("zoom").filter(|z| *z > 0.0)?])
    });
    Some((collapsed, cam))
}

/// The view state of the project at `root`: its sidecar, or a fresh one.
pub fn load(root: &Path) -> ViewState {
    let mut vs = ViewState { root: root.to_path_buf(), ..default() };
    if let Some((collapsed, camera)) = std::fs::read_to_string(sidecar(root)).ok().as_deref().and_then(from_json) {
        vs.folds.collapsed = collapsed;
        vs.camera = camera;
        vs.saved_camera = camera.unwrap_or_default();
        vs.sidecar = true;
    }
    vs
}

fn save(root: &Path, collapsed: &FxHashSet<u64>, camera: [f32; 3]) -> std::io::Result<()> {
    let path = sidecar(root);
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Never a half-written file: write beside it, then swap.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, to_json(collapsed, camera))?;
    std::fs::rename(tmp, path)
}

/// Folds changed → lay out again. Folds or camera changed and came to rest →
/// write the sidecar.
pub fn poll(
    mut vs: ResMut<ViewState>,
    mut req: ResMut<LoadRequest>,
    task: Res<LoadTask>,
    project: Res<Project>,
    view: Res<View>,
    mut ui: ResMut<UiState>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    if project.data.is_none() || vs.root != req.path {
        return;
    }
    if vs.rev != vs.laid_out && task.task.is_none() {
        req.relayout = true;
    }
    let camera = [view.target_center.x, view.target_center.y, view.target_zoom];
    if camera != vs.camera_seen || vs.persist_rev != vs.persist_seen {
        vs.camera_seen = camera;
        vs.persist_seen = vs.persist_rev;
        vs.changed_at = now;
    }
    // The camera alone does not create a sidecar; and not while a restored
    // camera is still waiting for the first layout.
    let folds_dirty = vs.persist_rev != vs.saved_rev;
    let camera_dirty = vs.sidecar && camera != vs.saved_camera;
    if (folds_dirty || camera_dirty) && vs.camera.is_none() && !view.needs_fit && now - vs.changed_at >= SAVE_AFTER {
        vs.saved_rev = vs.persist_rev;
        vs.saved_camera = camera;
        match save(&vs.root, &vs.folds.collapsed, camera) {
            Ok(()) => vs.sidecar = true,
            Err(e) => ui.status = format!("view state not saved: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trip() {
        let collapsed: FxHashSet<u64> = [3, u64::MAX, 0x0123_4567_89ab_cdef].into_iter().collect();
        let text = to_json(&collapsed, [1.5, -20.0, 0.25]);
        assert_eq!(from_json(&text), Some((collapsed, Some([1.5, -20.0, 0.25]))));
        // Unknown versions and broken files are ignored, not half-read.
        assert_eq!(from_json(&text.replace("\"version\": 1", "\"version\": 2")), None);
        assert_eq!(from_json("{"), None);
        assert_eq!(from_json("{\"version\": 1}"), Some((FxHashSet::default(), None)));
    }

    #[test]
    fn folds_survive_a_restart() {
        let root = std::env::temp_dir().join(format!("pcg-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!load(&root).sidecar);
        let collapsed: FxHashSet<u64> = [7, 9].into_iter().collect();
        save(&root, &collapsed, [0.0, 0.0, 1.0]).unwrap();
        let vs = load(&root);
        assert!(vs.sidecar && vs.folds.collapsed == collapsed && vs.camera == Some([0.0, 0.0, 1.0]));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn project(lib: &str) -> Loaded {
        let root = std::env::temp_dir().join(format!("pcg-folds-{}-{}", std::process::id(), lib.len()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), lib).unwrap();
        let p =
            crate::load::build(root.clone(), Default::default(), Default::default(), None, None, Folds::default(), 0);
        let _ = std::fs::remove_dir_all(&root);
        p
    }

    fn find(p: &Loaded, name: &str) -> NodeId {
        (0..p.graph.nodes.len()).map(NodeId::from_idx).find(|&n| p.graph.name(n) == name).expect(name)
    }

    const LIB: &str = "mod a { pub fn x() { super::b::y(); } pub fn x2() {} }\n\
                       mod b { pub fn y() {} pub fn y2() {} }\n\
                       mod c { pub fn z() {} }\n";

    #[test]
    fn fold_relayout_unfold() {
        let p0 = std::sync::Arc::new(project(LIB));
        let (a, x) = (find(&p0, "a"), find(&p0, "x"));
        let mut vs = ViewState::default();
        toggle(&mut vs, &p0, x);
        assert_eq!(vs.rev, 0, "a leaf has nothing to fold");
        toggle(&mut vs, &p0, a);
        assert_eq!((vs.rev, vs.persist_rev), (1, 1));

        // Same graph, new layout; the snapshot knows the view changed.
        let p1 = std::sync::Arc::new(crate::load::relayout(p0.clone(), vs.folds.clone(), vs.rev));
        assert!(std::sync::Arc::ptr_eq(&p0.graph, &p1.graph) && p1.view_changed);
        assert!(p1.diff.as_ref().unwrap().is_empty());
        assert!(p1.layout.collapsed[a.idx()] && p1.layout.shown[x.idx()] == a.0);
        assert!(p1.layout.w[a.idx()] < p0.layout.w[a.idx()]);

        // Jumping to something hidden opens what hides it.
        assert!(reveal(&mut vs, &p1, x) && vs.folds.collapsed.is_empty());
        let p2 = crate::load::relayout(p1.clone(), vs.folds.clone(), vs.rev);
        assert!(!p2.layout.collapsed[a.idx()] && p2.layout.shown[x.idx()] == x.0);
        assert_eq!(p2.layout.w[a.idx()], p0.layout.w[a.idx()]);
        assert!(!reveal(&mut vs, &p2, x));
    }

    #[test]
    fn focus_folds_what_is_unrelated() {
        let p0 = std::sync::Arc::new(project(LIB));
        let [a, b, c, x, y2] = ["a", "b", "c", "x", "y2"].map(|n| find(&p0, n));
        let mut vs = ViewState::default();
        focus(&mut vs, &p0, x);
        assert_eq!((vs.rev, vs.persist_rev), (1, 0), "focus is not persisted");
        let p1 = std::sync::Arc::new(crate::load::relayout(p0.clone(), vs.folds.clone(), vs.rev));
        let l = &p1.layout;
        // x calls b::y: a and b stay open, c is folded away; bystanders are dimmed.
        assert!(!l.collapsed[a.idx()] && !l.collapsed[b.idx()] && l.collapsed[c.idx()]);
        assert!(!p1.dim[x.idx()] && !p1.dim[b.idx()] && p1.dim[y2.idx()] && p1.dim[c.idx()]);
        // Opening a folded container by hand holds until the focus moves.
        toggle(&mut vs, &p1, c);
        let p2 = std::sync::Arc::new(crate::load::relayout(p1.clone(), vs.folds.clone(), vs.rev));
        assert!(!p2.layout.collapsed[c.idx()] && vs.persist_rev == 0);
        focus(&mut vs, &p2, NodeId::NONE);
        let p3 = crate::load::relayout(p2.clone(), vs.folds.clone(), vs.rev);
        assert!(p3.dim.is_empty() && !p3.layout.collapsed.contains(&true));
        assert_eq!(p3.layout.w[0], p0.layout.w[0]);
    }
}
