//! Per-file parse stage. Pure function of the source text, runs in parallel.
//!
//! Produces *file-local* tables (local indices, owned strings); the serial
//! [`assemble`](crate::assemble) stage maps them to global ids.

use crate::pcg_comment::{RawPcgComment, parse_doc_lines};
use pcg_core::{CommentKind, NodeKind, Span};
use std::cell::RefCell;
use tree_sitter::{Node, Parser, TreeCursor};
use xxhash_rust::xxh3::xxh3_64_with_seed;

pub const LOCAL_NONE: u32 = u32::MAX;

/// Items in pre-order, file-local.
#[derive(Default, Debug)]
pub struct LocalItems {
    pub kind: Vec<NodeKind>,
    pub name: Vec<String>,
    /// Local parent index, or `LOCAL_NONE` for top-level items (parent = file module).
    pub parent: Vec<u32>,
    pub subtree_end: Vec<u32>,
    /// Span including leading doc comments / attributes.
    pub bytes: Vec<Span>,
    pub lines: Vec<Span>,
    pub hash: Vec<u64>,
    /// For impls: the self type's base name (`Foo` in `impl<T> Tr for Foo<T>`).
    pub impl_self: Vec<Option<String>>,
    /// For trait impls: the trait's base name.
    pub impl_trait: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalleeForm {
    /// `foo(..)`
    Plain,
    /// `a::b::foo(..)` / `Type::foo(..)`; carries the qualifier's last segment.
    Qualified(String),
    /// `x.foo(..)`
    Method,
    /// `foo!(..)`
    Macro,
}

#[derive(Debug, Clone)]
pub struct LocalCall {
    /// Innermost enclosing item (local index).
    pub caller: u32,
    pub callee: String,
    pub form: CalleeForm,
}

#[derive(Debug, Clone)]
pub struct LocalComment {
    /// Local item index, or `LOCAL_NONE` for the file module (`//!` comments).
    pub item: u32,
    pub kind: CommentKind,
    pub stored_hash: Option<u32>,
    pub bytes: Span,
    pub text: String,
}

#[derive(Debug)]
pub struct FileSyntax {
    pub items: LocalItems,
    pub calls: Vec<LocalCall>,
    pub comments: Vec<LocalComment>,
    /// Hash of the whole file's code (comments/whitespace excluded).
    pub file_hash: u64,
    pub has_errors: bool,
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into()).expect("tree-sitter-rust ABI mismatch");
        p
    });
}

pub fn parse_file(src: &str) -> FileSyntax {
    let tree = PARSER.with_borrow_mut(|p| p.parse(src, None)).expect("parser has a language");
    let mut cx = Cx {
        src: src.as_bytes(),
        items: LocalItems::default(),
        code: Vec::new(),
        calls: Vec::new(),
        comments: Vec::new(),
    };
    let root = tree.root_node();
    cx.inner_doc_comments(root);
    cx.declarations(root, LOCAL_NONE);
    let file_hash = hash_tokens(root, cx.src, &cx.code, &mut cx.items.hash);
    FileSyntax { has_errors: root.has_error(), items: cx.items, calls: cx.calls, comments: cx.comments, file_hash }
    // `tree` is dropped here: syntax trees are ~10x the source size, so keeping
    // all of them does not scale. M2 adds a bounded tree cache for incremental reparse.
}

#[inline]
fn mix(acc: u64, t: u64) -> u64 {
    (acc ^ t).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29)
}

/// Content hashes for the file and every item, in **one** walk over the leaf
/// tokens. Each non-comment token is hashed once (`xxh3(text, seed = kind)`)
/// and folded into the accumulators of all items enclosing it, so nesting
/// depth does not multiply the cost. Whitespace is never a token and comments
/// are skipped, so formatting and comment edits don't change any hash.
fn hash_tokens(root: Node, src: &[u8], code: &[Span], out: &mut [u64]) -> u64 {
    let mut file = 0u64;
    let mut active: Vec<(u32, u32, u64)> = Vec::with_capacity(8); // (item, end, acc)
    let mut next_item = 0usize;
    let mut c = root.walk();
    'outer: loop {
        let n = c.node();
        let k = n.kind_id();
        let is_comment = n.is_extra() && matches!(n.kind(), "line_comment" | "block_comment");
        if !is_comment {
            if n.child_count() == 0 {
                let (s, e) = (n.start_byte(), n.end_byte());
                while let Some(&(item, end, acc)) = active.last() {
                    if end as usize > s {
                        break;
                    }
                    out[item as usize] = acc;
                    active.pop();
                }
                while next_item < code.len() && (code[next_item].start as usize) <= s {
                    if (code[next_item].end as usize) > s {
                        active.push((next_item as u32, code[next_item].end, 0x51_7cc1_b727_220a));
                    }
                    next_item += 1;
                }
                let t = xxh3_64_with_seed(&src[s..e], k as u64);
                file = mix(file, t);
                for a in active.iter_mut() {
                    a.2 = mix(a.2, t);
                }
            } else if c.goto_first_child() {
                continue;
            }
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
    for (item, _, acc) in active {
        out[item as usize] = acc;
    }
    file
}

struct Cx<'s> {
    src: &'s [u8],
    items: LocalItems,
    /// Byte range of each item's syntax node (without leading docs), for hashing.
    code: Vec<Span>,
    calls: Vec<LocalCall>,
    comments: Vec<LocalComment>,
}

fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[n.start_byte()..n.end_byte()]).unwrap_or("")
}

fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Base name of a type expression: `a::Foo<T>` → `Foo`, `&mut [u8]` → `[u8]`.
fn type_base_name(n: Node, src: &[u8]) -> String {
    match n.kind() {
        "generic_type" => n.child_by_field_name("type").map(|t| type_base_name(t, src)).unwrap_or_default(),
        "scoped_type_identifier" => n.child_by_field_name("name").map(|t| text(t, src).to_string()).unwrap_or_default(),
        "reference_type" | "pointer_type" => {
            n.child_by_field_name("type").map(|t| type_base_name(t, src)).unwrap_or_default()
        }
        _ => squash_ws(text(n, src)),
    }
}

fn is_outer_doc(n: Node, src: &[u8]) -> bool {
    let t = text(n, src);
    (t.starts_with("///") && !t.starts_with("////")) || (t.starts_with("/**") && !t.starts_with("/***"))
}

impl<'s> Cx<'s> {
    /// `//!` comments at the top of the file describe the file module.
    fn inner_doc_comments(&mut self, root: Node) {
        let mut lines: Vec<(Span, &'s str)> = Vec::new();
        let mut c = root.walk();
        for n in root.named_children(&mut c) {
            match n.kind() {
                "line_comment" => {
                    let t = text(n, self.src);
                    if let Some(rest) = t.strip_prefix("//!") {
                        lines.push((Span::new(n.start_byte() as u32, n.end_byte() as u32), rest));
                    }
                }
                "inner_attribute_item" => {}
                _ => break,
            }
        }
        self.push_comments(LOCAL_NONE, &lines);
    }

    fn push_comments(&mut self, item: u32, lines: &[(Span, &str)]) {
        for RawPcgComment { kind, stored_hash, bytes, text } in parse_doc_lines(lines) {
            self.comments.push(LocalComment { item, kind, stored_hash, bytes, text });
        }
    }

    /// Walk item declarations inside a `source_file` or `declaration_list`.
    fn declarations(&mut self, container: Node<'s>, parent: u32) {
        let mut c = container.walk();
        let children: Vec<Node> = container.named_children(&mut c).collect();
        for n in children {
            self.item(n, parent);
        }
    }

    fn item(&mut self, n: Node<'s>, parent: u32) {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src).to_string()).unwrap_or_default();
        let (kind, name, body, impl_self, impl_trait) = match n.kind() {
            "function_item" | "function_signature_item" => (NodeKind::Fn, name_of(n), None, None, None),
            "struct_item" => (NodeKind::Struct, name_of(n), None, None, None),
            "enum_item" => (NodeKind::Enum, name_of(n), None, None, None),
            "union_item" => (NodeKind::Union, name_of(n), None, None, None),
            "type_item" => (NodeKind::TypeAlias, name_of(n), None, None, None),
            "const_item" => (NodeKind::Const, name_of(n), None, None, None),
            "static_item" => (NodeKind::Static, name_of(n), None, None, None),
            "macro_definition" => (NodeKind::Macro, name_of(n), None, None, None),
            "trait_item" => (NodeKind::Trait, name_of(n), n.child_by_field_name("body"), None, None),
            "mod_item" => match n.child_by_field_name("body") {
                Some(b) => (NodeKind::InlineModule, name_of(n), Some(b), None, None),
                None => return, // `mod foo;` — the file module comes from the scan.
            },
            "impl_item" => {
                let ty = n.child_by_field_name("type");
                let self_name = ty.map(|t| type_base_name(t, src));
                let ty_text = ty.map(|t| squash_ws(text(t, src))).unwrap_or_default();
                let tr = n.child_by_field_name("trait");
                let name = match tr {
                    Some(tr) => format!("{} for {}", squash_ws(text(tr, src)), ty_text),
                    None => ty_text,
                };
                let trait_name = tr.map(|t| type_base_name(t, src));
                (NodeKind::Impl, name, n.child_by_field_name("body"), self_name, trait_name)
            }
            _ => return,
        };

        // Leading doc comments + attributes belong to the item.
        let mut start = n.start_byte();
        let mut start_row = n.start_position().row;
        let mut docs: Vec<(Span, &'s str)> = Vec::new();
        let mut p = n.prev_named_sibling();
        while let Some(s) = p {
            match s.kind() {
                "attribute_item" => {}
                "line_comment" | "block_comment" if is_outer_doc(s, src) => {
                    let t = text(s, src);
                    if let Some(rest) = t.strip_prefix("///") {
                        docs.push((Span::new(s.start_byte() as u32, s.end_byte() as u32), rest));
                    }
                }
                _ => break,
            }
            start = s.start_byte();
            start_row = s.start_position().row;
            p = s.prev_named_sibling();
        }
        docs.reverse();

        let idx = self.items.kind.len() as u32;
        let it = &mut self.items;
        it.kind.push(kind);
        it.name.push(name);
        it.parent.push(parent);
        it.subtree_end.push(idx + 1);
        it.bytes.push(Span::new(start as u32, n.end_byte() as u32));
        it.lines.push(Span::new(start_row as u32, n.end_position().row as u32 + 1));
        it.hash.push(0); // filled by `hash_tokens`
        self.code.push(Span::new(n.start_byte() as u32, n.end_byte() as u32));
        it.impl_self.push(impl_self);
        it.impl_trait.push(impl_trait);
        self.push_comments(idx, &docs);

        if let Some(body) = body {
            self.declarations(body, idx);
        } else if kind == NodeKind::Fn {
            if let Some(b) = n.child_by_field_name("body") {
                self.calls_in(b, idx);
            }
        } else if matches!(kind, NodeKind::Const | NodeKind::Static)
            && let Some(v) = n.child_by_field_name("value")
        {
            self.calls_in(v, idx);
        }
        self.items.subtree_end[idx as usize] = self.items.kind.len() as u32;
    }

    /// Collect call sites below `node` (iterative DFS with a cursor).
    fn calls_in(&mut self, node: Node, caller: u32) {
        let mut c: TreeCursor = node.walk();
        'outer: loop {
            let n = c.node();
            match n.kind() {
                "call_expression" => {
                    if let Some(f) = n.child_by_field_name("function") {
                        self.call_target(f, caller);
                    }
                }
                "macro_invocation" => {
                    if let Some(m) = n.child_by_field_name("macro") {
                        let name = match m.kind() {
                            "scoped_identifier" => m.child_by_field_name("name").map(|x| text(x, self.src)),
                            _ => Some(text(m, self.src)),
                        };
                        if let Some(name) = name {
                            self.calls.push(LocalCall { caller, callee: name.to_string(), form: CalleeForm::Macro });
                        }
                    }
                }
                _ => {}
            }
            if c.goto_first_child() {
                continue;
            }
            loop {
                if c.node() == node {
                    break 'outer;
                }
                if c.goto_next_sibling() {
                    continue 'outer;
                }
                if !c.goto_parent() {
                    break 'outer;
                }
            }
        }
    }

    fn call_target(&mut self, f: Node, caller: u32) {
        let src = self.src;
        let (callee, form) = match f.kind() {
            "identifier" => (text(f, src).to_string(), CalleeForm::Plain),
            "scoped_identifier" => {
                let Some(name) = f.child_by_field_name("name") else { return };
                let q = f
                    .child_by_field_name("path")
                    .map(|p| match p.kind() {
                        "scoped_identifier" => {
                            p.child_by_field_name("name").map(|x| text(x, src).to_string()).unwrap_or_default()
                        }
                        "generic_type" => type_base_name(p, src),
                        _ => text(p, src).to_string(),
                    })
                    .unwrap_or_default();
                (text(name, src).to_string(), CalleeForm::Qualified(q))
            }
            "field_expression" => {
                let Some(field) = f.child_by_field_name("field") else { return };
                (text(field, src).to_string(), CalleeForm::Method)
            }
            "generic_function" => {
                if let Some(inner) = f.child_by_field_name("function") {
                    self.call_target(inner, caller);
                }
                return;
            }
            _ => return,
        };
        self.calls.push(LocalCall { caller, callee, form });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"//! @pcg:summary[h=000000] The module.
use std::fmt;

/// Normal docs.
/// @pcg:intent Add two numbers
///   without overflow checks.
#[inline]
pub fn add(a: i32, b: i32) -> i32 { helper(a) + b }

fn helper(x: i32) -> i32 { Foo::make().get() + x }

pub struct Foo { v: i32 }

impl Foo {
    pub fn make() -> Self { Foo { v: 1 } }
    fn get(&self) -> i32 { println!("x"); self.v }
}

impl fmt::Display for Foo<u8> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "{}", self.v) }
}

