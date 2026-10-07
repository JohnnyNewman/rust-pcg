//! An open file: text + syntax tree, edited in place.
//!
//! This is the one place where syntax trees are kept. An in-editor edit comes
//! with its byte range, so the tree is [`Tree::edit`]ed and tree-sitter
//! reparses only what the edit touched; the extraction pass then re-derives the
//! file-local tables ([`FileSyntax`]) from the new tree. The pipeline picks the
//! result up as an *overlay* (see [`crate::cache::Overlays`]), so the graph can
//! show unsaved text without it ever touching the disk.

use crate::parse::{FileSyntax, LOCAL_NONE, parse_tree};
use pcg_core::NodeKind;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tree_sitter::{InputEdit, Node, Point, Tree};

/// Highlight class of a token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hl {
    Keyword,
    Type,
    Function,
    Macro,
    String,
    Number,
    Comment,
    Attribute,
    Lifetime,
}

/// An item's identity inside its file, independent of byte offsets: per
/// nesting level `(kind, name, ordinal among same-named siblings)`.
pub type ItemPath = Vec<(NodeKind, String, u32)>;

fn classify(n: Node) -> Option<Hl> {
    let k = n.kind();
    Some(match k {
        "line_comment" | "block_comment" => Hl::Comment,
        "string_literal" | "raw_string_literal" | "char_literal" => Hl::String,
        "integer_literal" | "float_literal" | "boolean_literal" => Hl::Number,
        "type_identifier" | "primitive_type" => Hl::Type,
        "lifetime" => Hl::Lifetime,
        "attribute_item" | "inner_attribute_item" => Hl::Attribute,
        "mutable_specifier" | "self" | "super" | "crate" => Hl::Keyword,
        "identifier" | "field_identifier" => {
            let p = n.parent()?;
            let is = |of: Node, field: &str| of.child_by_field_name(field) == Some(n);
            match p.kind() {
                "function_item" | "function_signature_item" if is(p, "name") => Hl::Function,
                "call_expression" if is(p, "function") => Hl::Function,
                "macro_invocation" if is(p, "macro") => Hl::Macro,
                "macro_definition" if is(p, "name") => Hl::Macro,
                // `a::b::f(..)`, `x.f(..)`, `a::m!(..)`
                "scoped_identifier" | "field_expression" if is(p, "name") || is(p, "field") => {
                    let g = p.parent()?;
                    match g.kind() {
                        "call_expression" if g.child_by_field_name("function") == Some(p) => Hl::Function,
                        "macro_invocation" if g.child_by_field_name("macro") == Some(p) => Hl::Macro,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        // Anonymous word tokens are the keywords (`fn`, `let`, `impl`, …).
        _ if !n.is_named() && n.child_count() == 0 && k.bytes().all(|b| b.is_ascii_lowercase()) => Hl::Keyword,
        _ => return None,
    })
}

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

    /// Highlighted tokens inside `span`: ascending, disjoint, clipped to it.
    /// Read off the syntax tree, so it costs a walk over the span only.
    pub fn highlights(&self, span: Range<usize>) -> Vec<(Range<usize>, Hl)> {
        let mut out = Vec::new();
        let mut c = self.tree.walk();
        'outer: loop {
            let n = c.node();
            let (s, e) = (n.start_byte(), n.end_byte());
            if s >= span.end {
                break; // pre-order: nothing later starts earlier
            }
            let mut descend = false;
            if e > span.start {
                match classify(n) {
                    Some(h) => out.push((s.max(span.start)..e.min(span.end), h)),
                    None => descend = true,
                }
            }
            if descend && c.goto_first_child() {
                continue;
            }
            loop {
                if c.goto_next_sibling() {
                    continue 'outer;
                }
                if !c.goto_parent() {
                    break 'outer;
                }
            }
        }
        out
    }

    /// Identity of the item whose bytes are exactly `span`.
    pub fn item_at(&self, span: &Range<usize>) -> Option<ItemPath> {
        let it = &self.syn.items;
        let mut l = (0..it.kind.len()).find(|&l| it.bytes[l].range() == *span)?;
        let mut path = ItemPath::new();
        loop {
            let same = |j: usize| it.parent[j] == it.parent[l] && it.kind[j] == it.kind[l] && it.name[j] == it.name[l];
            let ordinal = (0..l).filter(|&j| same(j)).count() as u32;
            path.push((it.kind[l], it.name[l].clone(), ordinal));
            if it.parent[l] == LOCAL_NONE {
                break;
            }
            l = it.parent[l] as usize;
        }
        path.reverse();
        Some(path)
    }

    /// Where the item with this identity is now.
    pub fn find_item(&self, path: &ItemPath) -> Option<Range<usize>> {
        let it = &self.syn.items;
        let mut parent = LOCAL_NONE;
        for (kind, name, ordinal) in path {
            parent = (0..it.kind.len())
                .filter(|&j| it.parent[j] == parent && it.kind[j] == *kind && it.name[j] == *name)
                .nth(*ordinal as usize)? as u32;
        }
        (parent != LOCAL_NONE).then(|| it.bytes[parent as usize].range())
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
    fn highlights_are_clipped_and_classified() {
        let src = "// c\nfn alpha<'x>(v: &'x mut Foo) -> u32 { b::callee(1); v.meth(\"s\"); println!(\"{}\", 2) }\nfn z() {}\n";
        let b = Buffer::new(src.to_string());
        let end = src.find("fn z").unwrap();
        let hl = b.highlights(5..end);
        let of = |t: &str| {
            let at = src.find(t).unwrap();
            hl.iter().find(|(r, _)| *r == (at..at + t.len())).map(|(_, h)| *h)
        };
        assert_eq!(of("fn"), Some(Hl::Keyword));
        assert_eq!(of("alpha"), Some(Hl::Function));
        assert_eq!(of("'x"), Some(Hl::Lifetime));
        assert_eq!(of("mut"), Some(Hl::Keyword));
        assert_eq!(of("Foo"), Some(Hl::Type));
        assert_eq!(of("u32"), Some(Hl::Type));
        assert_eq!(of("callee"), Some(Hl::Function));
        assert_eq!(of("meth"), Some(Hl::Function));
        assert_eq!(of("\"s\""), Some(Hl::String));
        assert_eq!(of("println"), Some(Hl::Macro));
        assert_eq!(of("// c"), None, "outside the span");
        assert!(hl.windows(2).all(|w| w[0].0.end <= w[1].0.start));
        assert!(hl.iter().all(|(r, _)| r.start >= 5 && r.end <= end));
        assert_eq!(b.highlights(0..4), [(0..4, Hl::Comment)]);
    }

    #[test]
    fn item_paths_survive_moves() {
        let a = Buffer::new("impl S { fn m() {} }\nimpl S { fn m() { 1 } }\nfn m() {}\n".to_string());
        let span = a.text().find("fn m() { 1 }").unwrap();
        let span = span..span + "fn m() { 1 }".len();
        let path = a.item_at(&span).unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].2, 1, "second `impl S`");
        // Same item after unrelated text moved and changed it.
        let b =
            Buffer::new("fn m() {}\n\nimpl S { fn m() {} }\nstruct S;\nimpl S {\n    fn m() { 2 }\n}\n".to_string());
        assert_eq!(&b.text()[b.find_item(&path).unwrap()], "fn m() { 2 }");
        assert_eq!(a.item_at(&(0..3)), None);
        assert_eq!(Buffer::new("fn m() {}".to_string()).find_item(&path), None);
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
