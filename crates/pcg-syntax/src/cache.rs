//! Per-file parse cache: the incremental part of the pipeline.
//!
//! Parsing is the dominant cost of a build, and it is a pure function of one
//! file's text. So a rebuild re-runs scan / assemble / resolve over the whole
//! project (cheap, serial, keeps the pre-order tables trivially consistent)
//! but only re-parses files whose text changed.
//!
//! Change detection per file, cheapest first:
//! 1. `(mtime, len)` unchanged and the mtime is not "racy" → reuse.
//! 2. Otherwise read the file; identical bytes → reuse the parse.
//! 3. Otherwise parse.
//!
//! "Racy" (as in git): an mtime within [`RACY_SECS`] of the previous build may
//! hide a second write in the same timestamp tick, so it is always re-read.
//!
//! Syntax trees are not cached: an external save gives no edit ranges, so
//! tree-sitter's incremental reparse has nothing to work with, and a fresh
//! per-file parse is already ~1 ms. Files open in an editor are different:
//! their edits have known ranges, so a [`crate::edit::Buffer`] keeps the tree
//! and hands its result to the build as an [`Overlays`] entry, which wins over
//! whatever is on disk.

use crate::parse::{FileSyntax, parse_file};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const RACY_SECS: u64 = 2;

#[derive(Debug, Clone)]
pub struct CachedFile {
    pub mtime: Option<SystemTime>,
    pub len: u64,
    pub src: Arc<str>,
    pub syn: Arc<FileSyntax>,
}

#[derive(Debug, Default)]
pub struct ParseCache {
    pub files: FxHashMap<PathBuf, CachedFile>,
    /// Wall-clock start of the build that filled the cache.
    pub built_at: Option<SystemTime>,
}

/// Unsaved editor buffers by path: text + parse that replace the file's
/// on-disk content for one build.
pub type Overlays = FxHashMap<PathBuf, (Arc<str>, Arc<FileSyntax>)>;

/// How a file's parse was obtained.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fetch {
    /// Metadata unchanged.
    Hit,
    /// Re-read, bytes identical.
    SameBytes,
    /// Parsed (new or changed).
    Parsed,
    /// Taken from an unsaved editor buffer.
    Overlay,
}

impl ParseCache {
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Look up or (re)parse one file. Pure w.r.t. the cache (read-only), so it can
/// run in parallel; the caller writes the results back.
pub fn fetch(cache: &ParseCache, overlays: &Overlays, path: &Path) -> Option<(CachedFile, Fetch)> {
    if let Some((src, syn)) = overlays.get(path) {
        // No mtime: once the overlay is gone the file is re-read and compared.
        return Some((CachedFile { mtime: None, len: 0, src: src.clone(), syn: syn.clone() }, Fetch::Overlay));
    }
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok();
    let len = meta.len();
    let old = cache.files.get(path);
    if let Some(old) = old
        && old.mtime == mtime
        && old.len == len
        && !is_racy(mtime, cache.built_at)
    {
        return Some((old.clone(), Fetch::Hit));
    }
    let text = std::fs::read_to_string(path).ok()?;
    if let Some(old) = old
        && *old.src == *text
    {
        return Some((CachedFile { mtime, len, ..old.clone() }, Fetch::SameBytes));
    }
    let syn = Arc::new(parse_file(&text));
    Some((CachedFile { mtime, len, src: text.into(), syn }, Fetch::Parsed))
}

fn is_racy(mtime: Option<SystemTime>, built_at: Option<SystemTime>) -> bool {
    match (mtime, built_at) {
        (Some(m), Some(b)) => m + Duration::from_secs(RACY_SECS) >= b,
        _ => true,
    }
}
