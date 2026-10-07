//! In-node editing: one node's source, edited in place on the canvas.
//!
//! The session owns a [`pcg_syntax::Buffer`] for the node's file and a byte
//! span into it (the node). Each keystroke becomes one minimal edit of the
//! buffer (incremental reparse). A quiet [`DEBOUNCE`] later [`poll`] requests
//! a rebuild with the buffer as an overlay, so the graph follows the unsaved
//! text — and animates the diff — while the file on disk stays untouched.
//! Only *Save* writes, and it refuses if the file changed on disk meanwhile.
//!
//! The editor is anchored to the byte span, not to a node id: while typing,
//! the node may be renamed or stop parsing, and is re-found after each rebuild.

use crate::model::*;
use crate::theme;
use bevy::prelude::*;
use bevy_egui::egui::{self, RichText, Vec2};
use pcg_core::{FileId, Graph, NodeId};
use pcg_syntax::{Buffer, Overlays};
use std::ops::Range;

const DEBOUNCE: f64 = 0.25;
const MIN_SIZE: Vec2 = Vec2::new(560.0, 180.0);

/// Start editing node `n`. `None` for nodes without source (workspace, crates,
/// directory modules).
pub fn open(p: &Loaded, n: NodeId) -> Option<EditSession> {
    let g = &p.graph;
    let f = g.nodes.file[n.idx()];
    if f.is_none() {
        return None;
    }
    let base = g.files.source[f.idx()].clone();
    let span = g.nodes.bytes[n.idx()].range();
    let i = n.idx();
    Some(EditSession {
        path: g.files.path[f.idx()].clone(),
        title: format!("{} {}", g.nodes.kind[i].label(), g.name(n)),
        // egui edits LF text; the file's line endings are restored on the way back.
        draft: base[span.clone()].replace("\r\n", "\n"),
        crlf: base.contains("\r\n"),
        buffer: Buffer::new(base.to_string()),
        base,
        span,
        dirty: false,
        node: n,
        anchor: [p.layout.x[i], p.layout.y[i], p.layout.w[i], p.layout.h[i]],
        changed_at: 0.0,
        graph_stale: false,
        want_focus: true,
        discard_armed: false,
    })
}

impl EditSession {
    /// The draft changed: apply it to the buffer as one minimal edit.
    fn typed(&mut self, now: f64) {
        let text = if self.crlf { self.draft.replace('\n', "\r\n") } else { self.draft.clone() };
        self.span = self.buffer.replace_span(self.span.clone(), &text);
        self.dirty = *self.buffer.text() != *self.base;
        self.changed_at = now;
        self.graph_stale = true;
        self.discard_armed = false;
    }

    /// Write the buffer to its file — unless the file is no longer what the
    /// session started from (never clobber newer edits).
    fn save(&mut self) -> std::io::Result<()> {
        if *std::fs::read_to_string(&self.path)? != *self.base {
            return Err(std::io::Error::other("file changed on disk since the editor opened — discard and reopen"));
        }
        std::fs::write(&self.path, self.buffer.text())?;
        self.base = self.buffer.text().into();
        self.dirty = false;
        Ok(())
    }
}

/// Unsaved text for the next build.
pub fn overlays(ed: &Editing) -> Overlays {
    let mut o = Overlays::default();
    if let Some(s) = ed.session.as_ref().filter(|s| s.dirty) {
        o.insert(s.path.clone(), (s.buffer.text().into(), s.buffer.syntax().clone()));
    }
    o
}

/// The node of `file` that covers `span`: an exact match, else the outermost
/// one starting where the span starts (the item is being reshaped).
fn locate(g: &Graph, file: FileId, span: &Range<usize>) -> NodeId {
    let m = g.files.module[file.idx()];
    let mut best = NodeId::NONE;
    for n in m.0..g.nodes.subtree_end[m.idx()].0 {
        let i = n as usize;
        if g.nodes.file[i] != file {
            continue;
        }
        let b = g.nodes.bytes[i].range();
        if b == *span {
            return NodeId(n);
        }
        if best.is_none() && n != m.0 && b.start == span.start && b.end <= span.end {
            best = NodeId(n);
        }
    }
    best
}

