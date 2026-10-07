//! # pcg-layout — hierarchical layout of nested boxes, with edge routes
//!
//! Every node becomes a rectangle inside its parent's, below a header strip.
//! How a container arranges its children depends on the edges *between* them
//! (each edge is lifted to the pair of siblings below its endpoints' lowest
//! common ancestor):
//!
//! * **Layered** (see [`layered`]): children that call each other are put in
//!   columns, callers left of callees, ordered to reduce crossings. Edges that
//!   skip columns get a reserved lane through each column they pass, and the
//!   lane's waypoints are returned as a [route](Routes) — so they run between
//!   the boxes, not across them.
//! * **Shelf**: children without sibling edges (and containers too big or too
//!   lopsided for layering) are packed into rows in source order.
//!
//! Two linear, non-recursive passes over the pre-order node table:
//!
//! 1. **Bottom-up** (descending ids — children before parents): size leaves
//!    from their name/line count, arrange children, size the parent, store
//!    child positions and route points *relative* to the parent.
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
    /// Horizontal gap between the columns of a layered container (room for
    /// the edges to bend).
    pub layer_gap: f32,
    /// Height of the lane a passing edge gets inside a column.
    pub lane: f32,
    /// Containers with more children connected by sibling edges are shelf-packed.
    pub max_layered: usize,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            header: 22.0,
            pad: 8.0,
            gap: 8.0,
            char_w: 7.5,
            min_leaf_w: 90.0,
            max_leaf_w: 320.0,
            aspect: 1.6,
            layer_gap: 36.0,
            lane: 10.0,
            max_layered: 400,
        }
    }
}

/// Waypoints of the edges that could not be drawn as one curve between
/// neighbouring columns: for a pair of sibling nodes, the points (world space)
/// the edge passes between leaving the first and entering the second.
#[derive(Default, Debug, Clone)]
pub struct Routes {
    /// `(from, to, start, end)` into `pts`, sorted by `(from, to)`.
    keys: Vec<(u32, u32, u32, u32)>,
    pts: Vec<[f32; 2]>,
}

impl Routes {
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
    pub fn get(&self, from: NodeId, to: NodeId) -> Option<&[[f32; 2]]> {
        let i = self.keys.binary_search_by_key(&(from.0, to.0), |k| (k.0, k.1)).ok()?;
        Some(&self.pts[self.keys[i].2 as usize..self.keys[i].3 as usize])
    }
}

#[derive(Default, Debug, Clone)]
pub struct Layout {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub w: Vec<f32>,
    pub h: Vec<f32>,
    pub routes: Routes,
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

/// Lift every edge to its lowest common ancestor: `(container, a, b)` where
/// `a` / `b` are the container's children holding the edge's ends. Sorted,
/// deduplicated; edges between a node and its own descendant are dropped.
fn sibling_edges(nodes: &NodeTable, src: &[NodeId], dst: &[NodeId]) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::with_capacity(src.len());
    for (&s, &d) in src.iter().zip(dst) {
        let (mut a, mut b) = (s.idx(), d.idx());
        while nodes.depth[a] > nodes.depth[b] {
            a = nodes.parent[a].idx();
        }
        while nodes.depth[b] > nodes.depth[a] {
            b = nodes.parent[b].idx();
        }
        if a == b {
            continue;
        }
        while nodes.parent[a] != nodes.parent[b] {
            a = nodes.parent[a].idx();
            b = nodes.parent[b].idx();
        }
        out.push((nodes.parent[a].0, a as u32, b as u32));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Shelf-pack `kids` (sizes in `l`) into rows of about `row_w`, writing their
/// positions relative to `(0, y0)`. Returns the block's `(width, height)`.
fn shelf(l: &mut Layout, kids: &[NodeId], row_w: f32, y0: f32, gap: f32) -> (f32, f32) {
    let (mut cx, mut cy, mut row_h, mut max_x) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &c in kids {
        let (cw, ch) = (l.w[c.idx()], l.h[c.idx()]);
        if cx > 0.0 && cx + cw > row_w {
            cy += row_h + gap;
            cx = 0.0;
            row_h = 0.0;
        }
        l.x[c.idx()] = cx;
        l.y[c.idx()] = y0 + cy;
        cx += cw + gap;
        row_h = row_h.max(ch);
        max_x = max_x.max(cx - gap);
    }
    (max_x, cy + row_h)
}

/// Scratch space of [`layered`], reused across containers.
#[derive(Default)]
struct Scratch {
    /// Global node id → index into the current container's connected children.
    slot: Vec<u32>,
    /// Pending routes: `(from, to, container, start, end)`, points relative
    /// to the container.
    routes: Vec<(u32, u32, u32, u32, u32)>,
    pts: Vec<[f32; 2]>,
}

/// Layered (Sugiyama-style) arrangement of the children `conn`, which are
/// connected by the sibling edges `es` (`(_, from, to)`, global ids).
///
/// 1. Break cycles: a depth-first search in source order reverses back edges.
/// 2. Layers: longest path from the sources, so every edge points rightwards.
/// 3. An edge spanning several layers gets a dummy vertex (a lane) in each
///    layer it crosses.
/// 4. Order each layer by the barycentre of its neighbours, sweeping right
///    and left a few times.
/// 5. Columns are stacked top to bottom and centred on the tallest one.
///
/// Writes the children's positions relative to `(0, 0)`, records the lanes as
/// routes, and returns the block's `(width, height)`.
fn layered(
    l: &mut Layout,
    sc: &mut Scratch,
    container: u32,
    conn: &[NodeId],
    es: &[(u32, u32, u32)],
    p: &LayoutParams,
) -> (f32, f32) {
    let k = conn.len();
    for (s, c) in conn.iter().enumerate() {
        sc.slot[c.idx()] = s as u32;
    }
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); k];
    for &(_, a, b) in es {
        out[sc.slot[a as usize] as usize].push(sc.slot[b as usize]);
    }

