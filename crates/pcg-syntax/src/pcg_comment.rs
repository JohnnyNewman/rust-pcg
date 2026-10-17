//! `@pcg:summary` / `@pcg:intent` structured comments: parsing and writing.
//!
//! Format (Rust):
//! ```text
//! /// @pcg:intent Parse the config file and fall back to defaults on error.
//! /// @pcg:summary[h=3fa9c1] Reads TOML from path, merges with Default,
//! ///   logs warnings.
//! ```
//! A tag's text continues on following doc lines that are indented by at
//! least two spaces after the `///`. `h=` is [`short_hash`] of the item's
//! code subtree; a mismatch marks the summary stale.

use pcg_core::{CommentKind, FileId, Graph, NodeId, NodeKind, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPcgComment {
    pub kind: CommentKind,
    pub stored_hash: Option<u32>,
    pub bytes: Span,
    pub text: String,
}

/// 24-bit short form of a content hash, as stored in `h=`.
#[inline]
pub fn short_hash(h: u64) -> u32 {
    (h ^ (h >> 24) ^ (h >> 48)) as u32 & 0xFF_FFFF
}

/// Parse doc-comment lines (text after `///` / `//!`, with their byte spans).
pub fn parse_doc_lines(lines: &[(Span, &str)]) -> Vec<RawPcgComment> {
    let mut out: Vec<RawPcgComment> = Vec::new();
    let mut open = false;
    for &(span, raw) in lines {
        let raw = raw.trim_end_matches(['\r', '\n']);
        let body = raw.strip_prefix(' ').unwrap_or(raw);
        let tag = if let Some(r) = body.strip_prefix("@pcg:summary") {
            Some((CommentKind::Summary, r))
        } else {
            body.strip_prefix("@pcg:intent").map(|r| (CommentKind::Intent, r))
        };
        if let Some((kind, mut rest)) = tag {
            let mut stored_hash = None;
            if let Some(r) = rest.strip_prefix("[h=")
                && let Some(end) = r.find(']')
            {
                stored_hash = u32::from_str_radix(r[..end].trim(), 16).ok();
                rest = &r[end + 1..];
            }
            out.push(RawPcgComment { kind, stored_hash, bytes: span, text: rest.trim().to_string() });
            open = true;
        } else if open && body.starts_with(char::is_whitespace) && !body.trim().is_empty() {
            let c = out.last_mut().unwrap();
            if !c.text.is_empty() {
                c.text.push(' ');
            }
            c.text.push_str(body.trim());
            c.bytes.end = span.end;
        } else {
            open = false;
        }
    }
    out
}

/// A replacement of `range` in `file` by `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub file: FileId,
    pub range: Span,
    pub text: String,
}

impl TextEdit {
    pub fn apply(&self, src: &str) -> String {
        let mut s = String::with_capacity(src.len() + self.text.len());
        s.push_str(&src[..self.range.start as usize]);
        s.push_str(&self.text);
        s.push_str(&src[self.range.end as usize..]);
        s
    }
}

/// Render a summary comment block, wrapped to `width` columns.
pub fn format_summary(prefix: &str, indent: &str, hash: u64, text: &str, width: usize) -> String {
    format_tag(prefix, indent, &format!("@pcg:summary[h={:06x}]", short_hash(hash)), text, width)
}

/// Render an intent comment block, wrapped to `width` columns.
pub fn format_intent(prefix: &str, indent: &str, text: &str, width: usize) -> String {
    format_tag(prefix, indent, "@pcg:intent", text, width)
}

