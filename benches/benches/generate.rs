//! Benchmarks for skeptic's build-script phase: discovering markdown files,
//! parsing them and emitting the generated test runners.
//!
//! The per-test runtime phase (`skeptic::rt`) spawns `cargo metadata` and
//! `rustc`, so it is intentionally not covered here.

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use divan::{black_box, Bencher};

fn main() {
    divan::main();
}

/// A page resembling a documentation chapter with `blocks` code snippets,
/// mixing plain rust blocks, attributes, templates and non-rust blocks.
fn markdown(blocks: usize) -> String {
    let mut s = String::from("# Page\n\nIntro text with *emphasis* and a [link](x).\n\n");
    for i in 0..blocks {
        writeln!(
            s,
            "## Section {i}: Doing things\n\nSome *prose* with `code`, **bold** text and a [link](https://example.com/{i}).\n"
        )
        .unwrap();
        let info = match i % 5 {
            0 => "rust",
            1 => "rust,no_run",
            2 => "rust,should_panic",
            3 => "rust,skt-wrap",
            _ => "toml",
        };
        writeln!(
            s,
            "```{info}\n# use std::collections::HashMap;\nfn main() {{\n    let mut m = HashMap::new();\n    m.insert({i}, {i});\n    assert_eq!(m[&{i}], {i});\n}}\n```\n"
        )
        .unwrap();
    }
    s
}

const TEMPLATE: &str = "```rust,skt-wrap\n#![allow(unused)]\n{}\n```\n";

/// `generate_doc_tests` reads `OUT_DIR`, `CARGO_MANIFEST_DIR` and `TARGET`
/// like a build script would.
fn bench_generate(bencher: Bencher, manifest_dir: &Path, docs: &[PathBuf]) {
    let out = tempfile::tempdir().unwrap();
    std::env::set_var("OUT_DIR", out.path());
    std::env::set_var("CARGO_MANIFEST_DIR", manifest_dir);
    std::env::set_var("TARGET", "x86_64-unknown-linux-gnu");
    let out_file = out.path().join("skeptic-tests.rs");
    bencher.bench_local(|| {
        // Remove previous output so the write path is measured too.
        let _ = fs::remove_file(&out_file);
        skeptic::generate_doc_tests(black_box(docs));
    });
}

/// Synthetic single page with a growing number of code blocks.
#[divan::bench(args = [10, 100, 1000])]
fn generate_doc_tests(bencher: Bencher, blocks: usize) {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.md");
    fs::write(&doc, markdown(blocks)).unwrap();
    fs::write(dir.path().join("doc.md.skt.md"), TEMPLATE).unwrap();
    bench_generate(bencher, dir.path(), &[doc]);
}

/// Many small pages, like an mdbook.
#[divan::bench]
fn generate_doc_tests_many_files(bencher: Bencher) {
    let dir = tempfile::tempdir().unwrap();
    let docs: Vec<PathBuf> = (0..50)
        .map(|i| {
            let doc = dir.path().join(format!("chapter_{i}.md"));
            fs::write(&doc, markdown(10)).unwrap();
            fs::write(dir.path().join(format!("chapter_{i}.md.skt.md")), TEMPLATE).unwrap();
            doc
        })
        .collect();
    bench_generate(bencher, dir.path(), &docs);
}

/// The repository's own documentation (README and test fixtures).
#[divan::bench]
fn generate_doc_tests_repo_docs(bencher: Bencher) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let docs = vec![
        root.join("README.md"),
        root.join("template-example.md"),
        root.join("testing/tests/hashtag-test.md"),
        root.join("testing/tests/section-names.md"),
        root.join("testing/tests/should-panic-test.md"),
    ];
    bench_generate(bencher, root, &docs);
}

/// Recursive markdown discovery in a nested directory tree.
#[divan::bench]
fn markdown_files_of_directory(bencher: Bencher) {
    let dir = tempfile::tempdir().unwrap();
    for d in 0..10 {
        let sub = dir.path().join(format!("section_{d}")).join("nested");
        fs::create_dir_all(&sub).unwrap();
        for f in 0..10 {
            fs::write(sub.join(format!("page_{f}.md")), "# Page\n").unwrap();
            fs::write(sub.join(format!("asset_{f}.png")), "").unwrap();
        }
    }
    let dir_str = dir.path().to_str().unwrap().to_owned();
    bencher.bench_local(|| skeptic::markdown_files_of_directory(black_box(&dir_str)));
}
