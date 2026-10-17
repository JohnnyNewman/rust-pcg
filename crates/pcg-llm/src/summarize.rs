//! Bottom-up, lazy summarization.
//!
//! Asked for one node, [`plan`] lists what has to be written first: every
//! summarizable node in its subtree that has neither a fresh `@pcg:summary`
//! in the source nor an entry in the [`SummaryCache`]. Each gets a *height*
//! (1 for nodes whose children are all known, else 1 + the highest planned
//! child), and [`run`] works through the heights in order — all nodes of one
//! height in parallel, since they only depend on lower ones. A container's
//! prompt carries its children's summaries, so summaries of big modules are
//! built from the summaries of their parts instead of from all their code.

use crate::backend::{Backend, Effort, Error, Request};
use crate::cache::{SummaryCache, key};
use pcg_core::{Graph, NodeId, NodeKind, SummaryState};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Containers up to this size are shown in full; bigger ones as header + items.
const FULL_CODE: usize = 8 * 1024;
/// A leaf bigger than this is not sent (it would not be one summary anyway).
const MAX_LEAF: usize = 256 * 1024;
/// Items listed in a container's prompt.
const MAX_ITEMS: usize = 200;

const SYSTEM: &str = "You write the summaries a visual code editor shows on the boxes of a code graph. \
A summary is one to three plain sentences, at most about 60 words, saying what the item does and what it \
is for, the way a reader skimming the graph needs it: inputs and results, side effects (I/O, threads, \
global state), notable error handling or invariants. For a module, impl or trait, say what the items \
together are responsible for. Do not restate the signature, do not begin with \"This function\" or the \
item's name, no markdown, no code formatting, no lists. Answer with the summary only.";

/// What is known about `n` already: a fresh summary in the source, else a cached one.
pub fn known<'a>(g: &'a Graph, cache: &'a SummaryCache, n: NodeId) -> Option<&'a str> {
    let i = n.idx();
    if g.nodes.summary_state[i] == SummaryState::Fresh {
        return Some(g.comments.text(g.nodes.summary[i]));
    }
    cache.get(g, n)
}

fn wanted(g: &Graph, n: NodeId) -> bool {
    let i = n.idx();
    g.summarizable(n) && !g.nodes.bytes[i].is_empty()
}

/// Nodes to summarize so that `root` has a summary, as `(node, height)`,
/// lowest height first. `redo_root`: write `root`'s summary again even if it
/// is known (its children are still taken as they are).
pub fn plan(g: &Graph, cache: &SummaryCache, root: NodeId, redo_root: bool) -> Vec<(NodeId, u32)> {
    let range = root.0..g.nodes.subtree_end[root.idx()].0;
    let base = range.start;
    let mut height = vec![0u32; range.len()];
    let mut out = Vec::new();
    for n in range.rev() {
        let id = NodeId(n);
        let planned = wanted(g, id) && ((redo_root && id == root) || known(g, cache, id).is_none());
        if !planned {
            continue;
        }
        let h = 1 + g.nodes.children(id).map(|c| height[(c.0 - base) as usize]).max().unwrap_or(0);
        height[(n - base) as usize] = h;
        out.push((id, h));
    }
    // Stable within a height: source order.
    out.sort_by_key(|&(n, h)| (h, n.0));
    out
}

/// First line of `n`'s code that is not a doc comment or attribute.
pub fn signature(g: &Graph, n: NodeId) -> String {
    let line = g
        .source(n)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("#["))
        .unwrap_or("");
    let line = line.trim_end_matches('{').trim_end();
    if line.chars().count() > 160 { format!("{}…", line.chars().take(160).collect::<String>()) } else { line.into() }
}

fn rel_path(g: &Graph, n: NodeId) -> &str {
    let f = g.nodes.file[n.idx()];
    if f.is_none() { "" } else { g.strings.resolve(g.files.rel_path[f.idx()]) }
}

