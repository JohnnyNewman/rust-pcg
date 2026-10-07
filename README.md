# pcg — a graphical, LLM-ready code editor

A visual IDE (Blueprint-like, but for general-purpose code) written in Rust with
**tree-sitter** and **bevy + bevy_egui**. Text stays the source of truth; the graph
is a projection of it. See the project docs (*vision-and-decisions*,
*roadmap-and-architecture*) for the full design.

**Status: M1 (skeleton & static graph), M2 (incremental reload, animated diff) and
M3 (in-node editing) implemented.**

![Focus on a function: callers (orange) and callees (blue) with flowing dots](docs/screenshots/focus-edges.png)

## Run

```sh
cargo run --release -p pcg-app -- <path-to-a-rust-project>   # default: current dir (dogfooding)
cargo run --release -p pcg-syntax --example dump -- <dir> [--tree]   # headless pipeline + timings
cargo run --release -p pcg-syntax --example parse_bench -- <dir>      # tree-sitter vs. extraction cost
cargo run --release -p pcg-syntax --example edit_bench -- <file.rs>   # cost of one keystroke in an open file
cargo test --workspace
```

First build compiles Bevy (several minutes). `dev` builds use `opt-level = 1` for
our crates and `3` for dependencies, so debug runs are usable.
On Windows the MSVC toolchain is required (tree-sitter compiles C code).

### Controls

| input | action |
|---|---|
| wheel / pinch | zoom at cursor (semantic zoom) |
| drag | pan |
| click | select · `Esc` deselect · `Backspace` select parent |
| double-click / `F` | fly to node (`F` with nothing selected: fit all) |
| search box | find by name, click to fly there |
| *code face / summary face* | what leaves show at deep zoom |
| *all edges* | aggregated call graph on the visible boxes |
| `Enter` / inspector → *Edit* | edit the selected node's source in place. The graph follows the unsaved text live (calls, new items, the diff animation); nothing touches the disk until `Ctrl+S` / *Save*. `Esc` closes (asks once before discarding unsaved text). |
| inspector → *Accept & write summary* | writes a `@pcg:summary[h=…]` comment into the file (only on this explicit accept, decision 7) and reloads |
| *(save a file in any editor)* | the project is watched: only changed files are re-parsed, and the graph animates the diff — moved boxes glide, new ones fade in (green), removed ones fade out (red), changed ones glow (yellow). Selection follows the node by identity. |

## Workspace

```
crates/
  pcg-core/    data only: dense ids, interner, SoA tables (nodes, edges, files, @pcg comments)
  pcg-syntax/  stages: scan → parse (rayon, tree-sitter) → assemble → @pcg comments → resolve edges;
               `Buffer` = an open file (text + syntax tree, edit-range reparse)
  pcg-layout/  nested-box layout, two linear passes (placeholder for the real engine)
  pcg-app/     Bevy shell + egui panels/canvas; resources = data, systems = control flow
```

### Data-oriented core

* **Pre-order node table with `subtree_end`.** Descendants of `n` are the contiguous
  range `n+1..subtree_end[n]`; skipping a subtree is one jump; bottom-up passes
  iterate ids in reverse. No pointers, no recursion — layout, culling and resolution are
  straight loops.
* **SoA everywhere** (`kind`, `name`, `parent`, `bytes`, `lines`, `content_hash`, …),
  edges with CSR adjacency (`out` / `inc`), one contiguous string interner.
* **Stages are pure functions** of tables. The per-file parse stage is embarrassingly
  parallel and produces file-local tables; assembly maps them to global ids serially.
* **Hashing in one token pass:** every non-comment token is hashed once
  (`xxh3(text, seed = kind)`) and folded into the accumulators of all enclosing items,
  so whitespace/comment edits never change a hash and nesting doesn't multiply cost.
* **Canvas cost scales with what is visible,** not project size: the pre-order sweep
  culls off-screen / sub-pixel subtrees in O(1) each. Semantic zoom is continuous —
  children fade in as their parent's on-screen size crosses a threshold, so zooming
  itself animates the level-of-detail change.

### In-node editing

The editor owns a `Buffer` for the node's file — the only place a syntax tree is
kept — and a byte span into it. Each keystroke is one minimal edit: `Tree::edit` +
tree-sitter's incremental reparse, then the extraction pass. A quarter second after
the last keystroke the pipeline is re-run with the buffer as an *overlay* that
replaces the file's on-disk text, so the graph is always a projection of what you
see, saved or not. The editor is anchored to the byte span, not to a node id: an
item that is renamed or briefly does not parse is re-found after each rebuild.
Saving refuses to write if the file changed on disk since the editor opened, and
keeps the file's line endings.

One keystroke in a 21 kB file costs ~3.5 ms (full parse: ~9 ms); in a 350 kB file
~50 ms (full: ~86 ms). Extraction is still a whole-file pass and now dominates.

### `@pcg` comments

```rust
/// @pcg:intent Parse the config file and fall back to defaults on error.
/// @pcg:summary[h=3fa9c1] Reads TOML from path, merges with Default,
///   logs warnings.
fn load_config(path: &Path) -> Config { … }
```

`//!` at the top of a file describes the file module. `h=` is a 24-bit short hash of
the item's code subtree. States: *missing / fresh / stale / unhashed* (dot badge on the
box, colour in the inspector). Writes preserve CRLF and refuse to touch a file that
changed on disk since analysis.

## Measurements (cloud VM, 2 cores)

| input | files | source | nodes | edges | total |
|---|---|---|---|---|---|
| this repo | 21 | 0.1 MB | 253 | 134 | 36 ms |
| `~/.cargo/registry/src` (bevy, wgpu, windows-sys, …) | 12 243 | 200 MB / 5.5 M lines | 869 k | 384 k | 25 s, ~620 MB peak RSS |

Layout of 869 k nodes: 53 ms. The parse stage is ~1.3× raw tree-sitter time
(tree-sitter itself: ~13 MB/s on 2 threads). With the whole registry loaded the canvas
draws only ~1–5 k boxes per frame.

![200 MB of crates, 869k nodes](docs/screenshots/registry-869k-nodes.png)

![reload diff: exits fade out, modules make room, new fns fade in](docs/screenshots/diff-animation.png)

## Known limitations

* **Call edges are name-based heuristics** (free fn / method / `Type::f` / macro, ranked
  same-file → same-crate → global, ambiguous calls dropped, common std method names
  skipped, unqualified method calls stay inside the crate). Precise resolution = LSP (M6).
  Calls inside macro arguments (`println!(…f()…)`) are not seen by tree-sitter.
* Syntax trees are dropped after extraction (memory), except for the file open in the
  editor. External saves reload per *file* (a parse cache keyed by mtime/size/bytes):
  they give no edit ranges for tree-sitter.
* In-node editing: one editor at a time, plain text (no syntax highlighting, completion
  or undo across sessions). Extraction after an edit re-walks the whole file, and the
  live rebuild re-runs assemble + resolve over the whole project.
* Stable node identity is `(parent, kind, name, ordinal)`: renaming an item, or moving
  it to another module, reads as exit + enter, not as a move.
* Layout is a simple shelf-packing of nested boxes; edges are drawn as beziers between
  the currently visible representatives. Real hierarchical layout + edge routing is
  still open.
* Rendering uses egui's painter; GPU instancing / custom WGSL comes with M5.
