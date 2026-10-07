//! End to end against a real rust-analyzer (skipped if it is not installed).

use pcg_core::*;
use pcg_lsp::{Client, resolve};
use pcg_syntax::{Overlays, ParseCache, build_graph_with};
use std::fs;
use std::time::Duration;

const LIB: &str = r#"pub struct A;
pub struct B;
impl A {
    pub fn size(&self) -> u32 { 1 }
    pub fn get(&self) -> u32 { 1 }
}
impl B {
    pub fn size(&self) -> u32 { 2 }
    pub fn get(&self) -> u32 { 2 }
}
pub fn use_a(a: &A) -> u32 { a.size() + a.get() }
pub fn use_vec(v: &Vec<u32>) -> usize { helper(v.len()) }
fn helper(n: usize) -> usize { let twice = |x: usize| x * 2; twice(n) }
"#;

fn find(g: &Graph, qname: &str) -> NodeId {
    (0..g.nodes.len())
        .map(NodeId::from_idx)
        .find(|&n| g.qualified_name(n) == qname)
        .unwrap_or_else(|| panic!("{qname} not found"))
}

fn callees(g: &Graph, n: NodeId) -> Vec<String> {
    let mut v: Vec<String> = g.edges.out.of(n).iter().map(|e| g.qualified_name(g.edges.dst[e.idx()])).collect();
    v.sort();
    v
}

#[test]
fn precise_targets_replace_the_guess() {
    if std::process::Command::new("rust-analyzer").arg("--version").output().is_err() {
        eprintln!("rust-analyzer not found: skipped");
        return;
    }
    let root = std::env::temp_dir().join(format!("pcg-lsp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    fs::write(root.join("src/lib.rs"), LIB).unwrap();

    // By name: `size` matches both impls, `get` is too common to guess at all.
    let mut cache = ParseCache::default();
    let (g, st) = build_graph_with(&root, &mut cache, &Overlays::default(), None);
    assert_eq!(st.calls_precise, 0);
    assert_eq!(callees(&g, find(&g, "demo::use_a")), ["demo::A::size", "demo::B::size"]);

    let mut client = Client::start(&root).expect("start rust-analyzer");
    assert!(client.wait_ready(Duration::from_secs(300)).unwrap(), "workspace loaded");
    let precise = resolve(&mut client, &g, &mut |_, _| {}).unwrap();

    let (g, st) = build_graph_with(&root, &mut cache, &Overlays::default(), Some(&precise));
    assert_eq!(st.calls, 5);
    assert_eq!(st.calls_precise, 5, "every call site answered");
    assert_eq!(callees(&g, find(&g, "demo::use_a")), ["demo::A::get", "demo::A::size"]);
    // `Vec::len` is outside the workspace; the closure call is local to `helper`.
    assert_eq!(callees(&g, find(&g, "demo::use_vec")), ["demo::helper"]);
    assert!(callees(&g, find(&g, "demo::helper")).is_empty());

    // The answers are tied to the text: change the file and the guess is back…
    fs::write(root.join("src/lib.rs"), LIB.replace("a.size() + a.get()", "a.size()")).unwrap();
    let (g2, st) = build_graph_with(&root, &mut cache, &Overlays::default(), Some(&precise));
    assert_eq!(st.calls_precise, 0);
    assert_eq!(callees(&g2, find(&g2, "demo::use_a")), ["demo::A::size", "demo::B::size"]);
    // …until the server is asked again, with the new text (it is not read from disk).
    let precise = resolve(&mut client, &g2, &mut |_, _| {}).unwrap();
    let (g3, st) = build_graph_with(&root, &mut cache, &Overlays::default(), Some(&precise));
    assert_eq!((st.calls, st.calls_precise), (4, 4));
    assert_eq!(callees(&g3, find(&g3, "demo::use_a")), ["demo::A::size"]);
    let _ = fs::remove_dir_all(&root);
}
