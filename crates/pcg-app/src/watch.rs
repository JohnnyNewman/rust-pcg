//! File watching: source changes on disk trigger an incremental reload.
//!
//! The notify callback runs on notify's thread and only filters the path and
//! bumps an atomic counter. [`poll`] (a Bevy system) turns a counter that has
//! been quiet for [`DEBOUNCE`] seconds into a `LoadRequest`, so an editor's
//! save burst (write temp, rename, touch) becomes a single reload.

use crate::model::*;
use bevy::prelude::*;
use notify::{EventKind, RecursiveMode, Watcher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const DEBOUNCE: f64 = 0.15;

/// Is this a path whose change can alter the graph?
fn relevant(root: &Path, p: &Path) -> bool {
    let rel = p.strip_prefix(root).unwrap_or(p);
    let ignored_dir = rel.components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy();
            s.starts_with('.') || s == "target" || s == "node_modules"
        }
        _ => false,
    });
    !ignored_dir && (p.extension().is_some_and(|e| e == "rs") || p.file_name().is_some_and(|n| n == "Cargo.toml"))
}

/// (Re)start watching `path` recursively.
pub fn watch(w: &mut Watch, path: &Path) {
    let root: PathBuf = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let events = Arc::new(AtomicU64::new(0));
    let counter = events.clone();
    let filter_root = root.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        if ev.paths.iter().any(|p| relevant(&filter_root, p)) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    };
    *w = Watch { root: path.to_path_buf(), events, ..default() };
    match notify::recommended_watcher(handler) {
        Ok(mut watcher) => match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => w.watcher = Some(watcher),
            Err(e) => w.error = Some(format!("watch {}: {e}", root.display())),
        },
        Err(e) => w.error = Some(format!("file watcher: {e}")),
    }
    if let Some(e) = &w.error {
        warn!("{e}");
    }
}

pub fn poll(mut w: ResMut<Watch>, mut req: ResMut<LoadRequest>, time: Res<Time>) {
    let n = w.events.load(Ordering::Relaxed);
    let now = time.elapsed_secs_f64();
    if n == w.seen {
        return;
    }
    if n != w.burst {
        // Still receiving events: restart the quiet period.
        w.burst = n;
        w.last_change = now;
        return;
    }
    if now - w.last_change >= DEBOUNCE && req.path == w.root {
        w.seen = n;
        req.pending = true;
        req.keep_view = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevance() {
        let root = Path::new("/p");
        assert!(relevant(root, Path::new("/p/src/a.rs")));
        assert!(relevant(root, Path::new("/p/crates/x/Cargo.toml")));
        assert!(!relevant(root, Path::new("/p/target/debug/build/x.rs")));
        assert!(!relevant(root, Path::new("/p/.git/index")));
        assert!(!relevant(root, Path::new("/p/src/a.rs.swp")));
        assert!(!relevant(root, Path::new("/p/README.md")));
    }
}
