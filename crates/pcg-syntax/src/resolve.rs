//! Edge resolution: call sites → weighted edges.
//!
//! A call site the language server has answered ([`Precise`]) is taken at its
//! word: one edge to the definition, or none if that lies outside the
//! workspace. Every other site is resolved by name:
//!
//! For each call site, candidates are definitions with the callee's name,
//! filtered by call form (free fn / method / `Qual::f` / macro), then ranked
//! by locality: same file → same crate → anywhere. The best non-empty tier is
//! used; if it has more than [`MAX_FANOUT`] candidates the call is considered
//! ambiguous and dropped. Call sites between the same pair are folded into one
//! weighted edge.

use pcg_core::*;
use rustc_hash::FxHashMap;
use std::path::PathBuf;

/// Call targets as answered by a language server, valid for specific file
/// texts. Applied by the build to every call site whose file — and whose
/// target's file — still has exactly that text.
#[derive(Default, Debug, Clone)]
pub struct Precise {
    /// Hash ([`text_hash`]) of each file's text the answers were computed for.
    pub files: FxHashMap<PathBuf, u64>,
    /// Per file: call site (byte offset of the callee's name) → definition
    /// (file, byte offset), or `None` if it is defined outside the workspace.
    pub sites: FxHashMap<PathBuf, FxHashMap<u32, Option<(PathBuf, u32)>>>,
}

pub fn text_hash(text: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(text.as_bytes())
}

/// The innermost node of `file` whose bytes contain `at`.
pub fn node_at(g: &Graph, file: FileId, at: u32) -> NodeId {
    let m = g.files.module[file.idx()];
    let mut best = m;
    for n in m.0 + 1..g.nodes.subtree_end[m.idx()].0 {
        let i = n as usize;
        // Pre-order: a later match is nested in (or equal to) the earlier one.
        if g.nodes.file[i] == file && g.nodes.bytes[i].start <= at && at < g.nodes.bytes[i].end {
            best = NodeId(n);
        }
    }
    best
}

/// Fill `g.calls.resolution` / `target` from the language server's answers.
fn apply_precise(g: &mut Graph, precise: &Precise) {
    if precise.sites.is_empty() {
        return;
    }
    // Files whose text is the one the answers were computed for.
    let current: FxHashMap<&PathBuf, FileId> = (0..g.files.len())
        .filter(|&f| precise.files.get(&g.files.path[f]) == Some(&text_hash(&g.files.source[f])))
        .map(|f| (&g.files.path[f], FileId::from_idx(f)))
        .collect();
    for s in 0..g.calls.len() {
        let caller = g.calls.caller[s];
        let file = g.nodes.file[caller.idx()];
        let path = &g.files.path[file.idx()];
        if !current.contains_key(path) {
            continue;
        }
        let Some(answer) = precise.sites.get(path).and_then(|m| m.get(&g.calls.at[s])) else { continue };
        let target = match answer {
            None => NodeId::NONE,
            Some((tpath, at)) => {
                let Some(&tf) = current.get(tpath) else { continue };
                let t = node_at(g, tf, *at);
                // A definition inside the calling function's body is a local
                // (closure, nested item the graph does not show): no edge.
                let body = &g.files.source[tf.idx()][g.nodes.bytes[t.idx()].start as usize..*at as usize];
                if t == caller && body.contains('{') { NodeId::NONE } else { t }
            }
        };
        g.calls.resolution[s] = Resolution::Precise;
        g.calls.target[s] = target;
    }
}

pub const MAX_FANOUT: usize = 4;

