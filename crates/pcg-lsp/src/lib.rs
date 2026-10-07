//! # pcg-lsp — precise call targets from rust-analyzer
//!
//! tree-sitter sees *that* `x.len()` is a call, not *which* `len`. This crate
//! asks a language server: for every call site of a snapshot it sends
//! `textDocument/definition` at the callee's name and records where the
//! definition is ([`resolve`] → [`Precise`]). The build then uses those
//! answers instead of its name-based guess.
//!
//! The server is given every file's text as the snapshot has it (`didOpen` /
//! `didChange`), so byte offsets on both sides refer to the same text — also
//! for unsaved editor buffers.
//!
//! rust-analyzer is started with build scripts, proc macros and `cargo check`
//! switched off: it then never runs cargo builds in the project's target
//! directory, at the price of not seeing through generated code.

use pcg_core::{FileId, Graph};
use pcg_syntax::{Precise, text_hash};
use rustc_hash::FxHashMap;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Requests in flight at once.
const WINDOW: usize = 64;

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: i64,
    /// Positions are (line, UTF-8 byte column) rather than UTF-16 units.
    utf8: bool,
    /// Workspace loaded and analysis idle, as last reported by the server.
    quiescent: bool,
    /// Documents the server has been given: version and text hash.
    opened: FxHashMap<PathBuf, (i64, u64)>,
}

/// Comparable form of a path: absolute, `/`-separated, case-folded on Windows.
fn norm(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn to_uri(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s);
    let mut uri = String::from(if s.starts_with('/') { "file://" } else { "file:///" });
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => uri.push(b as char),
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// Path of a `file:` URI, in [`norm`] form.
fn uri_to_norm(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    let s = String::from_utf8(bytes).ok()?;
    // `/C:/dir` → `C:/dir`
    let s = match s.as_bytes() {
        [b'/', _, b':', ..] => &s[1..],
        _ => &s,
    };
    Some(norm(Path::new(s)))
}

/// Byte offsets of the starts of all lines.
fn line_starts(text: &str) -> Vec<u32> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i as u32 + 1)).collect()
}

