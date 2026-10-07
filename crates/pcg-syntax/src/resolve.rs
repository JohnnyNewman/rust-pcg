//! Name-based edge resolution (heuristic until LSP integration, M6).
//!
//! For each call site, candidates are definitions with the callee's name,
//! filtered by call form (free fn / method / `Qual::f` / macro), then ranked
//! by locality: same file → same crate → anywhere. The best non-empty tier is
//! used; if it has more than [`MAX_FANOUT`] candidates the call is considered
//! ambiguous and dropped. Call sites between the same pair are folded into one
//! weighted edge.

use pcg_core::*;
use rustc_hash::FxHashMap;

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

/// SoA list of call sites with global caller ids.
#[derive(Default, Debug)]
pub struct CallSites {
    pub caller: Vec<NodeId>,
    pub callee: Vec<Sym>,
    pub form: Vec<CallForm>,
    pub qual: Vec<Sym>,
}

impl CallSites {
    pub fn push(&mut self, caller: NodeId, callee: Sym, form: CallForm, qual: Sym) {
        self.caller.push(caller);
        self.callee.push(callee);
        self.form.push(form);
        self.qual.push(qual);
    }
    pub fn len(&self) -> usize {
        self.caller.len()
    }
    pub fn is_empty(&self) -> bool {
        self.caller.is_empty()
    }
}

/// Resolve calls and trait impls into `g.edges`, then build adjacency.
pub fn resolve_edges(g: &mut Graph, sites: &CallSites, impl_self: &[Sym], impl_trait: &[Sym]) {
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

    let mut fns: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    let mut macros: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    let mut traits: FxHashMap<Sym, Vec<NodeId>> = FxHashMap::default();
    for i in 0..n {
        let id = NodeId::from_idx(i);
        match nodes.kind[i] {
            NodeKind::Fn => fns.entry(nodes.name[i]).or_default().push(id),
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
    let parent_kind = |c: NodeId| nodes.kind[nodes.parent[c.idx()].idx()];
    let enclosing_impl = |c: NodeId| nodes.ancestors(c).find(|a| nodes.kind[a.idx()] == NodeKind::Impl);

    let mut acc: FxHashMap<(u32, u32, EdgeKind), u32> = FxHashMap::default();
    let mut tier: [Vec<NodeId>; 3] = Default::default();

    // `max_tier`: 2 = anywhere, 1 = same crate at most.
    let mut pick = |src: NodeId,
                    cands: &mut dyn Iterator<Item = NodeId>,
                    kind: EdgeKind,
                    max_tier: usize,
                    acc: &mut FxHashMap<_, u32>| {
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
        }
    };

    for s in 0..sites.len() {
        let caller = sites.caller[s];
        let callee = sites.callee[s];
        match sites.form[s] {
            CallForm::Macro => {
                if let Some(c) = macros.get(&callee) {
                    pick(caller, &mut c.iter().copied(), EdgeKind::Calls, 2, &mut acc);
                }
            }
            form => {
                if form == CallForm::Method && std_methods.contains(&callee) {
                    continue;
                }
                let Some(c) = fns.get(&callee) else { continue };
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
                let mut it = c.iter().copied().filter(|&d| {
                    let p = nodes.parent[d.idx()];
                    let pk = parent_kind(d);
                    match form {
                        CallForm::Plain => pk.is_module() || pk == NodeKind::Crate,
                        CallForm::Method => matches!(pk, NodeKind::Impl | NodeKind::Trait),
                        _ if modq => pk.is_module() || pk == NodeKind::Crate,
                        _ => match pk {
                            NodeKind::Impl => impl_self[p.idx()] == q || impl_trait[p.idx()] == q,
                            _ => nodes.name[p.idx()] == q, // trait or module named `q`
                        },
                    }
                });
                // Unqualified method calls have no type info: stay inside the caller's crate.
                let max_tier = if form == CallForm::Method { 1 } else { 2 };
                pick(caller, &mut it, EdgeKind::Calls, max_tier, &mut acc);
            }
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
}