/// The question for `n`, given what is known about its children.
pub fn request(g: &Graph, cache: &SummaryCache, n: NodeId) -> Result<Request, Error> {
    let i = n.idx();
    let kind = g.nodes.kind[i];
    let code = g.source(n).replace("\r\n", "\n");
    let kids: Vec<NodeId> = g.nodes.children(n).filter(|&c| wanted(g, c)).collect();
    let mut p =
        format!("Summarize this Rust {} `{}` (file `{}`).\n\n", kind.label(), g.qualified_name(n), rel_path(g, n));
    if kids.is_empty() && code.len() > MAX_LEAF {
        return Err(Error::Config(format!("{} is too large to summarize ({} kB)", g.name(n), code.len() / 1024)));
    }
    if kids.is_empty() || code.len() <= FULL_CODE {
        p.push_str(&format!("```rust\n{code}\n```\n"));
    } else {
        // Header: everything before the first item (module docs, uses, the impl line).
        let start = g.nodes.bytes[i].start as usize;
        let first = g.nodes.bytes[kids[0].idx()].start as usize;
        let src = g.source(n);
        let mut cut = first.saturating_sub(start).min(3000).min(src.len());
        while !src.is_char_boundary(cut) {
            cut -= 1;
        }
        let header = src[..cut].replace("\r\n", "\n");
        p.push_str(&format!(
            "It is too long to show in full ({} lines); this is its beginning:\n```rust\n{}\n```\n",
            g.nodes.lines[i].len(),
            header.trim_end()
        ));
    }
    if !kids.is_empty() {
        p.push_str("\nIts items and what they do:\n");
        for &c in kids.iter().take(MAX_ITEMS) {
            let what = known(g, cache, c).unwrap_or("(not summarized)");
            p.push_str(&format!("- `{}`: {what}\n", signature(g, c)));
        }
        if kids.len() > MAX_ITEMS {
            p.push_str(&format!("- … and {} more items\n", kids.len() - MAX_ITEMS));
        }
    }
    if kind.is_module() || kind == NodeKind::Crate {
        p.push_str("\nSummarize the module as a whole.\n");
    }
    Ok(Request { system: SYSTEM.into(), prompt: p, max_tokens: 4096, effort: Effort::Low })
}

/// The answer as one line of plain text.
pub fn clean(answer: &str) -> String {
    let t = answer.trim().trim_matches('`').trim_matches('"').trim();
    let t = t.strip_prefix("Summary:").unwrap_or(t);
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A request for this node is on its way.
    Started(NodeId),
    /// Its summary, already in the cache under `key`.
    Done {
        node: NodeId,
        key: u64,
        text: String,
    },
    Failed {
        node: NodeId,
        error: Error,
    },
}

/// Summarize `planned` (from [`plan`]) with up to `concurrency` requests in
/// flight. Results go into `cache` as they arrive and are reported to `on`.
/// Stops early when `cancel` is set or an error says nothing else will work
/// either (returned); single failures are only reported.
pub fn run(
    backend: &dyn Backend,
    g: &Graph,
    planned: &[(NodeId, u32)],
    cache: &Mutex<SummaryCache>,
    concurrency: usize,
    cancel: &AtomicBool,
    on: &(dyn Fn(Event) + Sync),
) -> Result<usize, Error> {
    let fatal: Mutex<Option<Error>> = Mutex::new(None);
    let done = AtomicUsize::new(0);
    let lock = || cache.lock().unwrap_or_else(|e| e.into_inner());
    let mut rest = planned;
    while let Some(&(_, h)) = rest.first() {
        let len = rest.iter().take_while(|&&(_, x)| x == h).count();
        let (wave, next) = rest.split_at(len);
        rest = next;
        let at = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..concurrency.clamp(1, wave.len()) {
                s.spawn(|| {
                    loop {
                        let stop = cancel.load(Ordering::Relaxed) || fatal.lock().unwrap().is_some();
                        let k = at.fetch_add(1, Ordering::Relaxed);
                        if stop || k >= wave.len() {
                            return;
                        }
                        let n = wave[k].0;
                        let req = request(g, &lock(), n);
                        on(Event::Started(n));
                        let res = req.and_then(|r| backend.complete(&r, &mut |_| !cancel.load(Ordering::Relaxed)));
                        match res.map(|c| clean(&c.text)) {
                            Ok(text) if !text.is_empty() => {
                                let key = key(g, n);
                                lock().insert(key, text.clone());
                                done.fetch_add(1, Ordering::Relaxed);
                                on(Event::Done { node: n, key, text });
                            }
                            Ok(_) => on(Event::Failed { node: n, error: Error::Protocol("empty answer".into()) }),
                            Err(error) => {
                                if error.is_fatal() {
                                    fatal.lock().unwrap().get_or_insert(error.clone());
                                }
                                on(Event::Failed { node: n, error });
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = fatal.lock().unwrap().take() {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
    }
    Ok(done.into_inner())
}
