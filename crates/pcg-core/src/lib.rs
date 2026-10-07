//! # pcg-core — the data model
//!
//! Everything here is *data*: flat, struct-of-arrays tables indexed by dense
//! `u32` ids. There is deliberately no behaviour beyond trivial accessors and
//! builders; all logic lives in the stage crates (`pcg-syntax`, `pcg-layout`,
//! …) and in the app's systems.
//!
//! ## Hierarchy encoding
//! Nodes are stored in **pre-order**. Every node stores `subtree_end`, the id
//! one past its last descendant. That gives:
//! * descendants of `n` = the contiguous id range `n+1 .. subtree_end[n]`,
//! * "skip this subtree" = jump to `subtree_end[n]` (O(1) culling),
//! * children = `c = n+1; while c < end { visit(c); c = subtree_end[c] }`,
//! * bottom-up passes = iterate ids in *descending* order (children first).
//!
//! No pointers, no `Rc`, no recursion needed.

pub mod graph;
pub mod id;
pub mod interner;

pub use graph::*;
pub use id::*;
pub use interner::{Interner, Sym};
