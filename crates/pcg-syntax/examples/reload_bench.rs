//! Cold build vs. warm (cached) rebuild vs. rebuild after one file changed,
//! plus snapshot diff cost.
//!
//! `cargo run --release -p pcg-syntax --example reload_bench -- <dir> [file-to-touch]`

use pcg_syntax::{ParseCache, build_graph_cached, diff};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let touch = args.next();
    let mut cache = ParseCache::default();

    let (g0, st) = build_graph_cached(&root, &mut cache);
    println!("cold:\n{st}\n");

    // Make the cache look old so nothing is "racy".
    cache.built_at = cache.built_at.map(|t| t + std::time::Duration::from_secs(60));
    let (g1, st) = build_graph_cached(&root, &mut cache);
    println!("warm, nothing changed:\n{st}\n");

    if let Some(f) = touch {
        let text = std::fs::read_to_string(&f).expect("read file to touch");
        std::fs::write(&f, format!("{text}\nfn __pcg_bench_added() {{}}\n")).unwrap();
        cache.built_at = cache.built_at.map(|t| t + std::time::Duration::from_secs(60));
        let (g2, st) = build_graph_cached(&root, &mut cache);
        std::fs::write(&f, text).unwrap();
        println!("one file changed:\n{st}");
        let t = Instant::now();
        let d = diff(&g1.nodes, &g2.nodes);
        println!(
            "diff: {:.1} ms, +{} −{} ~{} ({} exit roots)",
            t.elapsed().as_secs_f64() * 1e3,
            d.entered,
            d.exited,
            d.changed,
            d.exit_roots.len()
        );
    }
    drop(g0);
}
