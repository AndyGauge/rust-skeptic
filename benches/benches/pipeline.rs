//! Benchmarks for the skeptic pipeline.
//!
//! * `generate*`: build-script phase (parse markdown, emit test runners)
//! * `rt::*`: per-test phase (`cargo metadata`, fingerprint lookup, `rustc`)
//!
//! By default a synthetic corpus is used. Set `COOKBOOK_DIR` to a checked-out
//! and already-built rust-cookbook to benchmark against its real pages and
//! dependency tree (the cookbook itself needs no changes).

use std::fmt::Write;
use std::path::{Path, PathBuf};

use divan::{black_box, Bencher};

fn main() {
    divan::main();
}

fn cookbook() -> Option<PathBuf> {
    std::env::var_os("COOKBOOK_DIR").map(PathBuf::from)
}

fn triple() -> &'static str {
    option_env!("TARGET").unwrap_or(HOST)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HOST: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const HOST: &str = "x86_64-apple-darwin";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const HOST: &str = "x86_64-unknown-linux-gnu";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HOST: &str = "aarch64-unknown-linux-gnu";
#[cfg(not(any(
    all(target_os = "macos", any(target_arch = "aarch64", target_arch = "x86_64")),
    all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))
)))]
const HOST: &str = "";

/// A page resembling a cookbook chapter with `blocks` rust snippets.
fn markdown(blocks: usize) -> String {
    let mut s = String::from("# Page\n\nIntro text.\n\n");
    for i in 0..blocks {
        writeln!(s, "## Section {i}\n\nSome *prose* with `code` and a [link](x).\n").unwrap();
        writeln!(
            s,
            "```rust\n# use std::collections::HashMap;\nfn main() {{\n    let mut m = HashMap::new();\n    m.insert({i}, {i});\n    assert_eq!(m[&{i}], {i});\n}}\n```\n"
        )
        .unwrap();
    }
    s
}

/// `generate_doc_tests` reads `OUT_DIR`, `CARGO_MANIFEST_DIR` and `TARGET`
/// like a build script would.
fn bench_generate(bencher: Bencher, manifest_dir: &Path, docs: &[PathBuf]) {
    let out = tempfile::tempdir().unwrap();
    std::env::set_var("OUT_DIR", out.path());
    std::env::set_var("CARGO_MANIFEST_DIR", manifest_dir);
    std::env::set_var("TARGET", triple());
    bencher.bench_local(|| {
        // Remove previous output so the write path is measured too.
        let _ = std::fs::remove_file(out.path().join("skeptic-tests.rs"));
        skeptic::generate_doc_tests(black_box(docs));
    });
}

#[divan::bench(args = [10, 100, 1000])]
fn generate(bencher: Bencher, blocks: usize) {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.md");
    std::fs::write(&doc, markdown(blocks)).unwrap();
    bench_generate(bencher, dir.path(), &[doc]);
}

/// Whole cookbook `src/` tree (no-op unless `COOKBOOK_DIR` is set).
#[divan::bench(sample_count = 10)]
fn generate_cookbook(bencher: Bencher) {
    let Some(dir) = cookbook() else {
        return bencher.bench_local(|| ()); // keep the bench registered
    };
    let docs = skeptic::markdown_files_of_directory(dir.join("src").to_str().unwrap());
    bench_generate(bencher, &dir, &docs);
}

mod rt {
    use super::*;

    const SNIPPET: &str =
        "fn main() { let v: Vec<u32> = (0..10).collect(); assert_eq!(v.len(), 10); }\n";

    fn root_dir() -> String {
        match cookbook() {
            Some(dir) => dir.to_str().unwrap().to_owned(),
            None => env!("CARGO_MANIFEST_DIR").to_owned(),
        }
    }

    /// `rt` finds rlibs relative to `<target>/<profile>/build/<pkg>/out`.
    fn out_dir() -> String {
        let profile_dir = match cookbook() {
            Some(dir) => dir.join("target/debug"),
            None => {
                let exe = std::env::current_exe().unwrap();
                exe.parent().unwrap().parent().unwrap().to_owned()
            }
        };
        profile_dir.join("build/bench-x/out").to_str().unwrap().to_owned()
    }

    /// A `no_run` snippet: metadata lookup + `rustc --emit=metadata`.
    #[divan::bench(sample_count = 10)]
    fn compile_test() {
        skeptic::rt::compile_test(&root_dir(), &out_dir(), triple(), black_box(SNIPPET));
    }

    /// A normal snippet: full compile, link and execute.
    #[divan::bench(sample_count = 10)]
    fn run_test() {
        skeptic::rt::run_test(&root_dir(), &out_dir(), triple(), black_box(SNIPPET));
    }
}
