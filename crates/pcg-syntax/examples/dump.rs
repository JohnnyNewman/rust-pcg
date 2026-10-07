//! Headless run of the static pipeline: `cargo run --release -p pcg-syntax --example dump -- <dir> [--tree]`
use pcg_core::*;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let tree = args.any(|a| a == "--tree");
    let (g, st) = pcg_syntax::build_graph(std::path::Path::new(&root));
    println!("{st}");

    let mut by_state = [0usize; 4];
    for s in &g.nodes.summary_state {
        by_state[*s as usize] += 1;
    }
    println!("summaries: missing {} fresh {} stale {} unhashed {}", by_state[0], by_state[1], by_state[2], by_state[3]);

    if tree {
        for i in 0..g.nodes.len() {
            let n = NodeId::from_idx(i);
            let out = g.edges.out.of(n).len();
            println!(
                "{:indent$}{} {}{}",
                "",
                g.nodes.kind[i].label(),
                g.name(n),
                if out > 0 { format!("  → {out}") } else { String::new() },
                indent = 2 * g.nodes.depth[i] as usize
            );
        }
    }
}
