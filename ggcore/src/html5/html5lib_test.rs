//! The html5lib-tests tree-construction suite, run against this
//! parser. The suite is vendored under validation/html5lib so the
//! score is reproducible and can be a gate.
//!
//! `cargo test html5lib -- --nocapture` prints the tally. Set
//! `GG_H5_SHOW=n` to dump the first n failures (input, expected, got),
//! and `GG_H5_FILE=name` to run one .dat file.

use std::fmt::Write as _;

struct Case {
    data: String,
    document: String,
    fragment: Option<String>,
    scripting: Option<bool>,
    file: String,
    index: usize,
}

/// The .dat format: `#data`, `#errors`, optional `#document-fragment`,
/// optional `#script-on`/`#script-off`, then `#document`. Sections run
/// to the next `#`-directive; the data section keeps its newlines
/// exactly, minus the single one before the next directive.
fn parse_dat(text: &str, file: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = text.lines().peekable();
    let mut index = 0;
    while let Some(line) = lines.next() {
        if line != "#data" {
            continue;
        }
        index += 1;
        let mut data = Vec::new();
        while let Some(&l) = lines.peek() {
            if l.starts_with('#')
                && matches!(
                    l,
                    "#errors" | "#new-errors" | "#document-fragment"
                        | "#document" | "#script-on" | "#script-off"
                )
            {
                break;
            }
            data.push(l);
            lines.next();
        }
        let mut fragment = None;
        let mut scripting = None;
        let mut document = Vec::new();
        while let Some(l) = lines.next() {
            match l {
                "#errors" | "#new-errors" => {
                    while let Some(&n) = lines.peek() {
                        if n.starts_with('#') {
                            break;
                        }
                        lines.next();
                    }
                }
                "#document-fragment" => {
                    fragment = lines.next().map(|s| s.to_string());
                }
                "#script-on" => scripting = Some(true),
                "#script-off" => scripting = Some(false),
                "#document" => {
                    while let Some(&n) = lines.peek() {
                        if n == "#data" {
                            break;
                        }
                        document.push(n);
                        lines.next();
                    }
                    break;
                }
                _ => {}
            }
        }
        // a trailing blank line separates cases; it is not part of the
        // expected tree
        while document.last().is_some_and(|l| l.is_empty()) {
            document.pop();
        }
        let mut doc = document.join("\n");
        if !doc.is_empty() {
            doc.push('\n');
        }
        cases.push(Case {
            data: data.join("\n"),
            document: doc,
            fragment,
            scripting,
            file: file.to_string(),
            index,
        });
    }
    cases
}

fn dat_files() -> Vec<(String, String)> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../validation/html5lib");
    let only = std::env::var("GG_H5_FILE").ok();
    let mut out = Vec::new();
    let mut dirs = vec![std::path::PathBuf::from(root)];
    while let Some(d) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("dat") {
                continue;
            }
            let name = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            if let Some(only) = &only {
                if !name.contains(only.as_str()) {
                    continue;
                }
            }
            // Some files are deliberately not valid UTF-8 (they test
            // byte-level recovery); read lossily rather than skip.
            let bytes = std::fs::read(&p).unwrap_or_default();
            out.push((name, String::from_utf8_lossy(&bytes).to_string()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn run_case(c: &Case) -> String {
    let scripting = c.scripting.unwrap_or(false);
    match &c.fragment {
        Some(ctx) => {
            let p = super::parse_fragment(&c.data, ctx, scripting);
            p.sink.serialize_children(p.root)
        }
        None => {
            let p = super::parse(&c.data, scripting);
            p.sink.serialize()
        }
    }
}

#[test]
fn html5lib_tree_construction() {
    let show: usize = std::env::var("GG_H5_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let files = dat_files();
    assert!(!files.is_empty(), "html5lib test data not found");
    let mut total = 0;
    let mut passed = 0;
    let mut shown = 0;
    let mut per_file: Vec<(String, usize, usize)> = Vec::new();
    let mut report = String::new();
    for (name, text) in &files {
        let cases = parse_dat(text, name);
        let mut fpass = 0;
        for c in &cases {
            total += 1;
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || run_case(c),
            ))
            .unwrap_or_else(|_| "<panic>\n".to_string());
            if got == c.document {
                passed += 1;
                fpass += 1;
            } else if shown < show {
                shown += 1;
                let _ = write!(
                    report,
                    "\n--- {}#{} \ninput:    {:?}\nexpected:\n{}got:\n{}",
                    c.file, c.index, c.data, c.document, got
                );
            }
        }
        per_file.push((name.clone(), fpass, cases.len()));
    }
    if show > 0 {
        eprintln!("{report}");
    }
    per_file.sort_by_key(|(_, p, t)| *t - *p);
    eprintln!("\nhtml5lib tree-construction: {passed}/{total} pass \
               ({:.1}%)", 100.0 * passed as f64 / total as f64);
    for (name, p, t) in per_file.iter().rev().take(12) {
        if p < t {
            eprintln!("  {:<42} {}/{}", name, p, t);
        }
    }
    // The gate: this suite is the parser's definition of correct.
    assert_eq!(passed, total, "html5lib tree-construction regressions");
}

/// Throughput on a real page, for the record. Not a gate — run with
/// `GG_H5_BENCH=/path/to/page.html cargo test --release bench_parse
/// -- --nocapture`.
#[test]
fn bench_parse() {
    let Ok(path) = std::env::var("GG_H5_BENCH") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let html = String::from_utf8_lossy(&bytes).to_string();
    let runs = 20;
    let t = std::time::Instant::now();
    let mut nodes = 0;
    for _ in 0..runs {
        nodes = super::parse(&html, false).sink.nodes.len();
    }
    let new = t.elapsed().as_secs_f64() / runs as f64;
    let t = std::time::Instant::now();
    let mut old = 0;
    for _ in 0..runs {
        old = crate::html::parse(&html).nodes.len();
    }
    let oldt = t.elapsed().as_secs_f64() / runs as f64;
    eprintln!(
        "\n{} KB\n  html5::parse {:6.2} ms  {} nodes\n  html::parse  {:6.2} ms  {} nodes",
        bytes.len() / 1024, new * 1e3, nodes, oldt * 1e3, old
    );
}
