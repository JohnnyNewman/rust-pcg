//! Serial assembly: file-local parse results → global pre-order tables.

use crate::cache::{Fetch, Overlays, ParseCache, fetch};
use crate::parse::{CalleeForm, LOCAL_NONE};
use crate::pcg_comment::short_hash;
use crate::resolve::{CallForm, CallSites, Precise, resolve_edges};
use crate::scan::scan;
use pcg_core::*;
use rayon::prelude::*;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct BuildStats {
    pub files: usize,
    pub bytes: usize,
    pub lines: usize,
    pub nodes: usize,
    pub edges: usize,
    /// Call sites, and how many of them the language server answered.
    pub calls: usize,
    pub calls_precise: usize,
    pub comments: usize,
    pub files_with_parse_errors: usize,
    /// Files actually parsed this build (the rest came from the [`ParseCache`]).
    pub files_parsed: usize,
    pub t_scan: Duration,
    pub t_parse: Duration,
    pub t_assemble: Duration,
    pub t_resolve: Duration,
    pub t_total: Duration,
}

impl std::fmt::Display for BuildStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        write!(
            f,
            "{} files ({} parsed), {:.1} MB, {} lines → {} nodes, {} edges, {} @pcg comments ({} files with parse errors)\n\
             scan {:.1} ms | parse {:.1} ms | assemble {:.1} ms | resolve {:.1} ms | total {:.1} ms ({:.0} MB/s)",
            self.files,
            self.files_parsed,
            self.bytes as f64 / 1e6,
            self.lines,
            self.nodes,
            self.edges,
            self.comments,
            self.files_with_parse_errors,
            ms(self.t_scan),
            ms(self.t_parse),
            ms(self.t_assemble),
            ms(self.t_resolve),
            ms(self.t_total),
            self.bytes as f64 / 1e6 / self.t_total.as_secs_f64().max(1e-9),
        )
    }
}

fn synthetic(kind: NodeKind, name: Sym) -> NewNode {
    NewNode { kind, name, file: FileId::NONE, bytes: Span::default(), lines: Span::default(), content_hash: 0 }
}

/// Run the full static pipeline on a directory, parsing every file.
pub fn build_graph(root: &Path) -> (Graph, BuildStats) {
    build_graph_cached(root, &mut ParseCache::default())
}

/// Run the full static pipeline, re-parsing only files whose text changed
/// since `cache` was filled. Updates `cache` (and evicts deleted files).
pub fn build_graph_cached(root: &Path, cache: &mut ParseCache) -> (Graph, BuildStats) {
    build_graph_overlaid(root, cache, &Overlays::default())
}

/// [`build_graph_cached`], with unsaved editor buffers taking the place of
/// their files' on-disk text.
pub fn build_graph_overlaid(root: &Path, cache: &mut ParseCache, overlays: &Overlays) -> (Graph, BuildStats) {
    build_graph_with(root, cache, overlays, None)
}

