//! The egui pass: panels + canvas. Reads the snapshot, writes only view /
//! selection / UI state and load requests.

use crate::canvas::canvas;
use crate::model::*;
use crate::theme;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui::{LayerId, RichText, Ui, UiBuilder};
use pcg_core::{CommentKind, Graph, NodeId, SummaryState};

#[allow(clippy::too_many_arguments)]
pub fn ui(
    mut contexts: EguiContexts,
    project: Res<Project>,
    task: Res<LoadTask>,
    mut req: ResMut<LoadRequest>,
    mut view: ResMut<View>,
    mut sel: ResMut<Selection>,
    mut st: ResMut<UiState>,
    mut scratch: ResMut<CanvasScratch>,
    tr: Res<Transition>,
    watch: Res<Watch>,
    mut editing: ResMut<Editing>,
    lsp: Res<Lsp>,
    time: Res<Time>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    if !st.themed {
        theme::apply(ctx);
        st.themed = true;
    }
    let now = time.elapsed_secs_f64();
    let mut root = Ui::new(
        ctx.clone(),
        "root".into(),
        UiBuilder::new().layer_id(LayerId::background()).max_rect(ctx.viewport_rect()),
    );
    let data = project.data.clone();
    let mut select: Option<NodeId> = None;
    let mut fly: Option<NodeId> = None;
    let mut edit: Option<NodeId> = None;

    // ---- top bar -----------------------------------------------------------
    egui::Panel::top("top").show(&mut root, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("pcg").strong().color(theme::ACCENT));
            ui.separator();
            if let (Some(p), true) = (&data, sel.selected.is_some()) {
                breadcrumbs(ui, &p.graph, sel.selected, &mut select, &mut fly);
            } else {
                ui.label(
                    RichText::new(
                        "click a box to select · double-click to focus · Enter to edit · scroll to zoom · drag to pan",
                    )
                    .color(theme::TEXT_DIM),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{:.0} fps", 1.0 / time.delta_secs().max(1e-6))).color(theme::TEXT_DIM));
                ui.label(RichText::new(format!("zoom {:.0}%", view.zoom * 100.0)).color(theme::TEXT_DIM));
                ui.label(RichText::new(format!("{} drawn", scratch.drawn)).color(theme::TEXT_DIM));
                ui.separator();
                ui.checkbox(&mut st.show_all_edges, "all edges");
                ui.selectable_value(&mut st.face, Face::Summary, "summary face");
                ui.selectable_value(&mut st.face, Face::Code, "code face");
            });
        });
    });

    // ---- left: project + search --------------------------------------------
    egui::Panel::left("left").default_size(280.0).show(&mut root, |ui| {
        ui.heading("Project");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.path_input).desired_width(180.0));
            if ui.button("Open").clicked() {
                *req = LoadRequest { path: st.path_input.clone().into(), pending: true, keep_view: false };
            }
        });
        if ui.button("⟳ Reload").clicked() {
            reload(&mut req);
        }
        if task.task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("analysing… {:.1}s", now - task.started));
            });
        }
        match (&watch.error, watch.watcher.is_some()) {
            (Some(e), _) => ui.colored_label(theme::DIFF_EXIT, format!("not watching: {e}")),
            (None, true) => ui.colored_label(theme::DIFF_ENTER, "watching for changes"),
            (None, false) => ui.label(""),
        };
        ui.label(RichText::new(&st.status).small().monospace().color(theme::TEXT_DIM));
        // Where the call edges come from.
        if let Some(p) = &data {
            let (precise, calls) = (p.stats.calls_precise, p.stats.calls);
            let edges = if !lsp.enabled {
                "name-based (--no-lsp)".to_string()
            } else if precise > 0 {
                format!("{precise} of {calls} call sites resolved by rust-analyzer")
            } else {
                "name-based so far".to_string()
            };
            ui.label(RichText::new(format!("edges: {edges}")).small().color(theme::TEXT_DIM));
            if !lsp.status.is_empty() {
                ui.horizontal(|ui| {
                    if lsp.busy || !lsp.failed {
                        ui.spinner();
                    }
                    ui.label(RichText::new(&lsp.status).small().color(theme::TEXT_DIM));
                });
            }
        }
        ui.separator();

        ui.heading("Search");
        ui.add(egui::TextEdit::singleline(&mut st.search).hint_text("name…").desired_width(f32::INFINITY));
        if let Some(p) = &data {
            if st.search != st.search_for {
                st.search_for = st.search.clone();
                st.search_hits = search(p, &st.search, 500);
            }
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                for &h in &st.search_hits {
                    let i = h.idx();
                    let k = p.graph.nodes.kind[i];
                    let resp = ui.selectable_label(
                        sel.selected == h,
                        RichText::new(format!("{} {}", k.label(), p.graph.name(h))).color(theme::kind(k)),
                    );
                    if resp.clicked() {
                        select = Some(h);
                        fly = Some(h);
                    }
                    resp.on_hover_text(p.graph.qualified_name(h));
                }
            });
        }
    });

    // ---- right: inspector ------------------------------------------------------
    if let Some(p) = &data
        && sel.selected.is_some()
        && sel.selected.idx() < p.graph.nodes.len()
    {
        egui::Panel::right("inspector").default_size(380.0).show(&mut root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                inspector(ui, p, &mut sel, &mut st, &mut req, &mut select, &mut fly, &mut edit);
            });
        });
    }

    // ---- canvas ----------------------------------------------------------------
    egui::CentralPanel::no_frame().show(&mut root, |ui| {
        if let Some(p) = &data {
            let anim = crate::anim::Anim::new(p, &tr, now);
            let out = canvas(ui, p, anim.as_ref(), project.loaded_at, &mut view, &sel, &st, &mut scratch, now);
            sel.hovered = out.hovered;
            if let Some(c) = out.clicked {
                select = Some(c);
            }
            if let Some(c) = out.double_clicked {
                fly = Some(c);
            }
            if out.clicked_background {
                select = Some(NodeId::NONE);
            }
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }
    });

    // ---- keyboard ----------------------------------------------------------------
    if let Some(p) = &data {
        let (esc, f, back, enter) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Enter),
            )
        });
        // Esc closes the focused editor first.
        let editing_now = editing.docs.iter().any(|d| d.editors.iter().any(|e| e.focused));
        if !ctx.egui_wants_keyboard_input() {
            if enter && sel.selected.is_some() {
                edit = Some(sel.selected);
            }
            if esc && !editing_now {
                select = Some(NodeId::NONE);
            }
            if f {
                if sel.selected.is_some() { fly = Some(sel.selected) } else { view.needs_fit = true }
            }
            if back && sel.selected.is_some() {
                let parent = p.graph.nodes.parent[sel.selected.idx()];
                if parent.is_some() {
                    select = Some(parent);
                    fly = Some(parent);
                }
            }
        }
        // ---- in-node editor ---------------------------------------------------
        if let Some(n) = edit {
            match crate::edit::open(&mut editing, p, n) {
                Ok(()) => fly = Some(n),
                Err(why) => st.status = why.into(),
            }
        }
        crate::edit::editors(ctx, p, &view, &mut editing, &mut st, &mut req, now);

        if let Some(n) = fly {
            view.fly_to_node(&p.layout, n);
        }
    }
    if let Some(s) = select
        && s != sel.selected
    {
        sel.selected = s;
        sel.changed_at = now;
    }
    Ok(())
}

