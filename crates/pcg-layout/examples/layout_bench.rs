//! Layout cost and shape on a real project.
//!
//! `cargo run --release -p pcg-layout --example layout_bench -- <dir>`

use pcg_core::NodeId;
use pcg_layout::{LayoutParams, layout};
use std::time::Instant;

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let (g, st) = pcg_syntax::build_graph(&root);
    println!("{st}");
    let names: Vec<u32> = (0..g.nodes.len()).map(|i| g.name(NodeId::from_idx(i)).chars().count() as u32).collect();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let shelf = layout(&g.nodes, &names, &[], &[], &LayoutParams::default());
    println!("shelf only: {:.1} ms, root {:.0} x {:.0}", ms(t), shelf.w[0], shelf.h[0]);
    let t = Instant::now();
    let l = layout(&g.nodes, &names, &g.edges.src, &g.edges.dst, &LayoutParams::default());
    println!(
        "layered:    {:.1} ms, root {:.0} x {:.0}, {} edges, {} routed through lanes",
        ms(t),
        l.w[0],
        l.h[0],
        g.edges.len(),
        l.routes.len()
    );
}