/// `{indent}{prefix} {tag} text…`, continuation lines indented two spaces
/// past the prefix (what [`parse_doc_lines`] reads back as one comment).
fn format_tag(prefix: &str, indent: &str, tag: &str, text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = format!("{indent}{prefix} {tag}");
    let cont = format!("{indent}{prefix}   ");
    let mut fresh_line = false;
    for word in text.split_whitespace() {
        if !fresh_line && line.len() + 1 + word.len() > width {
            out.push_str(&line);
            out.push('\n');
            line = cont.clone();
            fresh_line = true;
        }
        if !fresh_line {
            line.push(' ');
        }
        line.push_str(word);
        fresh_line = false;
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// Apply several non-overlapping edits of one file at once (back to front,
/// so earlier ranges stay valid). Inserts at the same place keep their order
/// in `edits` (a file module's `//!` before its first item's `///`).
pub fn apply_edits(src: &str, edits: &[&TextEdit]) -> String {
    let mut order: Vec<(usize, &TextEdit)> = edits.iter().copied().enumerate().collect();
    order.sort_by_key(|&(k, e)| std::cmp::Reverse((e.range.start, k)));
    let mut s = src.to_string();
    for (_, e) in order {
        s.replace_range(e.range.range(), &e.text);
    }
    s
}

/// Remove every `@pcg:` tag (with its continuation lines) from doc comments
/// in `code` — for code that comes back from a generator and must not carry
/// stale or invented summaries.
pub fn strip_tags(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut in_tag = false;
    for line in code.split_inclusive('\n') {
        let t = line.trim_start();
        let body = t.strip_prefix("///").or_else(|| t.strip_prefix("//!"));
        match body {
            Some(b) if b.trim_start().starts_with("@pcg:") => {
                in_tag = true;
                continue;
            }
            Some(b) if in_tag && b.starts_with("  ") && !b.trim().is_empty() => continue,
            _ => in_tag = false,
        }
        out.push_str(line);
    }
    out
}

fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end_incl(src: &str, at: usize) -> usize {
    src[at..].find('\n').map_or(src.len(), |i| at + i + 1)
}

/// Build the edit that writes (or rewrites) `node`'s summary with the node's
/// *current* content hash. Per decision 7 this is only applied on save/accept.
pub fn summary_edit(g: &Graph, node: NodeId, text: &str) -> Option<TextEdit> {
    let file = g.nodes.file[node.idx()];
    if !g.summarizable(node) {
        return None;
    }
    let src: &str = &g.files.source[file.idx()];
    let is_file_module = g.nodes.kind[node.idx()] == NodeKind::FileModule || g.files.module[file.idx()] == node;
    let prefix = if is_file_module { "//!" } else { "///" };
    let hash = if is_file_module { g.files.content_hash[file.idx()] } else { g.nodes.content_hash[node.idx()] };

    let existing = g.nodes.summary[node.idx()];
    let (range, indent_at) = if existing.is_some() {
        let b = g.comments.bytes[existing.idx()];
        let s = line_start(src, b.start as usize);
        (Span::new(s as u32, line_end_incl(src, (b.end as usize).saturating_sub(1).max(s)) as u32), s)
    } else if is_file_module {
        (Span::new(0, 0), 0)
    } else {
        let s = line_start(src, g.nodes.bytes[node.idx()].start as usize);
        (Span::new(s as u32, s as u32), s)
    };
    let indent: String = src[indent_at..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let mut out = format_summary(prefix, &indent, hash, text, 100);
    if src.contains("\r\n") {
        out = out.replace('\n', "\r\n"); // keep the file's line endings (Windows)
    }
    Some(TextEdit { file, range, text: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(i: u32) -> Span {
        Span::new(i * 10, i * 10 + 9)
    }

    #[test]
    fn parse_tags_and_continuations() {
        let lines = [
            (sp(0), " Plain docs."),
            (sp(1), " @pcg:summary[h=3fa9c1] Reads TOML,"),
            (sp(2), "   merges defaults."),
            (sp(3), " more plain docs"),
            (sp(4), " @pcg:intent Do it."),
        ];
        let c = parse_doc_lines(&lines);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].kind, CommentKind::Summary);
        assert_eq!(c[0].stored_hash, Some(0x3fa9c1));
        assert_eq!(c[0].text, "Reads TOML, merges defaults.");
        assert_eq!(c[0].bytes, Span::new(10, 29));
        assert_eq!(c[1].kind, CommentKind::Intent);
        assert_eq!(c[1].stored_hash, None);
    }

    #[test]
    fn strip_and_apply() {
        let code = "/// Docs.
/// @pcg:summary[h=000001] Old,
///   older.
/// @pcg:intent X
fn a() {}
";
        assert_eq!(
            strip_tags(code),
            "/// Docs.
fn a() {}
"
        );
        let f = FileId::from_idx(0);
        let e1 = TextEdit { file: f, range: Span::new(0, 1), text: "AA".into() };
        let e2 = TextEdit { file: f, range: Span::new(3, 3), text: "-".into() };
        assert_eq!(apply_edits("abcd", &[&e1, &e2]), "AAbc-d");
        let i1 = TextEdit { file: f, range: Span::new(0, 0), text: "1".into() };
        let i2 = TextEdit { file: f, range: Span::new(0, 0), text: "2".into() };
        assert_eq!(apply_edits("x", &[&i1, &i2]), "12x");
        let i = format_intent("///", "  ", "Do it.", 100);
        assert_eq!(
            i,
            "  /// @pcg:intent Do it.
"
        );
    }

    #[test]
    fn format_wraps_and_reparses() {
        let text = "word ".repeat(40);
        let s = format_summary("///", "    ", 0xabcdef, &text, 60);
        assert!(s.lines().all(|l| l.len() <= 60));
        assert!(s.lines().skip(1).all(|l| l.starts_with("    ///   ")));
        let lines: Vec<(Span, &str)> =
            s.lines().map(|l| (Span::default(), l.trim_start().strip_prefix("///").unwrap())).collect();
        let c = parse_doc_lines(&lines);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, text.trim());
        assert_eq!(c[0].stored_hash, Some(short_hash(0xabcdef)));
    }
}
