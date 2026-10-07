//! A simple append-only string interner backed by one contiguous byte buffer.
//!
//! All strings live in a single `String`; a symbol is an index into a span
//! column. Lookup goes through a hash map keyed by the string's 64-bit hash
//! (collisions are resolved by comparing bytes), so no string is stored twice.

use rustc_hash::FxHashMap;
use std::hash::{BuildHasher, BuildHasherDefault};

/// An interned string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Sym(pub u32);

impl Sym {
    /// The empty string; always symbol 0.
    pub const EMPTY: Sym = Sym(0);
}

#[derive(Debug)]
pub struct Interner {
    buf: String,
    /// (start, end) byte offsets into `buf`, indexed by `Sym`.
    spans: Vec<(u32, u32)>,
    /// hash → first symbol with that hash; chained via `next`.
    map: FxHashMap<u64, u32>,
    next: Vec<u32>,
}

impl Default for Interner {
    fn default() -> Self {
        let mut s = Self { buf: String::new(), spans: Vec::new(), map: FxHashMap::default(), next: Vec::new() };
        s.intern("");
        s
    }
}

impl Interner {
    fn hash(s: &str) -> u64 {
        BuildHasherDefault::<rustc_hash::FxHasher>::default().hash_one(s)
    }

    pub fn intern(&mut self, s: &str) -> Sym {
        let h = Self::hash(s);
        let mut cur = self.map.get(&h).copied().unwrap_or(u32::MAX);
        while cur != u32::MAX {
            if self.resolve(Sym(cur)) == s {
                return Sym(cur);
            }
            cur = self.next[cur as usize];
        }
        let id = self.spans.len() as u32;
        let start = self.buf.len() as u32;
        self.buf.push_str(s);
        self.spans.push((start, self.buf.len() as u32));
        // Push to the front of the chain.
        let head = self.map.insert(h, id).unwrap_or(u32::MAX);
        self.next.push(head);
        Sym(id)
    }

    /// Look up without inserting.
    pub fn get(&self, s: &str) -> Option<Sym> {
        let mut cur = *self.map.get(&Self::hash(s))?;
        while cur != u32::MAX {
            if self.resolve(Sym(cur)) == s {
                return Some(Sym(cur));
            }
            cur = self.next[cur as usize];
        }
        None
    }

    #[inline]
    pub fn resolve(&self, sym: Sym) -> &str {
        let (a, b) = self.spans[sym.0 as usize];
        &self.buf[a as usize..b as usize]
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.len() <= 1
    }

    /// Bytes used by string data (excluding the index).
    pub fn bytes(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_dedup() {
        let mut i = Interner::default();
        assert_eq!(i.intern(""), Sym::EMPTY);
        let a = i.intern("foo");
        let b = i.intern("bar");
        assert_ne!(a, b);
        assert_eq!(i.intern("foo"), a);
        assert_eq!(i.resolve(a), "foo");
        assert_eq!(i.resolve(b), "bar");
        assert_eq!(i.get("bar"), Some(b));
        assert_eq!(i.get("baz"), None);
        assert_eq!(i.len(), 3);
    }

    #[test]
    fn many() {
        let mut i = Interner::default();
        let syms: Vec<_> = (0..10_000).map(|n| i.intern(&format!("s{n}"))).collect();
        for (n, s) in syms.iter().enumerate() {
            assert_eq!(i.resolve(*s), format!("s{n}"));
            assert_eq!(i.intern(&format!("s{n}")), *s);
        }
    }
}