    // 1. Iterative DFS: `state` 0 = new, 1 = on the stack, 2 = done.
    let mut state = vec![0u8; k];
    let mut post: Vec<u32> = Vec::with_capacity(k);
    // DAG edges `(from, to, reversed)`.
    let mut dag: Vec<(u32, u32, bool)> = Vec::with_capacity(es.len());
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for root in 0..k as u32 {
        if state[root as usize] != 0 {
            continue;
        }
        state[root as usize] = 1;
        stack.push((root, 0));
        while let Some(&mut (v, ref mut next)) = stack.last_mut() {
            if let Some(&w) = out[v as usize].get(*next) {
                *next += 1;
                match state[w as usize] {
                    0 => {
                        dag.push((v, w, false));
                        state[w as usize] = 1;
                        stack.push((w, 0));
                    }
                    1 => dag.push((w, v, true)), // back edge: lay it out reversed
                    _ => dag.push((v, w, false)),
                }
            } else {
                state[v as usize] = 2;
                post.push(v);
                stack.pop();
            }
        }
    }

    // 2. Longest-path layers, relaxing edges in topological order of their tail.
    let mut rank = vec![0u32; k];
    for (r, &v) in post.iter().rev().enumerate() {
        rank[v as usize] = r as u32;
    }
    dag.sort_unstable_by_key(|e| rank[e.0 as usize]);
    let mut layer = vec![0u32; k];
    for &(a, b, _) in &dag {
        layer[b as usize] = layer[b as usize].max(layer[a as usize] + 1);
    }
    // Callers nobody calls would all sit in the first column: pull each one
    // right, next to its nearest callee (shorter edges, fewer lanes).
    let mut has_in = vec![false; k];
    let mut nearest = vec![u32::MAX; k];
    for &(a, b, _) in &dag {
        has_in[b as usize] = true;
        nearest[a as usize] = nearest[a as usize].min(layer[b as usize]);
    }
    for v in 0..k {
        if !has_in[v] && nearest[v] != u32::MAX {
            layer[v] = nearest[v] - 1;
        }
    }
    let first = layer.iter().copied().min().unwrap_or(0);
    layer.iter_mut().for_each(|x| *x -= first);
    let n_layers = layer.iter().max().map_or(0, |m| *m as usize + 1);

    // 3. Vertices `0..k` are children, `k..` are lane dummies.
    let mut layers: Vec<Vec<u32>> = vec![Vec::new(); n_layers];
    for v in 0..k {
        layers[layer[v] as usize].push(v as u32);
    }
    let mut left: Vec<Vec<u32>> = vec![Vec::new(); k];
    let mut right: Vec<Vec<u32>> = vec![Vec::new(); k];
    // Per long edge: `(from, to, reversed, first dummy, dummy count)`.
    let mut long: Vec<(u32, u32, bool, u32, u32)> = Vec::new();
    for &(a, b, rev) in &dag {
        let (la, lb) = (layer[a as usize], layer[b as usize]);
        let first = left.len() as u32;
        let mut prev = a;
        for at in la + 1..lb {
            let d = left.len() as u32;
            left.push(vec![prev]);
            right.push(Vec::new());
            right[prev as usize].push(d);
            layers[at as usize].push(d);
            prev = d;
        }
        right[prev as usize].push(b);
        left[b as usize].push(prev);
        if lb - la > 1 {
            long.push((a, b, rev, first, lb - la - 1));
        }
    }

