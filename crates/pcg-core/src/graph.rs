//! SoA graph tables.

use crate::{CommentId, EdgeId, FileId, Interner, NodeId, Sym};
use std::path::PathBuf;

/// Half-open `u32` range, `Copy` unlike `std::ops::Range`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
    pub const fn len(self) -> u32 {
        self.end - self.start
    }
    pub const fn is_empty(self) -> bool {
        self.end <= self.start
    }
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum NodeKind {
    Workspace,
    Crate,
    /// A module backed by its own file.
    FileModule,
    /// `mod foo { ... }`
    InlineModule,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Fn,
    Const,
    Static,
    TypeAlias,
    Macro,
}

impl NodeKind {
    pub const fn is_container(self) -> bool {
        matches!(self, Self::Workspace | Self::Crate | Self::FileModule | Self::InlineModule | Self::Trait | Self::Impl)
    }
    pub const fn is_module(self) -> bool {
        matches!(self, Self::FileModule | Self::InlineModule)
    }
    /// Nodes that may carry persisted `@pcg` comments (decision 6).
    pub const fn is_summarizable(self) -> bool {
        !matches!(self, Self::Workspace | Self::Crate)
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Crate => "crate",
            Self::FileModule => "mod",
            Self::InlineModule => "mod",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Union => "union",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Fn => "fn",
            Self::Const => "const",
            Self::Static => "static",
            Self::TypeAlias => "type",
            Self::Macro => "macro",
        }
    }
}

/// Freshness of a node's persisted `@pcg:summary`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
#[repr(u8)]
pub enum SummaryState {
    /// No summary comment.
    #[default]
    Missing,
    /// Stored hash matches the code.
    Fresh,
    /// Stored hash differs from the code: the summary describes old code.
    Stale,
    /// Summary exists but carries no `h=` hash.
    Unhashed,
}

/// Nodes in pre-order. See crate docs for the hierarchy encoding.
#[derive(Default, Debug)]
pub struct NodeTable {
    pub kind: Vec<NodeKind>,
    pub name: Vec<Sym>,
    pub parent: Vec<NodeId>,
    /// One past the last descendant.
    pub subtree_end: Vec<NodeId>,
    pub depth: Vec<u8>,
    pub file: Vec<FileId>,
    /// Byte range of the item in its file (including leading doc comments).
    pub bytes: Vec<Span>,
    /// 0-based line range, end exclusive.
    pub lines: Vec<Span>,
    /// Hash of the code subtree, comments and whitespace excluded. 0 for synthetic nodes.
    pub content_hash: Vec<u64>,
    pub summary: Vec<CommentId>,
    pub intent: Vec<CommentId>,
    pub summary_state: Vec<SummaryState>,
    /// Identity that survives reloads: hash of (parent key, kind, name, ordinal
    /// among same-kind same-name siblings). `NodeId`s are pre-order positions and
    /// shift on every insertion; this key is what diffs, tweens, view state and
    /// selection are matched by across snapshots. Filled by the assemble stage.
    pub stable_key: Vec<u64>,
}

/// Everything needed to append one node.
#[derive(Clone, Copy, Debug)]
pub struct NewNode {
    pub kind: NodeKind,
    pub name: Sym,
    pub file: FileId,
    pub bytes: Span,
    pub lines: Span,
    pub content_hash: u64,
}

impl NodeTable {
    #[inline]
    pub fn len(&self) -> usize {
        self.kind.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.kind.is_empty()
    }

    pub fn reserve(&mut self, n: usize) {
        self.kind.reserve(n);
        self.name.reserve(n);
        self.parent.reserve(n);
        self.subtree_end.reserve(n);
        self.depth.reserve(n);
        self.file.reserve(n);
        self.bytes.reserve(n);
        self.lines.reserve(n);
        self.content_hash.reserve(n);
        self.summary.reserve(n);
        self.intent.reserve(n);
        self.summary_state.reserve(n);
        self.stable_key.reserve(n);
    }

