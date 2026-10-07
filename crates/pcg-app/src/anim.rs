//! Graph-diff animation: pure functions of (old snapshot, diff, new snapshot,
//! time). Nothing is stored per node; the canvas asks for each node's rect and
//! fade factors while it sweeps.
//!
//! Timeline after a reload (seconds since the new snapshot arrived):
//! ```text
//! 0.00 ─ move ──────── 0.55            matched nodes tween old rect → new rect
//! 0.00 ─ exit ─── 0.40                 removed subtrees fade/shrink at their old place
//!            0.35 ─ enter ── 0.75      new nodes fade in once their parent has made room
//! 0.00 ─ flash ──────────────── 1.60   changed nodes glow, then settle
//! ```

use crate::model::*;
use crate::theme::smooth;
use bevy::prelude::*;
use pcg_core::NodeId;
use pcg_layout::Layout;
use pcg_syntax::GraphDiff;
use pcg_syntax::diff::flag;

pub const MOVE: f32 = 0.55;
pub const EXIT: f32 = 0.40;
pub const ENTER: (f32, f32) = (0.35, 0.75);
pub const FLASH: f32 = 1.60;

/// A transition in progress, borrowed for one frame.
pub struct Anim<'a> {
    pub prev: &'a Loaded,
    pub diff: &'a GraphDiff,
    /// Seconds since the transition started.
    pub t: f32,
    /// Eased move progress 0..1.
    pub k: f32,
}

#[inline]
fn ease_in_out(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x < 0.5 { 4.0 * x * x * x } else { 1.0 - (-2.0 * x + 2.0).powi(3) / 2.0 }
}

impl<'a> Anim<'a> {
    pub fn new(cur: &'a Loaded, tr: &'a Transition, now: f64) -> Option<Self> {
        let prev = tr.prev.as_deref()?;
        let diff = cur.diff.as_ref()?;
        let t = (now - tr.started) as f32;
        (t < FLASH).then(|| Anim { prev, diff, t, k: ease_in_out(t / MOVE) })
    }

    /// World rect `[x, y, w, h]` of new node `i` at this moment.
    #[inline]
    pub fn rect(&self, l: &Layout, i: usize) -> [f32; 4] {
        let new = [l.x[i], l.y[i], l.w[i], l.h[i]];
        let o = self.diff.old_of_new[i];
        if o.is_none() || self.k >= 1.0 {
            return new;
        }
        let o = o.idx();
        let pl = &self.prev.layout;
        let old = [pl.x[o], pl.y[o], pl.w[o], pl.h[o]];
        std::array::from_fn(|c| old[c] + (new[c] - old[c]) * self.k)
    }

    /// Opacity factor for entered nodes (1 for everything else).
    #[inline]
    pub fn appear(&self, i: usize) -> f32 {
        if self.diff.flags[i] & flag::ENTERED != 0 { smooth(ENTER.0, ENTER.1, self.t) } else { 1.0 }
    }

    /// Glow strength 0..1: full for nodes whose own code changed or that
    /// entered; faint for their ancestors, so a change is findable from any
    /// zoom level.
    #[inline]
    pub fn flash(&self, i: usize) -> f32 {
        let f = self.diff.flags[i];
        let w = if f & (flag::CHANGED | flag::ENTERED) != 0 {
            1.0
        } else if f & flag::DESC_CHANGED != 0 {
            0.4
        } else {
            return 0.0;
        };
        let rise = smooth(0.0, 0.15, self.t);
        let fall = 1.0 - smooth(0.5, FLASH, self.t);
        w * rise * fall
    }

    /// Exit progress 0..1 (1 = gone).
    #[inline]
    pub fn exit(&self) -> f32 {
        smooth(0.0, EXIT, self.t)
    }

    /// Where exit root `o` (an old id) is drawn: its old rect, carried along
    /// with the motion of its surviving parent.
    pub fn exit_rect(&self, cur: &Layout, o: NodeId) -> [f32; 4] {
        let i = o.idx();
        let r = [self.prev.layout.x[i], self.prev.layout.y[i], self.prev.layout.w[i], self.prev.layout.h[i]];
        let p = self.prev.graph.nodes.parent[i];
        if p.is_none() {
            return r;
        }
        let np = self.diff.new_of_old[p.idx()];
        let now = self.rect(cur, np.idx());
        let (dx, dy) = (now[0] - self.prev.layout.x[p.idx()], now[1] - self.prev.layout.y[p.idx()]);
        [r[0] + dx, r[1] + dy, r[2], r[3]]
    }
}

/// End the transition: drop the old snapshot so its memory is freed.
pub fn end(mut tr: ResMut<Transition>, time: Res<Time>) {
    if tr.prev.is_some() && (time.elapsed_secs_f64() - tr.started) as f32 >= FLASH {
        tr.prev = None;
    }
}