/// Method names so common in std / core traits that an unqualified `x.name()`
/// almost never refers to a workspace definition. Without type information
/// these produce mostly false edges, so they are not resolved.
pub const STD_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "push",
    "pop",
    "get",
    "get_mut",
    "insert",
    "remove",
    "contains",
    "contains_key",
    "iter",
    "iter_mut",
    "into_iter",
    "clone",
    "cloned",
    "copied",
    "to_string",
    "to_owned",
    "into",
    "from",
    "as_ref",
    "as_mut",
    "unwrap",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "expect",
    "map",
    "map_err",
    "and_then",
    "ok",
    "err",
    "ok_or",
    "default",
    "new",
    "next",
    "collect",
    "extend",
    "clear",
    "reserve",
    "fmt",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "borrow",
    "borrow_mut",
    "deref",
    "deref_mut",
    "drop",
    "index",
    "write",
    "read",
    "flush",
    "join",
    "split",
    "find",
    "filter",
    "filter_map",
    "fold",
    "sum",
    "count",
    "min",
    "max",
    "sort",
    "sort_by",
    "sort_by_key",
    "first",
    "last",
    "keys",
    "values",
    "entry",
    "take",
    "replace",
    "swap",
    "resize",
    "truncate",
    "drain",
    "as_str",
    "as_slice",
    "as_bytes",
    "parse",
    "lines",
    "chars",
    "bytes",
    "trim",
    "starts_with",
    "ends_with",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "lock",
    "send",
    "recv",
    "spawn",
    "with_capacity",
    "to_vec",
    "rev",
    "zip",
    "enumerate",
    "skip",
    "chain",
    "any",
    "all",
    "position",
    "flatten",
    "flat_map",
    "for_each",
    "step_by",
    "windows",
    "chunks",
    "abs",
    "clamp",
    "fill",
    "retain",
    "dedup",
    "append",
    "split_off",
    "push_str",
    "format",
    "display",
    "to_lowercase",
    "to_uppercase",
    "set",
    "add",
    "sub",
    "mul",
    "div",
    "neg",
    "not",
    "poll",
    "wake",
    "call",
    "run",
    "build",
    "show",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CallForm {
    Plain,
    Qualified,
    Method,
    Macro,
}

/// What the name-based resolution needs to know about each call site
/// (parallel to `Graph::calls`).
#[derive(Default, Debug)]
pub struct CallSites {
    pub callee: Vec<Sym>,
    pub form: Vec<CallForm>,
    pub qual: Vec<Sym>,
}

impl CallSites {
    pub fn push(&mut self, callee: Sym, form: CallForm, qual: Sym) {
        self.callee.push(callee);
        self.form.push(form);
        self.qual.push(qual);
    }
    pub fn len(&self) -> usize {
        self.callee.len()
    }
    pub fn is_empty(&self) -> bool {
        self.callee.is_empty()
    }
}