    /// Append a node as the next pre-order entry under `parent`. The caller must
    /// call [`close`](Self::close) after appending all its descendants.
    pub fn open(&mut self, parent: NodeId, n: NewNode) -> NodeId {
        let id = NodeId::from_idx(self.len());
        let depth = if parent.is_none() { 0 } else { self.depth[parent.idx()].saturating_add(1) };
        self.kind.push(n.kind);
        self.name.push(n.name);
        self.parent.push(parent);
        self.subtree_end.push(NodeId(id.0 + 1));
        self.depth.push(depth);
        self.file.push(n.file);
        self.bytes.push(n.bytes);
        self.lines.push(n.lines);
        self.content_hash.push(n.content_hash);
        self.summary.push(CommentId::NONE);
        self.intent.push(CommentId::NONE);
        self.summary_state.push(SummaryState::Missing);
        self.stable_key.push(0);
        id
    }

    /// Mark the end of `id`'s subtree (everything appended since `open`).
    #[inline]
    pub fn close(&mut self, id: NodeId) {
        self.subtree_end[id.idx()] = NodeId::from_idx(self.len());
    }

    /// Iterate the direct children of `id`.
    #[inline]
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children { end: self.subtree_end[id.idx()].0, cur: id.0 + 1, subtree_end: &self.subtree_end }
    }

    /// All descendants (excluding `id`) as a contiguous id range.
    #[inline]
    pub fn descendants(&self, id: NodeId) -> std::ops::Range<u32> {
        id.0 + 1..self.subtree_end[id.idx()].0
    }

    #[inline]
    pub fn is_ancestor_of(&self, a: NodeId, d: NodeId) -> bool {
        a.0 < d.0 && d.0 < self.subtree_end[a.idx()].0
    }

    /// Ancestors from parent to root.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let first = Some(self.parent[id.idx()]).filter(|p| p.is_some());
        std::iter::successors(first, |p| Some(self.parent[p.idx()]).filter(|q| q.is_some()))
    }
}

pub struct Children<'a> {
    cur: u32,
    end: u32,
    subtree_end: &'a [NodeId],
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.cur >= self.end {
            return None;
        }
        let c = NodeId(self.cur);
        self.cur = self.subtree_end[c.idx()].0;
        Some(c)
    }
}

// ---------------------------------------------------------------------------
// Edges
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(u8)]
pub enum EdgeKind {
    Calls,
    Uses,
    Implements,
    DataFlow,
    Channel,
}

/// Explicit edges. Containment is implicit in the node hierarchy.
#[derive(Default, Debug)]
pub struct EdgeTable {
    pub src: Vec<NodeId>,
    pub dst: Vec<NodeId>,
    pub kind: Vec<EdgeKind>,
    /// Number of syntactic occurrences folded into this edge (e.g. call sites).
    pub weight: Vec<u32>,
    /// Derived CSR adjacency; rebuilt by [`EdgeTable::build_adjacency`].
    pub out: Adjacency,
    pub inc: Adjacency,
}

/// Compressed sparse rows: edges of node `n` are `edges[offsets[n]..offsets[n+1]]`.
#[derive(Default, Debug)]
pub struct Adjacency {
    pub offsets: Vec<u32>,
    pub edges: Vec<EdgeId>,
}

impl Adjacency {
    #[inline]
    pub fn of(&self, n: NodeId) -> &[EdgeId] {
        if n.idx() + 1 >= self.offsets.len() {
            return &[];
        }
        &self.edges[self.offsets[n.idx()] as usize..self.offsets[n.idx() + 1] as usize]
    }

