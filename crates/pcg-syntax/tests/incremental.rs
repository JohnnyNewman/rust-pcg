//! Incremental rebuilds, stable keys and snapshot diffs.

use pcg_core::*;
use pcg_syntax::diff::flag;
use pcg_syntax::{ParseCache, build_graph_cached, diff};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pcg-inc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn find(g: &Graph, qname: &str) -> NodeId {
    (0..g.nodes.len())
        .map(NodeId::from_idx)
        .find(|&n| g.qualified_name(n) == qname)
        .unwrap_or_else(|| panic!("{qname} not found"))
}

fn fixture(root: &Path) {
    write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(root, "src/lib.rs", "pub mod a;\npub mod b;\npub fn top() { a::fa(); }\n");
    write(root, "src/a.rs", "pub fn fa() {}\npub struct S;\nimpl S { fn m(&self) {} }\nimpl S { fn n(&self) {} }\n");
    write(root, "src/b.rs", "pub fn fb() {}\n");
}

#[test]
fn stable_keys_survive_insertions() {
    let root = tmp("keys");
    fixture(&root);
    let mut cache = ParseCache::default();
    let (g1, _) = build_graph_cached(&root, &mut cache);
    let fb1 = find(&g1, "demo::b::fb");

    // Insert items *before* `b` in pre-order: every NodeId after them shifts.
    write(
        &root,
        "src/a.rs",
        "pub fn new1() {}\npub fn new2() {}\npub fn fa() {}\npub struct S;\nimpl S { fn m(&self) {} }\nimpl S { fn n(&self) {} }\n",
    );
    let (g2, _) = build_graph_cached(&root, &mut cache);
    let fb2 = find(&g2, "demo::b::fb");
    assert_ne!(fb1, fb2, "fixture should shift ids");
    assert_eq!(g1.nodes.stable_key[fb1.idx()], g2.nodes.stable_key[fb2.idx()]);

    // Keys are unique, including the two same-named `impl S` blocks.
    let mut keys = g2.nodes.stable_key.clone();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), g2.nodes.len());
}

#[test]
fn only_changed_files_are_reparsed() {
    let root = tmp("cache");
    fixture(&root);
    let mut cache = ParseCache::default();
    let (_, st) = build_graph_cached(&root, &mut cache);
    assert_eq!(st.files_parsed, 3);

    // Pretend the last build was a minute later, so the files are no longer "racy".
    cache.built_at = cache.built_at.map(|t| t + std::time::Duration::from_secs(60));
    let (_, st) = build_graph_cached(&root, &mut cache);
    assert_eq!(st.files_parsed, 0);

    write(&root, "src/b.rs", "pub fn fb() { 1; }\n");
    let (g, st) = build_graph_cached(&root, &mut cache);
    assert_eq!(st.files_parsed, 1);
    assert_eq!(st.files, 3);
    find(&g, "demo::b::fb");

    // Deleted files are evicted.
    fs::remove_file(root.join("src/b.rs")).unwrap();
    let (_, st) = build_graph_cached(&root, &mut cache);
    assert_eq!(st.files, 2);
    assert_eq!(cache.len(), 2);
}

#[test]
fn diff_flags() {
    let root = tmp("diff");
    fixture(&root);
    let mut cache = ParseCache::default();
    let (g1, _) = build_graph_cached(&root, &mut cache);
    // fa changes, fb2 enters, struct S + impls exit.
    write(&root, "src/a.rs", "pub fn fa() { 42; }\n");
    write(&root, "src/b.rs", "pub fn fb() {}\npub fn fb2() {}\n");
    let (g2, _) = build_graph_cached(&root, &mut cache);
    let d = diff(&g1.nodes, &g2.nodes);

    let fa = find(&g2, "demo::a::fa");
    let fb = find(&g2, "demo::b::fb");
    let fb2 = find(&g2, "demo::b::fb2");
    let top = find(&g2, "demo::top");
    assert_eq!(d.flags[fa.idx()] & flag::CHANGED, flag::CHANGED);
    assert_eq!(d.flags[fb.idx()], 0);
    assert_eq!(d.flags[top.idx()], 0);
    assert_eq!(d.flags[fb2.idx()], flag::ENTERED);
    assert_ne!(d.flags[find(&g2, "demo::a").idx()] & flag::DESC_CHANGED, 0);
    assert_ne!(d.flags[find(&g2, "demo").idx()] & flag::DESC_CHANGED, 0);
    assert_eq!(d.entered, 1);
    assert_eq!(d.exited, 5); // S, impl S ×2, m, n
    assert_eq!(d.old_of_new[fb.idx()], find(&g1, "demo::b::fb"));
    assert!(d.new_of_old[find(&g1, "demo::a::S").idx()].is_none());
    assert_eq!(d.exit_roots.len(), 3); // S and the two impl blocks, not m / n
}

#[test]
fn overlay_wins_over_disk_until_saved() {
    use pcg_syntax::{Buffer, Overlays, build_graph_overlaid};
    let root = tmp("overlay");
    fixture(&root);
    let mut cache = ParseCache::default();
    let (g1, _) = build_graph_cached(&root, &mut cache);
    let fb = find(&g1, "demo::b::fb");
    let file = g1.nodes.file[fb.idx()];
    let path = g1.files.path[file.idx()].clone();

    // Edit `fb` in a buffer: the graph follows, the disk does not.
    let mut buf = Buffer::new(g1.files.source[file.idx()].to_string());
    let span = buf.replace_span(g1.nodes.bytes[fb.idx()].range(), "pub fn fb() { fb2() }\npub fn fb2() {}");
    assert_eq!(&buf.text()[span], "pub fn fb() { fb2() }\npub fn fb2() {}");
    let mut ov = Overlays::default();
    ov.insert(path.clone(), (buf.text().into(), buf.syntax().clone()));
    let (g2, st) = build_graph_overlaid(&root, &mut cache, &ov);
    assert_eq!(st.files_parsed, 0);
    let fb2 = find(&g2, "demo::b::fb2");
    assert_eq!(g2.edges.out.of(find(&g2, "demo::b::fb")).len(), 1);
    assert_eq!(g2.source(fb2), "pub fn fb2() {}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "pub fn fb() {}\n");

    // Saving the buffer and dropping the overlay changes nothing (same bytes: no reparse).
    fs::write(&path, buf.text()).unwrap();
    let (g3, st) = build_graph_cached(&root, &mut cache);
    assert_eq!(st.files_parsed, 0);
    assert!(diff(&g2.nodes, &g3.nodes).is_empty());

    // Discarding instead brings the disk text back.
    fs::write(&path, "pub fn fb() {}\n").unwrap();
    let (g4, _) = build_graph_cached(&root, &mut cache);
    assert!(diff(&g1.nodes, &g4.nodes).is_empty());
}