mod inner {
    pub(crate) const N: usize = super::helper(1) as usize;
    trait T { fn req(&self); }
}
"#;

    #[test]
    fn items_and_hierarchy() {
        let fs = parse_file(SRC);
        let it = &fs.items;
        let names: Vec<_> = it.name.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            ["add", "helper", "Foo", "Foo", "make", "get", "fmt::Display for Foo<u8>", "fmt", "inner", "N", "T", "req"]
        );
        assert_eq!(it.kind[3], NodeKind::Impl);
        assert_eq!(it.parent[4], 3);
        assert_eq!(it.subtree_end[3], 6);
        assert_eq!(it.impl_self[6].as_deref(), Some("Foo"));
        assert_eq!(it.impl_trait[6].as_deref(), Some("Display"));
        assert_eq!(it.parent[11], 10);
        assert_eq!(it.subtree_end[8], 12);
        // `add` span starts at its doc comment.
        assert!(SRC[it.bytes[0].range()].starts_with("/// Normal docs."));
        assert_eq!(it.lines[0], Span::new(3, 8));
        assert!(!fs.has_errors);
    }

    #[test]
    fn calls() {
        let fs = parse_file(SRC);
        let c: Vec<_> = fs.calls.iter().map(|c| (c.caller, c.callee.as_str(), c.form.clone())).collect();
        assert!(c.contains(&(0, "helper", CalleeForm::Plain)));
        assert!(c.contains(&(1, "make", CalleeForm::Qualified("Foo".into()))));
        assert!(c.contains(&(1, "get", CalleeForm::Method)));
        assert!(c.contains(&(5, "println", CalleeForm::Macro)));
        assert!(c.contains(&(9, "helper", CalleeForm::Qualified("super".into()))));
    }

    #[test]
    fn comments() {
        let fs = parse_file(SRC);
        assert_eq!(fs.comments.len(), 2);
        let m = &fs.comments[0];
        assert_eq!((m.item, m.kind, m.stored_hash), (LOCAL_NONE, CommentKind::Summary, Some(0)));
        let i = &fs.comments[1];
        assert_eq!((i.item, i.kind), (0, CommentKind::Intent));
        assert_eq!(i.text, "Add two numbers without overflow checks.");
    }

    #[test]
    fn hash_ignores_whitespace_and_comments() {
        let a = parse_file("fn f() { let x = 1; g(x) }");
        let b = parse_file("fn f() {\n    // note\n    let x = 1;\n    g(x)\n}\n");
        let c = parse_file("fn f() { let x = 2; g(x) }");
        assert_eq!(a.items.hash[0], b.items.hash[0]);
        assert_ne!(a.items.hash[0], c.items.hash[0]);
        assert_eq!(a.file_hash, b.file_hash);
    }

    #[test]
    fn nested_hashes_are_local() {
        // Changing one method changes it and its impl, but not its sibling.
        let a = parse_file("struct S; impl S { fn a() { 1 } fn b() { 2 } }");
        let b = parse_file("struct S; impl S { fn a() { 1 } fn b() { 3 } }");
        let h = |f: &FileSyntax, i: usize| f.items.hash[i];
        assert_eq!(h(&a, 0), h(&b, 0)); // struct
        assert_ne!(h(&a, 1), h(&b, 1)); // impl
        assert_eq!(h(&a, 2), h(&b, 2)); // fn a
        assert_ne!(h(&a, 3), h(&b, 3)); // fn b
        assert!(a.items.hash.iter().all(|&x| x != 0));
    }
}
