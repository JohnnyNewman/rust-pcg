//! Language models in the background: summaries and code from intents.
//!
//! Each request of the user (summarize this subtree, rewrite this node) runs
//! on its own worker thread over the snapshot it was asked on; the worker
//! talks back through a channel that [`drive`] drains every frame. Results
//! never touch the source by themselves (decision 7):
//!
//! * summaries land in the [`SummaryCache`] (`<project>/.pcg/summaries.json`)
//!   and show as *proposed* on the boxes until the user accepts them, one by
//!   one or for a whole subtree, which writes `@pcg:summary` comments;
//! * a rewrite opens as unsaved text in an in-node editor — the graph follows
//!   it live and animates the diff — with *Accept* (save) and *Reject*
//!   (put the old text back).
//!
//! The backend is the Claude API when `ANTHROPIC_API_KEY` is set;
//! `PCG_LLM_BACKEND=demo` uses canned answers instead (no network).

use crate::model::*;
use bevy::prelude::*;
use pcg_core::{Graph, NodeId, SummaryState};
use pcg_llm::summarize::{self, Event as SumEvent};
use pcg_llm::{Backend, Error, Proposal, Request, Scripted, SummaryCache, generate};
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Requests in flight at once during a summarize run.
const CONCURRENCY: usize = 4;

pub enum Event {
    /// A summary request for this node (`stable_key`) is on its way.
    Asking(u64),
    Arrived(u64),
    Failed(u64, String),
    RunDone(Result<usize, Error>),
    GenText(String),
    GenDone(Result<Proposal, Error>),
}

/// A summarize run in progress.
pub struct Run {
    pub cancel: Arc<AtomicBool>,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
}

/// A rewrite in progress.
pub struct Gen {
    pub cancel: Arc<AtomicBool>,
    pub node_key: u64,
    /// The answer so far, as it streams in.
    pub text: String,
    pub started: f64,
}

/// Language-model state of the app (see the module docs).
#[derive(Resource)]
pub struct Llm {
    pub backend: Option<Arc<dyn Backend>>,
    /// Why there is no backend, for the inspector.
    pub missing: String,
    /// Project the cache belongs to.
    pub root: std::path::PathBuf,
    pub cache: Arc<Mutex<SummaryCache>>,
    tx: Sender<Event>,
    rx: Mutex<Receiver<Event>>,
    /// Nodes (`stable_key`) with a request in flight, since when.
    pub asking: FxHashMap<u64, f64>,
    /// Nodes whose summary just arrived, when (drives a short glow).
    pub arrived: FxHashMap<u64, f64>,
    pub run: Option<Run>,
    pub generating: Option<Gen>,
    pub status: String,
}

impl Default for Llm {
    fn default() -> Self {
        let (tx, rx) = channel();
        let (backend, missing) = make_backend();
        Self {
            backend,
            missing,
            root: Default::default(),
            cache: Default::default(),
            tx,
            rx: Mutex::new(rx),
            asking: Default::default(),
            arrived: Default::default(),
            run: None,
            generating: None,
            status: String::new(),
        }
    }
}

fn make_backend() -> (Option<Arc<dyn Backend>>, String) {
    if std::env::var("PCG_LLM_BACKEND").is_ok_and(|v| v == "demo") {
        return (Some(Arc::new(Scripted(demo))), String::new());
    }
    match pcg_llm::Claude::from_env() {
        Ok(c) => (Some(Arc::new(c)), String::new()),
        Err(e) => (None, e.to_string()),
    }
}

/// Canned answers: lets the whole flow be tried without a key or network.
fn demo(r: &Request) -> Result<String, Error> {
    std::thread::sleep(Duration::from_millis(400 + (r.prompt.len() % 7) as u64 * 120));
    let name = r.prompt.split('`').nth(1).unwrap_or("this item");
    if !r.system.contains("code generator") {
        return Ok(format!("Demo summary of {name}: what it does would be written here by the model."));
    }
    let code = pcg_llm::generate::extract_code(&r.prompt).unwrap_or_default();
    let intent = r.prompt.lines().nth(1).unwrap_or("").trim();
    // Put a marker comment after the first line that opens a block.
    let mut out = String::new();
    let mut done = false;
    for l in code.lines() {
        out.push_str(l);
        out.push('\n');
        if !done && l.trim_end().ends_with('{') {
            let ind: String = l.chars().take_while(|c| c.is_whitespace()).collect();
            out.push_str(&format!("{ind}    // demo: {intent}\n"));
            done = true;
        }
    }
    Ok(format!("Demo rewrite.\n```rust\n{}```\n", out))
}

impl Llm {
    pub fn cache(&self) -> std::sync::MutexGuard<'_, SummaryCache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Summaries `root` still needs, bottom-up.
    pub fn plan(&self, g: &Graph, root: NodeId) -> Vec<(NodeId, u32)> {
        summarize::plan(g, &self.cache(), root, false)
    }