/// [`build_graph_overlaid`], taking the language server's answers ([`Precise`])
/// over the name-based guess wherever they still apply.
pub fn build_graph_with(
    root: &Path,
    cache: &mut ParseCache,
    overlays: &Overlays,
    precise: Option<&Precise>,
) -> (Graph, BuildStats) {
    let t0 = Instant::now();
    let wall0 = std::time::SystemTime::now();
    let mut st = BuildStats::default();

    // --- scan -------------------------------------------------------------
    let sc = scan(root);
    st.t_scan = t0.elapsed();

    // --- parse (parallel, cached) ----------------------------------------
    let t = Instant::now();
    let fetched: Vec<_> = sc.files.par_iter().map(|f| fetch(cache, overlays, &f.path)).collect();
    st.files_parsed = fetched.iter().flatten().filter(|(_, how)| *how == Fetch::Parsed).count();
    cache.files.clear();
    for (f, r) in sc.files.iter().zip(&fetched) {
        if let Some((c, _)) = r {
            cache.files.insert(f.path.clone(), c.clone());
        }
    }
    cache.built_at = Some(wall0);
    let parsed: Vec<Option<(Arc<str>, Arc<crate::parse::FileSyntax>)>> =
        fetched.into_iter().map(|r| r.map(|(c, _)| (c.src, c.syn))).collect();
    st.t_parse = t.elapsed();

    // --- assemble (serial) ------------------------------------------------
    let t = Instant::now();
    let mut g = Graph { root: sc.root.clone(), ..Default::default() };
    let est_nodes: usize = parsed.iter().flatten().map(|(_, s)| s.items.kind.len() + 1).sum();
    g.nodes.reserve(est_nodes + sc.crates.len() + 1);
    // Build-local columns (not part of the persistent model).
    let mut impl_self: Vec<Sym> = Vec::with_capacity(est_nodes);
    let mut impl_trait: Vec<Sym> = Vec::with_capacity(est_nodes);
    let mut sites = CallSites::default();

    let ws_name = g.strings.intern(&sc.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let ws = g.nodes.open(NodeId::NONE, synthetic(NodeKind::Workspace, ws_name));
    impl_self.push(Sym::EMPTY);
    impl_trait.push(Sym::EMPTY);

    let mut i = 0;
    while i < sc.files.len() {
        let krate = sc.files[i].krate;
        let cname = g.strings.intern(&sc.crates[krate as usize].name);
        let crate_node = g.nodes.open(ws, synthetic(NodeKind::Crate, cname));
        impl_self.push(Sym::EMPTY);
        impl_trait.push(Sym::EMPTY);
        let mut stack: Vec<(&[String], NodeId)> = vec![(&[], crate_node)];

        while i < sc.files.len() && sc.files[i].krate == krate {
            let sf = &sc.files[i];
            i += 1;
            let Some((src, syn)) = &parsed[i - 1] else { continue };
            let mp: &[String] = &sf.module_path;

            while !mp.starts_with(stack.last().unwrap().0) {
                let (_, n) = stack.pop().unwrap();
                g.nodes.close(n);
            }
            // Create intermediate (file-less) modules, then the file's module.
            let mut module = stack.last().unwrap().1;
            let need_new = stack.last().unwrap().0.len() < mp.len() || g.nodes.file[module.idx()].is_some();
            if need_new {
                let from = stack.last().unwrap().0.len();
                for d in from..mp.len().max(from + 1) {
                    let seg = mp
                        .get(d)
                        .map(String::as_str)
                        .unwrap_or_else(|| Path::new(&sf.rel_path).file_stem().and_then(|s| s.to_str()).unwrap_or("?"));
                    let name = g.strings.intern(seg);
                    module = g.nodes.open(module, synthetic(NodeKind::FileModule, name));
                    impl_self.push(Sym::EMPTY);
                    impl_trait.push(Sym::EMPTY);
                    stack.push((&mp[..(d + 1).min(mp.len())], module));
                }
            }

            // Register the file.
            let file = FileId::from_idx(g.files.len());
            let line_count = src.lines().count() as u32;
            g.files.path.push(sf.path.clone());
            let rel = g.strings.intern(&sf.rel_path);
            g.files.rel_path.push(rel);
            g.files.source.push(src.clone());
            g.files.content_hash.push(syn.file_hash);
            g.files.line_count.push(line_count);
            g.files.module.push(module);
            let m = module.idx();
            g.nodes.file[m] = file;
            g.nodes.bytes[m] = Span::new(0, src.len() as u32);
            g.nodes.lines[m] = Span::new(0, line_count);
            g.nodes.content_hash[m] = syn.file_hash;
            st.bytes += src.len();
            st.lines += line_count as usize;
            st.files_with_parse_errors += syn.has_errors as usize;

            // Items: local pre-order → global pre-order.
            let it = &syn.items;
            let base = g.nodes.len() as u32;
            for l in 0..it.kind.len() {
                let parent = if it.parent[l] == LOCAL_NONE { module } else { NodeId(base + it.parent[l]) };
                let name = g.strings.intern(&it.name[l]);
                g.nodes.open(
                    parent,
                    NewNode {
                        kind: it.kind[l],
                        name,
                        file,
                        bytes: it.bytes[l],
                        lines: it.lines[l],
                        content_hash: it.hash[l],
                    },
                );
                impl_self.push(it.impl_self[l].as_deref().map_or(Sym::EMPTY, |s| g.strings.intern(s)));
                impl_trait.push(it.impl_trait[l].as_deref().map_or(Sym::EMPTY, |s| g.strings.intern(s)));
            }
            for l in 0..it.kind.len() {
                g.nodes.subtree_end[base as usize + l] = NodeId(base + it.subtree_end[l]);
            }

            // @pcg comments.
            for c in &syn.comments {
                let node = if c.item == LOCAL_NONE { module } else { NodeId(base + c.item) };
                let id = g.comments.push(node, c.kind, c.stored_hash, c.bytes, &c.text);
                match c.kind {
                    CommentKind::Summary => g.nodes.summary[node.idx()] = id,
                    CommentKind::Intent => g.nodes.intent[node.idx()] = id,
                }
            }

            // Call sites.
            for c in &syn.calls {
                let callee = g.strings.intern(&c.callee);
                let (form, qual) = match &c.form {
                    CalleeForm::Plain => (CallForm::Plain, Sym::EMPTY),
                    CalleeForm::Method => (CallForm::Method, Sym::EMPTY),
                    CalleeForm::Macro => (CallForm::Macro, Sym::EMPTY),
                    CalleeForm::Qualified(q) => (CallForm::Qualified, g.strings.intern(q)),
                };
                sites.push(callee, form, qual);
                g.calls.caller.push(NodeId(base + c.caller));
                g.calls.at.push(c.at);
            }
        }
        while let Some((_, n)) = stack.pop() {
            g.nodes.close(n);
        }
    }
    g.nodes.close(ws);
    drop(parsed);
    stable_keys(&mut g);

    // Summary freshness.
    for n in 0..g.nodes.len() {
        let c = g.nodes.summary[n];
        if c.is_some() {
            g.nodes.summary_state[n] = match g.comments.stored_hash[c.idx()] {
                None => SummaryState::Unhashed,
                Some(h) if h == short_hash(g.nodes.content_hash[n]) => SummaryState::Fresh,
                Some(_) => SummaryState::Stale,
            };
        }
    }
    st.t_assemble = t.elapsed();

    // --- resolve ----------------------------------------------------------
    let t = Instant::now();
    g.calls.resolution = vec![Resolution::None; sites.len()];
    g.calls.target = vec![NodeId::NONE; sites.len()];
    resolve_edges(&mut g, &sites, &impl_self, &impl_trait, precise);
    st.t_resolve = t.elapsed();

    st.files = g.files.len();
    st.nodes = g.nodes.len();
    st.edges = g.edges.len();
    st.calls = g.calls.len();
    st.calls_precise = g.calls.resolution.iter().filter(|r| **r == Resolution::Precise).count();
    st.comments = g.comments.len();
    st.t_total = t0.elapsed();
    (g, st)
}

/// Fill [`NodeTable::stable_key`]: `xxh3(parent key, kind, name, ordinal)`,
/// where the ordinal counts earlier siblings with the same kind and name
/// (e.g. several `impl Foo` blocks). One linear pre-order pass.
pub fn stable_keys(g: &mut Graph) {
    use xxhash_rust::xxh3::xxh3_64;
    let n = g.nodes.len();
    let mut occ: rustc_hash::FxHashMap<(u64, u8, u64), u32> = Default::default();
    occ.reserve(n);
    for i in 0..n {
        let p = g.nodes.parent[i];
        let pk = if p.is_none() { 0 } else { g.nodes.stable_key[p.idx()] };
        let kind = g.nodes.kind[i] as u8;
        let nh = xxh3_64(g.strings.resolve(g.nodes.name[i]).as_bytes());
        let o = occ.entry((pk, kind, nh)).or_insert(0);
        let mut buf = [0u8; 21];
        buf[..8].copy_from_slice(&pk.to_le_bytes());
        buf[8] = kind;
        buf[9..17].copy_from_slice(&nh.to_le_bytes());
        buf[17..].copy_from_slice(&o.to_le_bytes());
        *o += 1;
        g.nodes.stable_key[i] = xxh3_64(&buf);
    }
}
