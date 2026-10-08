//! rust-analyzer in the background: precise call targets for the graph.
//!
//! One worker thread per project owns the language-server client. Whenever
//! the current snapshot contains text the server has not been asked about,
//! [`drive`] hands the snapshot to the worker; the worker resolves every call
//! site — after an edit, only those whose answer may have changed
//! ([`pcg_lsp::resolve`]) — and sends the answers back, which triggers a
//! rebuild that uses them. Until then — and for whatever the server cannot
//! answer — the graph shows the name-based edges.

use crate::model::*;
use bevy::prelude::*;
use pcg_syntax::Precise;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Above this many call sites the server is not asked (opening a directory
/// full of unrelated crates is not a workspace it can load).
const MAX_CALLS: usize = 200_000;

pub enum Event {
    Status(String),
    Done(Precise),
    Failed(String),
}

fn worker(root: std::path::PathBuf, jobs: Receiver<Arc<Loaded>>, events: Sender<Event>) {
    let say = |s: &str| events.send(Event::Status(s.to_string())).is_ok();
    say("rust-analyzer: starting…");
    let mut client = match pcg_lsp::Client::start(&root) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send(Event::Failed(format!("rust-analyzer not available ({e})")));
            return;
        }
    };
    say("rust-analyzer: loading workspace…");
    if let Err(e) = client.wait_ready(Duration::from_secs(900)) {
        let _ = events.send(Event::Failed(format!("rust-analyzer: {e}")));
        return;
    }
    say("rust-analyzer: ready");
    // Answers for the last snapshot: the next one only asks what may have changed.
    let mut last: Option<Precise> = None;
    while let Ok(job) = jobs.recv() {
        let mut progress = |done: usize, total: usize| {
            say(&format!("rust-analyzer: resolving calls {done}/{total}"));
        };
        match pcg_lsp::resolve(&mut client, &job.graph, last.as_ref(), &mut progress) {
            Ok((p, _)) => {
                last = Some(p.clone());
                if events.send(Event::Done(p)).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = events.send(Event::Failed(format!("rust-analyzer: {e}")));
                return;
            }
        }
    }
}

pub fn drive(mut lsp: ResMut<Lsp>, project: Res<Project>, mut req: ResMut<LoadRequest>) {
    // Worker → app.
    let events: Vec<Event> = match &lsp.events {
        Some(rx) => rx.lock().unwrap_or_else(|e| e.into_inner()).try_iter().collect(),
        None => Vec::new(),
    };
    for ev in events {
        match ev {
            Event::Status(s) => lsp.status = s,
            Event::Done(p) => {
                lsp.precise = Some(Arc::new(p));
                lsp.busy = false;
                lsp.status.clear();
                // Rebuild with the answers.
                req.pending = true;
                req.keep_view = true;
            }
            Event::Failed(why) => {
                warn!("{why}");
                lsp.status = format!("{why} — edges are name-based");
                lsp.jobs = None;
                lsp.busy = false;
                lsp.failed = true;
            }
        }
    }
    let Some(p) = &project.data else { return };
    if !lsp.enabled {
        return;
    }
    // Another project: another server.
    if lsp.root != p.graph.root {
        let (job_tx, job_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let root = p.graph.root.clone();
        *lsp = Lsp { enabled: true, root: root.clone(), ..default() };
        if p.graph.calls.len() > MAX_CALLS {
            lsp.status = "too many call sites for rust-analyzer — edges are name-based".into();
            lsp.failed = true;
            return;
        }
        lsp.jobs = Some(job_tx);
        lsp.events = Some(Mutex::new(ev_rx));
        std::thread::spawn(move || worker(root, job_rx, ev_tx));
    }
    // App → worker: the snapshot has text the server was not asked about.
    let id = Arc::as_ptr(p) as usize;
    if p.lsp_stale
        && !lsp.busy
        && !lsp.failed
        && lsp.asked != id
        && let Some(tx) = &lsp.jobs
        && tx.send(p.clone()).is_ok()
    {
        lsp.busy = true;
        lsp.asked = id;
    }
}
