//! # pcg-layout — nested-box layout (M1 placeholder for the M2 layout engine)
//!
//! Every node becomes a rectangle; children are shelf-packed inside their
//! parent below a header strip. Two linear, non-recursive passes over the
//! pre-order node table:
//!
//! 1. **Bottom-up** (descending ids — children before parents): size leaves
//!    from their name/line count, pack children into rows, size the parent,
//!    store child positions *relative* to the parent.
//! 2. **Top-down** (ascending ids — parents before children): convert to
//!    absolute positions.
//!
//! Output is SoA (`x, y, w, h`) so the renderer can cull with straight loops.

use pcg_core::{NodeId, NodeKind, NodeTable};

#[derive(Clone, Copy, Debug)]
pub struct LayoutParams {
    pub header: f32,
    pub pad: f32,
    pub gap: f32,
    pub char_w: f32,
    pub min_leaf_w: f32,
    pub max_leaf_w: f32,
    /// Target width/height ratio of packed containers.
    pub aspect: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self { header: 22.0, pad: 8.0, gap: 8.0, char_w: 7.5, min_leaf_w: 90.0, max_leaf_w: 320.0, aspect: 1.6 }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Layout {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub w: Vec<f32>,
    pub h: Vec<f32>,
}

impl Layout {
    pub fn len(&self) -> usize {
        self.x.len()
    }
    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }
    #[inline]
    pub fn contains(&self, n: NodeId, px: f32, py: f32) -> bool {
        let i = n.idx();
        px >= self.x[i] && py >= self.y[i] && px <= self.x[i] + self.w[i] && py <= self.y[i] + self.h[i]
    }
}

/// Header scale per kind: higher levels get taller headers so their labels
/// stay readable when zoomed out (semantic zoom).
pub fn header_scale(k: NodeKind) -> f32 {
    match k {
        NodeKind::Workspace => 3.0,
        NodeKind::Crate => 2.4,
        NodeKind::FileModule | NodeKind::InlineModule => 1.6,
        NodeKind::Trait | NodeKind::Impl => 1.1,
        _ => 1.0,
    }
}

/// `name_len[i]`: display name length in chars (callers resolve names; this
/// crate only sees the node table).
pub fn layout(nodes: &NodeTable, name_len: &[u32], p: &LayoutParams) -> Layout {
    let n = nodes.len();
    let mut l = Layout { x: vec![0.0; n], y: vec![0.0; n], w: vec![0.0; n], h: vec![0.0; n] };
    let mut kids: Vec<NodeId> = Vec::new();

    for i in (0..n).rev() {
        let id = NodeId::from_idx(i);
        let hs = header_scale(nodes.kind[i]);
        let header = p.header * hs;
        let label_w = (name_len[i] as f32 + nodes.kind[i].label().len() as f32 + 2.0) * p.char_w * hs + 2.0 * p.pad;
        kids.clear();
        kids.extend(nodes.children(id));
        // Items keep source order; sub-modules follow, tallest first (less waste).
        kids.sort_by(|a, b| {
            let big = |k: NodeKind| k.is_module() || k == NodeKind::Crate;
            let (ma, mb) = (big(nodes.kind[a.idx()]), big(nodes.kind[b.idx()]));
            ma.cmp(&mb)
                .then_with(|| if ma && mb { l.h[b.idx()].total_cmp(&l.h[a.idx()]) } else { std::cmp::Ordering::Equal })
        });
        if kids.is_empty() {
            let lines = nodes.lines[i].len().max(1) as f32;
            let w = label_w.clamp(p.min_leaf_w, p.max_leaf_w);
            let body = match nodes.kind[i] {
                NodeKind::Fn => lines.sqrt() * 6.0,
                k if k.is_container() => 0.0,
                _ => lines.sqrt() * 3.0,
            };
            l.w[i] = w;
            l.h[i] = header + body.min(240.0) + 4.0;
            continue;
        }
        // Shelf packing of children, in source order.
        let area: f32 = kids.iter().map(|c| (l.w[c.idx()] + p.gap) * (l.h[c.idx()] + p.gap)).sum();
        let widest = kids.iter().map(|c| l.w[c.idx()]).fold(0.0, f32::max);
        let row_w = (area * p.aspect).sqrt().max(widest).max(label_w - 2.0 * p.pad);
        let (mut cx, mut cy, mut row_h, mut max_x) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for &c in &kids {
            let (cw, ch) = (l.w[c.idx()], l.h[c.idx()]);
            if cx > 0.0 && cx + cw > row_w {
                cy += row_h + p.gap;
                cx = 0.0;
                row_h = 0.0;
            }
            l.x[c.idx()] = p.pad + cx;
            l.y[c.idx()] = header + p.pad + cy;
            cx += cw + p.gap;
            row_h = row_h.max(ch);
            max_x = max_x.max(cx - p.gap);
        }
        l.w[i] = max_x.max(label_w - 2.0 * p.pad) + 2.0 * p.pad;
        l.h[i] = header + p.pad + cy + row_h + p.pad;
    }

    for i in 0..n {
        let par = nodes.parent[i];
        if par.is_some() {
            l.x[i] += l.x[par.idx()];
            l.y[i] += l.y[par.idx()];
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcg_core::{FileId, NewNode, Span, Sym};

    fn nn(kind: NodeKind, lines: u32) -> NewNode {
        NewNode {
            kind,
            name: Sym::EMPTY,
            file: FileId::NONE,
            bytes: Span::default(),
            lines: Span::new(0, lines),
            content_hash: 0,
        }
    }

    #[test]
    fn children_inside_parents_and_disjoint() {
        let mut t = NodeTable::default();
        let ws = t.open(NodeId::NONE, nn(NodeKind::Workspace, 0));
        let m = t.open(ws, nn(NodeKind::FileModule, 100));
        for k in 0..20 {
            let f = t.open(m, nn(NodeKind::Fn, k * 3));
            t.close(f);
        }
        t.close(m);
        t.close(ws);
        let names = vec![6u32; t.len()];
        let l = layout(&t, &names, &LayoutParams::default());
        for i in 1..t.len() {
            let p = t.parent[i].idx();
            assert!(l.x[i] >= l.x[p] && l.y[i] >= l.y[p]);
            assert!(l.x[i] + l.w[i] <= l.x[p] + l.w[p] + 1e-3);
            assert!(l.y[i] + l.h[i] <= l.y[p] + l.h[p] + 1e-3);
        }
        let kids: Vec<_> = t.children(m).collect();
        for (a, &i) in kids.iter().enumerate() {
            for &j in &kids[a + 1..] {
                let (i, j) = (i.idx(), j.idx());
                let overlap = l.x[i] < l.x[j] + l.w[j]
                    && l.x[j] < l.x[i] + l.w[i]
                    && l.y[i] < l.y[j] + l.h[j]
                    && l.y[j] < l.y[i] + l.h[i];
                assert!(!overlap, "{i} overlaps {j}");
            }
        }
    }
}