impl Client {
    /// Start `rust-analyzer` for the workspace at `root` and initialize it.
    pub fn start(root: &Path) -> io::Result<Self> {
        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(root)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            // One message: headers, blank line, `Content-Length` bytes of JSON.
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                if stdout.read_exact(&mut body).is_err() {
                    return;
                }
                if let Ok(v) = serde_json::from_slice(&body)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        let mut c =
            Client { child, stdin, rx, next_id: 0, utf8: false, quiescent: false, opened: FxHashMap::default() };
        let id = c.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": to_uri(root),
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8", "utf-16"] },
                    "textDocument": { "definition": { "linkSupport": true } },
                    "experimental": { "serverStatusNotification": true },
                },
                "initializationOptions": {
                    "cargo": { "buildScripts": { "enable": false } },
                    "procMacro": { "enable": false },
                    "checkOnSave": false,
                    "cachePriming": { "enable": false },
                },
            }),
        )?;
        let reply = c.wait_for(id, Duration::from_secs(60))?;
        c.utf8 = reply["result"]["capabilities"]["positionEncoding"] == "utf-8";
        c.notify("initialized", json!({}))?;
        Ok(c)
    }

    fn send(&mut self, msg: Value) -> io::Result<()> {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len())?;
        self.stdin.flush()
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<i64> {
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params }))?;
        Ok(self.next_id)
    }

    fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Next message from the server, with its own requests and notifications
    /// handled here. Returns responses to our requests as `(id, message)`.
    fn next(&mut self, timeout: Duration) -> io::Result<Option<(i64, Value)>> {
        let msg = match self.rx.recv_timeout(timeout) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => return Err(io::Error::new(io::ErrorKind::TimedOut, "language server")),
            Err(RecvTimeoutError::Disconnected) => return Err(io::Error::other("language server exited")),
        };
        match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
            // Server → client request: answer so it does not wait for us.
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        json!(vec![Value::Null; msg["params"]["items"].as_array().map_or(0, Vec::len)])
                    }
                    _ => Value::Null,
                };
                self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))?;
                Ok(None)
            }
            (Some("experimental/serverStatus"), None) => {
                self.quiescent = msg["params"]["quiescent"] == true;
                Ok(None)
            }
            (Some(_), None) => Ok(None),
            (None, Some(id)) => Ok(id.as_i64().map(|id| (id, msg))),
            (None, None) => Ok(None),
        }
    }

    fn wait_for(&mut self, id: i64, timeout: Duration) -> io::Result<Value> {
        let end = Instant::now() + timeout;
        loop {
            if let Some((got, msg)) = self.next(end.saturating_duration_since(Instant::now()))?
                && got == id
            {
                return Ok(msg);
            }
        }
    }

    /// Block until the server has loaded the workspace. `false` on timeout
    /// (requests still work then, but may come back empty).
    pub fn wait_ready(&mut self, timeout: Duration) -> io::Result<bool> {
        let end = Instant::now() + timeout;
        while !self.quiescent {
            match self.next(end.saturating_duration_since(Instant::now())) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Give the server the exact text of `path` (if it does not have it yet).
    fn sync(&mut self, path: &Path, text: &str) -> io::Result<()> {
        let hash = text_hash(text);
        let uri = to_uri(path);
        match self.opened.get(path).copied() {
            Some((_, h)) if h == hash => Ok(()),
            Some((version, _)) => {
                self.opened.insert(path.to_path_buf(), (version + 1, hash));
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version + 1 },
                        "contentChanges": [{ "text": text }],
                    }),
                )
            }
            None => {
                self.opened.insert(path.to_path_buf(), (1, hash));
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": { "uri": uri, "languageId": "rust", "version": 1, "text": text } }),
                )
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.request("shutdown", Value::Null);
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Column of byte `at` within its line (`line` = the line's text up to `at`).
fn column(prefix: &str, utf8: bool) -> usize {
    if utf8 { prefix.len() } else { prefix.encode_utf16().count() }
}

/// Byte offset of `(line, character)` in `text`.
fn offset(text: &str, starts: &[u32], line: usize, character: usize, utf8: bool) -> Option<u32> {
    let start = *starts.get(line)? as usize;
    let rest = &text[start..];
    let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let col = if utf8 {
        character.min(rest.len())
    } else {
        let mut units = 0;
        rest.char_indices()
            .find(|(_, c)| {
                let here = units >= character;
                units += c.len_utf16();
                here
            })
            .map_or(rest.len(), |(i, _)| i)
    };
    Some((start + col) as u32)
}

/// First location of a `textDocument/definition` result: `(uri, line, character)`.
fn first_location(result: &Value) -> Option<(&str, usize, usize)> {
    let loc = match result {
        Value::Array(a) => a.first()?,
        Value::Object(_) => result,
        _ => return None,
    };
    // `LocationLink` (the name inside the definition) or plain `Location`.
    let (uri, pos) = match loc.get("targetUri") {
        Some(uri) => (uri, &loc["targetSelectionRange"]["start"]),
        None => (&loc["uri"], &loc["range"]["start"]),
    };
    Some((uri.as_str()?, pos["line"].as_u64()? as usize, pos["character"].as_u64()? as usize))
}

/// Ask the server for the definition behind every call site of `g`.
///
/// `progress(done, total)` is called now and then. Sites the server gives no
/// answer for are left out of the result (the build keeps its own guess for
/// them); a definition in a file that is not part of the graph is recorded as
/// "outside the workspace".
pub fn resolve(c: &mut Client, g: &Graph, progress: &mut dyn FnMut(usize, usize)) -> io::Result<Precise> {
    let mut precise = Precise::default();
    let mut by_norm: FxHashMap<String, FileId> = FxHashMap::default();
    for f in 0..g.files.len() {
        let path = &g.files.path[f];
        c.sync(path, &g.files.source[f])?;
        precise.files.insert(path.clone(), text_hash(&g.files.source[f]));
        by_norm.insert(norm(path), FileId::from_idx(f));
    }
    let starts: Vec<Vec<u32>> = g.files.source.iter().map(|s| line_starts(s)).collect();
    let uris: Vec<String> = g.files.path.iter().map(|p| to_uri(p)).collect();

    let total = g.calls.len();
    // Request id → call site.
    let mut pending: FxHashMap<i64, usize> = FxHashMap::default();
    let (mut next, mut done) = (0usize, 0usize);
    while done < total {
        while next < total && pending.len() < WINDOW {
            let f = g.nodes.file[g.calls.caller[next].idx()].idx();
            let at = g.calls.at[next] as usize;
            let line = starts[f].partition_point(|&s| s as usize <= at) - 1;
            let character = column(&g.files.source[f][starts[f][line] as usize..at], c.utf8);
            let id = c.request(
                "textDocument/definition",
                json!({ "textDocument": { "uri": uris[f] }, "position": { "line": line, "character": character } }),
            )?;
            pending.insert(id, next);
            next += 1;
        }
        let Some((id, msg)) = c.next(Duration::from_secs(120))? else { continue };
        let Some(site) = pending.remove(&id) else { continue };
        done += 1;
        if done % 64 == 0 || done == total {
            progress(done, total);
        }
        let Some((uri, line, character)) = msg.get("result").and_then(first_location) else { continue };
        let target = uri_to_norm(uri).and_then(|n| by_norm.get(&n).copied()).and_then(|tf| {
            let at = offset(&g.files.source[tf.idx()], &starts[tf.idx()], line, character, c.utf8)?;
            Some((g.files.path[tf.idx()].clone(), at))
        });
        let file = g.nodes.file[g.calls.caller[site].idx()];
        precise.sites.entry(g.files.path[file.idx()].clone()).or_default().insert(g.calls.at[site], target);
    }
    Ok(precise)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let p = if cfg!(windows) {
            Path::new(r"C:\Users\me\my proj\src\lib.rs")
        } else {
            Path::new("/home/me/my proj/src/lib.rs")
        };
        let uri = to_uri(p);
        assert!(uri.starts_with("file:///") && uri.contains("my%20proj"), "{uri}");
        assert_eq!(uri_to_norm(&uri).unwrap(), norm(p));
        // rust-analyzer's own spelling on Windows: lower-case drive, encoded colon.
        if cfg!(windows) {
            assert_eq!(uri_to_norm("file:///c%3A/Users/me/my%20proj/src/lib.rs").unwrap(), norm(p));
        }
    }

    #[test]
    fn positions() {
        let text = "fn a() {}\nlet s = \"äö\"; f();\r\nx";
        let starts = line_starts(text);
        assert_eq!(starts, [0, 10, 32]);
        let at = text.find("f()").unwrap();
        let prefix = &text[10..at];
        for utf8 in [true, false] {
            let col = column(prefix, utf8);
            assert_eq!(col, if utf8 { 16 } else { 14 });
            assert_eq!(offset(text, &starts, 1, col, utf8), Some(at as u32));
        }
        assert_eq!(offset(text, &starts, 1, 999, false), Some(31), "clamped to the line");
        assert_eq!(offset(text, &starts, 9, 0, true), None);
    }

    #[test]
    fn locations() {
        let link = json!([{ "targetUri": "file:///a.rs", "targetSelectionRange": { "start": { "line": 3, "character": 7 } } }]);
        assert_eq!(first_location(&link), Some(("file:///a.rs", 3, 7)));
        let loc = json!({ "uri": "file:///b.rs", "range": { "start": { "line": 1, "character": 2 } } });
        assert_eq!(first_location(&loc), Some(("file:///b.rs", 1, 2)));
        assert_eq!(first_location(&Value::Null), None);
        assert_eq!(first_location(&json!([])), None);
    }
}
