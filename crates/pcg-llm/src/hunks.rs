//! A rewrite, item by item.
//!
//! A proposal for a container (a module, an impl, a trait) usually touches
//! several of its items. [`hunks`] lists them — each item of the old or new
//! text one level below the rewritten node (plus helper items placed beside
//! it) that was changed, added or removed — and [`revert`] undoes one of
//! them in the proposal text, keeping the others. Items are matched by
//! identity `(kind, name, ordinal)`, as everywhere else in pcg, so a renamed
//! item reads as one removed and one added.

use pcg_syntax::parse::LOCAL_NONE;
use pcg_syntax::{Buffer, ItemPath};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Changed,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The item's identity within the rewritten text.
    pub path: ItemPath,
    pub change: Change,
    /// `fn name`, for the UI.
    pub label: String,
}

/// The items one level below the rewritten node (index 0), and items beside
/// it: `(identity, bytes, label)` in text order.
fn items(b: &Buffer) -> Vec<(ItemPath, Range<usize>, String)> {
    let it = &b.syntax().items;
    (1..it.kind.len())
        .filter(|&l| it.parent[l] == 0 || it.parent[l] == LOCAL_NONE)
        .filter_map(|l| {
            let r = it.bytes[l].range();
            Some((b.item_at(&r)?, r, format!("{} {}", it.kind[l].label(), it.name[l])))
        })
        .collect()
}

/// What `new` changes in `old`, item by item. Empty when the rewritten node
/// has no items (a function): then the proposal is one change.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let (ob, nb) = (Buffer::new(old.to_string()), Buffer::new(new.to_string()));
    let (oi, ni) = (items(&ob), items(&nb));
    let mut out = Vec::new();
    for (path, r, label) in &oi {
        match ni.iter().find(|(p, ..)| p == path) {
            Some((_, nr, _)) if old[r.clone()] != new[nr.clone()] => {
                out.push(Hunk { path: path.clone(), change: Change::Changed, label: label.clone() })
            }
            Some(_) => {}
            None => out.push(Hunk { path: path.clone(), change: Change::Removed, label: label.clone() }),
        }
    }
    for (path, _, label) in &ni {
        if !oi.iter().any(|(p, ..)| p == path) {
            out.push(Hunk { path: path.clone(), change: Change::Added, label: label.clone() });
        }
    }
    out
}

fn line_start(s: &str, at: usize) -> usize {
    s[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn indent_at(s: &str, at: usize) -> &str {
    let ls = line_start(s, at);
    let pre = &s[ls..at];
    if pre.chars().all(|c| c == ' ' || c == '\t') { pre } else { "" }
}

/// `new` with hunk `h` undone (the item as it was in `old`, gone again, or
/// back in its place). `None` if the item cannot be placed.
pub fn revert(old: &str, new: &str, h: &Hunk) -> Option<String> {
    let (ob, nb) = (Buffer::new(old.to_string()), Buffer::new(new.to_string()));
    let mut s = new.to_string();
    match h.change {
        Change::Changed => {
            let (o, n) = (ob.find_item(&h.path)?, nb.find_item(&h.path)?);
            s.replace_range(n, &old[o]);
        }
        Change::Added => {
            let n = nb.find_item(&h.path)?;
            // The whole lines it occupies, and the blank line after it (or else before it).
            let mut start = line_start(new, n.start);
            let mut end = n.end;
            let rest = &new[end..];
            let trail = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            end += trail;
            if new[end..].starts_with('\n') {
                end += 1;
                let next = new[end..].find('\n').map(|i| end + i);
                match next {
                    Some(nl) if new[end..nl].trim().is_empty() => end = nl + 1,
                    _ if start > 0 => {
                        let prev = line_start(new, start - 1);
                        if new[prev..start].trim().is_empty() {
                            start = prev;
                        }
                    }
                    _ => {}
                }
            }
            s.replace_range(start..end, "");
        }
        Change::Removed => {
            let o = ob.find_item(&h.path)?;
            let text = &old[o.clone()];
            let indent = indent_at(old, o.start);
            // After the nearest earlier item that is still there, else before the nearest later one.
            let order = items(&ob);
            let k = order.iter().position(|(p, ..)| *p == h.path)?;
            let before = order[..k].iter().rev().find_map(|(p, ..)| nb.find_item(p));
            let after = order[k + 1..].iter().find_map(|(p, ..)| nb.find_item(p));
            match (before, after) {
                (Some(b), _) => s.insert_str(b.end, &format!("\n\n{indent}{text}")),
                (None, Some(a)) => s.insert_str(a.start, &format!("{text}\n\n{indent}")),
                (None, None) => {
                    // The container is empty now: put it before its closing brace.
                    let close = new.trim_end().rfind('}')?;
                    let at = line_start(new, close);
                    s.insert_str(at, &format!("{indent}{text}\n"));
                }
            }
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "impl S {\n    fn a() {}\n\n    fn b() {}\n\n    fn c() {}\n}";
    const NEW: &str = "/// @pcg:intent X\nimpl S {\n    fn a() { 1; }\n\n    fn c() {}\n\n    fn d() {}\n}";

    #[test]
    fn lists_changes() {
        let h: Vec<(Change, String)> = hunks(OLD, NEW).into_iter().map(|h| (h.change, h.label)).collect();
        let want = [(Change::Changed, "fn a"), (Change::Removed, "fn b"), (Change::Added, "fn d")];
        assert_eq!(h, want.map(|(c, l)| (c, l.to_string())));
        assert!(hunks("fn f() {}", "fn f() { 2; }").is_empty(), "a function is one change");
    }

    #[test]
    fn reverts_each() {
        let hs = hunks(OLD, NEW);
        let a = revert(OLD, NEW, &hs[0]).unwrap();
        assert_eq!(a, "/// @pcg:intent X\nimpl S {\n    fn a() {}\n\n    fn c() {}\n\n    fn d() {}\n}");
        let b = revert(OLD, NEW, &hs[1]).unwrap();
        assert_eq!(
            b,
            "/// @pcg:intent X\nimpl S {\n    fn a() { 1; }\n\n    fn b() {}\n\n    fn c() {}\n\n    fn d() {}\n}"
        );
        let d = revert(OLD, NEW, &hs[2]).unwrap();
        assert_eq!(d, "/// @pcg:intent X\nimpl S {\n    fn a() { 1; }\n\n    fn c() {}\n}");
        // All three undone, one after the other: only the intent is left.
        let mut s = NEW.to_string();
        while let Some(h) = hunks(OLD, &s).first() {
            s = revert(OLD, &s, h).unwrap();
        }
        assert_eq!(s, format!("/// @pcg:intent X\n{OLD}"));
    }
}
