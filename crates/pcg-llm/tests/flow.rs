use pcg_core::{Graph, NodeId, SummaryState};
use pcg_llm::{Error, Request, Scripted, SummaryCache, generate, summarize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

fn project(name: &str, lib: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pcg-llm-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(d.join("src/lib.rs"), lib).unwrap();
    d
}

fn build(root: &Path) -> Graph {
    pcg_syntax::build_graph(root).0
}

fn find(g: &Graph, name: &str) -> NodeId {
    (0..g.nodes.len()).map(NodeId::from_idx).find(|&n| g.name(n) == name).expect(name)
}

/// Answers "S(<first item in the prompt>)", and records the prompts.
fn echo(log: &Mutex<Vec<String>>) -> impl Fn(&Request) -> Result<String, Error> + Send + Sync + '_ {
    move |r: &Request| {
        log.lock().unwrap().push(r.prompt.clone());
        let name = r.prompt.split('`').nth(1).unwrap_or("?").to_string();
        Ok(format!("  S({name})\n"))
    }
}

const LIB: &str =
    "mod m {\n    /// @pcg:summary[h=000000] Stale.\n    pub fn a() { b(); }\n    fn b() {}\n}\n\nfn c() {}\n";

#[test]
fn bottom_up_and_cached() {
    let root = project("bottom-up", LIB);
    let g = build(&root);
    let cache = Mutex::new(SummaryCache::default());
    let m = find(&g, "m");
    assert_eq!(g.nodes.summary_state[find(&g, "a").idx()], SummaryState::Stale);

    let plan = summarize::plan(&g, &cache.lock().unwrap(), m, false);
    let order: Vec<(&str, u32)> = plan.iter().map(|&(n, h)| (g.name(n), h)).collect();
    assert_eq!(order, vec![("a", 1), ("b", 1), ("m", 2)], "leaves first; a stale summary is redone");

    let log = Mutex::new(Vec::new());
    let backend = Scripted(echo(&log));
    let n = summarize::run(&backend, &g, &plan, &cache, 4, &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(n, 3);
    let c = cache.lock().unwrap();
    assert_eq!(c.get(&g, find(&g, "b")), Some("S(demo::m::b)"));
    // The container was asked with its children's summaries, not its code alone.
    let pm = log.lock().unwrap().iter().find(|p| p.contains("mod `")).unwrap().clone();
    assert!(pm.contains("- `pub fn a() { b(); }`: S(demo::m::a)"), "{pm}");
    assert!(summarize::plan(&g, &c, m, false).is_empty(), "everything is known now");

    // Cache survives a restart, and an edit misses exactly the changed nodes.
    let mut c2 = c.clone();
    drop(c);
    c2.save(&root).unwrap();
    let c3 = SummaryCache::load(&root);
    assert_eq!(c3.entries.len(), 3);
    fs::write(root.join("src/lib.rs"), LIB.replace("fn b() {}", "fn b() { 1; }")).unwrap();
    let g2 = build(&root);
    let again: Vec<&str> = summarize::plan(&g2, &c3, find(&g2, "m"), false).iter().map(|&(n, _)| g2.name(n)).collect();
    assert_eq!(again, vec!["b", "m"], "a's code is unchanged, so its summary still applies");
}

#[test]
fn fatal_errors_stop_the_run() {
    let root = project("fatal", LIB);
    let g = build(&root);
    let cache = Mutex::new(SummaryCache::default());
    let plan = summarize::plan(&g, &cache.lock().unwrap(), find(&g, "m"), false);
    let backend = Scripted(|_: &Request| Err(Error::Http { status: 401, message: "bad key".into() }));
    let r = summarize::run(&backend, &g, &plan, &cache, 1, &AtomicBool::new(false), &|_| {});
    assert!(matches!(r, Err(Error::Http { status: 401, .. })));
}

#[test]
fn generate_proposal_in_place() {
    let root = project(
        "gen",
        "mod m {\r\n    /// Adds.\r\n    pub fn add(a: i32, b: i32) -> i32 {\r\n        0\r\n    }\r\n    fn other() { add(1, 2); }\r\n}\r\n",
    );
    let g = build(&root);
    let n = find(&g, "add");
    let cache = SummaryCache::default();
    let req = generate::request(&g, &cache, n, "Return the sum.");
    assert!(req.prompt.contains("    /// Adds.\n    pub fn add"), "shown with its indentation: {}", req.prompt);
    assert!(req.prompt.contains("It is called by"), "{}", req.prompt);
    assert!(req.prompt.contains("- `fn other() { add(1, 2); }` (in `demo::m::other`)"), "{}", req.prompt);

    let answer = "Here it is.\n```rust\n    /// Adds.\n    /// @pcg:intent old\n    pub fn add(a: i32, b: i32) -> i32 {\n        a + b\n    }\n```\n";
    let backend = Scripted(move |_: &Request| Ok(answer.to_string()));
    let p = generate::generate(&backend, &g, &cache, n, "Return the sum.", &mut |_| true).unwrap();
    assert_eq!(p.old, "/// Adds.\n    pub fn add(a: i32, b: i32) -> i32 {\n        0\n    }");
    assert_eq!(
        p.new,
        "/// @pcg:intent Return the sum.\n    /// Adds.\n    pub fn add(a: i32, b: i32) -> i32 {\n        a + b\n    }"
    );
    assert_eq!(&g.files.source[0][p.span.clone()].replace("\r\n", "\n"), &p.old);

    // Applied to the file, the intent is read back and the code is new.
    let src = g.files.source[0].to_string();
    let applied = format!("{}{}{}", &src[..p.span.start], p.new.replace('\n', "\r\n"), &src[p.span.end..]);
    fs::write(root.join("src/lib.rs"), applied).unwrap();
    let g2 = build(&root);
    let n2 = find(&g2, "add");
    assert_eq!(g2.comments.text(g2.nodes.intent[n2.idx()]), "Return the sum.");
    assert!(g2.source(n2).contains("a + b"));
}

#[test]
fn extract_code_variants() {
    assert_eq!(generate::extract_code("```rust\nfn a() {}\n```").as_deref(), Some("fn a() {}"));
    assert_eq!(generate::extract_code("fn a() {}\n").as_deref(), Some("fn a() {}"));
    assert_eq!(generate::extract_code("x\n```\n```"), None);
}