/// Re-run the pipeline on the same project. Selection and camera carry over
/// via the snapshot diff (see `load::poll`).
fn reload(req: &mut LoadRequest) {
    req.pending = true;
    req.keep_view = true;
}

fn search(p: &Loaded, q: &str, limit: usize) -> Vec<NodeId> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    // Exact / prefix matches first, then substring.
    let mut hits: Vec<(u8, NodeId)> = p
        .search_names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let rank = if **n == *q {
                0
            } else if n.starts_with(&q) {
                1
            } else if n.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, NodeId::from_idx(i)))
        })
        .collect();
    hits.sort_by_key(|h| h.0);
    hits.truncate(limit);
    hits.into_iter().map(|h| h.1).collect()
}

fn breadcrumbs(ui: &mut Ui, g: &Graph, n: NodeId, select: &mut Option<NodeId>, fly: &mut Option<NodeId>) {
    let mut chain: Vec<NodeId> = g.nodes.ancestors(n).collect();
    chain.reverse();
    chain.push(n);
    for (k, a) in chain.into_iter().enumerate() {
        if k > 0 {
            ui.label(RichText::new("›").color(theme::TEXT_DIM));
        }
        let kind = g.nodes.kind[a.idx()];
        if ui.link(RichText::new(g.name(a)).color(theme::kind(kind))).clicked() {
            *select = Some(a);
            *fly = Some(a);
        }
    }
}

fn node_link(ui: &mut Ui, g: &Graph, n: NodeId, extra: &str, select: &mut Option<NodeId>, fly: &mut Option<NodeId>) {
    let k = g.nodes.kind[n.idx()];
    let r = ui.link(RichText::new(format!("{} {}{extra}", k.label(), g.name(n))).color(theme::kind(k)));
    if r.clicked() {
        *select = Some(n);
        *fly = Some(n);
    }
    r.on_hover_text(g.qualified_name(n));
}

