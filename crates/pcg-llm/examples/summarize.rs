//! `cargo run -p pcg-llm --example summarize -- <dir> <item name> [--write-cache]`
//!
//! Summarize one item of a project (and, bottom-up, whatever inside it has no
//! summary yet) with the Claude API. Needs `ANTHROPIC_API_KEY`. Prints each
//! summary as it arrives; `--write-cache` keeps them in `<dir>/.pcg/summaries.json`
//! where the IDE finds them.

use pcg_core::NodeId;
use pcg_llm::summarize::{self, Event};
use pcg_llm::{Claude, SummaryCache};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let write = args.iter().any(|a| a == "--write-cache");
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let [dir, name] = pos[..] else {
        eprintln!("usage: summarize <dir> <item name> [--write-cache]");
        std::process::exit(2);
    };
    let backend = Claude::from_env().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let root = std::path::Path::new(dir);
    let (g, _) = pcg_syntax::build_graph(root);
    let Some(n) = (0..g.nodes.len()).map(NodeId::from_idx).find(|&n| g.name(n) == name.as_str()) else {
        eprintln!("no item named {name}");
        std::process::exit(1)
    };
    let cache = Mutex::new(SummaryCache::load(root));
    let plan = summarize::plan(&g, &cache.lock().unwrap(), n, true);
    println!("{}: {} summaries to write", g.qualified_name(n), plan.len());
    let t = Instant::now();
    let r = summarize::run(&backend, &g, &plan, &cache, 4, &AtomicBool::new(false), &|ev| match ev {
        Event::Started(_) => {}
        Event::Done { node, text, .. } => println!("\n{}\n  {text}", g.qualified_name(node)),
        Event::Failed { node, error } => println!("\n{}\n  FAILED: {error}", g.qualified_name(node)),
    });
    println!("\n{r:?} in {:.1} s", t.elapsed().as_secs_f64());
    if write {
        cache.lock().unwrap().save(root).expect("write cache");
    }
}
