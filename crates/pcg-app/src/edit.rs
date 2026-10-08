//! In-node editing: nodes' source, edited in place on the canvas.
//!
//! An open file is a [`Doc`]: a [`pcg_syntax::Buffer`] plus the editors looking
//! into it, each a byte span (one node). Each keystroke becomes one minimal
//! edit of the buffer (incremental reparse); editors further down the same
//! file shift along. A quiet [`DEBOUNCE`] later [`poll`] requests a rebuild
//! with the buffers as overlays, so the graph follows the unsaved text — and
//! animates the diff — while the files on disk stay untouched.
//!
//! Only *Save* writes. If the file changed on disk meanwhile, a clean doc just
//! follows it (editors re-find their item by [`ItemPath`]); a doc with unsaved
//! text is flagged as in conflict and the user picks a side.
//!
//! Editors are anchored to byte spans, not node ids: while typing, a node may
//! be renamed or stop parsing, and is re-found after each rebuild.

use crate::model::*;
use crate::theme;
use bevy::prelude::*;
use bevy_egui::egui::{self, RichText, Vec2, text::LayoutJob};
use pcg_core::{FileId, Graph, NodeId};
use pcg_syntax::{Buffer, Overlays};
use std::ops::Range;

const DEBOUNCE: f64 = 0.25;
const MIN_SIZE: Vec2 = Vec2::new(560.0, 180.0);
const FONT: f32 = 13.0;

fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// The editor's text as a coloured layout job. `text` is `span` of the buffer
/// with LF line endings; token offsets are mapped across the dropped `\r`s.
fn highlight(buffer: &Buffer, span: &Range<usize>, text: &str) -> LayoutJob {
    let font = egui::FontId::monospace(FONT);
    let fmt = |c| egui::TextFormat::simple(font.clone(), c);
    let file = &buffer.text()[span.clone()];
    let crs: Vec<usize> =
        if file.len() == text.len() { Vec::new() } else { file.match_indices("\r\n").map(|(i, _)| i).collect() };
    let map = |at: usize| {
        let rel = at - span.start;
        rel - crs.partition_point(|&c| c < rel)
    };
    let mut job = LayoutJob::default();
    let mut pos = 0;
    for (r, h) in buffer.highlights(span.clone()) {
        let (s, e) = (map(r.start), map(r.end));
        if s < pos || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        job.append(&text[pos..s], 0.0, fmt(theme::TEXT));
        job.append(&text[s..e], 0.0, fmt(theme::highlight(h)));
        pos = e;
    }
    job.append(&text[pos..], 0.0, fmt(theme::TEXT));
    job.wrap.max_width = f32::INFINITY;
    job
}

impl Doc {
    fn new(path: std::path::PathBuf, text: std::sync::Arc<str>) -> Self {
        Self {
            path,
            crlf: text.contains("\r\n"),
            buffer: Buffer::new(text.to_string()),
            base: text,
            dirty: false,
            conflict: false,
            changed_at: 0.0,
            graph_stale: false,
            editors: Vec::new(),
        }
    }

    /// An editor (currently taken out of `self.editors`) now reads `text`:
    /// apply it to the buffer as one minimal edit and shift the editors below.
    fn sync(&mut self, span: &mut Range<usize>, text: &str, now: f64) {
        let old_end = span.end;
        let file_text = if self.crlf { text.replace('\n', "\r\n") } else { text.to_string() };
        *span = self.buffer.replace_span(span.clone(), &file_text);
        let delta = span.end as isize - old_end as isize;
        for o in self.editors.iter_mut().filter(|o| o.span.start >= old_end) {
            o.span = (o.span.start as isize + delta) as usize..(o.span.end as isize + delta) as usize;
        }
        self.dirty = *self.buffer.text() != *self.base;
        self.changed_at = now;
        self.graph_stale = true;
    }

    /// Set editor `i`'s text, as typing would.
    #[cfg(test)]
    fn type_into(&mut self, i: usize, text: &str, now: f64) {
        let mut e = self.editors.remove(i);
        e.draft = text.to_string();
        self.sync(&mut e.span, text, now);
        e.synced = e.draft.clone();
        e.job = highlight(&self.buffer, &e.span, &e.draft);
        self.editors.insert(i, e);
    }

