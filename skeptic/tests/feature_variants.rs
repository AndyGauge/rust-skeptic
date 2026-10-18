//! A target directory can hold several builds of the same dependency, one per
//! feature set that was ever tested. A snippet must be compiled against the
//! build the running test binary was linked with, whatever else is lying
//! around in `target/`.

use std::fs;
use std::path::Path;
use std::process::Command;

fn write(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn run_tests(project: &Path, target: &Path, features: &[&str], expect_on: bool) {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("test")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .args(features)
        .env("CARGO_TARGET_DIR", target)
        .env_remove("EXPECT_ON");
    if expect_on {
        cmd.env("EXPECT_ON", "1");
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "cargo test {:?} (expecting the feature {}) failed:\n{}\n{}",
        features,
        if expect_on { "on" } else { "off" },
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn snippet_links_the_feature_set_of_the_running_test() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("feature-variants");
    let _ = fs::remove_dir_all(root.join("project"));
    let project = root.join("project");
    // Kept between runs of this test so the dependencies of skeptic are only
    // compiled once, but the leftover builds of `marker` are discarded.
    let target = root.join("target");
    let _ = fs::remove_dir_all(&target);

    write(
        project.join("marker/Cargo.toml"),
        r#"[package]
name = "marker"
version = "0.1.0"
edition = "2021"

[features]
on = []
"#,
    );
    write(
        project.join("marker/src/lib.rs"),
        "pub const ON: bool = cfg!(feature = \"on\");\n",
    );
    write(
        project.join("Cargo.toml"),
        &format!(
            r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[workspace]

[features]
on = ["marker/on"]

[dependencies]
marker = {{ path = "marker" }}

[build-dependencies]
skeptic = {{ path = {skeptic:?} }}

[dev-dependencies]
skeptic = {{ path = {skeptic:?} }}
"#,
            skeptic = env!("CARGO_MANIFEST_DIR"),
        ),
    );
    write(project.join("src/lib.rs"), "");
    write(
        project.join("build.rs"),
        "fn main() { skeptic::generate_doc_tests(&[\"doc.md\"]); }\n",
    );
    write(
        project.join("tests/skeptic.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/skeptic-tests.rs\"));\n",
    );
    write(
        project.join("doc.md"),
        r#"# Which build of marker?

```rust
fn main() {
    assert_eq!(marker::ON, std::env::var("EXPECT_ON").is_ok());
}
```
"#,
    );

    // Each run leaves its build of `marker` behind for the next one.
    run_tests(&project, &target, &[], false);
    run_tests(&project, &target, &["--features", "on"], true);
    run_tests(&project, &target, &[], false);
    run_tests(&project, &target, &["--features", "on"], true);
}
