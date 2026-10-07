//! The graph canvas: culled, semantically-zoomed drawing of the nested-box
//! layout plus edges, and pointer interaction.
//!
//! One pre-order sweep per frame. Off-screen or too-small subtrees are skipped
//! in O(1) via `subtree_end`, so cost scales with what is *visible*, not with
//! project size. Semantic zoom is continuous: a container's children fade in as
//! its on-screen width crosses [`CHILD_LOD`], so zooming itself animates the
//! level-of-detail transition.

use crate::anim::Anim;
use crate::model::*;
use crate::theme::{self, smooth};
use bevy_egui::egui::{
    self, Align2, CornerRadius, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2,
};
use pcg_core::{EdgeKind, Graph, NodeId, SummaryState};
use pcg_layout::Layout;

/// On-screen width (px) at which a container's children start / finish fading in.
const CHILD_LOD: (f32, f32) = (70.0, 150.0);
/// Smallest box drawn at all.
const MIN_PX: f32 = 4.0;
/// Cap for the "all edges" overlay.
const MAX_OVERVIEW_EDGES: usize = 40_000;

pub struct CanvasOut {
    pub hovered: NodeId,
    pub clicked: Option<NodeId>,
    pub double_clicked: Option<NodeId>,
    pub clicked_background: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn canvas(
    ui: &mut egui::Ui,
    p: &Loaded,
    anim: Option<&Anim>,
    loaded_at: f64,
    view: &mut View,
    sel: &Selection,
    st: &UiState,
    scratch: &mut CanvasScratch,
    now: f64,
) -> CanvasOut {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
    let rect = resp.rect;
    view.canvas = rect;
    let g = &p.graph;
    let l = &p.layout;
    let n = g.nodes.len();

    // ---- input → view ----------------------------------------------------
    let root_w = l.w.first().copied().unwrap_or(1.0).max(l.h.first().copied().unwrap_or(1.0));
    let min_zoom = (rect.width().min(rect.height()) / root_w) * 0.25;
    if view.needs_fit && n > 0 {
        view.fly_to_rect(Rect::from_min_size(Pos2::new(l.x[0], l.y[0]), Vec2::new(l.w[0], l.h[0])), 1.08);
        // Start zoomed-out a bit, then glide in.
        view.center = view.target_center;
        view.zoom = view.target_zoom * 0.6;
        view.needs_fit = false;
    }
    if resp.dragged() {
        view.pan(resp.drag_delta());
    }
    if let Some(hover) = resp.hover_pos() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
        let factor = (scroll.y * 0.0018).exp() * pinch;
        if (factor - 1.0).abs() > 1e-4 {
            view.zoom_at(hover, factor, min_zoom);
        }
    }

    painter.rect_filled(rect, CornerRadius::ZERO, theme::BG);
    draw_grid(&painter, view, rect);

    // ---- nodes -----------------------------------------------------------
    scratch.rep.clear();
    scratch.rep.resize(n, u32::MAX);
    scratch.child_alpha.clear();
    scratch.child_alpha.resize(n, 0.0);
    scratch.drawn = 0;

    let pointer = resp.hover_pos();
    let mut hovered = NodeId::NONE;
    let since_load = (now - loaded_at) as f32;
    let pulse = 0.5 + 0.5 * ((now - sel.changed_at) as f32 * 4.0).sin();

    let mut tabs: Vec<(Rect, NodeId, f32)> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let parent = g.nodes.parent[i];
        let end = g.nodes.subtree_end[i].idx();
        let (parent_alpha, parent_rep) = if parent.is_none() {
            (1.0, u32::MAX)
        } else {
            (scratch.child_alpha[parent.idx()], scratch.rep[parent.idx()])
        };

        let r = srect(view, l, anim, i as u32);
        // Grow-in after load: deeper levels appear slightly later. Nodes that
        // entered in a reload fade in on their own schedule.
        let appear = smooth(0.0, 0.45, since_load - g.nodes.depth[i] as f32 * 0.07) * anim.map_or(1.0, |a| a.appear(i));
        let alpha = parent_alpha * appear;

