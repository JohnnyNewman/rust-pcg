//! # pcg-layout — hierarchical layout of nested boxes, with edge routes
//!
//! Every node becomes a rectangle inside its parent's, below a header strip.
//! How a container arranges its children depends on the edges *between* them
//! (each edge is lifted to the pair of siblings below its endpoints' lowest
//! common ancestor):
//!
//! * **Layered** (see [`layered`]): children that call each other are put in
//!   layers, callers before callees, ordered to reduce crossings. Layers run
//!   left-to-right or top-to-bottom and wrap into several bands, whichever
//!   gives the best-shaped box. Every edge gets a [route](Routes): an
//!   orthogonal polyline through the gutters between layers, along a lane
//!   reserved in each layer it skips, and around the band ends — so edges
//!   run between the boxes, not across them.
//! * **Ports**: edges that leave or enter a layered container are collected
//!   on a bus (one lane per layer) that ends in a port on the container's
//!   side; the edge continues from that very point one level up. So an edge
//!   between distant nodes is routed level by level, bundled with its like.
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
    /// Target width/height ratio of containers.
    pub aspect: f32,
    /// Gap between the layers of a layered container (room for the edges
    /// to bend).
    pub layer_gap: f32,
    /// Thickness of the lane a passing edge gets inside a layer.
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

/// How edges run inside layered containers. A route belongs to a pair of nodes:
///
/// * two siblings `(a, b)` — the edge from `a`'s side to `b`'s side;
/// * `(child, parent)` — from `child` to the parent's out-port on its side;
/// * `(parent, child)` — from the parent's in-port to `child`.
///
/// It is a polyline in world space with axis-parallel segments, clear of the
/// container's children. Routes of consecutive levels meet where the ports
/// line up, so they chain into one path.
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
    /// The polyline (at least two points).
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

/// What an edge means to one container: `(container, kind, a, b)`.
type Local = (u32, u8, u32, u32);
/// Edge between the container's children `a` → `b`.
const SIBLING: u8 = 0;
/// Child `a` holds the tail of an edge that leaves the container.
const LEAVES: u8 = 1;
/// Child `a` holds the head of an edge that enters the container.
const ENTERS: u8 = 2;