/// Resolve calls and trait impls into `g.edges`, then build adjacency.
pub fn resolve_edges(
    g: &mut Graph,
    sites: &CallSites,
    impl_self: &[Sym],
    impl_trait: &[Sym],
    precise: Option<&Precise>,
) {
    if let Some(p) = precise {
        apply_precise(g, p);
    }
    let nodes = &g.nodes;
    let n = nodes.len();

    // crate_of: pre-order makes this a single forward pass.
    let mut crate_of = vec![NodeId::NONE; n];
    for i in 0..n {
        crate_of[i] = if nodes.kind[i] == NodeKind::Crate {
            NodeId::from_idx(i)
        } else if nodes.parent[i].is_some() {
            crate_of[nodes.parent[i].idx()]
        } else {
            NodeId::NONE
        };
    }

    // Candidate indexes, one per call form, so each call site only visits
    // functions that can match it (the old single name → fns map made common
    // names like `new` cost O(#fns named new) per call site).
    //   free:      name → fns directly in a module/crate        (Plain, crate::/super::/self::)
    //   in_crate:  (crate, name) → fns in an impl/trait          (Method: same crate only)
    //   qualified: (name, qualifier) → fns in an impl/trait/module named by the qualifier
    let mut free: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    let mut in_crate: FxHashMap<(NodeId, Sym), Vec<NodeId>> = FxHashMap::default();
    let mut qualified: FxHashMap<(Sym, Sym), Vec<NodeId>> = FxHashMap::default();
    let mut macros: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    let mut traits: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    for (i, &kind) in nodes.kind.iter().enumerate() {
        let id = NodeId::from_idx(i);
        match kind {
            NodeKind::Fn => {
                let name = nodes.name[i];
                let p = nodes.parent[i];
                let pk = nodes.kind[p.idx()];
                if pk.is_module() || pk == NodeKind::Crate {
                    free.entry(name).or_default().push(id);
                }
                if matches!(pk, NodeKind::Impl | NodeKind::Trait) {
                    in_crate.entry((crate_of[i], name)).or_default().push(id);
                }
                if pk == NodeKind::Impl {
                    let (s, t) = (impl_self[p.idx()], impl_trait[p.idx()]);
                    qualified.entry((name, s)).or_default().push(id);
                    if t != s {
                        qualified.entry((name, t)).or_default().push(id);
                    }
                } else {
                    qualified.entry((name, nodes.name[p.idx()])).or_default().push(id);
                }
            }
            NodeKind::Macro => macros.entry(nodes.name[i]).or_default().push(id),
            NodeKind::Trait => traits.entry(nodes.name[i]).or_default().push(id),
            _ => {}
        }
    }

    let s_self = g.strings.get("Self");
    let s_crate = g.strings.get("crate");
    let s_super = g.strings.get("super");
    let s_selfmod = g.strings.get("self");

    let impl_traits: rustc_hash::FxHashSet<Sym> = impl_trait.iter().copied().filter(|s| *s != Sym::EMPTY).collect();
    let std_methods: rustc_hash::FxHashSet<Sym> = STD_METHODS.iter().filter_map(|m| g.strings.get(m)).collect();
    let enclosing_impl = |c: NodeId| nodes.ancestors(c).find(|a| nodes.kind[a.idx()] == NodeKind::Impl);

    let mut acc: FxHashMap<(u32, u32, EdgeKind), u32> = FxHashMap::default();
    let mut tier: [Vec<NodeId>; 3] = Default::default();

    // `max_tier`: 2 = anywhere, 1 = same crate at most. Returns whether an edge was made.
    let mut pick = |src: NodeId,
                    cands: &mut dyn Iterator<Item = NodeId>,
                    kind: EdgeKind,
                    max_tier: usize,
                    acc: &mut FxHashMap<_, u32>|
     -> bool {
        for t in tier.iter_mut() {
            t.clear();
        }
        let (sf, sc) = (nodes.file[src.idx()], crate_of[src.idx()]);
        for c in cands {
            let t = if nodes.file[c.idx()] == sf {
                0
            } else if crate_of[c.idx()] == sc {
                1
            } else {
                2
            };
            tier[t].push(c);
        }
        if let Some(best) = tier[..=max_tier].iter().find(|t| !t.is_empty())
            && best.len() <= MAX_FANOUT
        {
            for &d in best {
                *acc.entry((src.0, d.0, kind)).or_default() += 1;
            }
            return true;
        }
        false
    };

    let mut resolution = std::mem::take(&mut g.calls.resolution);
    #[allow(clippy::needless_range_loop)] // `s` indexes four parallel tables
    for s in 0..sites.len() {
        let caller = g.calls.caller[s];
        let callee = sites.callee[s];
        if resolution[s] == Resolution::Precise {
            let t = g.calls.target[s];
            if t.is_some() {
                *acc.entry((caller.0, t.0, EdgeKind::Calls)).or_default() += 1;
            }
            continue;
        }
        let mut found = false;
        match sites.form[s] {
            CallForm::Macro => {
                if let Some(c) = macros.get(&callee) {
                    found = pick(caller, &mut c.iter().copied(), EdgeKind::Calls, 2, &mut acc);
                }
            }
            form => {
                if form == CallForm::Method && std_methods.contains(&callee) {
                    continue;
                }
                let mut q = sites.qual[s];
                if form == CallForm::Qualified && Some(q) == s_self {
                    q = enclosing_impl(caller).map_or(Sym::EMPTY, |i| impl_self[i.idx()]);
                }
                // `Trait::f()` for a trait not defined in the workspace (e.g. `Default::default()`)
                // cannot be resolved by name.
                let foreign_trait = form == CallForm::Qualified && !traits.contains_key(&q) && impl_traits.contains(&q);
                if foreign_trait {
                    continue;
                }
                let modq =
                    form == CallForm::Qualified && (Some(q) == s_crate || Some(q) == s_super || Some(q) == s_selfmod);
                let cands = match form {
                    CallForm::Plain => free.get(&callee),
                    CallForm::Method => in_crate.get(&(crate_of[caller.idx()], callee)),
                    _ if modq => free.get(&callee),
                    _ => qualified.get(&(callee, q)),
                };
                let Some(c) = cands else { continue };
                let mut it = c.iter().copied();
                // Unqualified method calls have no type info: stay inside the caller's crate.
                let max_tier = if form == CallForm::Method { 1 } else { 2 };
                found = pick(caller, &mut it, EdgeKind::Calls, max_tier, &mut acc);
            }
        }
        if found {
            resolution[s] = Resolution::Heuristic;
        }
    }

    // impl Trait for T  →  Implements edge to the trait.
    for (i, &tr) in impl_trait.iter().enumerate().take(n) {
        if nodes.kind[i] == NodeKind::Impl
            && tr != Sym::EMPTY
            && let Some(t) = traits.get(&tr)
        {
            pick(NodeId::from_idx(i), &mut t.iter().copied(), EdgeKind::Implements, 2, &mut acc);
        }
    }

    let mut list: Vec<_> = acc.into_iter().collect();
    list.sort_unstable_by_key(|&((s, d, k), _)| (s, d, k));
    g.edges = EdgeTable::default();
    for ((s, d, k), w) in list {
        g.edges.push(NodeId(s), NodeId(d), k, w);
    }
    g.edges.build_adjacency(n);
    g.calls.resolution = resolution;
}