        if alpha < 0.01 || !rect.intersects(r) || r.width() < MIN_PX {
            scratch.rep[i..end].fill(parent_rep);
            i = end;
            continue;
        }
        scratch.drawn += 1;
        scratch.rep[i] = i as u32;
        let has_children = end > i + 1;
        let show_children = smooth(CHILD_LOD.0, CHILD_LOD.1, r.width());
        scratch.child_alpha[i] = alpha * show_children;
        let id = NodeId::from_idx(i);
        let r = r.translate(Vec2::new(0.0, (1.0 - appear) * 10.0));

        let mut tab = 0.0;
        draw_node(&painter, g, id, r, alpha, has_children, show_children, st.face, view.zoom, &mut tab);
        if tab > 0.01 {
            tabs.push((r, id, tab));
        }

        if let Some(f) = anim.map(|a| a.flash(i)).filter(|&f| f > 0.01) {
            use pcg_syntax::diff::flag;
            let fl = anim.map_or(0, |a| a.diff.flags[i]);
            let entered = fl & flag::ENTERED != 0;
            let c = if entered { theme::DIFF_ENTER } else { theme::DIFF_CHANGE };
            // Only the changed node itself gets a fill; its ancestors just a ring,
            // so a change deep inside does not tint the whole project.
            if fl & (flag::CHANGED | flag::ENTERED) != 0 {
                painter.rect_filled(r, CornerRadius::same(5), c.gamma_multiply(0.18 * f * alpha));
            }
            painter.rect_stroke(
                r.expand(1.0 + 3.0 * f),
                CornerRadius::same(6),
                Stroke::new(1.0 + 1.5 * f, c.gamma_multiply(0.9 * f)),
                StrokeKind::Outside,
            );
        }

        if id == sel.selected {
            let glow = theme::ACCENT.gamma_multiply(0.35 + 0.45 * pulse);
            painter.rect_stroke(
                r.expand(2.0 + 2.0 * pulse),
                CornerRadius::same(6),
                Stroke::new(2.0, glow),
                StrokeKind::Outside,
            );
        }
        if pointer.is_some_and(|pt| r.contains(pt)) {
            hovered = id;
        }

        if has_children && show_children <= 0.0 {
            scratch.rep[i + 1..end].fill(i as u32);
            i = end;
        } else {
            i += 1;
        }
    }

    // Name tabs for far-away expanded containers (outermost drawn last = on top).
    for &(r, id, a) in tabs.iter().rev() {
        let kind = g.nodes.kind[id.idx()];
        let font = FontId::proportional(if kind == pcg_core::NodeKind::Crate { 13.0 } else { 11.0 });
        let galley = painter.layout_no_wrap(g.name(id).to_string(), font, theme::TEXT.gamma_multiply(a));
        let pos = r.min + Vec2::new(4.0, 3.0);
        let bg = Rect::from_min_size(pos, galley.size() + Vec2::new(10.0, 4.0));
        if bg.width() > r.width() - 6.0 {
            continue;
        }
        painter.rect(
            bg,
            CornerRadius::same(4),
            theme::BG.gamma_multiply(0.85 * a),
            Stroke::new(1.0, theme::kind(kind).gamma_multiply(0.8 * a)),
            StrokeKind::Inside,
        );
        painter.galley(pos + Vec2::new(5.0, 2.0), galley, theme::TEXT);
    }

    if let Some(a) = anim {
        draw_exits(&painter, a, l, view, rect);
    }

    if hovered.is_some() && hovered != sel.selected {
        let r = srect(view, l, anim, hovered.0);
        painter.rect_stroke(
            r,
            CornerRadius::same(4),
            Stroke::new(1.5, theme::TEXT.gamma_multiply(0.7)),
            StrokeKind::Outside,
        );
    }

    // ---- edges -----------------------------------------------------------
    if st.show_all_edges {
        draw_overview_edges(&painter, g, l, anim, view, scratch);
    }
    if sel.selected.is_some() && sel.selected.idx() < n {
        draw_focus_edges(&painter, g, l, anim, view, scratch, sel.selected, now);
    }

    // ---- tooltip -----------------------------------------------------------
    if hovered.is_some() {
        resp.clone().on_hover_ui_at_pointer(|ui| {
            let i = hovered.idx();
            ui.label(egui::RichText::new(g.qualified_name(hovered)).strong());
            let lines = g.nodes.lines[i];
            ui.label(format!(
                "{} · lines {}–{} · {} children",
                g.nodes.kind[i].label(),
                lines.start + 1,
                lines.end,
                g.nodes.children(hovered).count()
            ));
            let st = g.nodes.summary_state[i];
            if st != SummaryState::Missing {
                let (c, label) = theme::summary_state(st);
                ui.colored_label(c, label);
            }
        });
    }

    CanvasOut {
        hovered,
        clicked: (resp.clicked() && hovered.is_some()).then_some(hovered),
        double_clicked: (resp.double_clicked() && hovered.is_some()).then_some(hovered),
        clicked_background: resp.clicked() && hovered.is_none(),
    }
}

