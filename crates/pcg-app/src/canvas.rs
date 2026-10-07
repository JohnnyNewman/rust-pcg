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
    self, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2,
    epaint::CubicBezierShape,
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

/// Bezier from the right side of `a` to the left side of `b` (or around, when
/// `b` is to the left).
fn curve(a: Rect, b: Rect) -> [Pos2; 4] {
    let p0 = Pos2::new(a.max.x, a.center().y);
    let p3 = Pos2::new(b.min.x, b.center().y);
    let dx = ((p3.x - p0.x).abs() * 0.5).max(40.0);
    [p0, p0 + Vec2::new(dx, 0.0), p3 - Vec2::new(dx, 0.0), p3]
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
    for n in range.clone() {
        let lists = [(g.edges.out.of(NodeId(n)), true), (g.edges.inc.of(NodeId(n)), false)];
        for (list, outgoing) in lists {
            for &e in list {
                let (s, d) = (g.edges.src[e.idx()], g.edges.dst[e.idx()]);
                let other = if outgoing { d } else { s };
                if range.contains(&other.0) {
                    continue;
                }
                let (rs, rd) = (scratch.rep[s.idx()], scratch.rep[d.idx()]);
                if rs == u32::MAX || rd == u32::MAX || rs == rd || !seen.insert((rs, rd, outgoing)) {
                    continue;
                }
                let color = match (g.edges.kind[e.idx()], outgoing) {
                    (EdgeKind::Implements, _) => theme::INTENT,
                    (_, true) => theme::EDGE_OUT,
                    (_, false) => theme::EDGE_IN,
                };
                let pts = curve(srect(view, l, anim, rs), srect(view, l, anim, rd));
                let w = 1.2 + (g.edges.weight[e.idx()] as f32).log2().max(0.0) * 0.6;
                painter.add(Shape::CubicBezier(CubicBezierShape::from_points_stroke(
                    pts,
                    false,
                    Color32::TRANSPARENT,
                    Stroke::new(w, color.gamma_multiply(0.75)),
                )));
                // Flow dots, src → dst.
                let len = pts[0].distance(pts[3]).max(1.0);
                let dots = ((len / 60.0) as usize).clamp(1, 12);
                let phase = (now as f32 * 120.0 / len).fract();
                for k in 0..dots {
                    let t = (phase + k as f32 / dots as f32).fract();
                    painter.circle_filled(bez_point(&pts, t), w + 1.0, color);
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
        let (a, b) = (srect(view, l, anim, rs), srect(view, l, anim, rd));
        painter.line_segment([Pos2::new(a.max.x, a.center().y), Pos2::new(b.min.x, b.center().y)], stroke);
        if scratch.edge_pairs.len() >= MAX_OVERVIEW_EDGES {
            break;
        }
    }
}
