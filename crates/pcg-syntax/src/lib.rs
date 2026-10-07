//! # pcg-syntax — the tree-sitter stage
//!
//! Stateless stages, each a plain function from tables to tables:
//!
//! ```text
//! scan (dirs → files + module paths)
//!   → parse   (rayon, one task per file → file-local item/call/comment lists)
//!   → assemble (serial, file-local → global pre-order NodeTable)
//!   → comments (@pcg comments → CommentTable + SummaryState)
//!   → resolve  (name-based call edges → EdgeTable)
//! ```
//!
//! [`build_graph_cached`] keeps parses in a [`ParseCache`] and re-parses only
//! changed files; [`diff`] matches two snapshots by `stable_key`.
//!
//! [`build_graph`] runs the whole pipeline and reports per-stage timings.

pub mod assemble;
pub mod cache;
pub mod diff;
pub mod parse;
pub mod pcg_comment;
pub mod resolve;
pub mod scan;

pub use assemble::{BuildStats, build_graph, build_graph_cached};
pub use cache::ParseCache;
pub use diff::{GraphDiff, diff};
pub use pcg_comment::{TextEdit, short_hash, summary_edit};