    fn build(node_count: usize, key: &[NodeId]) -> Self {
        let mut offsets = vec![0u32; node_count + 1];
        for k in key {
            offsets[k.idx() + 1] += 1;
        }
        for i in 0..node_count {
            offsets[i + 1] += offsets[i];
        }
        let mut fill = offsets.clone();
        let mut edges = vec![EdgeId(0); key.len()];
        for (e, k) in key.iter().enumerate() {
            let slot = &mut fill[k.idx()];
            edges[*slot as usize] = EdgeId::from_idx(e);
            *slot += 1;
        }
        Self { offsets, edges }
    }
}

impl EdgeTable {
    pub fn len(&self) -> usize {
        self.src.len()
    }
    pub fn is_empty(&self) -> bool {
        self.src.is_empty()
    }
    pub fn push(&mut self, src: NodeId, dst: NodeId, kind: EdgeKind, weight: u32) -> EdgeId {
        let id = EdgeId::from_idx(self.len());
        self.src.push(src);
        self.dst.push(dst);
        self.kind.push(kind);
        self.weight.push(weight);
        id
    }
    pub fn build_adjacency(&mut self, node_count: usize) {
        self.out = Adjacency::build(node_count, &self.src);
        self.inc = Adjacency::build(node_count, &self.dst);
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
pub struct FileTable {
    pub path: Vec<PathBuf>,
    /// Path relative to the workspace root, `/`-separated, for display.
    pub rel_path: Vec<Sym>,
    pub source: Vec<std::sync::Arc<str>>,
    pub content_hash: Vec<u64>,
    pub line_count: Vec<u32>,
    /// The `FileModule` node that represents this file.
    pub module: Vec<NodeId>,
}

impl FileTable {
    pub fn len(&self) -> usize {
        self.path.len()
    }
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }
}

// ---------------------------------------------------------------------------
// @pcg comments
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum CommentKind {
    /// Derived from code (`@pcg:summary`).
    Summary,
    /// Authored by the user, drives generation (`@pcg:intent`).
    Intent,
}

#[derive(Default, Debug)]
pub struct CommentTable {
    pub node: Vec<NodeId>,
    pub kind: Vec<CommentKind>,
    /// Stored short hash (`h=`), if present.
    pub stored_hash: Vec<Option<u32>>,
    /// Byte range of the comment lines in the source file (for rewriting).
    pub bytes: Vec<Span>,
    /// Text, as a span into `text_buf`.
    pub text: Vec<Span>,
    pub text_buf: String,
}

impl CommentTable {
    pub fn len(&self) -> usize {
        self.node.len()
    }
    pub fn is_empty(&self) -> bool {
        self.node.is_empty()
    }
    pub fn push(
        &mut self,
        node: NodeId,
        kind: CommentKind,
        stored_hash: Option<u32>,
        bytes: Span,
        text: &str,
    ) -> CommentId {
        let id = CommentId::from_idx(self.len());
        let start = self.text_buf.len() as u32;
        self.text_buf.push_str(text);
        self.node.push(node);
        self.kind.push(kind);
        self.stored_hash.push(stored_hash);
        self.bytes.push(bytes);
        self.text.push(Span::new(start, self.text_buf.len() as u32));
        id
    }
    pub fn text(&self, c: CommentId) -> &str {
        &self.text_buf[self.text[c.idx()].range()]
    }
}

// ---------------------------------------------------------------------------
// Call sites
// ---------------------------------------------------------------------------

/// How a call site's target was determined.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Resolution {
    /// Not resolved (unknown name, ambiguous, or deliberately skipped).
    #[default]
    None,
    /// Matched by name (may be wrong, may have several targets).
    Heuristic,
    /// Answered by the language server: `target`, or outside the workspace.
    Precise,
}

/// Every call site (calls, method calls, macro invocations), SoA. The edges
/// are an aggregation of these.
#[derive(Default, Debug)]
pub struct CallTable {
    /// Innermost item containing the call.
    pub caller: Vec<NodeId>,
    /// Byte offset of the callee's name in the caller's file.
    pub at: Vec<u32>,
    pub resolution: Vec<Resolution>,
    /// The definition, for [`Resolution::Precise`] sites inside the workspace.
    pub target: Vec<NodeId>,
}

