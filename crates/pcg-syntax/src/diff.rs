//! Snapshot diff: match the nodes of two graphs by `stable_key`.
//!
//! Output is a pair of dense id maps plus per-node change flags — plain
//! columns that the animation stage indexes directly.

use pcg_core::{NodeId, NodeTable};
use rustc_hash::FxHashMap;

/// Per-new-node flags.
pub mod flag {
    /// No counterpart in the old snapshot.
    pub const ENTERED: u8 = 1;
    /// Matched, but the code (content hash) changed.
    pub const CHANGED: u8 = 2;
    /// Matched, unchanged itself, but something in its subtree entered,
    /// exited or changed.
    pub const DESC_CHANGED: u8 = 4;
}

#[derive(Debug, Default, Clone)]
pub struct GraphDiff {
    /// For each new node: its old id, or `NONE` if it entered.
    pub old_of_new: Vec<NodeId>,
    /// For each old node: its new id, or `NONE` if it exited.
    pub new_of_old: Vec<NodeId>,
    /// For each new node: [`flag`] bits.
    pub flags: Vec<u8>,
    /// Old nodes that exited while their parent survived: the roots of the
    /// removed subtrees (what an exit animation draws).
    pub exit_roots: Vec<NodeId>,
    pub entered: usize,
    pub exited: usize,
    pub changed: usize,
}

impl GraphDiff {
    pub fn is_empty(&self) -> bool {
        self.entered == 0 && self.exited == 0 && self.changed == 0
    }
}

pub fn diff(old: &NodeTable, new: &NodeTable) -> GraphDiff {
    let mut by_key: FxHashMap<u64, NodeId> = FxHashMap::default();
    by_key.reserve(old.len());
    for (i, &k) in old.stable_key.iter().enumerate() {
        by_key.insert(k, NodeId::from_idx(i));
    }
    let mut d = GraphDiff {
        old_of_new: vec![NodeId::NONE; new.len()],
        new_of_old: vec![NodeId::NONE; old.len()],
        flags: vec![0; new.len()],
        ..Default::default()
    };
    for i in 0..new.len() {
        match by_key.get(&new.stable_key[i]) {
            Some(&o) if old.kind[o.idx()] == new.kind[i] => {
                d.old_of_new[i] = o;
                d.new_of_old[o.idx()] = NodeId::from_idx(i);
                // Synthetic nodes (hash 0) aggregate their children instead.
                if new.content_hash[i] != 0 && new.content_hash[i] != old.content_hash[o.idx()] {
                    d.flags[i] |= flag::CHANGED;
                    d.changed += 1;
                }
            }
            _ => {
                d.flags[i] |= flag::ENTERED;
                d.entered += 1;
            }
        }
    }
    d.exited = d.new_of_old.iter().filter(|n| n.is_none()).count();

    // Exit roots: exited nodes whose parent survived. Each marks the new
    // counterpart of that parent; then "something below changed" propagates
    // bottom-up (reverse pre-order).
    for o in 0..old.len() {
        if d.new_of_old[o].is_some() {
            continue;
        }
        let p = old.parent[o];
        let np = if p.is_none() { NodeId::NONE } else { d.new_of_old[p.idx()] };
        if p.is_none() || np.is_some() {
            d.exit_roots.push(NodeId::from_idx(o));
        }
        if np.is_some() {
            d.flags[np.idx()] |= flag::DESC_CHANGED;
        }
    }
    for i in (0..new.len()).rev() {
        let p = new.parent[i];
        if p.is_some() && d.flags[i] != 0 {
            d.flags[p.idx()] |= flag::DESC_CHANGED;
        }
    }
    d
}