    /// Write the buffer to its file — unless the file is no longer what the
    /// buffer started from (never clobber newer edits unasked).
    fn save(&mut self) -> std::io::Result<()> {
        if *std::fs::read_to_string(&self.path)? != *self.base {
            self.conflict = true;
            return Err(std::io::Error::other("file changed on disk — keep mine or take theirs first"));
        }
        std::fs::write(&self.path, self.buffer.text())?;
        self.base = self.buffer.text().into();
        self.dirty = false;
        // Items may have been renamed: remember them as they are now.
        for e in self.editors.iter_mut().filter(|e| !e.whole_file) {
            if let Some(p) = self.buffer.item_at(&e.span) {
                e.item = p;
            }
        }
        Ok(())
    }

    /// Conflict, resolved for the editor: the next save overwrites the disk.
    fn keep_mine(&mut self) -> std::io::Result<()> {
        self.base = std::fs::read_to_string(&self.path)?.into();
        self.dirty = *self.buffer.text() != *self.base;
        self.conflict = false;
        Ok(())
    }

    /// Restart from `disk`, dropping unsaved text. Editors re-find their item;
    /// those whose item is gone are closed.
    fn take_theirs(&mut self, disk: String) {
        *self = Doc { editors: std::mem::take(&mut self.editors), ..Doc::new(self.path.clone(), disk.into()) };
        let buffer = &self.buffer;
        self.editors.retain_mut(|e| {
            let span = if e.whole_file { Some(0..buffer.text().len()) } else { buffer.find_item(&e.item) };
            let Some(span) = span else { return false };
            e.draft = lf(&buffer.text()[span.clone()]);
            e.synced = e.draft.clone();
            e.job = highlight(buffer, &span, &e.draft);
            e.span = span;
            e.discard_armed = false;
            true
        });
    }
}

/// Start editing node `n` (or focus its editor if it is already open).
pub fn open(ed: &mut Editing, p: &Loaded, n: NodeId) -> Result<(), &'static str> {
    let g = &p.graph;
    let i = n.idx();
    let f = g.nodes.file[i];
    if f.is_none() {
        return Err("this node has no source to edit");
    }
    let path = &g.files.path[f.idx()];
    let source = &g.files.source[f.idx()];
    let d = match ed.docs.iter().position(|d| d.path == *path) {
        Some(d) => d,
        None => {
            ed.docs.push(Doc::new(path.clone(), source.clone()));
            ed.docs.len() - 1
        }
    };
    let doc = &mut ed.docs[d];
    // The node's bytes are only meaningful in the text the graph was built from.
    if **source != *doc.buffer.text() {
        return Err("the graph is still catching up with this file — try again in a moment");
    }
    let span = g.nodes.bytes[i].range();
    if let Some(e) = doc.editors.iter_mut().find(|e| e.span == span) {
        e.want_focus = true;
        return Ok(());
    }
    if doc.editors.iter().any(|e| e.span.start < span.end && span.start < e.span.end) {
        return Err("an open editor already covers this code");
    }
    let whole_file = g.files.module[f.idx()] == n;
    let draft = lf(&source[span.clone()]);
    ed.next_id += 1;
    doc.editors.push(Editor {
        id: ed.next_id,
        title: format!("{} {}", g.nodes.kind[i].label(), g.name(n)),
        item: doc.buffer.item_at(&span).unwrap_or_default(),
        whole_file,
        job: highlight(&doc.buffer, &span, &draft),
        synced: draft.clone(),
        draft,
        span,
        node: n,
        anchor: [p.layout.x[i], p.layout.y[i], p.layout.w[i], p.layout.h[i]],
        want_focus: true,
        focused: false,
        discard_armed: false,
    });
    Ok(())
}