/// A new snapshot arrived: re-find the edited node in it. Returns `false` if
/// the session lost its file (deleted, or changed on disk under a clean buffer).
pub fn rebind(s: &mut EditSession, p: &Loaded) -> bool {
    let g = &p.graph;
    let Some(f) = g.files.path.iter().position(|q| *q == s.path).map(FileId::from_idx) else { return false };
    if !s.dirty
        && *g.files.source[f.idx()] != *s.buffer.text()
        // A build started before the last save may still carry older text.
        && std::fs::read_to_string(&s.path).map_or(true, |disk| *disk != *s.base)
    {
        return false;
    }
    s.node = locate(g, f, &s.span);
    if s.node.is_some() {
        s.title = format!("{} {}", g.nodes.kind[s.node.idx()].label(), g.name(s.node));
    }
    true
}

/// Typing paused: rebuild the graph from the buffer.
pub fn poll(mut ed: ResMut<Editing>, mut req: ResMut<LoadRequest>, task: Res<LoadTask>, time: Res<Time>) {
    let Some(s) = ed.session.as_mut() else { return };
    if s.graph_stale && task.task.is_none() && time.elapsed_secs_f64() - s.changed_at >= DEBOUNCE {
        s.graph_stale = false;
        req.pending = true;
        req.keep_view = true;
    }
}