    /// Summarize `root` and whatever its summary depends on.
    pub fn summarize(&mut self, p: &Arc<Loaded>, root: NodeId, redo: bool) {
        let Some(backend) = self.backend.clone() else { return };
        if self.run.is_some() {
            return;
        }
        let planned = summarize::plan(&p.graph, &self.cache(), root, redo);
        if planned.is_empty() {
            self.status = "nothing to summarize: every summary here is known".into();
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.run = Some(Run { cancel: cancel.clone(), total: planned.len(), done: 0, failed: 0 });
        self.status = format!("summarizing {} items with {}…", planned.len(), backend.name());
        let (g, cache, tx) = (p.graph.clone(), self.cache.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let key = |n: NodeId| g.nodes.stable_key[n.idx()];
            let on = |ev: SumEvent| {
                let _ = tx.send(match ev {
                    SumEvent::Started(n) => Event::Asking(key(n)),
                    SumEvent::Done { node, .. } => Event::Arrived(key(node)),
                    SumEvent::Failed { node, error } => Event::Failed(key(node), error.to_string()),
                });
            };
            let r = summarize::run(&*backend, &g, &planned, &cache, CONCURRENCY, &cancel, &on);
            let _ = tx.send(Event::RunDone(r));
        });
    }

    /// Rewrite `n` according to `intent`.
    pub fn generate(&mut self, p: &Arc<Loaded>, n: NodeId, intent: String, now: f64) {
        let Some(backend) = self.backend.clone() else { return };
        if self.generating.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let node_key = p.graph.nodes.stable_key[n.idx()];
        self.generating = Some(Gen { cancel: cancel.clone(), node_key, text: String::new(), started: now });
        self.asking.insert(node_key, now);
        self.status = format!("writing {} with {}…", p.graph.name(n), backend.name());
        let (g, cache, tx) = (p.graph.clone(), self.cache.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let snapshot = cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let r = generate::generate(&*backend, &g, &snapshot, n, &intent, &mut |t| {
                let _ = tx.send(Event::GenText(t.to_string()));
                !cancel.load(Ordering::Relaxed)
            });
            let _ = tx.send(Event::GenDone(r));
        });
    }

    pub fn cancel(&mut self) {
        for c in
            [self.run.as_ref().map(|r| &r.cancel), self.generating.as_ref().map(|g| &g.cancel)].into_iter().flatten()
        {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// A proposed summary for `n` (not in the source yet).
    pub fn proposed(&self, g: &Graph, n: NodeId) -> Option<String> {
        (g.nodes.summary_state[n.idx()] != SummaryState::Fresh).then(|| self.cache().get(g, n).map(str::to_string))?
    }
}

/// Write every proposed summary in `root`'s subtree as `@pcg:summary`
/// comments. Files that changed on disk since the snapshot are left alone.
pub fn write_summaries(g: &Graph, cache: &SummaryCache, root: NodeId) -> Result<usize, String> {
    let mut per_file: FxHashMap<u32, Vec<pcg_syntax::TextEdit>> = FxHashMap::default();
    for n in root.0..g.nodes.subtree_end[root.idx()].0 {
        let n = NodeId(n);
        if g.nodes.summary_state[n.idx()] == SummaryState::Fresh {
            continue;
        }
        if let Some(text) = cache.get(g, n)
            && let Some(e) = pcg_syntax::summary_edit(g, n, text)
        {
            per_file.entry(e.file.0).or_default().push(e);
        }
    }
    let mut written = 0;
    let mut skipped = Vec::new();
    for (f, edits) in per_file {
        let f = f as usize;
        let path = &g.files.path[f];
        let disk = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if *disk != *g.files.source[f] {
            skipped.push(g.strings.resolve(g.files.rel_path[f]).to_string());
            continue;
        }
        let refs: Vec<&pcg_syntax::TextEdit> = edits.iter().collect();
        std::fs::write(path, pcg_syntax::apply_edits(&disk, &refs)).map_err(|e| format!("{}: {e}", path.display()))?;
        written += edits.len();
    }
    if skipped.is_empty() {
        Ok(written)
    } else {
        Err(format!(
            "wrote {written}; skipped files that changed since analysis (unsaved or edited): {}",
            skipped.join(", ")
        ))
    }
}

/// The node with `key` in `g`.
pub fn find(g: &Graph, key: u64) -> NodeId {
    g.nodes.stable_key.iter().position(|&k| k == key).map_or(NodeId::NONE, NodeId::from_idx)
}

pub fn drive(mut llm: ResMut<Llm>, project: Res<Project>, mut editing: ResMut<Editing>, time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    let Some(p) = &project.data else { return };
    // Another project: its own cache.
    if llm.root != p.graph.root {
        llm.cancel();
        llm.root = p.graph.root.clone();
        *llm.cache() = SummaryCache::load(&p.graph.root);
        llm.asking.clear();
        llm.arrived.clear();
    }
    let events: Vec<Event> = llm.rx.lock().unwrap_or_else(|e| e.into_inner()).try_iter().collect();
    for ev in events {
        match ev {
            Event::Asking(k) => {
                llm.asking.insert(k, now);
            }
            Event::Arrived(k) => {
                llm.asking.remove(&k);
                llm.arrived.insert(k, now);
                if let Some(r) = &mut llm.run {
                    r.done += 1;
                }
            }
            Event::Failed(k, why) => {
                llm.asking.remove(&k);
                if let Some(r) = &mut llm.run {
                    r.failed += 1;
                }
                llm.status = format!("a summary failed: {why}");
            }
            Event::RunDone(r) => {
                let run = llm.run.take();
                let gen_key = llm.generating.as_ref().map(|g| g.node_key);
                llm.asking.retain(|k, _| Some(*k) == gen_key);
                llm.status = match (r, run) {
                    (Ok(n), Some(run)) if run.failed == 0 => {
                        format!("{n} summaries written to the cache — accept them to put them in the code")
                    }
                    (Ok(n), Some(run)) => format!("{n} summaries, {} failed", run.failed),
                    (Ok(n), None) => format!("{n} summaries"),
                    (Err(e), _) => format!("summaries stopped: {e}"),
                };
                let root = llm.root.clone();
                let saved = llm.cache().save(&root);
                if let Err(e) = saved {
                    llm.status += &format!(" (cache not saved: {e})");
                }
            }
            Event::GenText(t) => {
                if let Some(g) = &mut llm.generating {
                    g.text.push_str(&t);
                }
            }
            Event::GenDone(r) => {
                if let Some(g) = llm.generating.take() {
                    llm.asking.remove(&g.node_key);
                    llm.arrived.insert(g.node_key, now);
                }
                llm.status = match r
                    .map_err(|e| e.to_string())
                    .and_then(|prop| crate::edit::propose(&mut editing, p, &prop, now))
                {
                    Ok(()) => "proposal open in the editor — Accept saves it, Reject restores the old code".into(),
                    Err(e) => format!("no proposal: {e}"),
                };
            }
        }
    }
    llm.arrived.retain(|_, t| now - *t < 2.0);
}

/// What the inspector asked for this frame.
pub enum Act {
    /// Summarize the node's subtree; `true`: also redo the node's own summary.
    Summarize(NodeId, bool),
    Generate(NodeId, String),
    Cancel,
    /// Write the proposed summaries of the node's subtree into the code.
    Write(NodeId),
}

/// Carry out `act` on snapshot `p`.
pub fn act(llm: &mut Llm, p: &Arc<Loaded>, act: Act, req: &mut LoadRequest, now: f64) {
    match act {
        Act::Summarize(n, redo) => llm.summarize(p, n, redo),
        Act::Generate(n, intent) => llm.generate(p, n, intent, now),
        Act::Cancel => llm.cancel(),
        Act::Write(n) => {
            let r = write_summaries(&p.graph, &llm.cache(), n);
            llm.status = match r {
                Ok(k) => format!("{k} summaries written into the code"),
                Err(e) => e,
            };
            req.pending = true;
            req.keep_view = true;
        }
    }
}

/// Proposed summaries in `root`'s subtree (cached, not fresh in the source).
pub fn proposed_in(g: &Graph, cache: &SummaryCache, root: NodeId) -> usize {
    (root.0..g.nodes.subtree_end[root.idx()].0)
        .map(NodeId)
        .filter(|&n| g.nodes.summary_state[n.idx()] != SummaryState::Fresh && cache.get(g, n).is_some())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn summarize_then_accept_all() {
        let d = std::env::temp_dir().join(format!("pcg-llm-app-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(d.join("src/lib.rs"), "fn a() {}\r\n\r\nmod m {\r\n    fn b() {}\r\n}\r\n").unwrap();
        let p = crate::load::build(d.clone(), Arc::default(), Default::default(), None, None, Folds::default(), 0);
        let g = &p.graph;
        let file = g.files.module[0];
        let cache = Mutex::new(SummaryCache::default());
        let plan = summarize::plan(g, &cache.lock().unwrap(), file, false);
        assert_eq!(plan.len(), 3, "a, m, b (the crate root is the crate node)");
        let backend = Scripted(|r: &Request| Ok(format!("Does {}.", r.prompt.split('`').nth(1).unwrap())));
        summarize::run(&backend, g, &plan, &cache, 2, &AtomicBool::new(false), &|_| {}).unwrap();
        let c = cache.lock().unwrap();
        assert_eq!(proposed_in(g, &c, file), 3);
        assert_eq!(write_summaries(g, &c, file), Ok(3));

        let text = fs::read_to_string(d.join("src/lib.rs")).unwrap();
        assert!(text.starts_with("/// @pcg:summary["), "{text}");
        assert!(text.contains("\r\n    /// @pcg:summary["), "nested, indented, CRLF: {text}");
        let p2 = crate::load::build(d.clone(), Arc::default(), Default::default(), None, None, Folds::default(), 0);
        let g2 = &p2.graph;
        let fresh = (0..g2.nodes.len()).filter(|&i| g2.nodes.summary_state[i] == SummaryState::Fresh).count();
        assert_eq!(fresh, 3, "{text}");
        assert_eq!(proposed_in(g2, &c, NodeId(0)), 0);
    }
}
