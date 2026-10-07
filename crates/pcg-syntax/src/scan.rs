//! Directory scan: find crates (`Cargo.toml` with `[package]`) and `.rs` files,
//! and derive each file's module path from its location.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct ScannedCrate {
    pub name: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    /// `/`-separated path relative to the scan root.
    pub rel_path: String,
    /// Index into [`Scan::crates`].
    pub krate: u32,
    /// Module path inside the crate; empty for the crate root file.
    pub module_path: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub root: PathBuf,
    pub crates: Vec<ScannedCrate>,
    /// Sorted by (crate, module path), i.e. module-tree pre-order per crate.
    pub files: Vec<ScannedFile>,
}

const SKIP_DIRS: &[&str] = &["target", "node_modules"];

fn skip(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.depth() > 0 && entry.file_type().is_dir() && (name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()))
}

/// Extract `name = "…"` from the `[package]` table of a Cargo.toml.
pub fn package_name(cargo_toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package && let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Module path of a file relative to its crate dir, e.g. `src/a/mod.rs` → `[a]`.
pub fn module_path(rel_to_crate: &Path, crate_has_lib: bool) -> Vec<String> {
    let mut parts: Vec<String> =
        rel_to_crate.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.first().is_some_and(|p| p == "src") {
        parts.remove(0);
    }
    if let Some(last) = parts.last_mut()
        && let Some(stem) = last.strip_suffix(".rs")
    {
        *last = stem.to_string();
    }
    let is_root = parts.len() == 1 && (parts[0] == "lib" || (parts[0] == "main" && !crate_has_lib));
    if is_root || parts.last().is_some_and(|p| p == "mod") {
        parts.pop();
    }
    parts
}

pub fn scan(root: &Path) -> Scan {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut crates = Vec::new();
    let mut rs_files = Vec::new();

    for entry in WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|e| !skip(e)).flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name();
        if name == "Cargo.toml" {
            if let Ok(text) = std::fs::read_to_string(entry.path())
                && let Some(pkg) = package_name(&text)
            {
                crates.push(ScannedCrate { name: pkg, dir: entry.path().parent().unwrap().to_path_buf() });
            }
        } else if entry.path().extension().is_some_and(|e| e == "rs") {
            rs_files.push(entry.into_path());
        }
    }

    // Files outside any crate go into a synthetic crate named after the root dir.
    let loose = crates.len();
    crates.push(ScannedCrate {
        name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into()),
        dir: root.clone(),
    });

    // Deepest crate dir first, so nested crates win.
    let mut by_depth: Vec<usize> = (0..loose).collect();
    by_depth.sort_by_key(|&i| std::cmp::Reverse(crates[i].dir.components().count()));
    let has_lib: Vec<bool> = crates.iter().map(|c| c.dir.join("src/lib.rs").is_file()).collect();

    let mut files: Vec<ScannedFile> = rs_files
        .into_iter()
        .map(|path| {
            let krate = by_depth.iter().copied().find(|&i| path.starts_with(&crates[i].dir)).unwrap_or(loose);
            let rel_crate = path.strip_prefix(&crates[krate].dir).unwrap_or(&path);
            let module_path = module_path(rel_crate, has_lib[krate]);
            let rel_path = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            ScannedFile { path, rel_path, krate: krate as u32, module_path }
        })
        .collect();

    // Drop the synthetic crate if unused.
    if !files.iter().any(|f| f.krate as usize == loose) {
        crates.pop();
    }

    files.sort_by(|a, b| {
        (crates[a.krate as usize].name.as_str(), a.krate, &a.module_path, &a.rel_path).cmp(&(
            crates[b.krate as usize].name.as_str(),
            b.krate,
            &b.module_path,
            &b.rel_path,
        ))
    });
    Scan { root, crates, files }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths() {
        let p = |s: &str, lib| module_path(Path::new(s), lib);
        assert_eq!(p("src/lib.rs", true), Vec::<String>::new());
        assert_eq!(p("src/main.rs", false), Vec::<String>::new());
        assert_eq!(p("src/main.rs", true), vec!["main"]);
        assert_eq!(p("src/a.rs", true), vec!["a"]);
        assert_eq!(p("src/a/mod.rs", true), vec!["a"]);
        assert_eq!(p("src/a/b.rs", true), vec!["a", "b"]);
        assert_eq!(p("tests/it.rs", true), vec!["tests", "it"]);
    }

    #[test]
    fn package_names() {
        let toml = "[workspace]\nmembers=[]\n[package]\nname = \"pcg-core\"\nversion = \"0.1\"\n";
        assert_eq!(package_name(toml).as_deref(), Some("pcg-core"));
        assert_eq!(package_name("[workspace]\nname=\"x\"\n"), None);
    }
}
