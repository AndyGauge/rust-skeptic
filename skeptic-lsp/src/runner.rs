//! Runs `cargo test` for a workspace and collects diagnostics.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::cargo_output::{self, FileDiagnostics};

/// Finds the workspace root (directory holding the root `Cargo.toml`) for a file.
pub async fn workspace_root(file: &Path) -> Option<PathBuf> {
    let dir = file.parent()?;
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let manifest = String::from_utf8(output.stdout).ok()?;
    PathBuf::from(manifest.trim())
        .parent()
        .map(Path::to_path_buf)
}

/// Builds and tests the workspace, returning every diagnostic found.
///
/// The child is killed if the returned future is dropped, so a newer save can
/// cancel a stale run simply by aborting the task.
///
/// `test_filter` narrows the run to tests whose name contains it; used for
/// markdown saves so only that file's skeptic snippets are run.
pub async fn check(
    root: &Path,
    extra_args: &[String],
    test_filter: Option<&str>,
) -> std::io::Result<FileDiagnostics> {
    let mut args = extra_args.to_vec();
    if let Some(filter) = test_filter {
        if !args.iter().any(|a| a == "--") {
            args.push("--".into());
        }
        args.push(filter.into());
    }
    let output = Command::new("cargo")
        .args(["test", "--no-fail-fast", "--message-format=json"])
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(cargo_output::parse(&stdout, root))
}
