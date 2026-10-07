//! Cost of one keystroke in an open file: full parse vs. edit-range reparse
//! (both including extraction).
//!
//! `cargo run --release -p pcg-syntax --example edit_bench -- <file.rs>`

use pcg_syntax::Buffer;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: edit_bench <file.rs>");
    let text = std::fs::read_to_string(&path).expect("read file");
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;

    let mut buf = Buffer::new(text.clone());
    println!("{path}: {:.0} kB, {} items", text.len() as f64 / 1e3, buf.syntax().items.kind.len());
    println!("full parse + extract: {:.2} ms", ms(buf.t_reparse));

    // Type and delete a space in the middle of the file, 200 times.
    let mut at = text.len() / 2;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    const N: u32 = 200;
    let t = Instant::now();
    for _ in 0..N / 2 {
        buf.edit(at..at, " ");
        buf.edit(at..at + 1, "");
    }
    println!("edit + reparse + extract: {:.2} ms per keystroke", ms(t.elapsed()) / N as f64);
    assert_eq!(buf.text(), text);
}
