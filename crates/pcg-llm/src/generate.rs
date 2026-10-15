//! Intent → code: rewrite one node so it does what its intent says.
//!
//! The prompt is graph-aware: besides the node's code it carries what is
//! around it — the other items of its container, its callers and callees —
//! each as a signature plus summary, so the model sees the neighbourhood
//! without being sent whole files. The answer becomes a [`Proposal`]: new
//! text for the node's byte span, with the intent as an `@pcg:intent` comment
//! on top. Nothing is written here; the editor shows the proposal as unsaved
//! text (the graph animates the diff) and the user accepts or rejects it.

use crate::backend::{Backend, Effort, Error, Request};
use crate::cache::SummaryCache;
use crate::summarize::{known, signature};
use pcg_core::{Graph, NodeId, NodeKind};
use std::ops::Range;
use std::path::PathBuf;

/// Neighbours listed per group (siblings, callers, callees).
const MAX_NEIGHBOURS: usize = 40;

const SYSTEM: &str = "You are the code generator of a visual Rust IDE. The user states, as an intent, what \
one item of their code base should do; you rewrite that item so that it does it. Reply with exactly one \
```rust fenced code block holding the complete new item, ready to replace the old one in its file. Keep \
its name, and its signature unless the intent needs a different one; keep its indentation, its useful doc \
comments and attributes; use the surrounding items listed when they help, and do not invent APIs that \
are not shown or standard. Leave out any `@pcg:` comment lines: the editor writes those. Private helper \
items may follow the item inside the same block if they are needed. No explanation outside the block \
beyond one short sentence.";

/// New text for one node, not yet applied anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Identity of the rewritten node (`stable_key`).
    pub node_key: u64,
    pub file: PathBuf,
    /// [`pcg_syntax::text_hash`] of the file text `span` refers to.
    pub file_hash: u64,
    /// Bytes of the node in that text.
    pub span: Range<usize>,
    /// The node's text before and after, LF line endings.
    pub old: String,
    pub new: String,
    pub intent: String,
}

fn indent_of(src: &str, at: usize) -> &str {
    let ls = src[..at].rfind('\n').map_or(0, |i| i + 1);
    let pre = &src[ls..at];
    if pre.chars().all(|c| c == ' ' || c == '\t') { pre } else { "" }
}

fn is_whole_file(g: &Graph, n: NodeId) -> bool {
    let f = g.nodes.file[n.idx()];
    f.is_some() && g.files.module[f.idx()] == n
}

fn list(g: &Graph, cache: &SummaryCache, p: &mut String, title: &str, nodes: &[NodeId]) {
    if nodes.is_empty() {
        return;
    }
    p.push_str(&format!("\n{title}:\n"));
    for &c in nodes.iter().take(MAX_NEIGHBOURS) {
        let what = known(g, cache, c).map(|s| format!(" — {s}")).unwrap_or_default();
        p.push_str(&format!("- `{}` (in `{}`){what}\n", signature(g, c), g.qualified_name(c)));
    }
    if nodes.len() > MAX_NEIGHBOURS {
        p.push_str(&format!("- … and {} more\n", nodes.len() - MAX_NEIGHBOURS));
    }
}

