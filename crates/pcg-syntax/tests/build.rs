use pcg_core::*;
use pcg_syntax::{build_graph, summary_edit};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn find(g: &Graph, qname: &str) -> NodeId {
    (0..g.nodes.len())
        .map(NodeId::from_idx)
        .find(|&n| g.qualified_name(n) == qname)
        .unwrap_or_else(|| panic!("{qname} not found"))
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pcg-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn fixture(root: &Path) {
    write(root, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    write(root, "a/Cargo.toml", "[package]\nname = \"alpha\"\n");
    write(root, "a/src/lib.rs", "pub mod util;\npub fn entry() { util::help(); local(); }\nfn local() {}\n");
    write(root, "a/src/util/mod.rs", "pub fn help() { inner::deep(); }\npub mod inner;\n");
    write(root, "a/src/util/inner.rs", "pub fn deep() {}\npub trait Shape { fn area(&self) -> f32; }\n");
    write(root, "b/Cargo.toml", "[package]\nname = \"beta\"\n");
    write(
        root,
        "b/src/main.rs",
        "struct Sq(f32);\nimpl alpha::util::inner::Shape for Sq { fn area(&self) -> f32 { self.0 * self.0 } }\n\
         fn main() { let s = Sq(2.0); s.area(); Sq::new(); }\nimpl Sq { fn new() -> Self { Sq(1.0) } }\n",
    );
}

#[test]
fn hierarchy_and_edges() {
    let root = tmp("hier");
    fixture(&root);
    let (g, st) = build_graph(&root);
    assert_eq!(st.files, 4);

    let alpha = find(&g, "alpha");
    assert_eq!(g.nodes.kind[alpha.idx()], NodeKind::Crate);
    let deep = find(&g, "alpha::util::inner::deep");
    assert_eq!(g.nodes.kind[deep.idx()], NodeKind::Fn);
    assert!(g.nodes.is_ancestor_of(find(&g, "alpha::util"), deep));

    let has = |s: &str, d: &str, k: EdgeKind| {
        let (s, d) = (find(&g, s), find(&g, d));
        g.edges.out.of(s).iter().any(|e| g.edges.dst[e.idx()] == d && g.edges.kind[e.idx()] == k)
    };
    assert!(has("alpha::entry", "alpha::util::help", EdgeKind::Calls));
    assert!(has("alpha::entry", "alpha::local", EdgeKind::Calls));
    assert!(has("alpha::util::help", "alpha::util::inner::deep", EdgeKind::Calls));
    assert!(has("beta::main", "beta::alpha::util::inner::Shape for Sq::area", EdgeKind::Calls));
    assert!(has("beta::main", "beta::Sq::new", EdgeKind::Calls));
    assert!(has("beta::alpha::util::inner::Shape for Sq", "alpha::util::inner::Shape", EdgeKind::Implements));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn summary_roundtrip_fresh_then_stale() {
    let root = tmp("sum");
    write(&root, "Cargo.toml", "[package]\nname = \"s\"\n");
    let lib = root.join("src/lib.rs");
    write(&root, "src/lib.rs", "/// Docs.\npub fn f(x: u32) -> u32 {\n    x + 1\n}\n");

    let (g, _) = build_graph(&root);
    let f = find(&g, "s::f");
    assert_eq!(g.nodes.summary_state[f.idx()], SummaryState::Missing);

    // Write a summary → fresh.
    let e = summary_edit(&g, f, "Adds one.").unwrap();
    fs::write(&lib, e.apply(&g.files.source[e.file.idx()])).unwrap();
    let (g, _) = build_graph(&root);
    let f = find(&g, "s::f");
    assert_eq!(g.nodes.summary_state[f.idx()], SummaryState::Fresh);
    assert_eq!(g.comments.text(g.nodes.summary[f.idx()]), "Adds one.");

    // Reformatting keeps it fresh; changing code makes it stale.
    let src = fs::read_to_string(&lib).unwrap();
    fs::write(&lib, src.replace("    x + 1\n", "  x  +  1 // comment\n")).unwrap();
    let (g, _) = build_graph(&root);
    assert_eq!(g.nodes.summary_state[find(&g, "s::f").idx()], SummaryState::Fresh);
    let src = fs::read_to_string(&lib).unwrap();
    fs::write(&lib, src.replace("x  +  1", "x + 2")).unwrap();
    let (g, _) = build_graph(&root);
    let f = find(&g, "s::f");
    assert_eq!(g.nodes.summary_state[f.idx()], SummaryState::Stale);

    // Rewriting replaces (not duplicates) the comment → fresh again.
    let e = summary_edit(&g, f, "Adds two.").unwrap();
    let new = e.apply(&g.files.source[e.file.idx()]);
    assert_eq!(new.matches("@pcg:summary").count(), 1);
    fs::write(&lib, new).unwrap();
    let (g, _) = build_graph(&root);
    let f = find(&g, "s::f");
    assert_eq!(g.nodes.summary_state[f.idx()], SummaryState::Fresh);
    assert_eq!(g.comments.text(g.nodes.summary[f.idx()]), "Adds two.");
    fs::remove_dir_all(&root).ok();
}
