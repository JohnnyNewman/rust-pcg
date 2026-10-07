//! Splits parse-stage time into raw tree-sitter parsing vs. our extraction.
//! `cargo run --release -p pcg-syntax --example parse_bench -- <dir>`
use rayon::prelude::*;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let sc = pcg_syntax::scan::scan(std::path::Path::new(&root));
    let srcs: Vec<String> = sc.files.par_iter().filter_map(|f| std::fs::read_to_string(&f.path).ok()).collect();
    let mb = srcs.iter().map(|s| s.len()).sum::<usize>() as f64 / 1e6;

    let t = Instant::now();
    srcs.par_iter().for_each(|s| {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        std::hint::black_box(p.parse(s, None));
    });
    let raw = t.elapsed().as_secs_f64();

    let t = Instant::now();
    srcs.par_iter().for_each(|s| {
        std::hint::black_box(pcg_syntax::parse::parse_file(s));
    });
    let full = t.elapsed().as_secs_f64();
    println!(
        "{mb:.1} MB on {} threads: tree-sitter {raw:.2}s ({:.1} MB/s) | full parse stage {full:.2}s ({:.1} MB/s) | extraction overhead {:.0}%",
        rayon::current_num_threads(),
        mb / raw,
        mb / full,
        (full / raw - 1.0) * 100.0
    );
}