/// The request for rewriting `n` according to `intent`.
pub fn request(g: &Graph, cache: &SummaryCache, n: NodeId, intent: &str) -> Request {
    let i = n.idx();
    let f = g.nodes.file[i];
    let src = &g.files.source[f.idx()];
    let start = g.nodes.bytes[i].start as usize;
    let indent = indent_of(src, start);
    let code = format!("{indent}{}", g.source(n)).replace("\r\n", "\n");
    let kind = g.nodes.kind[i];
    let mut p = format!(
        "Intent for the {} `{}` (file `{}`):\n{}\n\nIts current code:\n```rust\n{code}\n```\n",
        kind.label(),
        g.qualified_name(n),
        g.strings.resolve(g.files.rel_path[f.idx()]),
        intent.trim(),
    );
    if kind == NodeKind::FileModule {
        p.push_str("\nThis is the whole file: reply with the whole new file.\n");
    }
    let parent = g.nodes.parent[i];
    if parent.is_some() && g.nodes.kind[parent.idx()].is_summarizable() {
        let sib: Vec<NodeId> = g.nodes.children(parent).filter(|&c| c != n).collect();
        list(g, cache, &mut p, &format!("Other items in `{}`", g.qualified_name(parent)), &sib);
    }
    let mut callees: Vec<NodeId> = g.edges.out.of(n).iter().map(|e| g.edges.dst[e.idx()]).collect();
    let mut callers: Vec<NodeId> = g.edges.inc.of(n).iter().map(|e| g.edges.src[e.idx()]).collect();
    callees.dedup();
    callers.dedup();
    list(g, cache, &mut p, "It calls", &callees);
    list(g, cache, &mut p, "It is called by (keep these working)", &callers);
    Request { system: SYSTEM.into(), prompt: p, max_tokens: 32_000, effort: Effort::High }
}

/// The code inside the answer's first fenced block (the whole answer if it has none).
pub fn extract_code(answer: &str) -> Option<String> {
    let mut lines = answer.lines();
    let mut fenced = false;
    let mut out = Vec::new();
    for l in lines.by_ref() {
        if l.trim_start().starts_with("```") {
            fenced = true;
            break;
        }
    }
    if !fenced {
        let t = answer.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    for l in lines {
        if l.trim_start().starts_with("```") {
            break;
        }
        out.push(l);
    }
    let code = out.join("\n");
    (!code.trim().is_empty()).then_some(code)
}

/// Turn the model's answer into a proposal for `n`.
pub fn proposal(g: &Graph, n: NodeId, intent: &str, answer: &str) -> Result<Proposal, Error> {
    let i = n.idx();
    let f = g.nodes.file[i];
    let src = &g.files.source[f.idx()];
    let span = g.nodes.bytes[i].range();
    let indent = indent_of(src, span.start);
    let whole = is_whole_file(g, n);
    let code = extract_code(answer).ok_or_else(|| Error::Protocol("no code in the answer".into()))?;
    let code = pcg_syntax::strip_tags(&code.replace("\r\n", "\n"));
    // The span starts after the first line's indentation; the rest keeps theirs.
    let code = code.trim_start_matches('\n');
    let code = code.strip_prefix(indent).unwrap_or(code.trim_start_matches([' ', '\t']));
    let code = code.trim_end();
    let prefix = if whole { "//!" } else { "///" };
    let tag = pcg_syntax::format_intent(prefix, indent, intent, 100);
    let mut new = format!("{}{indent}{code}", &tag[indent.len()..]);
    if whole {
        new.push('\n');
    }
    Ok(Proposal {
        node_key: g.nodes.stable_key[i],
        file: g.files.path[f.idx()].clone(),
        file_hash: pcg_syntax::text_hash(src),
        old: src[span.clone()].replace("\r\n", "\n"),
        span,
        new,
        intent: intent.trim().to_string(),
    })
}

/// Ask `backend` for a rewrite of `n`; `on_text` sees the answer stream in.
pub fn generate(
    backend: &dyn Backend,
    g: &Graph,
    cache: &SummaryCache,
    n: NodeId,
    intent: &str,
    on_text: &mut dyn FnMut(&str) -> bool,
) -> Result<Proposal, Error> {
    let i = n.idx();
    if g.nodes.file[i].is_none() || !g.nodes.kind[i].is_summarizable() {
        return Err(Error::Config("this node has no code of its own to rewrite".into()));
    }
    if intent.trim().is_empty() {
        return Err(Error::Config("write an intent first".into()));
    }
    let c = backend.complete(&request(g, cache, n, intent), on_text)?;
    proposal(g, n, intent, &c.text)
}