#[allow(clippy::too_many_arguments)]
fn inspector(
    ui: &mut Ui,
    p: &Loaded,
    sel: &mut Selection,
    st: &mut UiState,
    req: &mut LoadRequest,
    select: &mut Option<NodeId>,
    fly: &mut Option<NodeId>,
    edit: &mut Option<NodeId>,
) {
    let g = &p.graph;
    let n = sel.selected;
    let i = n.idx();
    let kind = g.nodes.kind[i];

    ui.label(RichText::new(kind.label()).color(theme::kind(kind)).monospace());
    ui.label(RichText::new(g.name(n)).heading().strong());
    ui.label(RichText::new(g.qualified_name(n)).small().color(theme::TEXT_DIM));
    let f = g.nodes.file[i];
    if f.is_some() {
        let lines = g.nodes.lines[i];
        ui.label(
            RichText::new(format!(
                "{}:{}–{}",
                g.strings.resolve(g.files.rel_path[f.idx()]),
                lines.start + 1,
                lines.end
            ))
            .small()
            .monospace(),
        );
    }
    ui.label(
        RichText::new(format!("hash {:06x}", pcg_syntax::short_hash(g.nodes.content_hash[i])))
            .small()
            .monospace()
            .color(theme::TEXT_DIM),
    );
    ui.separator();

    // Summary / intent faces.
    let (c, label) = theme::summary_state(g.nodes.summary_state[i]);
    ui.colored_label(c, RichText::new(label).strong());
    for (cid, kind_) in [(g.nodes.intent[i], CommentKind::Intent), (g.nodes.summary[i], CommentKind::Summary)] {
        if cid.is_some() {
            let (color, tag) = match kind_ {
                CommentKind::Intent => (theme::INTENT, "intent"),
                CommentKind::Summary => (c, "summary"),
            };
            ui.label(RichText::new(tag).small().color(color));
            ui.label(g.comments.text(cid));
        }
    }

    if kind.is_summarizable() && g.nodes.file[i].is_some() {
        if st.draft_for != n {
            st.draft_for = n;
            st.summary_draft = if g.nodes.summary[i].is_some() {
                g.comments.text(g.nodes.summary[i]).to_string()
            } else {
                String::new()
            };
        }
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut st.summary_draft)
                .hint_text("Write a summary…")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        let verb = if g.nodes.summary_state[i] == SummaryState::Stale {
            "Accept & refresh hash"
        } else {
            "Accept & write summary"
        };
        if ui
            .add_enabled(!st.summary_draft.trim().is_empty(), egui::Button::new(verb))
            .on_hover_text("Writes a @pcg:summary comment into the source file (decision 7: only on explicit accept).")
            .clicked()
        {
            match write_summary(g, n, &st.summary_draft) {
                Ok(()) => {
                    st.status = "summary written".into();
                    reload(req);
                }
                Err(e) => st.status = format!("write failed: {e}"),
            }
        }
    }
    ui.separator();

    // Edges.
    let out = g.edges.out.of(n);
    let inc = g.edges.inc.of(n);
    egui::CollapsingHeader::new(RichText::new(format!("→ calls ({})", out.len())).color(theme::EDGE_OUT))
        .default_open(true)
        .show(ui, |ui| {
            for &e in out.iter().take(200) {
                let w = g.edges.weight[e.idx()];
                node_link(
                    ui,
                    g,
                    g.edges.dst[e.idx()],
                    &if w > 1 { format!("  ×{w}") } else { String::new() },
                    select,
                    fly,
                );
            }
        });
    egui::CollapsingHeader::new(RichText::new(format!("← called by ({})", inc.len())).color(theme::EDGE_IN))
        .default_open(true)
        .show(ui, |ui| {
            for &e in inc.iter().take(200) {
                node_link(ui, g, g.edges.src[e.idx()], "", select, fly);
            }
        });
    let kids: Vec<NodeId> = g.nodes.children(n).collect();
    if !kids.is_empty() {
        egui::CollapsingHeader::new(format!("children ({})", kids.len())).show(ui, |ui| {
            for &k in kids.iter().take(500) {
                node_link(ui, g, k, "", select, fly);
            }
        });
    }

    // Code.
    let src = g.source(n);
    if !src.is_empty() {
        if ui.button("✏ Edit").on_hover_text("Edit this node's source in place (Enter)").clicked() {
            *edit = Some(n);
        }
        egui::CollapsingHeader::new("code").default_open(!kind.is_container()).show(ui, |ui| {
            let shown: String = src.lines().take(400).collect::<Vec<_>>().join("\n");
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(shown).monospace().size(11.5)).extend());
                });
            });
        });
    }
}

fn write_summary(g: &Graph, n: NodeId, text: &str) -> std::io::Result<()> {
    let edit =
        pcg_syntax::summary_edit(g, n, text).ok_or_else(|| std::io::Error::other("node cannot carry a summary"))?;
    let path = &g.files.path[edit.file.idx()];
    // Re-read from disk and check it is unchanged since analysis, to never clobber newer edits.
    let disk = std::fs::read_to_string(path)?;
    if *disk != *g.files.source[edit.file.idx()] {
        return Err(std::io::Error::other("file changed on disk since analysis — reload first"));
    }
    std::fs::write(path, edit.apply(&disk))
}
