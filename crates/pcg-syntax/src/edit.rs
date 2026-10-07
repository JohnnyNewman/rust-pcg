//! An open file: text + syntax tree, edited in place.
//!
//! This is the one place where syntax trees are kept. An in-editor edit comes
//! with its byte range, so the tree is [`Tree::edit`]ed and tree-sitter
//! reparses only what the edit touched; the extraction pass then re-derives the
//! file-local tables ([`FileSyntax`]) from the new tree. The pipeline picks the
//! result up as an *overlay* (see [`crate::cache::Overlays`]), so the graph can
//! show unsaved text without it ever touching the disk.

use crate::parse::{FileSyntax, parse_tree};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tree_sitter::{InputEdit, Point, Tree};

pub struct Buffer {
    text: String,
    tree: Tree,
    syn: Arc<FileSyntax>,
    /// Cost of the last reparse + extraction.
    pub t_reparse: Duration,
}

/// tree-sitter position of byte `at` (row, byte column).
fn point(text: &str, at: usize) -> Point {
    let before = &text.as_bytes()[..at];
    let row = before.iter().filter(|&&b| b == b'\n').count();
    let column = at - before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    Point { row, column }
}

impl Buffer {
    pub fn new(text: String) -> Self {
        let t = Instant::now();
        let (syn, tree) = parse_tree(&text, None);
        Self { text, tree, syn: Arc::new(syn), t_reparse: t.elapsed() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn syntax(&self) -> &Arc<FileSyntax> {
        &self.syn
    }

    /// Replace `range` (bytes, on char boundaries) by `with` and reparse
    /// incrementally.
    pub fn edit(&mut self, range: Range<usize>, with: &str) {
        let t = Instant::now();
        let start_position = point(&self.text, range.start);
        let old_end_position = point(&self.text, range.end);
        self.text.replace_range(range.clone(), with);
        let new_end_byte = range.start + with.len();
        self.tree.edit(&InputEdit {
            start_byte: range.start,
            old_end_byte: range.end,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position: point(&self.text, new_end_byte),
        });
        let (syn, tree) = parse_tree(&self.text, Some(&self.tree));
        self.tree = tree;
        self.syn = Arc::new(syn);
        self.t_reparse = t.elapsed();
    }

    /// Make `span` read `new`, as the smallest single edit (common prefix and
    /// suffix are kept). Returns the span's new range; no-op if nothing differs.
    pub fn replace_span(&mut self, span: Range<usize>, new: &str) -> Range<usize> {
        let old = &self.text[span.clone()];
        let (ob, nb) = (old.as_bytes(), new.as_bytes());
        let mut pre = ob.iter().zip(nb).take_while(|(a, b)| a == b).count();
        while !old.is_char_boundary(pre) {
            pre -= 1;
        }
        let max_suf = ob.len().min(nb.len()) - pre;
        let mut suf = ob.iter().rev().zip(nb.iter().rev()).take(max_suf).take_while(|(a, b)| a == b).count();
        while !old.is_char_boundary(ob.len() - suf) {
            suf -= 1;
        }
        if pre + suf < ob.len() || pre + suf < nb.len() {
            self.edit(span.start + pre..span.end - suf, &new[pre..nb.len() - suf]);
        }
        span.start..span.start + new.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_file;

    /// The incremental result must be indistinguishable from a fresh parse.
    fn assert_same_as_fresh(b: &Buffer) {
        let fresh = parse_file(b.text());
        let inc = b.syntax();
        assert_eq!(inc.items.name, fresh.items.name);
        assert_eq!(inc.items.kind, fresh.items.kind);
        assert_eq!(inc.items.bytes, fresh.items.bytes);
        assert_eq!(inc.items.lines, fresh.items.lines);
        assert_eq!(inc.items.hash, fresh.items.hash);
        assert_eq!(inc.items.subtree_end, fresh.items.subtree_end);
        assert_eq!(inc.file_hash, fresh.file_hash);
        assert_eq!(inc.has_errors, fresh.has_errors);
        assert_eq!(inc.calls.len(), fresh.calls.len());
        assert_eq!(inc.comments.len(), fresh.comments.len());
    }

    const SRC: &str = "/// Docs.\nfn a() { b(); }\n\nfn b() -> u32 { 1 }\n\nimpl S {\n    fn m(&self) {}\n}\n";

    #[test]
    fn edits_match_a_fresh_parse() {
        let mut b = Buffer::new(SRC.to_string());
        let at = b.text().find("1 }").unwrap();
        b.edit(at..at + 1, "2 + c()");
        assert_same_as_fresh(&b);
        assert_eq!(b.syntax().calls.len(), 2);

        // Insert a new item, delete one, break and repair the syntax.
        let at = b.text().find("impl").unwrap();
        b.edit(at..at, "struct S;\n\n");
        assert_same_as_fresh(&b);
        let r = b.text().find("fn b").unwrap()..b.text().find("struct").unwrap();
        b.edit(r, "");
        assert_same_as_fresh(&b);
        assert_eq!(b.syntax().items.name, ["a", "S", "S", "m"]);
        let at = b.text().find("{ b();").unwrap();
        b.edit(at..at + 1, "");
        assert_same_as_fresh(&b);
        assert!(b.syntax().has_errors);
        b.edit(at..at, "{");
        assert_same_as_fresh(&b);
        assert!(!b.syntax().has_errors);
    }

    #[test]
    fn multibyte_and_crlf_positions() {
        let mut b = Buffer::new("fn a() { \"äö\" }\r\nfn b() {}\r\n".to_string());
        let at = b.text().find('ö').unwrap();
        b.edit(at..at + 'ö'.len_utf8(), "ü€");
        assert_same_as_fresh(&b);
        let at = b.text().find("fn b").unwrap();
        b.edit(at..at, "fn c() {\r\n    a()\r\n}\r\n");
        assert_same_as_fresh(&b);
        assert_eq!(b.syntax().items.name, ["a", "c", "b"]);
    }

    #[test]
    fn replace_span_is_minimal_and_tracks_the_span() {
        let mut b = Buffer::new(SRC.to_string());
        let s = b.text().find("fn b").unwrap();
        let span = s..s + "fn b() -> u32 { 1 }".len();
        let span = b.replace_span(span, "fn b() -> u32 { 41 + 1 }");
        assert_eq!(&b.text()[span.clone()], "fn b() -> u32 { 41 + 1 }");
        assert_same_as_fresh(&b);
        // Unchanged text is a no-op; multi-byte neighbours stay intact.
        let before = b.text().to_string();
        assert_eq!(b.replace_span(span.clone(), "fn b() -> u32 { 41 + 1 }"), span);
        assert_eq!(b.text(), before);
        let span = b.replace_span(span, "fn b() -> &str { \"é\" }");
        let span = b.replace_span(span, "fn b() -> &str { \"è\" }");
        assert_eq!(&b.text()[span], "fn b() -> &str { \"è\" }");
        assert_same_as_fresh(&b);
    }
}