/// Draw the editor over its node. Call after the canvas (uses `view.canvas`).
pub fn editor(
    ctx: &egui::Context,
    p: &Loaded,
    view: &View,
    ed: &mut Editing,
    st: &mut UiState,
    req: &mut LoadRequest,
    now: f64,
) {
    let Some(s) = ed.session.as_mut() else { return };
    if s.node.is_some() && s.node.idx() < p.layout.x.len() {
        let i = s.node.idx();
        s.anchor = [p.layout.x[i], p.layout.y[i], p.layout.w[i], p.layout.h[i]];
    }
    let [x, y, w, h] = s.anchor;
    let node = view.world_rect_to_screen(x, y, w, h);
    let canvas = view.canvas.shrink(8.0);
    let want_h = (s.draft.lines().count() as f32 * 17.0 + 80.0).min(520.0);
    let size = node.size().max(MIN_SIZE).max(Vec2::new(0.0, want_h)).min(canvas.size());

    let (save, esc) =
        ctx.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::S), i.key_pressed(egui::Key::Escape)));
    let mut save = save;
    let mut close = false;

    egui::Area::new(egui::Id::new("pcg-editor"))
        .order(egui::Order::Middle)
        .fixed_pos(node.min)
        .constrain_to(canvas)
        .show(ctx, |ui| {
            let frame = egui::Frame::new()
                .fill(theme::BG)
                .stroke(egui::Stroke::new(1.5, if s.dirty { theme::DIFF_CHANGE } else { theme::ACCENT }))
                .corner_radius(5.0)
                .inner_margin(6.0);
            frame.show(ui, |ui| {
                ui.set_min_size(size - Vec2::splat(12.0));
                ui.set_max_size(size - Vec2::splat(12.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.title).strong());
                    let syn = s.buffer.syntax();
                    let state = if s.dirty { "● unsaved" } else { "saved" };
                    ui.label(
                        RichText::new(format!("{state} · reparse {:.2} ms", s.buffer.t_reparse.as_secs_f64() * 1e3))
                            .small()
                            .color(if s.dirty { theme::DIFF_CHANGE } else { theme::TEXT_DIM }),
                    );
                    if syn.has_errors {
                        ui.label(RichText::new("syntax error").small().color(theme::DIFF_EXIT));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if s.dirty { "Discard" } else { "Close" };
                        close |= ui.button(label).on_hover_text("Esc").clicked();
                        save |= ui.add_enabled(s.dirty, egui::Button::new("Save")).on_hover_text("Ctrl+S").clicked();
                    });
                });
                egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
                    let out = egui::TextEdit::multiline(&mut s.draft)
                        .id(egui::Id::new("pcg-editor-text"))
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .show(ui);
                    if s.want_focus {
                        out.response.request_focus();
                        s.want_focus = false;
                    }
                    if out.response.changed() {
                        s.typed(now);
                    }
                });
            });
        });

    if save && s.dirty {
        match s.save() {
            Ok(()) => {
                st.status = format!("saved {}", s.path.display());
                s.graph_stale = false;
                req.pending = true;
                req.keep_view = true;
            }
            Err(e) => st.status = format!("save failed: {e}"),
        }
    }
    // Esc on unsaved text asks once before throwing it away.
    if esc && s.dirty && !s.discard_armed {
        s.discard_armed = true;
        s.want_focus = true;
        st.status = "unsaved changes — Ctrl+S saves, Esc again discards".into();
    } else if esc || close {
        if s.dirty {
            // The graph may show the discarded text: rebuild from disk.
            st.status = "changes discarded".into();
            req.pending = true;
            req.keep_view = true;
        }
        ed.session = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    fn project(name: &str, lib: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pcg-edit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(d.join("src/lib.rs"), lib).unwrap();
        d
    }

    fn find(p: &Loaded, name: &str) -> NodeId {
        (0..p.graph.nodes.len()).map(NodeId::from_idx).find(|&n| p.graph.name(n) == name).expect(name)
    }

    #[test]
    fn type_rebuild_save() {
        let root = project("flow", "fn a() {}\r\n\r\nfn b() {\r\n    1;\r\n}\r\n");
        let cache = Arc::default();
        let build = |ed: &Editing, prev| crate::load::build(root.clone(), Arc::clone(&cache), overlays(ed), prev);
        let p0 = Arc::new(build(&Editing::default(), None));
        let mut ed = Editing { session: open(&p0, find(&p0, "b")) };
        let s = ed.session.as_mut().unwrap();
        assert_eq!(s.draft, "fn b() {\n    1;\n}");

        // Type: call `a`, and add a function below.
        s.draft = "fn b() {\n    a();\n}\n\nfn c() {}".into();
        s.typed(1.0);
        assert!(s.dirty && s.graph_stale);
        let p1 = build(&ed, Some(p0.clone()));
        let s = ed.session.as_mut().unwrap();
        assert!(rebind(s, &p1));
        assert_eq!(s.node, find(&p1, "b"), "span now covers b and c: bound to the first");
        assert_eq!(p1.graph.edges.out.of(s.node).len(), 1);
        assert_eq!(p1.diff.as_ref().unwrap().entered, 1);
        let disk = root.join("src/lib.rs");
        assert!(!fs::read_to_string(&disk).unwrap().contains("fn c"), "nothing written before save");

        // Save keeps the file's CRLF line endings; the rebuilt graph is unchanged.
        s.save().unwrap();
        assert_eq!(
            fs::read_to_string(&disk).unwrap(),
            "fn a() {}\r\n\r\nfn b() {\r\n    a();\r\n}\r\n\r\nfn c() {}\r\n"
        );
        let p1 = Arc::new(p1);
        let p2 = build(&ed, Some(p1));
        assert!(p2.diff.as_ref().unwrap().is_empty());
        assert!(rebind(ed.session.as_mut().unwrap(), &p2));
    }

    #[test]
    fn never_clobbers_external_changes() {
        let root = project("guard", "fn a() {}\n");
        let disk = root.join("src/lib.rs");
        let cache = Arc::default();
        let p0 = crate::load::build(root.clone(), Arc::clone(&cache), Overlays::default(), None);
        let mut s = open(&p0, find(&p0, "a")).unwrap();
        s.draft = "fn a() { 1; }".into();
        s.typed(0.0);
        fs::write(&disk, "fn a() {}\nfn other() {}\n").unwrap();
        assert!(s.save().is_err());
        assert_eq!(fs::read_to_string(&disk).unwrap(), "fn a() {}\nfn other() {}\n");

        // A clean editor is closed when its file changes underneath it.
        let mut clean = open(&p0, find(&p0, "a")).unwrap();
        let p1 = crate::load::build(root, cache, Overlays::default(), None);
        assert!(!rebind(&mut clean, &p1));
        // Typing back to the original text is not "unsaved".
        s.draft = "fn a() {}".into();
        s.typed(0.0);
        assert!(!s.dirty);
    }
}
