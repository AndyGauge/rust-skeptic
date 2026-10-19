//! What each code fence header (`ignore`, `no_run`, `should_panic`) does to a
//! snippet that compiles, doesn't compile, panics or runs to completion.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const COMPILES: &str = "fn main() {}";
const NO_COMPILE: &str = "fn main() { let _: u32 = \"text\"; }";
const PANICS: &str = "fn main() { panic!(\"on purpose\"); }";

/// (header, body, what libtest reports for it)
const CASES: &[(&str, &str, &str)] = &[
    ("rust", COMPILES, "ok"),
    ("rust", NO_COMPILE, "FAILED"),
    ("rust", PANICS, "FAILED"),
    ("rust,should_panic", PANICS, "ok"),
    // Not compiling is not the panic that was asked for.
    ("rust,should_panic", NO_COMPILE, "FAILED"),
    ("rust,should_panic", COMPILES, "FAILED"),
    ("rust,no_run", COMPILES, "ok"),
    ("rust,no_run", NO_COMPILE, "FAILED"),
    // Never run, so it can't panic.
    ("rust,no_run", PANICS, "ok"),
    ("rust,ignore", NO_COMPILE, "ignored"),
    // Without running it, failing to compile is the expected panic.
    ("rust,no_run,should_panic", NO_COMPILE, "ok"),
    ("rust,no_run,should_panic", COMPILES, "FAILED"),
];

#[test]
fn headers_decide_what_a_snippet_must_do() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("headers");
    let project = root.join("project");
    let _ = fs::remove_dir_all(&project);
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(project.join("tests")).unwrap();

    fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"[package]
name = "headers"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[workspace]

[build-dependencies]
skeptic = {{ path = {skeptic:?} }}

[dev-dependencies]
skeptic = {{ path = {skeptic:?} }}
"#,
            skeptic = env!("CARGO_MANIFEST_DIR"),
        ),
    )
    .unwrap();
    fs::write(project.join("src/lib.rs"), "").unwrap();
    fs::write(
        project.join("build.rs"),
        "fn main() { skeptic::generate_doc_tests(&[\"doc.md\"]); }\n",
    )
    .unwrap();
    fs::write(
        project.join("tests/skeptic.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/skeptic-tests.rs\"));\n",
    )
    .unwrap();

    // The line of each opening fence names its test.
    let mut markdown = String::from("# Headers\n");
    let mut expected = BTreeMap::new();
    for (header, body, outcome) in CASES {
        markdown.push('\n');
        let fence_line = markdown.lines().count() + 1;
        markdown.push_str(&format!("```{header}\n{body}\n```\n"));
        expected.insert(
            format!("doc_sect_headers_line_{fence_line}"),
            (outcome.to_string(), format!("{header}: {body}")),
        );
    }
    fs::write(project.join("doc.md"), markdown).unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["test", "--no-fail-fast", "--test", "skeptic"])
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut actual = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((name, outcome)) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.rsplit_once(" ... "))
        {
            let name = name.trim_end_matches(" - should panic");
            actual.insert(name.to_string(), outcome.to_string());
        }
    }

    for (name, (outcome, case)) in &expected {
        assert_eq!(
            actual.get(name),
            Some(outcome),
            "{name} ({case}) should be {outcome}\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