fn draw_grid(painter: &Painter, view: &View, rect: Rect) {
    let mut step = 64.0;
    while step * view.zoom < 18.0 {
        step *= 4.0;
    }
    let px = step * view.zoom;
    let fade = smooth(18.0, 48.0, px);
    let c = theme::GRID.gamma_multiply(fade);
    let w0 = view.s2w(rect.min);
    let (x0, y0) = ((w0.x / step).floor() * step, (w0.y / step).floor() * step);
    let cols = (rect.width() / px) as usize + 2;
    let rows = (rect.height() / px) as usize + 2;
    if cols * rows > 20_000 {
        return;
    }
    for r in 0..rows {
        for c_ in 0..cols {
            let s = view.w2s(Pos2::new(x0 + c_ as f32 * step, y0 + r as f32 * step));
            painter.circle_filled(s, 1.0, c);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    painter: &Painter,
    g: &Graph,
    id: NodeId,
    r: Rect,
    alpha: f32,
    has_children: bool,
    show_children: f32,
    face: Face,
    zoom: f32,
    tab: &mut f32,
) {
    let i = id.idx();
    let kind = g.nodes.kind[i];
    let kc = theme::kind(kind);
    let radius = CornerRadius::same(if r.width() > 40.0 { 5 } else { 2 });
    let expanded = has_children && show_children > 0.0;

    // Collapsed containers look "solid"; expanded ones become frames.
    let fill_a = if has_children { 0.05 + 0.20 * (1.0 - show_children) } else { 0.16 };
    painter.rect(
        r,
        radius,
        kc.gamma_multiply(fill_a * alpha),
        Stroke::new(if expanded { 1.0 } else { 1.2 }, kc.gamma_multiply(0.75 * alpha)),
        StrokeKind::Inside,
    );

    // Collapsed container seen from far away: a centered, screen-space label
    // (name + size) so the overview stays readable at any zoom.
    let hs = pcg_layout::header_scale(kind);
    let world_font = (pcg_layout::LayoutParams::default().header * hs * zoom * 0.7).min(14.0 * hs.min(1.6));
    if has_children && show_children < 1.0 && world_font < 9.0 && r.width() > 60.0 && r.height() > 26.0 {
        let a = alpha * (1.0 - show_children).powi(3) * smooth(60.0, 90.0, r.width());
        let fs = (r.width() / 14.0).clamp(10.0, 15.0);
        let clip = painter.with_clip_rect(r.shrink(3.0).intersect(painter.clip_rect()));
        clip.text(
            r.center() - Vec2::new(0.0, fs * 0.45),
            Align2::CENTER_CENTER,
            g.name(id),
            FontId::proportional(fs),
            theme::TEXT.gamma_multiply(a),
        );
        clip.text(
            r.center() + Vec2::new(0.0, fs * 0.75),
            Align2::CENTER_CENTER,
            format!("{} · {} items", kind.label(), g.nodes.descendants(id).len()),
            FontId::proportional(fs * 0.75),
            kc.gamma_multiply(a * 0.9),
        );
    }
    // Expanded container seen from far away: request a name tab, drawn after
    // all boxes so children don't cover it.
    if has_children && show_children > 0.0 && world_font < 9.0 && r.width() > 110.0 {
        *tab = alpha * show_children * smooth(110.0, 160.0, r.width());
    }

    // Header label.
    // The world-space header grows with zoom; the label strip is capped so text
    // stays at the top and leaf bodies get the room.
    let font_px = (pcg_layout::LayoutParams::default().header * hs * zoom * 0.7).min(14.0 * hs.min(1.6));
    let header_px = (pcg_layout::LayoutParams::default().header * hs * zoom).min(font_px * 1.9);
    if font_px < 6.0 || r.width() < 24.0 {
        return;
    }
    let label_a = alpha * smooth(6.0, 9.0, font_px);
    let header = Rect::from_min_size(r.min, Vec2::new(r.width(), header_px.min(r.height())));
    let clip = painter.with_clip_rect(header.shrink(2.0).intersect(painter.clip_rect()));
    let text_pos = Pos2::new(r.min.x + 6.0, header.center().y);
    let kind_rect = clip.text(
        text_pos,
        Align2::LEFT_CENTER,
        kind.label(),
        FontId::monospace(font_px * 0.85),
        kc.gamma_multiply(label_a),
    );
    clip.text(
        Pos2::new(kind_rect.max.x + font_px * 0.4, header.center().y),
        Align2::LEFT_CENTER,
        g.name(id),
        FontId::proportional(font_px),
        theme::TEXT.gamma_multiply(label_a),
    );

    // Badges (top-right): summary freshness, intent.
    let mut bx = r.max.x - 8.0;
    let by = header.center().y;
    let st = g.nodes.summary_state[i];
    if st != SummaryState::Missing {
        let (c, _) = theme::summary_state(st);
        painter.circle_filled(Pos2::new(bx, by), 3.5, c.gamma_multiply(label_a));
        bx -= 10.0;
    }
    if g.nodes.intent[i].is_some() {
        painter.circle_stroke(Pos2::new(bx, by), 3.5, Stroke::new(1.5, theme::INTENT.gamma_multiply(label_a)));
    }

    // Deep zoom: code or summary face inside leaves.
    if !has_children {
        let body = Rect::from_min_max(Pos2::new(r.min.x + 6.0, header.max.y), r.max - Vec2::splat(4.0));
        let line_px = (zoom * 9.0).min(14.0);
        if line_px >= 6.5 && body.height() > line_px {
            let a = alpha * smooth(6.5, 9.0, line_px);
            let clip = painter.with_clip_rect(body.intersect(painter.clip_rect()));
            match face {
                Face::Code => draw_code(&clip, g, id, body, line_px, a),
                Face::Summary => draw_summary(&clip, g, id, body, line_px, a),
            }
        }
    }
}

fn draw_code(painter: &Painter, g: &Graph, id: NodeId, body: Rect, line_px: f32, a: f32) {
    let max_lines = (body.height() / (line_px * 1.25)) as usize;
    let font = FontId::monospace(line_px * 0.9);
    let color = theme::TEXT.gamma_multiply(0.8 * a);
    let mut y = body.min.y + line_px * 0.2;
    for line in g.source(id).lines().filter(|l| !l.trim_start().starts_with("///")).take(max_lines) {
        painter.text(Pos2::new(body.min.x, y), Align2::LEFT_TOP, line, font.clone(), color);
        y += line_px * 1.25;
    }
}

fn draw_summary(painter: &Painter, g: &Graph, id: NodeId, body: Rect, line_px: f32, a: f32) {
    let i = id.idx();
    let (text, color) = if g.nodes.intent[i].is_some() {
        (g.comments.text(g.nodes.intent[i]).to_string(), theme::INTENT)
    } else if g.nodes.summary[i].is_some() {
        (g.comments.text(g.nodes.summary[i]).to_string(), theme::summary_state(g.nodes.summary_state[i]).0)
    } else {
        ("no summary yet".to_string(), theme::TEXT_DIM)
    };
    let galley = painter.layout(text, FontId::proportional(line_px), color.gamma_multiply(a), body.width());
    painter.galley(body.min + Vec2::new(0.0, line_px * 0.2), galley, color);
}

/// Screen rect of a node (tweened while a transition runs).
#[inline]
fn srect(view: &View, l: &Layout, anim: Option<&Anim>, n: u32) -> Rect {
    let i = n as usize;
    let [x, y, w, h] = match anim {
        Some(a) => a.rect(l, i),
        None => [l.x[i], l.y[i], l.w[i], l.h[i]],
    };
    view.world_rect_to_screen(x, y, w, h)
}

/// Removed subtrees: drawn once, as their root box, fading and shrinking at
/// their old place (carried along with the surviving parent).
fn draw_exits(painter: &Painter, a: &Anim, l: &Layout, view: &View, clip: Rect) {
    let e = a.exit();
    if e >= 1.0 {
        return;
    }
    let og = &a.prev.graph;
    for &o in a.diff.exit_roots.iter().take(2000) {
        let [x, y, w, h] = a.exit_rect(l, o);
        let r = view.world_rect_to_screen(x, y, w, h);
        if !clip.intersects(r) || r.width() < MIN_PX {
            continue;
        }
        let r = Rect::from_center_size(r.center(), r.size() * (1.0 - 0.25 * e));
        let alpha = 1.0 - e;
        painter.rect(
            r,
            CornerRadius::same(5),
            theme::DIFF_EXIT.gamma_multiply(0.22 * alpha),
            Stroke::new(1.5, theme::DIFF_EXIT.gamma_multiply(0.9 * alpha)),
            StrokeKind::Inside,
        );
        if r.width() > 40.0 && r.height() > 14.0 {
            let fs = (r.height() * 0.4).clamp(8.0, 13.0);
            painter.with_clip_rect(r.intersect(clip)).text(
                r.center(),
                Align2::CENTER_CENTER,
                og.name(o),
                FontId::proportional(fs),
                theme::TEXT.gamma_multiply(alpha),
            );
        }
    }
}

/// Where an edge leaves `a` and enters `b`: the points, and the outward
/// direction at each. The boxes' facing sides are used, so the edge never has
/// to cross its own endpoints; along the side, each port slides towards the
/// other box, which keeps the edge short and out of the neighbours.
fn ports(a: Rect, b: Rect) -> (Pos2, Vec2, Pos2, Vec2) {
    let (ac, bc) = (a.center(), b.center());
    // `v` moved into `lo..hi`, a little away from the corners.
    let within = |v: f32, lo: f32, hi: f32| {
        let m = ((hi - lo) * 0.5).min(6.0);
        v.clamp(lo + m, hi - m)
    };
    let horizontal = |from: f32, to: f32, d: Vec2| {
        let y0 = within(bc.y, a.min.y, a.max.y);
        (Pos2::new(from, y0), d, Pos2::new(to, within(y0, b.min.y, b.max.y)), -d)
    };
    let vertical = |from: f32, to: f32, d: Vec2| {
        let x0 = within(bc.x, a.min.x, a.max.x);
        (Pos2::new(x0, from), d, Pos2::new(within(x0, b.min.x, b.max.x), to), -d)
    };
    if b.min.x >= a.max.x {
        horizontal(a.max.x, b.min.x, Vec2::X)
    } else if b.max.x <= a.min.x {
        horizontal(a.min.x, b.max.x, -Vec2::X)
    } else if b.min.y >= a.max.y {
        vertical(a.max.y, b.min.y, Vec2::Y)
    } else if b.max.y <= a.min.y {
        vertical(a.min.y, b.max.y, -Vec2::Y)
    } else {
        // Overlapping boxes (an ancestor and its descendant): loop out to the right.
        (Pos2::new(a.max.x, ac.y), Vec2::X, Pos2::new(b.min.x, bc.y), -Vec2::X)
    }
}

/// Outward normal of the side of `r` that `q` (a point on its border) lies on.
fn normal(r: Rect, q: Pos2) -> Vec2 {
    let d = [(q.x - r.min.x).abs(), (q.x - r.max.x).abs(), (q.y - r.min.y).abs(), (q.y - r.max.y).abs()];
    let side = (0..4).min_by(|&a, &b| d[a].total_cmp(&d[b])).unwrap();
    [-Vec2::X, Vec2::X, -Vec2::Y, Vec2::Y][side]
}

/// Continue `path`, which ends on the border of `r`, to `q` on the same
/// border — around the outside of the box, not through it.
fn around(path: &mut Vec<Pos2>, r: Rect, q: Pos2, clearance: f32) {
    let Some(&p) = path.last() else { return };
    if p.distance(q) < 0.5 {
        return;
    }
    let (np, nq) = (normal(r, p), normal(r, q));
    let (p1, q1) = (p + np * clearance, q + nq * clearance);
    path.push(p1);
    if np == -nq {
        // Opposite sides: pass the nearer of the two sides in between.
        let out = r.expand(clearance);
        if np.x != 0.0 {
            let y = if (p.y - out.min.y) + (q.y - out.min.y) < (out.max.y - p.y) + (out.max.y - q.y) {
                out.min.y
            } else {
                out.max.y
            };
            path.extend([Pos2::new(p1.x, y), Pos2::new(q1.x, y)]);
        } else {
            let x = if (p.x - out.min.x) + (q.x - out.min.x) < (out.max.x - p.x) + (out.max.x - q.x) {
                out.min.x
            } else {
                out.max.x
            };
            path.extend([Pos2::new(x, p1.y), Pos2::new(x, q1.y)]);
        }
    } else if np != nq {
        // Neighbouring sides: one corner.
        path.push(if np.x != 0.0 { Pos2::new(p1.x, q1.y) } else { Pos2::new(q1.x, p1.y) });
    }
    path.push(q1);
}

/// The path of the edge between drawn boxes `rs` → `rd`, as a polyline in
/// screen space.
///
/// The edge is followed level by level: `ta` / `tb` are the siblings
/// (children of the lowest common ancestor) holding `rs` / `rd`. From `rs`
/// it takes each container's out-bus up to `ta`, the sibling route from `ta`
/// to `tb`, and the in-buses down to `rd` — the layout's routes, which run
/// between the boxes, and which all edges between two containers share.
/// Where the layout has no route (shelf-packed containers, or boxes still
/// moving), a curve between facing sides fills in.
fn edge_path(g: &Graph, view: &View, l: &Layout, anim: Option<&Anim>, rs: u32, rd: u32, out: &mut Vec<Pos2>) {
    out.clear();
    let nodes = &g.nodes;
    let (mut ta, mut tb) = (rs as usize, rd as usize);
    while nodes.depth[ta] > nodes.depth[tb] {
        ta = nodes.parent[ta].idx();
    }
    while nodes.depth[tb] > nodes.depth[ta] {
        tb = nodes.parent[tb].idx();
    }
    if ta == tb {
        // One end contains the other: nothing to route around.
        (ta, tb) = (rs as usize, rd as usize);
    } else {
        while nodes.parent[ta] != nodes.parent[tb] {
            ta = nodes.parent[ta].idx();
            tb = nodes.parent[tb].idx();
        }
    }
    // Routes belong to the final layout: not while boxes are still gliding to it.
    let settled = anim.is_none_or(|a| a.k >= 1.0);
    let route = |a: usize, b: usize| l.routes.get(NodeId::from_idx(a), NodeId::from_idx(b)).filter(|_| settled);
    let screen = |q: &[f32; 2]| view.w2s(Pos2::new(q[0], q[1]));
    let rect = |n: usize| srect(view, l, anim, n as u32);
    let clearance = (3.0 * view.zoom).clamp(1.5, 12.0);
    // Append a route that starts on the border of `at`, where `out` has arrived.
    let follow = |out: &mut Vec<Pos2>, at: usize, r: &[[f32; 2]]| {
        around(out, rect(at), screen(&r[0]), clearance);
        out.extend(r.iter().map(screen));
    };

    // Up: out-buses from `rs` to `ta`. A level without a route ends the
    // chain; the edge then jumps to the next piece.
    let mut n = rs as usize;
    while n != ta {
        let parent = nodes.parent[n].idx();
        let Some(r) = route(n, parent) else { break };
        follow(out, n, r);
        n = parent;
    }
    let up_done = n == ta;
    // Down, collected bottom-up: in-buses from `tb` to `rd`.
    let mut down: Vec<(usize, &[[f32; 2]])> = Vec::new();
    let mut n = rd as usize;
    while n != tb {
        let parent = nodes.parent[n].idx();
        let Some(r) = route(parent, n) else { break };
        down.push((parent, r));
        n = parent;
    }
    let down_done = n == tb;

    // Across: `ta` → `tb`.
    let (a, b) = (rect(ta), rect(tb));
    match route(ta, tb) {
        Some(r) if up_done || out.is_empty() => {
            if out.is_empty() && ta as u32 != rs {
                out.push(side(rect(rs as usize), normal(a, screen(&r[0]))));
            }
            follow(out, ta, r);
        }
        Some(r) => out.extend(r.iter().map(screen)),
        None => {
            let (p0, d0, p3, d3) = ports(a, b);
            if out.is_empty() && ta as u32 != rs {
                out.push(side(rect(rs as usize), d0));
            }
            let reach = ((p3 - p0).length() * 0.4).clamp(4.0, 160.0);
            let curve = [p0, p0 + d0 * reach, p3 + d3 * reach, p3];
            out.extend((0..=16).map(|i| bez_point(&curve, i as f32 / 16.0)));
        }
    }
    if down_done {
        for &(at, r) in down.iter().rev() {
            follow(out, at, r);
        }
    } else if let Some(&last) = out.last() {
        // Enter `rd` on the side the edge comes from.
        let r = rect(rd as usize);
        let to = r.center() - last;
        let d = if to.x.abs() * r.height() > to.y.abs() * r.width() {
            Vec2::new(to.x.signum(), 0.0)
        } else {
            Vec2::new(0.0, to.y.signum())
        };
        out.push(side(r, -d));
    }
    out.dedup_by(|a, b| a.distance(*b) < 0.25);
}

/// Point on the side of `r` that faces along `dir` (an axis unit vector).
fn side(r: Rect, dir: Vec2) -> Pos2 {
    r.center() + dir * r.size() * 0.5
}

/// `path` with its corners rounded off (radius at most `radius`).
fn rounded(path: &[Pos2], radius: f32, out: &mut Vec<Pos2>) {
    out.clear();
    for (i, &p) in path.iter().enumerate() {
        if i == 0 || i + 1 == path.len() {
            out.push(p);
            continue;
        }
        let (a, b) = (path[i - 1] - p, path[i + 1] - p);
        let r = radius.min(a.length() * 0.5).min(b.length() * 0.5);
        if r < 0.75 {
            out.push(p);
            continue;
        }
        let (p0, p2) = (p + a.normalized() * r, p + b.normalized() * r);
        // Quadratic bezier p0 → p → p2.
        out.extend((0..=4).map(|k| {
            let t = k as f32 / 4.0;
            let (u, v) = (p0 + (p - p0) * t, p + (p2 - p) * t);
            u + (v - u) * t
        }));
    }
}

/// Point at arc length `at` along `path`.
fn along_path(path: &[Pos2], mut at: f32) -> Pos2 {
    for w in path.windows(2) {
        let len = w[0].distance(w[1]);
        if at <= len && len > 0.0 {
            return w[0] + (w[1] - w[0]) * (at / len);
        }
        at -= len;
    }
    path[path.len() - 1]
}

fn bez_point(p: &[Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    Pos2::new(
        w[0] * p[0].x + w[1] * p[1].x + w[2] * p[2].x + w[3] * p[3].x,
        w[0] * p[0].y + w[1] * p[1].y + w[2] * p[2].y + w[3] * p[3].y,
    )
}

/// Edges in/out of the selection (and its descendants), with flowing dots
/// showing direction — a preview of the M5 particle encoding.
#[allow(clippy::too_many_arguments)]
fn draw_focus_edges(
    painter: &Painter,
    g: &Graph,
    l: &Layout,
    anim: Option<&Anim>,
    view: &View,
    scratch: &CanvasScratch,
    sel: NodeId,
    now: f64,
) {
    let range = sel.0..g.nodes.subtree_end[sel.idx()].0;
    let mut seen = rustc_hash::FxHashSet::<(u32, u32, bool)>::default();
    let mut budget = 3000usize;
    let mut path: Vec<Pos2> = Vec::new();
    let mut smooth_path: Vec<Pos2> = Vec::new();
    for n in range.clone() {
        let lists = [(g.edges.out.of(NodeId(n)), true), (g.edges.inc.of(NodeId(n)), false)];
        for (list, outgoing) in lists {
            for &e in list {
                let (s, d) = (g.edges.src[e.idx()], g.edges.dst[e.idx()]);
                let other = if outgoing { d } else { s };
                if range.contains(&other.0) {
                    continue;
                }
                let (mut rs, mut rd) = (scratch.rep[s.idx()], scratch.rep[d.idx()]);
                if rs == u32::MAX || rd == u32::MAX || rs == rd {
                    continue;
                }
                // An end that is off-screen is represented by a box around the
                // other end: aim at where it really is instead.
                if g.nodes.is_ancestor_of(NodeId(rd), NodeId(rs)) {
                    rd = d.0;
                } else if g.nodes.is_ancestor_of(NodeId(rs), NodeId(rd)) {
                    rs = s.0;
                }
                if !seen.insert((rs, rd, outgoing)) {
                    continue;
                }
                let color = match (g.edges.kind[e.idx()], outgoing) {
                    (EdgeKind::Implements, _) => theme::INTENT,
                    (_, true) => theme::EDGE_OUT,
                    (_, false) => theme::EDGE_IN,
                };
                edge_path(g, view, l, anim, rs, rd, &mut path);
                if path.len() < 2 {
                    continue;
                }
                rounded(&path, 7.0, &mut smooth_path);
                let w = 1.2 + (g.edges.weight[e.idx()] as f32).log2().max(0.0) * 0.6;
                painter.add(Shape::line(smooth_path.clone(), Stroke::new(w, color.gamma_multiply(0.75))));
                // Flow dots, src → dst.
                let len: f32 = smooth_path.windows(2).map(|s| s[0].distance(s[1])).sum::<f32>().max(1.0);
                let dots = ((len / 60.0) as usize).clamp(1, 24);
                let phase = (now as f32 * 120.0 / len).fract();
                for k in 0..dots {
                    let t = (phase + k as f32 / dots as f32).fract();
                    painter.circle_filled(along_path(&smooth_path, t * len), w + 1.0, color);
                }
                budget = budget.saturating_sub(1);
                if budget == 0 {
                    return;
                }
            }
        }
    }
}

/// All edges, aggregated onto the currently drawn boxes (deduplicated).
fn draw_overview_edges(
    painter: &Painter,
    g: &Graph,
    l: &Layout,
    anim: Option<&Anim>,
    view: &View,
    scratch: &mut CanvasScratch,
) {
    scratch.edge_pairs.clear();
    let stroke = Stroke::new(1.0, theme::EDGE_OUT.gamma_multiply(0.18));
    for e in 0..g.edges.len() {
        let (rs, rd) = (scratch.rep[g.edges.src[e].idx()], scratch.rep[g.edges.dst[e].idx()]);
        if rs == u32::MAX || rd == u32::MAX || rs == rd || !scratch.edge_pairs.insert((rs, rd)) {
            continue;
        }
        let (p0, _, p3, _) = ports(srect(view, l, anim, rs), srect(view, l, anim, rd));
        painter.line_segment([p0, p3], stroke);
        if scratch.edge_pairs.len() >= MAX_OVERVIEW_EDGES {
            break;
        }
    }
}