impl CallTable {
    pub fn len(&self) -> usize {
        self.caller.len()
    }
    pub fn is_empty(&self) -> bool {
        self.caller.is_empty()
    }
}

// ---------------------------------------------------------------------------
// The whole graph
// ---------------------------------------------------------------------------

/// One immutable snapshot of the analysed project.
#[derive(Default, Debug)]
pub struct Graph {
    pub root: PathBuf,
    pub strings: Interner,
    pub nodes: NodeTable,
    pub edges: EdgeTable,
    pub calls: CallTable,
    pub files: FileTable,
    pub comments: CommentTable,
}

impl Graph {
    #[inline]
    pub fn name(&self, n: NodeId) -> &str {
        self.strings.resolve(self.nodes.name[n.idx()])
    }

    /// `crate::a::b::Item`-style path for display.
    pub fn qualified_name(&self, n: NodeId) -> String {
        let mut parts: Vec<&str> = self
            .nodes
            .ancestors(n)
            .filter(|a| self.nodes.kind[a.idx()] != NodeKind::Workspace)
            .map(|a| self.name(a))
            .collect();
        parts.reverse();
        parts.push(self.name(n));
        parts.join("::")
    }

    /// Source text of a node (empty for synthetic nodes).
    pub fn source(&self, n: NodeId) -> &str {
        let f = self.nodes.file[n.idx()];
        if f.is_none() {
            return "";
        }
        let src = &self.files.source[f.idx()];
        src.get(self.nodes.bytes[n.idx()].range()).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nn(kind: NodeKind) -> NewNode {
        NewNode {
            kind,
            name: Sym::EMPTY,
            file: FileId::NONE,
            bytes: Span::default(),
            lines: Span::default(),
            content_hash: 0,
        }
    }

    #[test]
    fn preorder_hierarchy() {
        // 0 ws { 1 crate { 2 mod { 3 fn, 4 fn }, 5 mod } }
        let mut t = NodeTable::default();
        let ws = t.open(NodeId::NONE, nn(NodeKind::Workspace));
        let cr = t.open(ws, nn(NodeKind::Crate));
        let m = t.open(cr, nn(NodeKind::FileModule));
        let f1 = t.open(m, nn(NodeKind::Fn));
        t.close(f1);
        let f2 = t.open(m, nn(NodeKind::Fn));
        t.close(f2);
        t.close(m);
        let m2 = t.open(cr, nn(NodeKind::FileModule));
        t.close(m2);
        t.close(cr);
        t.close(ws);

        assert_eq!(t.children(cr).collect::<Vec<_>>(), vec![m, m2]);
        assert_eq!(t.children(m).collect::<Vec<_>>(), vec![f1, f2]);
        assert_eq!(t.children(f1).count(), 0);
        assert_eq!(t.descendants(ws), 1..6);
        assert_eq!(t.depth[f2.idx()], 3);
        assert!(t.is_ancestor_of(cr, f2));
        assert!(!t.is_ancestor_of(m2, f2));
        assert_eq!(t.ancestors(f2).collect::<Vec<_>>(), vec![m, cr, ws]);
    }

    #[test]
    fn adjacency() {
        let mut e = EdgeTable::default();
        e.push(NodeId(0), NodeId(2), EdgeKind::Calls, 1);
        e.push(NodeId(1), NodeId(2), EdgeKind::Calls, 3);
        e.push(NodeId(0), NodeId(1), EdgeKind::Calls, 1);
        e.build_adjacency(3);
        assert_eq!(e.out.of(NodeId(0)), &[EdgeId(0), EdgeId(2)]);
        assert_eq!(e.out.of(NodeId(2)), &[] as &[EdgeId]);
        assert_eq!(e.inc.of(NodeId(2)), &[EdgeId(0), EdgeId(1)]);
    }
}