/// Unsaved text for the next build.
pub fn overlays(ed: &Editing) -> Overlays {
    ed.docs
        .iter()
        .filter(|d| d.dirty)
        .map(|d| (d.path.clone(), (d.buffer.text().into(), d.buffer.syntax().clone())))
        .collect()
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

/// A new snapshot arrived: notice files that changed on disk, and re-find the
/// edited nodes. Returns how many editors had to be closed (their file or item
/// is gone).
pub fn rebind(ed: &mut Editing, p: &Loaded) -> usize {
    let g = &p.graph;
    let mut closed = 0;
    ed.docs.retain_mut(|doc| {
        let before = doc.editors.len();
        let file = g.files.path.iter().position(|q| *q == doc.path).map(FileId::from_idx);
        let (Some(f), Ok(disk)) = (file, std::fs::read_to_string(&doc.path)) else {
            closed += before;
            return false;
        };
        if *disk == *doc.base {
            doc.conflict = false;
        } else if doc.dirty {
            doc.conflict = true;
        } else {
            doc.take_theirs(disk);
        }
        closed += before - doc.editors.len();
        // A build started before the last keystroke or save carries older text;
        // its byte offsets mean nothing here. The next one is already on its way.
        let current = *g.files.source[f.idx()] == *doc.buffer.text();
        for e in &mut doc.editors {
            e.node = if current { locate(g, f, &e.span) } else { NodeId::NONE };
            if e.node.is_some() {
                e.title = format!("{} {}", g.nodes.kind[e.node.idx()].label(), g.name(e.node));
            }
        }
        !doc.editors.is_empty()
    });
    closed
}

/// Typing paused: rebuild the graph from the buffers.
pub fn poll(mut ed: ResMut<Editing>, mut req: ResMut<LoadRequest>, task: Res<LoadTask>, time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    if task.task.is_none() && ed.docs.iter().any(|d| d.graph_stale && now - d.changed_at >= DEBOUNCE) {
        ed.docs.iter_mut().for_each(|d| d.graph_stale = false);
        req.pending = true;
        req.keep_view = true;
    }
}

enum DocAction {
    Save,
    KeepMine,
    TakeTheirs,
}

/// Draw the editors over their nodes. Call after the canvas (uses `view.canvas`).
pub fn editors(
    ctx: &egui::Context,
    p: &Loaded,
    view: &View,
    ed: &mut Editing,
    st: &mut UiState,
    req: &mut LoadRequest,
    now: f64,
) {
    if ed.docs.is_empty() {
        return;
    }
    let (save_all, esc) =
        ctx.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::S), i.key_pressed(egui::Key::Escape)));
    let canvas = view.canvas.shrink(8.0);
    let mut reload = false;

    for doc in &mut ed.docs {
        let mut action = (save_all && doc.dirty).then_some(DocAction::Save);
        let mut i = 0;
        while i < doc.editors.len() {
            // Taken out while drawn, so typing can shift its siblings.
            let mut e = doc.editors.remove(i);
            if e.node.is_some() && e.node.idx() < p.layout.x.len() {
                let n = e.node.idx();
                e.anchor = [p.layout.x[n], p.layout.y[n], p.layout.w[n], p.layout.h[n]];
            }
            let [x, y, w, h] = e.anchor;
            let node = view.world_rect_to_screen(x, y, w, h);
            let want_h = (e.draft.lines().count() as f32 * 17.0 + 80.0).min(520.0);
            let size = node.size().max(MIN_SIZE).max(Vec2::new(0.0, want_h)).min(canvas.size());
            // Closing the last view of unsaved text throws it away.
            let discards = doc.dirty && doc.editors.is_empty();
            let mut close = false;
            let had_focus = e.focused;

            egui::Area::new(egui::Id::new(("pcg-editor", e.id)))
                .order(egui::Order::Middle)
                .fixed_pos(node.min)
                .constrain_to(canvas)
                .show(ctx, |ui| {
                    let border = if doc.conflict {
                        theme::DIFF_EXIT
                    } else if doc.dirty {
                        theme::DIFF_CHANGE
                    } else {
                        theme::ACCENT
                    };
                    let frame = egui::Frame::new()
                        .fill(theme::BG)
                        .stroke(egui::Stroke::new(1.5, border))
                        .corner_radius(5.0)
                        .inner_margin(6.0);
                    frame.show(ui, |ui| {
                        ui.set_min_size(size - Vec2::splat(12.0));
                        ui.set_max_size(size - Vec2::splat(12.0));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&e.title).strong());
                            let state = if doc.dirty { "unsaved" } else { "saved" };
                            ui.label(
                                RichText::new(format!(
                                    "{state} · reparse {:.2} ms",
                                    doc.buffer.t_reparse.as_secs_f64() * 1e3
                                ))
                                .small()
                                .color(if doc.dirty {
                                    theme::DIFF_CHANGE
                                } else {
                                    theme::TEXT_DIM
                                }),
                            );
                            if doc.buffer.syntax().has_errors {
                                ui.label(RichText::new("syntax error").small().color(theme::DIFF_EXIT));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let label = if discards { "Discard" } else { "Close" };
                                close |= ui.button(label).on_hover_text("Esc").clicked();
                                if doc.conflict {
                                    if ui
                                        .button("Take theirs")
                                        .on_hover_text("Drop the unsaved text and load the file from disk")
                                        .clicked()
                                    {
                                        action = Some(DocAction::TakeTheirs);
                                    }
                                    if ui
                                        .button("Keep mine")
                                        .on_hover_text("Keep the unsaved text; saving will overwrite the file on disk")
                                        .clicked()
                                    {
                                        action = Some(DocAction::KeepMine);
                                    }
                                    ui.label(RichText::new("changed on disk").small().color(theme::DIFF_EXIT));
                                } else if ui
                                    .add_enabled(doc.dirty, egui::Button::new("Save"))
                                    .on_hover_text("Ctrl+S saves all files")
                                    .clicked()
                                {
                                    action = Some(DocAction::Save);
                                }
                            });
                        });
                        egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
                            // The layouter sees every text change first: it applies the change
                            // to the buffer, so the colours always belong to the text shown.
                            let (span, synced, job) = (&mut e.span, &mut e.synced, &mut e.job);
                            let mut changed = false;
                            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, _wrap: f32| {
                                let text = text.as_str();
                                if text != synced.as_str() {
                                    doc.sync(span, text, now);
                                    *synced = text.to_string();
                                    *job = highlight(&doc.buffer, span, text);
                                    changed = true;
                                }
                                ui.fonts_mut(|f| f.layout_job(job.clone()))
                            };
                            let out = egui::TextEdit::multiline(&mut e.draft)
                                .id(egui::Id::new(("pcg-editor-text", e.id)))
                                .code_editor()
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .layouter(&mut layouter)
                                .show(ui);
                            if e.want_focus {
                                out.response.request_focus();
                                e.want_focus = false;
                            }
                            e.focused = out.response.has_focus();
                            if changed {
                                e.discard_armed = false;
                            }
                        });
                    });
                });

            // Esc on the last view of unsaved text asks once before throwing it away.
            let esc = esc && had_focus;
            if esc && discards && !e.discard_armed {
                e.discard_armed = true;
                e.want_focus = true;
                st.status = "unsaved changes — Ctrl+S saves, Esc again discards".into();
            } else if esc || close {
                if discards {
                    st.status = "changes discarded".into();
                    doc.dirty = false;
                    reload = true; // the graph may show the discarded text
                }
                continue;
            }
            doc.editors.insert(i, e);
            i += 1;
        }

        let done = match action {
            Some(DocAction::Save) => doc.save().map(|()| format!("saved {}", doc.path.display())),
            Some(DocAction::KeepMine) => doc.keep_mine().map(|()| "keeping the editor's text".into()),
            Some(DocAction::TakeTheirs) => std::fs::read_to_string(&doc.path).map(|disk| {
                doc.take_theirs(disk);
                "loaded the file from disk".into()
            }),
            None => continue,
        };
        match done {
            Ok(msg) => {
                st.status = msg;
                doc.graph_stale = false;
                reload = true;
            }
            Err(e) => st.status = format!("{}: {e}", doc.path.display()),
        }
    }
    ed.docs.retain(|d| !d.editors.is_empty());
    if reload {
        req.pending = true;
        req.keep_view = true;
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
        let build = |ed: &Editing, prev| {
            crate::load::build(root.clone(), Arc::clone(&cache), overlays(ed), None, prev, Folds::default(), 0)
        };
        let p0 = Arc::new(build(&Editing::default(), None));
        let mut ed = Editing::default();
        open(&mut ed, &p0, find(&p0, "b")).unwrap();
        assert_eq!(ed.docs[0].editors[0].draft, "fn b() {\n    1;\n}");

        // Type: call `a`, and add a function below.
        ed.docs[0].type_into(0, "fn b() {\n    a();\n}\n\nfn c() {}", 1.0);
        assert!(ed.docs[0].dirty && ed.docs[0].graph_stale);
        let e = &ed.docs[0].editors[0];
        assert_eq!(e.job.text, e.draft, "highlighting covers exactly the text, across CRLF");
        assert!(e.job.sections.iter().any(|s| s.format.color == theme::highlight(pcg_syntax::Hl::Keyword)));
        let p1 = build(&ed, Some(p0.clone()));
        assert_eq!(rebind(&mut ed, &p1), 0);
        let node = ed.docs[0].editors[0].node;
        assert_eq!(node, find(&p1, "b"), "span now covers b and c: bound to the first");
        assert_eq!(p1.graph.edges.out.of(node).len(), 1);
        assert_eq!(p1.diff.as_ref().unwrap().entered, 1);
        let disk = root.join("src/lib.rs");
        assert!(!fs::read_to_string(&disk).unwrap().contains("fn c"), "nothing written before save");

        // Save keeps the file's CRLF line endings; the rebuilt graph is unchanged.
        ed.docs[0].save().unwrap();
        assert_eq!(
            fs::read_to_string(&disk).unwrap(),
            "fn a() {}\r\n\r\nfn b() {\r\n    a();\r\n}\r\n\r\nfn c() {}\r\n"
        );
        let p2 = build(&ed, Some(Arc::new(p1)));
        assert!(p2.diff.as_ref().unwrap().is_empty());
        assert_eq!(rebind(&mut ed, &p2), 0);
    }

    #[test]
    fn editors_of_one_file_share_the_buffer() {
        let root = project("multi", "fn a() {}\n\nmod m {\n    fn b() {}\n}\n\nfn c() {}\n");
        let p = crate::load::build(root.clone(), Arc::default(), Overlays::default(), None, None, Folds::default(), 0);
        let mut ed = Editing::default();
        open(&mut ed, &p, find(&p, "a")).unwrap();
        open(&mut ed, &p, find(&p, "c")).unwrap();
        assert_eq!((ed.docs.len(), ed.docs[0].editors.len()), (1, 2));
        // Opening the same node again focuses it; overlapping code is refused.
        open(&mut ed, &p, find(&p, "a")).unwrap();
        assert_eq!(ed.docs[0].editors.len(), 2);
        assert!(open(&mut ed, &p, find(&p, "demo")).is_err(), "the whole file overlaps both");

        // Growing `a` shifts `c`'s span; both edits land in one file.
        ed.docs[0].type_into(0, "fn a() { c() }", 0.0);
        let doc = &mut ed.docs[0];
        assert_eq!(&doc.buffer.text()[doc.editors[1].span.clone()], "fn c() {}");
        doc.type_into(1, "fn c() { 2; }", 0.0);
        assert_eq!(&doc.buffer.text()[doc.editors[0].span.clone()], "fn a() { c() }");
        // The stale snapshot cannot place a new editor in the edited buffer.
        assert!(open(&mut ed, &p, find(&p, "b")).is_err());
        ed.docs[0].save().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "fn a() { c() }\n\nmod m {\n    fn b() {}\n}\n\nfn c() { 2; }\n"
        );
    }

    #[test]
    fn disk_changes_under_an_editor() {
        let root = project("guard", "fn a() {}\nfn gone() {}\n");
        let disk = root.join("src/lib.rs");
        let cache = Arc::default();
        let build = |ed: &Editing| {
            crate::load::build(root.clone(), Arc::clone(&cache), overlays(ed), None, None, Folds::default(), 0)
        };
        let p0 = build(&Editing::default());
        let mut ed = Editing::default();
        open(&mut ed, &p0, find(&p0, "a")).unwrap();
        open(&mut ed, &p0, find(&p0, "gone")).unwrap();

        // Clean editors follow the file: `a` moved and changed, `gone` is gone.
        fs::write(&disk, "fn first() {}\nfn a() { 1; }\n").unwrap();
        let p1 = build(&ed);
        assert_eq!(rebind(&mut ed, &p1), 1);
        let doc = &mut ed.docs[0];
        assert_eq!(doc.editors.len(), 1);
        assert_eq!(doc.editors[0].draft, "fn a() { 1; }");
        assert_eq!(doc.editors[0].node, find(&p1, "a"));

        // Unsaved text + a change on disk = conflict; saving refuses.
        doc.type_into(0, "fn a() { mine(); }", 0.0);
        fs::write(&disk, "fn a() { theirs(); }\n").unwrap();
        assert!(doc.save().is_err());
        assert!(doc.conflict);
        assert_eq!(fs::read_to_string(&disk).unwrap(), "fn a() { theirs(); }\n");
        let p2 = build(&ed);
        assert_eq!(rebind(&mut ed, &p2), 0);
        assert!(ed.docs[0].conflict && ed.docs[0].dirty);
        assert_eq!(p2.graph.source(ed.docs[0].editors[0].node), "fn a() { mine(); }", "the graph shows the buffer");

        // Take theirs…
        let doc = &mut ed.docs[0];
        doc.take_theirs(fs::read_to_string(&disk).unwrap());
        assert!(!doc.dirty && !doc.conflict);
        assert_eq!(doc.editors[0].draft, "fn a() { theirs(); }");
        // …or keep mine: the next save overwrites.
        doc.type_into(0, "fn a() { mine(); }", 0.0);
        fs::write(&disk, "fn a() { theirs2(); }\n").unwrap();
        assert!(doc.save().is_err());
        doc.keep_mine().unwrap();
        doc.save().unwrap();
        assert_eq!(fs::read_to_string(&disk).unwrap(), "fn a() { mine(); }\n");

        // Typing back to the saved text is not "unsaved".
        doc.type_into(0, "fn a() {}", 0.0);
        assert!(doc.dirty);
        doc.type_into(0, "fn a() { mine(); }", 0.0);
        assert!(!doc.dirty);
    }
}
