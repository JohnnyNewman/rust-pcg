//! Generated summaries, keyed by what they describe.
//!
//! A node's `content_hash` covers its whole code subtree (comments and
//! whitespace excluded), so `(kind, content_hash)` names exactly the code a
//! summary was written for: the same function in another branch, after a
//! revert, or in a copy of the project hits the same entry. Any edit misses
//! it, and the stale entry simply stops being looked up.
//!
//! Decision 6/7: this cache is not the source. It lives in the gitignored
//! `<project>/.pcg/summaries.json`; a summary reaches the code only as a
//! `@pcg:summary` comment the user accepted.

use pcg_core::{Graph, NodeId};
use rustc_hash::FxHashMap;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Bumped when prompts change enough that old summaries should be redone.
const PROMPT_VERSION: u64 = 1;

/// Cache key of `n`'s summary.
pub fn key(g: &Graph, n: NodeId) -> u64 {
    let i = n.idx();
    let mut b = [0u8; 17];
    b[0] = g.nodes.kind[i] as u8;
    b[1..9].copy_from_slice(&g.nodes.content_hash[i].to_le_bytes());
    b[9..].copy_from_slice(&PROMPT_VERSION.to_le_bytes());
    xxhash_rust::xxh3::xxh3_64(&b)
}

#[derive(Default, Debug, Clone)]
pub struct SummaryCache {
    pub entries: FxHashMap<u64, String>,
    /// Entries added since the last [`save`](Self::save).
    pub dirty: bool,
}

impl SummaryCache {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".pcg").join("summaries.json")
    }

    pub fn get(&self, g: &Graph, n: NodeId) -> Option<&str> {
        self.entries.get(&key(g, n)).map(String::as_str)
    }

    pub fn insert(&mut self, key: u64, text: String) {
        self.entries.insert(key, text);
        self.dirty = true;
    }

    /// The cache of the project at `root`; empty if there is none (or it is unreadable).
    pub fn load(root: &Path) -> Self {
        let Ok(s) = std::fs::read_to_string(Self::path(root)) else { return Self::default() };
        let Ok(v) = serde_json::from_str::<Value>(&s) else { return Self::default() };
        let entries = v["summaries"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, t)| Some((u64::from_str_radix(k, 16).ok()?, t.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Self { entries, dirty: false }
    }

    pub fn save(&mut self, root: &Path) -> std::io::Result<()> {
        let path = Self::path(root);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut keys: Vec<&u64> = self.entries.keys().collect();
        keys.sort_unstable(); // stable file for stable diffs
        let map: serde_json::Map<String, Value> =
            keys.into_iter().map(|k| (format!("{k:016x}"), json!(self.entries[k]))).collect();
        let text = serde_json::to_string_pretty(&json!({ "version": 1, "summaries": map }))?;
        // Write-then-rename: a crash never leaves half a file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        self.dirty = false;
        Ok(())
    }
}
