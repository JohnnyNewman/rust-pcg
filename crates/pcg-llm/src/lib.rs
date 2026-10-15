//! # pcg-llm — language models behind the graph
//!
//! Headless like the other analysis crates: no UI, no async runtime. Calls
//! block, so callers run them on worker threads.
//!
//! * [`Backend`] — the pluggable seam: a request in, streamed text out.
//!   [`Claude`] (the Claude API) is the first implementation; [`Scripted`]
//!   answers from a function (tests, demos).
//! * [`summarize`] — bottom-up, lazy summaries: [`summarize::plan`] lists what
//!   a node's summary depends on, [`summarize::run`] writes them level by
//!   level, in parallel within a level, into a [`SummaryCache`] keyed by
//!   content hash.
//! * [`generate`] — intent → code: a graph-aware prompt for one node and a
//!   [`generate::Proposal`] (new text for its span, with an `@pcg:intent`
//!   comment) that the editor shows, and the user accepts or rejects.
//!
//! Nothing here writes source files (decision 7): summaries go to the cache,
//! proposals back to the caller.

pub mod backend;
pub mod cache;
pub mod claude;
pub mod generate;
pub mod summarize;

pub use backend::{Backend, Completion, Effort, Error, Request, Scripted};
pub use cache::SummaryCache;
pub use claude::Claude;
pub use generate::Proposal;