    // 4. Barycentre sweeps.
    let mut pos = vec![0f32; left.len()];
    let place = |layer: &[u32], pos: &mut [f32]| {
        for (i, &v) in layer.iter().enumerate() {
            pos[v as usize] = i as f32;
        }
    };
    layers.iter().for_each(|ly| place(ly, &mut pos));
    for sweep in 0..4 {
        let order: Box<dyn Iterator<Item = usize>> =
            if sweep % 2 == 0 { Box::new(1..n_layers) } else { Box::new((0..n_layers.saturating_sub(1)).rev()) };
        let nb = if sweep % 2 == 0 { &left } else { &right };
        for at in order {
            let mut keyed: Vec<(f32, u32)> = layers[at]
                .iter()
                .map(|&v| {
                    let ns = &nb[v as usize];
                    let b = if ns.is_empty() {
                        pos[v as usize]
                    } else {
                        ns.iter().map(|&u| pos[u as usize]).sum::<f32>() / ns.len() as f32
                    };
                    (b, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
            layers[at].clear();
            layers[at].extend(keyed.iter().map(|e| e.1));
            place(&layers[at], &mut pos);
        }
    }

    // 5. Coordinates.
    let size = |l: &Layout, v: u32| match conn.get(v as usize) {
        Some(c) => (l.w[c.idx()], l.h[c.idx()]),
        None => (0.0, p.lane),
    };
    let col_h: Vec<f32> = layers
        .iter()
        .map(|ly| ly.iter().map(|&v| size(l, v).1).sum::<f32>() + p.gap * ly.len().saturating_sub(1) as f32)
        .collect();
    let total_h = col_h.iter().copied().fold(0.0, f32::max);
    // Lane of each dummy: `[x0, x1, y]`.
    let mut lane = vec![[0f32; 3]; left.len() - k];
    let mut x = 0.0f32;
    for (at, ly) in layers.iter().enumerate() {
        let col_w = ly.iter().map(|&v| size(l, v).0).fold(0.0, f32::max);
        let mut y = (total_h - col_h[at]) * 0.5;
        for &v in ly {
            let (w, h) = size(l, v);
            match conn.get(v as usize) {
                Some(c) => {
                    l.x[c.idx()] = x + (col_w - w) * 0.5;
                    l.y[c.idx()] = y;
                }
                None => lane[v as usize - k] = [x, x + col_w, y + h * 0.5],
            }
            y += h + p.gap;
        }
        x += col_w + p.layer_gap;
    }
    for (a, b, rev, first, count) in long {
        let start = sc.pts.len() as u32;
        for d in first..first + count {
            let [x0, x1, y] = lane[d as usize - k];
            sc.pts.extend([[x0, y], [x1, y]]);
        }
        let (mut from, mut to) = (conn[a as usize].0, conn[b as usize].0);
        if rev {
            // Laid out right-to-left: the real edge runs the lanes backwards.
            sc.pts[start as usize..].reverse();
            std::mem::swap(&mut from, &mut to);
        }
        sc.routes.push((from, to, container, start, sc.pts.len() as u32));
    }
    (x - p.layer_gap, total_h)
}

/// `name_len[i]`: display name length in chars (callers resolve names; this
/// crate only sees the node table). `src` / `dst`: the graph's edges.
pub fn layout(nodes: &NodeTable, name_len: &[u32], src: &[NodeId], dst: &[NodeId], p: &LayoutParams) -> Layout {
    let n = nodes.len();
    let mut l =
        Layout { x: vec![0.0; n], y: vec![0.0; n], w: vec![0.0; n], h: vec![0.0; n], routes: Routes::default() };
    let edges = sibling_edges(nodes, src, dst);
    let mut sc = Scratch { slot: vec![u32::MAX; n], ..Default::default() };
    let mut kids: Vec<NodeId> = Vec::new();
    let mut conn: Vec<NodeId> = Vec::new();
    let mut loose: Vec<NodeId> = Vec::new();
    let mut edge_end = edges.len();

    for i in (0..n).rev() {
        let id = NodeId::from_idx(i);
        let hs = header_scale(nodes.kind[i]);
        let header = p.header * hs;
        let label_w = (name_len[i] as f32 + nodes.kind[i].label().len() as f32 + 2.0) * p.char_w * hs + 2.0 * p.pad;
        // This container's sibling edges: containers are visited in descending
        // order, so they are the tail of what is left.
        let edge_start = edges[..edge_end].partition_point(|e| (e.0 as usize) < i);
        let es = &edges[edge_start..edge_end];
        edge_end = edge_start;

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

        // Children with sibling edges are layered, the rest shelf-packed below.
        conn.clear();
        loose.clear();
        if !es.is_empty() {
            for &(_, a, b) in es {
                sc.slot[a as usize] = 0;
                sc.slot[b as usize] = 0;
            }
            let (c, o): (Vec<NodeId>, Vec<NodeId>) = kids.iter().partition(|c| sc.slot[c.idx()] == 0);
            (conn, loose) = (c, o);
        }
        let (routes_mark, pts_mark) = (sc.routes.len(), sc.pts.len());
        let mut top = (0.0f32, 0.0f32);
        if !conn.is_empty() && conn.len() <= p.max_layered {
            top = layered(&mut l, &mut sc, i as u32, &conn, es, p);
            // Too lopsided (one caller of fifty, a long chain): shelf instead.
            if top.1 > 4.0 * top.0.max(p.max_leaf_w) || top.0 > 12.0 * top.1 {
                sc.routes.truncate(routes_mark);
                sc.pts.truncate(pts_mark);
                top = (0.0, 0.0);
            }
        }
        for c in &conn {
            sc.slot[c.idx()] = u32::MAX;
        }
        let rest: &[NodeId] = if top.0 > 0.0 { &loose } else { &kids };
        let area: f32 = rest.iter().map(|c| (l.w[c.idx()] + p.gap) * (l.h[c.idx()] + p.gap)).sum();
        let widest = rest.iter().map(|c| l.w[c.idx()]).fold(0.0, f32::max);
        let row_w = (area * p.aspect).sqrt().max(widest).max(label_w - 2.0 * p.pad).max(top.0);
        let y0 = if top.1 > 0.0 && !rest.is_empty() { top.1 + p.gap * 2.0 } else { top.1 };
        let below = shelf(&mut l, rest, row_w, y0, p.gap);

        // Move everything below the header, inside the padding.
        let (ox, oy) = (p.pad, header + p.pad);
        for &c in &kids {
            l.x[c.idx()] += ox;
            l.y[c.idx()] += oy;
        }
        for pt in &mut sc.pts[pts_mark..] {
            *pt = [pt[0] + ox, pt[1] + oy];
        }
        l.w[i] = top.0.max(below.0).max(label_w - 2.0 * p.pad) + 2.0 * p.pad;
        l.h[i] = oy + y0 + below.1 + p.pad;
    }

    for i in 0..n {
        let par = nodes.parent[i];
        if par.is_some() {
            l.x[i] += l.x[par.idx()];
            l.y[i] += l.y[par.idx()];
        }
    }
    // Routes: container-relative → world, sorted for lookup.
    sc.routes.sort_unstable_by_key(|r| (r.0, r.1));
    for &(from, to, c, start, end) in &sc.routes {
        let (cx, cy) = (l.x[c as usize], l.y[c as usize]);
        let at = l.routes.pts.len() as u32;
        l.routes.pts.extend(sc.pts[start as usize..end as usize].iter().map(|q| [q[0] + cx, q[1] + cy]));
        l.routes.keys.push((from, to, at, at + end - start));
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

    /// A workspace with one module of `fns` functions; returns the fn ids.
    fn module(fns: u32) -> (NodeTable, NodeId, Vec<NodeId>) {
        let mut t = NodeTable::default();
        let ws = t.open(NodeId::NONE, nn(NodeKind::Workspace, 0));
        let m = t.open(ws, nn(NodeKind::FileModule, 100));
        let f = (0..fns)
            .map(|k| {
                let f = t.open(m, nn(NodeKind::Fn, k * 3));
                t.close(f);
                f
            })
            .collect();
        t.close(m);
        t.close(ws);
        (t, m, f)
    }

    fn run(t: &NodeTable, edges: &[(NodeId, NodeId)]) -> Layout {
        let (src, dst): (Vec<_>, Vec<_>) = edges.iter().copied().unzip();
        layout(t, &vec![6u32; t.len()], &src, &dst, &LayoutParams::default())
    }

    fn rect(l: &Layout, n: NodeId) -> [f32; 4] {
        let i = n.idx();
        [l.x[i], l.y[i], l.x[i] + l.w[i], l.y[i] + l.h[i]]
    }

    fn assert_nested_and_disjoint(t: &NodeTable, l: &Layout, m: NodeId) {
        for i in 1..t.len() {
            let p = t.parent[i].idx();
            assert!(l.x[i] >= l.x[p] && l.y[i] >= l.y[p]);
            assert!(l.x[i] + l.w[i] <= l.x[p] + l.w[p] + 1e-3);
            assert!(l.y[i] + l.h[i] <= l.y[p] + l.h[p] + 1e-3);
        }
        let kids: Vec<_> = t.children(m).collect();
        for (a, &i) in kids.iter().enumerate() {
            for &j in &kids[a + 1..] {
                let (p, q) = (rect(l, i), rect(l, j));
                let overlap = p[0] < q[2] && q[0] < p[2] && p[1] < q[3] && q[1] < p[3];
                assert!(!overlap, "{i:?} overlaps {j:?}");
            }
        }
    }

    #[test]
    fn children_inside_parents_and_disjoint() {
        let (t, m, f) = module(20);
        assert_nested_and_disjoint(&t, &run(&t, &[]), m);
        // Same with a mix of layered and loose children, and a cycle.
        let e = [(f[0], f[1]), (f[1], f[2]), (f[2], f[0]), (f[0], f[5]), (f[7], f[5]), (f[9], f[9])];
        let l = run(&t, &e);
        assert_nested_and_disjoint(&t, &l, m);
    }

    #[test]
    fn callers_left_of_callees() {
        let (t, _, f) = module(6);
        // f3 → f0 → f1 → f2, and f4 → f1; f5 is loose.
        let l = run(&t, &[(f[3], f[0]), (f[0], f[1]), (f[1], f[2]), (f[4], f[1])]);
        let right = |n: NodeId| rect(&l, n)[2];
        let left = |n: NodeId| rect(&l, n)[0];
        assert!(right(f[3]) < left(f[0]) && right(f[0]) < left(f[1]) && right(f[1]) < left(f[2]));
        assert!(right(f[4]) < left(f[1]));
        // The loose child sits below the layered block.
        assert!(rect(&l, f[5])[1] > (0..5).map(|k| rect(&l, f[k])[3]).fold(0.0, f32::max));
        assert!(l.routes.is_empty(), "all edges join neighbouring columns");
    }

    #[test]
    fn long_edges_get_lanes_around_boxes() {
        let (t, _, f) = module(4);
        // Chain f0 → f1 → f2 → f3 plus the shortcut f0 → f3, and a back edge f3 → f1… which
        // closes a cycle and is laid out reversed.
        let l = run(&t, &[(f[0], f[1]), (f[1], f[2]), (f[2], f[3]), (f[0], f[3]), (f[3], f[1])]);
        let route = l.routes.get(f[0], f[3]).expect("shortcut is routed");
        assert_eq!(route.len(), 4, "two columns crossed, a lane (2 points) in each");
        assert!(route.windows(2).all(|w| w[0][0] <= w[1][0]), "left to right");
        for q in route {
            for &b in &f {
                let r = rect(&l, b);
                let inside = q[0] > r[0] && q[0] < r[2] && q[1] > r[1] && q[1] < r[3];
                assert!(!inside, "waypoint {q:?} inside {b:?}");
            }
        }
        // The back edge runs its lane right to left.
        let back = l.routes.get(f[3], f[1]).expect("back edge is routed");
        assert!(back.windows(2).all(|w| w[0][0] >= w[1][0]));
        assert!(l.routes.get(f[1], f[3]).is_none());
        assert!(l.routes.get(f[0], f[1]).is_none());
    }

    #[test]
    fn edges_are_lifted_to_siblings() {
        // ws { a { x }, b { y } } with x → y: a is laid out left of b.
        let mut t = NodeTable::default();
        let ws = t.open(NodeId::NONE, nn(NodeKind::Workspace, 0));
        let b = t.open(ws, nn(NodeKind::FileModule, 1));
        let y = t.open(b, nn(NodeKind::Fn, 1));
        t.close(y);
        t.close(b);
        let a = t.open(ws, nn(NodeKind::FileModule, 1));
        let x = t.open(a, nn(NodeKind::Fn, 1));
        t.close(x);
        t.close(a);
        t.close(ws);
        let l = run(&t, &[(x, y), (ws, x)]);
        assert!(rect(&l, a)[2] < rect(&l, b)[0]);
    }

    #[test]
    fn lopsided_or_huge_containers_fall_back_to_the_shelf() {
        let (t, m, f) = module(60);
        let fan: Vec<_> = f[1..].iter().map(|&c| (f[0], c)).collect();
        let l = run(&t, &fan);
        assert_nested_and_disjoint(&t, &l, m);
        let [x0, y0, x1, y1] = rect(&l, m);
        assert!((y1 - y0) < 3.0 * (x1 - x0), "a 59-high column would be far taller");
    }
}