/// Lift every edge to its lowest common ancestor `c`: a [`SIBLING`] edge for
/// `c` between its children holding the two ends, and [`LEAVES`] / [`ENTERS`]
/// marks for every container the edge crosses on its way up / down. Sorted,
/// deduplicated; edges between a node and its own descendant are dropped.
fn local_edges(nodes: &NodeTable, src: &[NodeId], dst: &[NodeId]) -> Vec<Local> {
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
        out.push((nodes.parent[a].0, SIBLING, a as u32, b as u32));
        for (end, top, kind) in [(s.idx(), a, LEAVES), (d.idx(), b, ENTERS)] {
            let mut n = end;
            while n != top {
                out.push((nodes.parent[n].0, kind, n as u32, 0));
                n = nodes.parent[n].idx();
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Shelf-pack `kids` (sizes in `l`) into rows of about `row_w`, writing their
/// positions relative to `(x0, y0)`. Returns the block's `(width, height)`.
fn shelf(l: &mut Layout, kids: &[NodeId], row_w: f32, (x0, y0): (f32, f32), gap: f32) -> (f32, f32) {
    let (mut cx, mut cy, mut row_h, mut max_x) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &c in kids {
        let (cw, ch) = (l.w[c.idx()], l.h[c.idx()]);
        if cx > 0.0 && cx + cw > row_w {
            cy += row_h + gap;
            cx = 0.0;
            row_h = 0.0;
        }
        l.x[c.idx()] = x0 + cx;
        l.y[c.idx()] = y0 + cy;
        cx += cw + gap;
        row_h = row_h.max(ch);
        max_x = max_x.max(cx - gap);
    }
    (max_x, cy + row_h)
}

/// Per-node results of the bottom-up pass that parents build on, and scratch
/// space of [`layered`], reused across containers.
struct Scratch {
    /// Global node id → index into the current container's connected children.
    slot: Vec<u32>,
    /// 0 = not layered, 1 = layers run along x, 2 = along y.
    orient: Vec<u8>,
    /// Where the node's in / out port sits on its side: offset from the
    /// node's origin across its layer direction (`NAN` = no port).
    port_in: Vec<f32>,
    port_out: Vec<f32>,
    /// Pending routes: `(from, to, container, start, end, kind)`, points
    /// relative to the container; `kind` as in [`Local`].
    routes: Vec<(u32, u32, u32, u32, u32, u8)>,
    pts: Vec<[f32; 2]>,
}

/// The layered graph under construction: real children first, then lane
/// dummies and the two ports.
struct Net {
    layers: Vec<Vec<u32>>,
    left: Vec<Vec<u32>>,
    right: Vec<Vec<u32>>,
}

impl Net {
    fn add(&mut self, layer: usize) -> u32 {
        let v = self.left.len() as u32;
        self.left.push(Vec::new());
        self.right.push(Vec::new());
        self.layers[layer].push(v);
        v
    }
    fn link(&mut self, a: u32, b: u32) {
        self.right[a as usize].push(b);
        self.left[b as usize].push(a);
    }
}

/// Result of [`layered`]: the block's size and where its ports are.
struct Block {
    w: f32,
    h: f32,
    orient: u8,
    port_in: f32,
    port_out: f32,
}

/// Layered (Sugiyama-style) arrangement of the children `conn`, which are
/// connected by the sibling edges in `es`.
///
/// 1. Break cycles: a depth-first search in source order reverses back edges.
/// 2. Layers: longest path from the sources, so every edge points forwards.
/// 3. An edge spanning several layers gets a dummy vertex (a lane) in each
///    layer it crosses. Edges leaving / entering the container share a bus:
///    one lane per layer, ending in a port vertex in an extra last / first
///    layer.
/// 4. Order each layer by the barycentre of its neighbours, sweeping forth
///    and back a few times.
/// 5. Layers are stacked along x or y and wrapped into bands — whichever
///    brings the block closest to the target aspect — and centred in their
///    band.
/// 6. Routes: each edge as an orthogonal polyline through gutters and lanes;
///    from the end of one band to the start of the next it runs around
///    through the channel between the two.
///
/// Writes the children's positions relative to `(0, 0)` and records a route
/// per edge.
fn layered(l: &mut Layout, sc: &mut Scratch, container: u32, conn: &[NodeId], es: &[Local], p: &LayoutParams) -> Block {
    const NONE: u32 = u32::MAX;
    let k = conn.len();
    for (s, c) in conn.iter().enumerate() {
        sc.slot[c.idx()] = s as u32;
    }
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); k];
    let mut leaves = vec![false; k];
    let mut enters = vec![false; k];
    for &(_, kind, a, b) in es {
        let sa = sc.slot[a as usize];
        match kind {
            SIBLING => out[sa as usize].push(sc.slot[b as usize]),
            LEAVES if sa != NONE => leaves[sa as usize] = true,
            ENTERS if sa != NONE => enters[sa as usize] = true,
            _ => {}
        }
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
    // Callers nobody calls would all sit in the first layer: pull each one
    // forward, next to its nearest callee (shorter edges, fewer lanes).
    let mut has_in = vec![false; k];
    let mut nearest = vec![u32::MAX; k];
    for &(a, b, _) in &dag {
        has_in[b as usize] = true;
        nearest[a as usize] = nearest[a as usize].min(layer[b as usize]);
    }
    for (v, at) in layer.iter_mut().enumerate() {
        if !has_in[v] && nearest[v] != u32::MAX {
            *at = nearest[v] - 1;
        }
    }
    // The in-port gets a layer of its own before the first, the out-port after the last.
    let (any_in, any_out) = (enters.contains(&true), leaves.contains(&true));
    let first = layer.iter().copied().min().unwrap_or(0);
    layer.iter_mut().for_each(|x| *x = *x - first + any_in as u32);
    let n_layers = layer.iter().max().map_or(0, |m| *m as usize + 1) + any_out as usize;

    // 3. Vertices `0..k` are children, the rest lanes and ports.
    let mut net = Net { layers: vec![Vec::new(); n_layers], left: vec![Vec::new(); k], right: vec![Vec::new(); k] };
    for (v, &at) in layer.iter().enumerate() {
        net.layers[at as usize].push(v as u32);
    }
    // Per edge: `(from, to, reversed, first dummy, dummy count)`.
    let mut wires: Vec<(u32, u32, bool, u32, u32)> = Vec::with_capacity(dag.len());
    for &(a, b, rev) in &dag {
        let (la, lb) = (layer[a as usize], layer[b as usize]);
        let first = net.left.len() as u32;
        let mut prev = a;
        for at in la + 1..lb {
            let d = net.add(at as usize);
            net.link(prev, d);
            prev = d;
        }
        net.link(prev, b);
        wires.push((a, b, rev, first, lb - la - 1));
    }
    // Buses: `bus_out[L]` carries the leaving edges through layer `L` (the
    // last one is the out-port), `bus_in[L]` the entering ones (the first is
    // the in-port).
    let mut bus_out = vec![NONE; n_layers];
    let mut bus_in = vec![NONE; n_layers];
    if any_out {
        let from = (0..k).filter(|&v| leaves[v]).map(|v| layer[v] as usize + 1).min().unwrap();
        for at in from..n_layers {
            bus_out[at] = net.add(at);
            if at > from {
                net.link(bus_out[at - 1], bus_out[at]);
            }
        }
        for v in (0..k).filter(|&v| leaves[v]) {
            net.link(v as u32, bus_out[layer[v] as usize + 1]);
        }
    }
    if any_in {
        let to = (0..k).filter(|&v| enters[v]).map(|v| layer[v] as usize - 1).max().unwrap();
        for at in 0..=to {
            bus_in[at] = net.add(at);
            if at > 0 {
                net.link(bus_in[at - 1], bus_in[at]);
            }
        }
        for v in (0..k).filter(|&v| enters[v]) {
            net.link(bus_in[layer[v] as usize - 1], v as u32);
        }
    }

    // 4. Barycentre sweeps.
    let n_vertices = net.left.len();
    let mut pos = vec![0f32; n_vertices];
    let place = |layer: &[u32], pos: &mut [f32]| {
        for (i, &v) in layer.iter().enumerate() {
            pos[v as usize] = i as f32;
        }
    };
    net.layers.iter().for_each(|ly| place(ly, &mut pos));
    for sweep in 0..4 {
        let order: Box<dyn Iterator<Item = usize>> =
            if sweep % 2 == 0 { Box::new(1..n_layers) } else { Box::new((0..n_layers.saturating_sub(1)).rev()) };
        let nb = if sweep % 2 == 0 { &net.left } else { &net.right };
        for at in order {
            let mut keyed: Vec<(f32, u32)> = net.layers[at]
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
            net.layers[at].clear();
            net.layers[at].extend(keyed.iter().map(|e| e.1));
            place(&net.layers[at], &mut pos);
        }
    }

    // 5. Coordinates, in (u, v) = (along the layers, across them).
    let g = p.layer_gap;
    let size = |l: &Layout, horiz: bool, x: u32| match conn.get(x as usize) {
        Some(c) if horiz => (l.w[c.idx()], l.h[c.idx()]),
        Some(c) => (l.h[c.idx()], l.w[c.idx()]),
        None => (0.0, p.lane),
    };
    // Per layer `(thickness, length)`, for layers along x or along y.
    let dims = |l: &Layout, horiz: bool| -> Vec<(f32, f32)> {
        net.layers
            .iter()
            .map(|ly| {
                let thick = ly.iter().map(|&x| size(l, horiz, x).0).fold(0.0, f32::max);
                let len = ly.iter().map(|&x| size(l, horiz, x).1).sum::<f32>() + p.gap * (ly.len() - 1) as f32;
                (thick, len)
            })
            .collect()
    };
    // A block's `(u, v)` extent with `per` layers in each band.
    let extent = |d: &[(f32, f32)], per: usize| {
        let bands = d.chunks(per);
        let wrapped = if bands.len() > 1 { g } else { 0.0 };
        let v_gaps = g * (bands.len() - 1) as f32;
        let (u, v) = bands.fold((0.0f32, 0.0f32), |(u, v), b| {
            let along = b.iter().map(|e| e.0).sum::<f32>() + g * (b.len() - 1) as f32;
            (u.max(along), v + b.iter().map(|e| e.1).fold(0.0, f32::max))
        });
        (u + wrapped, v + v_gaps)
    };
    let (dh, dv) = (dims(l, true), dims(l, false));
    let mut best = (f32::MAX, true, n_layers);
    for horiz in [true, false] {
        for per in (1..=n_layers).rev() {
            let (u, v) = extent(if horiz { &dh } else { &dv }, per);
            let aspect = if horiz { u / v.max(1.0) } else { v / u.max(1.0) };
            // A wrap costs a detour for the edges across it: only for a clearly better shape.
            let score = (aspect / p.aspect).ln().abs() + 0.15 * (n_layers.div_ceil(per) - 1) as f32;
            if score < best.0 {
                best = (score, horiz, per);
            }
        }
    }
    let (_, horiz, per) = best;
    let d = if horiz { dh } else { dv };
    let (total_u, total_v) = extent(&d, per);
    let wrapped = n_layers > per;
    // Per layer its `[u0, u1]` and band; per band the channel line behind it.
    let mut span = vec![[0f32; 2]; n_layers];
    let mut channel: Vec<f32> = Vec::new();
    // Per vertex its centre line `v`; per child its `[v0, v1]` and `[u0, u1]`.
    let mut mid = vec![0f32; n_vertices];
    let mut across = vec![[0f32; 2]; k];
    let mut along = vec![[0f32; 2]; k];
    let mut band_v = 0.0f32;
    for (band, layers) in net.layers.chunks(per).enumerate() {
        let band_len = d[band * per..].iter().take(per).map(|e| e.1).fold(0.0, f32::max);
        let mut u = if wrapped { g * 0.5 } else { 0.0 };
        for (i, ly) in layers.iter().enumerate() {
            let at = band * per + i;
            let (thick, len) = d[at];
            let mut v = band_v + (band_len - len) * 0.5;
            for &x in ly {
                let (su, sv) = size(l, horiz, x);
                mid[x as usize] = v + sv * 0.5;
                if let Some(c) = conn.get(x as usize) {
                    let cu = u + (thick - su) * 0.5;
                    (l.x[c.idx()], l.y[c.idx()]) = if horiz { (cu, v) } else { (v, cu) };
                    across[x as usize] = [v, v + sv];
                    along[x as usize] = [cu, cu + su];
                }
                v += sv + p.gap;
            }
            span[at] = [u, u + thick];
            u += thick + g;
        }
        channel.push(band_v + band_len + g * 0.5);
        band_v += band_len + g;
    }

    // 6. Routes. A child's own port is used where it faces the right way: then
    // the bundle inside the child and the edge out here meet in one point.
    let orient = if horiz { 1 } else { 2 };
    let port = |sc: &Scratch, x: u32, toward: f32, own: Option<bool>| {
        let c = conn[x as usize].idx();
        let [v0, v1] = across[x as usize];
        let at = match own {
            Some(true) => sc.port_out[c],
            Some(false) => sc.port_in[c],
            None => f32::NAN,
        };
        if sc.orient[c] == orient && !at.is_nan() {
            v0 + at
        } else {
            let m = ((v1 - v0) * 0.5).min(6.0);
            toward.clamp(v0 + m, v1 - m)
        }
    };
    let xy = |u: f32, v: f32| if horiz { [u, v] } else { [v, u] };
    // From the end of layer `at` (at `v0`) to the start of the next (at `v1`):
    // a jog in the gutter between them, or around the band ends.
    let hop = |pts: &mut Vec<[f32; 2]>, at: usize, v0: f32, v1: f32| {
        if !(at + 1).is_multiple_of(per) {
            let m = (span[at][1] + span[at + 1][0]) * 0.5;
            pts.extend([xy(m, v0), xy(m, v1)]);
        } else {
            let (right, left, ch) = (span[at][1] + g * 0.25, g * 0.25, channel[at / per]);
            pts.extend([xy(right, v0), xy(right, ch), xy(left, ch), xy(left, v1)]);
        }
    };
    // Through layers `from+1..to` along the lanes `lane(layer)`, starting at
    // `v0` and arriving at `v1`.
    let run = |pts: &mut Vec<[f32; 2]>, from: usize, to: usize, v0: f32, v1: f32, lane: &dyn Fn(usize) -> f32| {
        let mut v = v0;
        pts.push(xy(span[from][1], v));
        for at in from..to {
            let next = if at + 1 == to { v1 } else { lane(at + 1) };
            hop(pts, at, v, next);
            v = next;
            pts.push(xy(span[at + 1][0], v));
            if at + 1 != to {
                pts.push(xy(span[at + 1][1], v));
            }
        }
    };
    for (a, b, rev, first, count) in wires {
        let (la, lb) = (layer[a as usize] as usize, layer[b as usize] as usize);
        let lane = |at: usize| mid[first as usize + at - la - 1];
        let va = port(sc, a, if count > 0 { lane(la + 1) } else { mid[b as usize] }, (!rev).then_some(true));
        let vb = port(sc, b, if count > 0 { lane(lb - 1) } else { va }, (!rev).then_some(false));
        let start = sc.pts.len() as u32;
        sc.pts.push(xy(along[a as usize][1], va));
        run(&mut sc.pts, la, lb, va, vb, &lane);
        sc.pts.push(xy(along[b as usize][0], vb));
        let (mut from, mut to) = (conn[a as usize].0, conn[b as usize].0);
        if rev {
            // Laid out backwards: the real edge runs the route the other way.
            sc.pts[start as usize..].reverse();
            std::mem::swap(&mut from, &mut to);
        }
        sc.routes.push((from, to, container, start, sc.pts.len() as u32, SIBLING));
    }
    let (v_in, v_out) = (bus_in[0], bus_out[n_layers - 1]);
    for v in (0..k).filter(|&v| leaves[v]) {
        let la = layer[v] as usize;
        let lane = |at: usize| mid[bus_out[at] as usize];
        let pv = port(sc, v as u32, lane(la + 1), Some(true));
        let start = sc.pts.len() as u32;
        sc.pts.push(xy(along[v][1], pv));
        run(&mut sc.pts, la, n_layers - 1, pv, mid[v_out as usize], &lane);
        // The last point is moved onto the container's side once that is known.
        sc.pts.push(xy(total_u, mid[v_out as usize]));
        sc.routes.push((conn[v].0, container, container, start, sc.pts.len() as u32, LEAVES));
    }
    for v in (0..k).filter(|&v| enters[v]) {
        let lb = layer[v] as usize;
        let lane = |at: usize| mid[bus_in[at] as usize];
        let pv = port(sc, v as u32, lane(lb - 1), Some(false));
        let start = sc.pts.len() as u32;
        sc.pts.push(xy(0.0, mid[v_in as usize]));
        run(&mut sc.pts, 0, lb, mid[v_in as usize], pv, &lane);
        sc.pts.push(xy(along[v][0], pv));
        sc.routes.push((container, conn[v].0, container, start, sc.pts.len() as u32, ENTERS));
    }
    let port_of = |bus: u32| if bus == NONE { f32::NAN } else { mid[bus as usize] };
    let (w, h) = if horiz { (total_u, total_v) } else { (total_v, total_u) };
    Block { w, h, orient, port_in: port_of(v_in), port_out: port_of(v_out) }
}

/// `name_len[i]`: display name length in chars (callers resolve names; this
/// crate only sees the node table). `src` / `dst`: the graph's edges.
pub fn layout(nodes: &NodeTable, name_len: &[u32], src: &[NodeId], dst: &[NodeId], p: &LayoutParams) -> Layout {
    let n = nodes.len();
    let mut l =
        Layout { x: vec![0.0; n], y: vec![0.0; n], w: vec![0.0; n], h: vec![0.0; n], routes: Routes::default() };
    let edges = local_edges(nodes, src, dst);
    let mut sc = Scratch {
        slot: vec![u32::MAX; n],
        orient: vec![0; n],
        port_in: vec![f32::NAN; n],
        port_out: vec![f32::NAN; n],
        routes: Vec::new(),
        pts: Vec::new(),
    };
    let mut kids: Vec<NodeId> = Vec::new();
    let mut conn: Vec<NodeId> = Vec::new();
    let mut loose: Vec<NodeId> = Vec::new();
    let mut edge_end = edges.len();

    for i in (0..n).rev() {
        let id = NodeId::from_idx(i);
        let hs = header_scale(nodes.kind[i]);
        let header = p.header * hs;
        let label_w = (name_len[i] as f32 + nodes.kind[i].label().len() as f32 + 2.0) * p.char_w * hs + 2.0 * p.pad;
        // This container's edges: containers are visited in descending order,
        // so they are the tail of what is left.
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
        if es.iter().any(|e| e.1 == SIBLING) {
            for &(_, _, a, b) in es.iter().filter(|e| e.1 == SIBLING) {
                sc.slot[a as usize] = 0;
                sc.slot[b as usize] = 0;
            }
            let (c, o): (Vec<NodeId>, Vec<NodeId>) = kids.iter().partition(|c| sc.slot[c.idx()] == 0);
            (conn, loose) = (c, o);
        }
        let (routes_mark, pts_mark) = (sc.routes.len(), sc.pts.len());
        let mut top = Block { w: 0.0, h: 0.0, orient: 0, port_in: f32::NAN, port_out: f32::NAN };
        if !conn.is_empty() && conn.len() <= p.max_layered {
            let b = layered(&mut l, &mut sc, i as u32, &conn, es, p);
            // Too lopsided (one caller of fifty, a long chain): shelf instead.
            if b.h > 4.0 * b.w.max(p.max_leaf_w) || b.w > 12.0 * b.h {
                sc.routes.truncate(routes_mark);
                sc.pts.truncate(pts_mark);
            } else {
                top = b;
            }
        }
        for c in &conn {
            sc.slot[c.idx()] = u32::MAX;
        }
        // The rest is shelf-packed clear of the ports: below a block whose
        // layers run along x, beside one whose layers run along y.
        let rest: &[NodeId] = if top.orient != 0 { &loose } else { &kids };
        let area: f32 = rest.iter().map(|c| (l.w[c.idx()] + p.gap) * (l.h[c.idx()] + p.gap)).sum();
        let widest = rest.iter().map(|c| l.w[c.idx()]).fold(0.0, f32::max);
        let mut row_w = (area * p.aspect).sqrt().max(widest);
        let sep = if top.orient != 0 && !rest.is_empty() { p.gap * 2.0 } else { 0.0 };
        let (inner_w, inner_h) = if top.orient == 2 {
            let side = shelf(&mut l, rest, row_w, (top.w + sep, 0.0), p.gap);
            (top.w + sep + side.0, top.h.max(side.1))
        } else {
            row_w = row_w.max(label_w - 2.0 * p.pad).max(top.w);
            let below = shelf(&mut l, rest, row_w, (0.0, top.h + sep), p.gap);
            (top.w.max(below.0), top.h + sep + below.1)
        };

        // Move everything below the header, inside the padding.
        let (ox, oy) = (p.pad, header + p.pad);
        for &c in &kids {
            l.x[c.idx()] += ox;
            l.y[c.idx()] += oy;
        }
        for pt in &mut sc.pts[pts_mark..] {
            *pt = [pt[0] + ox, pt[1] + oy];
        }
        l.w[i] = inner_w.max(label_w - 2.0 * p.pad) + 2.0 * p.pad;
        l.h[i] = oy + inner_h + p.pad;
        // Ports sit on the container's sides: stretch the buses out to them.
        let axis = if top.orient == 1 { 0 } else { 1 };
        for &(_, _, _, start, end, kind) in &sc.routes[routes_mark..] {
            match kind {
                LEAVES => sc.pts[end as usize - 1][axis] = if axis == 0 { l.w[i] } else { l.h[i] },
                ENTERS => sc.pts[start as usize][axis] = 0.0,
                _ => {}
            }
        }
        sc.orient[i] = top.orient;
        sc.port_in[i] = top.port_in + if axis == 0 { oy } else { ox };
        sc.port_out[i] = top.port_out + if axis == 0 { oy } else { ox };
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
    for &(from, to, c, start, end, _) in &sc.routes {
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

    fn inside(q: &[f32; 2], r: [f32; 4]) -> bool {
        q[0] > r[0] + 1e-3 && q[0] < r[2] - 1e-3 && q[1] > r[1] + 1e-3 && q[1] < r[3] - 1e-3
    }

    /// The route is axis-parallel and never runs through one of `boxes`.
    fn assert_clear(l: &Layout, route: &[[f32; 2]], boxes: &[NodeId]) {
        assert!(route.len() >= 2);
        for w in route.windows(2) {
            assert!(w[0][0] == w[1][0] || w[0][1] == w[1][1], "diagonal segment {w:?}");
            for t in 0..=20 {
                let t = t as f32 / 20.0;
                let q = [w[0][0] + (w[1][0] - w[0][0]) * t, w[0][1] + (w[1][1] - w[0][1]) * t];
                assert!(boxes.iter().all(|&c| !inside(&q, rect(l, c))), "segment {w:?} crosses a box");
            }
        }
    }

    /// `a` lies entirely before `b`, along x or along y.
    fn before(l: &Layout, a: NodeId, b: NodeId) -> bool {
        let (p, q) = (rect(l, a), rect(l, b));
        p[2] < q[0] || p[3] < q[1]
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
    fn callers_before_callees() {
        let (t, _, f) = module(6);
        // f3 → f0 → f1 → f2, and f4 → f1; f5 is loose.
        let l = run(&t, &[(f[3], f[0]), (f[0], f[1]), (f[1], f[2]), (f[4], f[1])]);
        for (a, b) in [(3, 0), (0, 1), (1, 2), (4, 1)] {
            assert!(before(&l, f[a], f[b]), "f{a} before f{b}");
            let route = l.routes.get(f[a], f[b]).unwrap();
            // Leaves f[a]'s side, ends on f[b]'s.
            let (p, q) = (rect(&l, f[a]), rect(&l, f[b]));
            let on = |pt: &[f32; 2], r: [f32; 4]| pt[0] == r[0] || pt[0] == r[2] || pt[1] == r[1] || pt[1] == r[3];
            assert!(on(&route[0], p) && on(route.last().unwrap(), q));
        }
        // The loose child sits clear of the layered block.
        assert!((0..5).all(|k| before(&l, f[k], f[5])));
    }

    #[test]
    fn routes_stay_clear_of_boxes() {
        let (t, _, f) = module(4);
        // Chain f0 → f1 → f2 → f3 plus the shortcut f0 → f3, and a back edge f3 → f1… which
        // closes a cycle and is laid out reversed.
        let edges = [(f[0], f[1]), (f[1], f[2]), (f[2], f[3]), (f[0], f[3]), (f[3], f[1])];
        let l = run(&t, &edges);
        for (a, b) in edges {
            assert_clear(&l, l.routes.get(a, b).expect("every sibling edge is routed"), &f);
        }
        // The shortcut is longer than a neighbour hop: it takes lanes past f1 and f2.
        assert!(l.routes.get(f[0], f[3]).unwrap().len() > l.routes.get(f[0], f[1]).unwrap().len());
        // The back edge starts at f3 and ends at f1.
        let back = l.routes.get(f[3], f[1]).unwrap();
        assert!(!inside(&back[0], rect(&l, f[1])) && l.routes.get(f[1], f[3]).is_none());
        let d = |q: &[f32; 2], n: NodeId| {
            let r = rect(&l, n);
            (q[0] - (r[0] + r[2]) * 0.5).abs() + (q[1] - (r[1] + r[3]) * 0.5).abs()
        };
        assert!(d(&back[0], f[3]) < d(&back[0], f[1]) && d(back.last().unwrap(), f[1]) < d(back.last().unwrap(), f[3]));
    }

    /// ws { a { x, x2 }, b { y, y2 } } with `x → x2`, `y → y2` (so both modules
    /// are layered) plus the given cross edges.
    fn two_modules(cross: &[(usize, usize)]) -> (NodeTable, [NodeId; 2], [NodeId; 4], Layout) {
        let mut t = NodeTable::default();
        let ws = t.open(NodeId::NONE, nn(NodeKind::Workspace, 0));
        let mut f = Vec::new();
        let mut mods = Vec::new();
        for _ in 0..2 {
            let m = t.open(ws, nn(NodeKind::FileModule, 1));
            for _ in 0..2 {
                let x = t.open(m, nn(NodeKind::Fn, 1));
                t.close(x);
                f.push(x);
            }
            t.close(m);
            mods.push(m);
        }
        t.close(ws);
        let mut edges = vec![(f[0], f[1]), (f[2], f[3])];
        edges.extend(cross.iter().map(|&(a, b)| (f[a], f[b])));
        let l = run(&t, &edges);
        (t, [mods[0], mods[1]], [f[0], f[1], f[2], f[3]], l)
    }

    #[test]
    fn edges_are_lifted_to_siblings() {
        // b's `y` calls a's `x`: b is laid out before a, although a comes first.
        let (_, [a, b], _, l) = two_modules(&[(2, 0)]);
        assert!(l.routes.get(b, a).is_some(), "lifted to the modules");
        assert!(before(&l, b, a));
    }

    #[test]
    fn bundles_meet_at_ports() {
        // x2 (in a) → y (in b): out of a through its out-port, into b through its in-port.
        let (_, [a, b], f, l) = two_modules(&[(1, 2)]);
        let up = l.routes.get(f[1], a).expect("x2 → a's out-port");
        let mid = l.routes.get(a, b).expect("a → b");
        let down = l.routes.get(b, f[2]).expect("b's in-port → y");
        assert!(l.routes.get(a, f[1]).is_none() && l.routes.get(f[2], b).is_none());
        for r in [up, mid, down] {
            assert_clear(&l, r, &f);
        }
        assert_clear(&l, mid, &[a, b]);
        // The three chain into one path: the bus ends on a's side, where the
        // a → b route starts (in the same point, if both run along the same
        // axis; else the renderer walks around a's corner), and likewise at b.
        let on = |q: &[f32; 2], r: [f32; 4]| q[0] == r[0] || q[0] == r[2] || q[1] == r[1] || q[1] == r[3];
        let (ra, rb) = (rect(&l, a), rect(&l, b));
        assert!(on(up.last().unwrap(), ra) && on(&mid[0], ra));
        assert!(on(mid.last().unwrap(), rb) && on(&down[0], rb));
        let axis = |r: &[[f32; 2]], i: usize| (r[i][0] == r[i + 1][0]) as u8;
        if axis(up, up.len() - 2) == axis(mid, 0) {
            assert_eq!(up.last(), mid.first());
        }
        if axis(mid, mid.len() - 2) == axis(down, 0) {
            assert_eq!(mid.last(), down.first());
        }
    }

    #[test]
    fn long_chains_wrap_into_bands() {
        let (t, m, f) = module(12);
        let chain: Vec<_> = f.windows(2).map(|w| (w[0], w[1])).collect();
        let l = run(&t, &chain);
        assert_nested_and_disjoint(&t, &l, m);
        let [x0, y0, x1, y1] = rect(&l, m);
        let aspect = (x1 - x0) / (y1 - y0);
        assert!((0.8..3.2).contains(&aspect), "a single row of 12 would be ~20:1, got {aspect}");
        for (a, b) in chain {
            assert_clear(&l, l.routes.get(a, b).unwrap(), &f);
        }
    }

    #[test]
    fn lopsided_or_huge_containers_fall_back_to_the_shelf() {
        let (t, m, f) = module(60);
        let fan: Vec<_> = f[1..].iter().map(|&c| (f[0], c)).collect();
        let l = run(&t, &fan);
        assert_nested_and_disjoint(&t, &l, m);
        let [x0, y0, x1, y1] = rect(&l, m);
        assert!((y1 - y0) < 3.0 * (x1 - x0) && (x1 - x0) < 6.0 * (y1 - y0), "neither a tower nor a ribbon");
    }
}
