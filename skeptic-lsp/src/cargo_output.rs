//! Turns the output of `cargo test --message-format=json` into diagnostics.
//!
//! Cargo writes one JSON object per line on stdout for compiler messages, and
//! the test binaries write their (non-JSON) output to the same stream. We
//! handle both: JSON lines become compiler diagnostics, and libtest's failure
//! report is scanned for panic locations (`panic!`, `assert!`, `unwrap`, ...).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargo_metadata::diagnostic::{Diagnostic as RustcDiagnostic, DiagnosticLevel};
use cargo_metadata::Message;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

pub const COMPILER_SOURCE: &str = "rustc";
pub const TEST_SOURCE: &str = "cargo test";

/// Diagnostics grouped by the (absolute) file they belong to.
pub type FileDiagnostics = BTreeMap<PathBuf, Vec<Diagnostic>>;

pub fn parse(stdout: &str, workspace_root: &Path) -> FileDiagnostics {
    let mut out = FileDiagnostics::new();
    let mut plain = Vec::new();
    let mut out_dirs = Vec::new();

    for line in stdout.lines() {
        if line.starts_with('{') {
            if let Ok(Message::BuildScriptExecuted(script)) = serde_json::from_str::<Message>(line)
            {
                out_dirs.push(PathBuf::from(script.out_dir.as_str()));
                continue;
            }
            if let Ok(Message::CompilerMessage(msg)) = serde_json::from_str::<Message>(line) {
                compiler_diagnostic(&msg.message, workspace_root, &mut out);
                continue;
            }
            if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                continue; // other cargo artifact/build-script messages
            }
        }
        plain.push(line);
    }

    let mut md_files = None;
    for section in split_sections(&plain) {
        if let Some(name) = section.name {
            if let Some(block) = locate_block(workspace_root, name, &mut md_files) {
                let snippet = snippet_text(&out_dirs, name);
                skeptic_section(&block, snippet.as_deref(), &section.lines, &mut out);
                continue;
            }
        }
        for panic in find_panics(&section.lines) {
            let path = resolve(workspace_root, &panic.file);
            push_panic(&mut out, path, panic.line, panic.column, 1, &panic);
        }
    }
    out
}

fn push_panic(
    out: &mut FileDiagnostics,
    path: PathBuf,
    line: u32,
    column: u32,
    len: u32,
    panic: &Panic,
) {
    let line = line.saturating_sub(1);
    let col = column.saturating_sub(1);
    push_unique(
        out,
        path,
        Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + len)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(TEST_SOURCE.into()),
            message: format!("test `{}` panicked:\n{}", panic.test, panic.message),
            ..Default::default()
        },
    );
}

fn resolve(root: &Path, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn push_unique(out: &mut FileDiagnostics, path: PathBuf, diag: Diagnostic) {
    let list = out.entry(path).or_default();
    if !list.contains(&diag) {
        list.push(diag);
    }
}

fn severity(level: &DiagnosticLevel) -> Option<DiagnosticSeverity> {
    match level {
        DiagnosticLevel::Ice | DiagnosticLevel::Error => Some(DiagnosticSeverity::ERROR),
        DiagnosticLevel::Warning => Some(DiagnosticSeverity::WARNING),
        DiagnosticLevel::Note => Some(DiagnosticSeverity::INFORMATION),
        DiagnosticLevel::Help => Some(DiagnosticSeverity::HINT),
        _ => None,
    }
}

fn compiler_diagnostic(d: &RustcDiagnostic, root: &Path, out: &mut FileDiagnostics) {
    let Some(severity) = severity(&d.level) else {
        return;
    };
    let mut message = d.message.clone();
    for child in &d.children {
        message.push_str(&format!(
            "\n{}: {}",
            format!("{:?}", child.level).to_lowercase(),
            child.message
        ));
    }

    for span in d.spans.iter().filter(|s| s.is_primary) {
        // rustc columns are 1-based char offsets; LSP wants 0-based UTF-16.
        // They agree for the common (BMP) case.
        let range = Range::new(
            Position::new(
                span.line_start.saturating_sub(1) as u32,
                span.column_start.saturating_sub(1) as u32,
            ),
            Position::new(
                span.line_end.saturating_sub(1) as u32,
                span.column_end.saturating_sub(1) as u32,
            ),
        );
        let mut text = message.clone();
        if let Some(label) = span.label.as_deref().filter(|l| !l.is_empty()) {
            text = format!("{text}\n{label}");
        }
        push_unique(
            out,
            resolve(root, &span.file_name),
            Diagnostic {
                range,
                severity: Some(severity),
                code: d
                    .code
                    .as_ref()
                    .map(|c| NumberOrString::String(c.code.clone())),
                source: Some(COMPILER_SOURCE.into()),
                message: text,
                ..Default::default()
            },
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Panic {
    test: String,
    file: String,
    line: u32,
    column: u32,
    message: String,
}

/// Scans libtest output for `thread '<test>' panicked at ...` reports.
///
/// Handles both the current format
/// (`thread 'name' (123) panicked at src/lib.rs:3:5:` + message lines) and
/// the pre-1.73 one (`thread 'name' panicked at 'msg', src/lib.rs:3:5`).
fn find_panics(lines: &[&str]) -> Vec<Panic> {
    let mut panics = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some(rest) = line.strip_prefix("thread '") else {
            continue;
        };
        let Some((test, after)) = rest.split_once("' ") else {
            continue;
        };
        // Skip an optional "(thread id) " between the name and "panicked".
        let after = match after.strip_prefix('(') {
            Some(a) => a.split_once(") ").map_or(a, |(_, r)| r),
            None => after,
        };
        let Some(location) = after.strip_prefix("panicked at ") else {
            continue;
        };

        if let Some(loc) = location.strip_suffix(':') {
            let Some((file, l, c)) = split_location(loc) else {
                continue;
            };
            let mut msg = Vec::new();
            while i < lines.len() && !is_panic_footer(lines[i]) {
                msg.push(lines[i]);
                i += 1;
            }
            panics.push(Panic {
                test: test.into(),
                file,
                line: l,
                column: c,
                message: msg.join("\n").trim_end().to_string(),
            });
        } else if let Some((msg, loc)) = location
            .strip_prefix('\'')
            .and_then(|r| r.rsplit_once("', "))
        {
            if let Some((file, l, c)) = split_location(loc.trim()) {
                panics.push(Panic {
                    test: test.into(),
                    file,
                    line: l,
                    column: c,
                    message: msg.to_string(),
                });
            }
        }
    }
    panics
}

fn is_panic_footer(line: &str) -> bool {
    line.trim().is_empty()
        || line.starts_with("note: run with `RUST_BACKTRACE")
        || line.starts_with("stack backtrace:")
        || line.starts_with("---- ")
        || line.starts_with("thread '")
        || line == "failures:"
}

/// Splits `path/to/file.rs:LINE:COL` (paths may themselves contain colons on Windows).
fn split_location(s: &str) -> Option<(String, u32, u32)> {
    let (rest, col) = s.rsplit_once(':')?;
    let (file, line) = rest.rsplit_once(':')?;
    Some((file.to_string(), line.parse().ok()?, col.parse().ok()?))
}

struct Section<'a> {
    /// Test name from a `---- NAME stdout ----` header, if inside one.
    name: Option<&'a str>,
    lines: Vec<&'a str>,
}

fn split_sections<'a>(lines: &[&'a str]) -> Vec<Section<'a>> {
    let mut sections = vec![Section {
        name: None,
        lines: Vec::new(),
    }];
    for &line in lines {
        let header = line
            .strip_prefix("---- ")
            .and_then(|r| r.strip_suffix(" stdout ----"));
        if let Some(name) = header {
            sections.push(Section {
                name: Some(name),
                lines: Vec::new(),
            });
        } else if line == "failures:" {
            sections.push(Section {
                name: None,
                lines: Vec::new(),
            });
        } else {
            sections.last_mut().unwrap().lines.push(line);
        }
    }
    sections
}

/// A fenced code block in a markdown file that skeptic turned into a test.
struct MdBlock {
    path: PathBuf,
    /// 1-based line of the opening fence.
    fence_line: u32,
    /// With a template the snippet's line numbers don't map onto the markdown.
    templated: bool,
    /// The lines between the opening and closing fence.
    code: Vec<String>,
}

/// Skeptic names tests `<file stem>[_sect_<section>]_line_<N>`, where `N` is
/// the 1-based line of the opening fence. Finds the markdown file and block.
fn locate_block(
    root: &Path,
    test_name: &str,
    md_files: &mut Option<Vec<PathBuf>>,
) -> Option<MdBlock> {
    let (prefix, n) = test_name.rsplit_once("_line_")?;
    let fence_line: u32 = n.parse().ok()?;
    let files = md_files.get_or_insert_with(|| {
        let mut found = Vec::new();
        collect_markdown(root, &mut found);
        found
    });
    for path in files.iter() {
        let stem = sanitize(path.file_stem()?.to_str()?);
        let matches = prefix == stem
            || prefix
                .strip_prefix(&stem)
                .is_some_and(|r| r.starts_with("_sect_"));
        if !matches {
            continue;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let fence = text
            .lines()
            .nth(fence_line.checked_sub(1)? as usize)?
            .trim_start();
        if !(fence.starts_with("```") || fence.starts_with("~~~")) {
            continue;
        }
        let templated = fence.contains("skt-") || text.contains("skeptic-template");
        let marker = &fence[..3];
        let code = text
            .lines()
            .skip(fence_line as usize)
            .take_while(|l| !l.trim_start().starts_with(marker))
            .map(String::from)
            .collect();
        return Some(MdBlock {
            path: path.clone(),
            fence_line,
            templated,
            code,
        });
    }
    None
}

/// Mirrors skeptic's `sanitize_test_name`.
pub fn sanitize(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                collect_markdown(&path, out);
            }
        } else if name.ends_with(".md") && !name.ends_with(".skt.md") {
            out.push(path);
        }
    }
}

/// A rustc diagnostic from human-readable output (`error[E0308]: msg` + ` --> file:l:c`).
struct TextDiag {
    level: DiagnosticSeverity,
    code: Option<String>,
    message: String,
    line: u32,
    column: u32,
    len: u32,
}

fn find_text_diagnostics(lines: &[&str]) -> Vec<TextDiag> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let (level, rest) = if let Some(r) = line.strip_prefix("error") {
            (DiagnosticSeverity::ERROR, r)
        } else if let Some(r) = line.strip_prefix("warning") {
            (DiagnosticSeverity::WARNING, r)
        } else {
            continue;
        };
        let (code, message) = match rest.strip_prefix('[').and_then(|r| r.split_once("]: ")) {
            Some((code, msg)) => (Some(code.to_string()), msg),
            None => match rest.strip_prefix(": ") {
                Some(msg) => (None, msg),
                None => continue,
            },
        };
        let Some(loc) = lines
            .get(i)
            .and_then(|l| l.trim_start().strip_prefix("--> "))
        else {
            continue;
        };
        let Some((_, l, c)) = split_location(loc.trim()) else {
            continue;
        };
        // The `^^^` underline in the code frame tells us the span width.
        let len = lines[i + 1..]
            .iter()
            .take(6)
            .filter_map(|l| {
                l.split_once('|')
                    .map(|(_, r)| r.chars().filter(|&c| c == '^').count())
            })
            .find(|&n| n > 0)
            .unwrap_or(1) as u32;
        found.push(TextDiag {
            level,
            code,
            message: message.to_string(),
            line: l,
            column: c,
            len,
        });
    }
    found
}

/// Skeptic's generated `skeptic-tests.rs` holds the exact text each snippet is
/// compiled from. Blank lines can be added or dropped relative to the markdown,
/// so line arithmetic is unreliable; we match on content instead.
fn snippet_text(out_dirs: &[PathBuf], test_name: &str) -> Option<String> {
    let marker = format!("#[test] fn {test_name}() {{");
    for dir in out_dirs {
        let Ok(generated) = std::fs::read_to_string(dir.join("skeptic-tests.rs")) else {
            continue;
        };
        let Some(at) = generated.find(&marker) else {
            continue;
        };
        let rest = &generated[at..];
        let (open, mid, close) = ("format!(r####\"", "\"####, r####\"", "\"####);");
        let start = rest.find(open)? + open.len();
        let split = start + rest[start..].find(mid)?;
        let code_start = split + mid.len();
        let end = code_start + rest[code_start..].find(close)?;
        return Some(rest[start..split].replacen("{}", &rest[code_start..end], 1));
    }
    None
}

/// Finds the markdown line (index into `block.code`) a snippet line came
/// from, plus how many columns the markdown line is shifted by (e.g. a hidden
/// `# ` prefix). Repeated lines are matched by occurrence order.
fn find_in_block(block: &MdBlock, snippet: &[&str], line: u32) -> Option<(usize, u32)> {
    let target = snippet.get(line.checked_sub(1)? as usize)?.trim_end();
    if target.trim().is_empty() {
        return None;
    }
    let seen = snippet[..line as usize - 1]
        .iter()
        .filter(|l| l.trim_end() == target)
        .count();
    let candidates: Vec<(usize, u32)> = block
        .code
        .iter()
        .enumerate()
        .filter_map(|(i, md)| {
            let md = md.trim_end();
            md.ends_with(target)
                .then(|| (i, (md.len() - target.len()) as u32))
        })
        .collect();
    candidates.get(seen).or(candidates.last()).copied()
}

/// Reports a failed markdown snippet at the right place in the markdown file.
///
/// Skeptic compiles each snippet with a bare `rustc` and prints its output
/// into the test's captured stdout, then panics with an unhelpful
/// `Command failed` from generated code. We read the snippet's own rustc errors
/// and runtime panics (both located in a temporary `test.rs`) and translate
/// them to markdown lines.
fn skeptic_section(
    block: &MdBlock,
    snippet: Option<&str>,
    lines: &[&str],
    out: &mut FileDiagnostics,
) {
    let snippet: Option<Vec<&str>> = snippet.map(|t| t.lines().collect());
    // Maps a 1-based snippet line/column to a 0-based markdown position.
    let map = |l: u32, col: u32| -> (u32, u32) {
        let col = col.saturating_sub(1);
        if let Some((i, offset)) = snippet.as_deref().and_then(|s| find_in_block(block, s, l)) {
            return (block.fence_line + i as u32, col + offset);
        }
        if block.templated {
            (block.fence_line - 1, 0)
        } else {
            // Snippet text is "\n" + code, so line L is code line L-1.
            ((block.fence_line + l).saturating_sub(2), col)
        }
    };
    let fence = block.fence_line - 1;
    let mut reported = false;

    for d in find_text_diagnostics(lines) {
        let (line, col) = map(d.line, d.column);
        push_unique(
            out,
            block.path.clone(),
            Diagnostic {
                range: Range::new(Position::new(line, col), Position::new(line, col + d.len)),
                severity: Some(d.level),
                code: d.code.map(NumberOrString::String),
                source: Some(COMPILER_SOURCE.into()),
                message: d.message,
                ..Default::default()
            },
        );
        reported |= d.level == DiagnosticSeverity::ERROR;
    }

    let mut wrapper = None;
    for panic in find_panics(lines) {
        if panic.file.ends_with("test.rs") && panic.file.contains("rust-skeptic") {
            let (line, col) = map(panic.line, panic.column);
            push_panic(out, block.path.clone(), line + 1, col + 1, 1, &panic);
            reported = true;
        } else {
            wrapper = Some(panic);
        }
    }

    if !reported {
        // No compile error or panic: e.g. `main` returned `Err`, which prints
        // `Error: ...` and exits non-zero without saying where. Point at `fn main`.
        let mut message: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| l.starts_with("Error: "))
            .collect();
        if message.is_empty() {
            message.push(wrapper.as_ref().map_or("snippet failed", |p| &p.message));
        }
        let (line, end) = match block.code.iter().position(|l| l.contains("fn main")) {
            Some(i) => (
                block.fence_line + i as u32,
                block.code[i].len().max(1) as u32,
            ),
            None => (fence, 3),
        };
        push_unique(
            out,
            block.path.clone(),
            Diagnostic {
                range: Range::new(Position::new(line, 0), Position::new(line, end)),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some(TEST_SOURCE.into()),
                message: message.join("\n"),
                ..Default::default()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/ws";

    #[test]
    fn parses_modern_assert_panic() {
        let out = "\
running 1 test
test tests::eq ... FAILED

failures:

---- tests::eq stdout ----

thread 'tests::eq' (4242) panicked at src/lib.rs:10:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    tests::eq
";
        let d = parse(out, Path::new(ROOT));
        let list = &d[Path::new("/ws/src/lib.rs")];
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].range.start, Position::new(9, 8));
        assert_eq!(list[0].source.as_deref(), Some(TEST_SOURCE));
        assert!(list[0].message.contains("tests::eq"));
        assert!(list[0].message.contains("right: 2"));
        assert!(!list[0].message.contains("RUST_BACKTRACE"));
    }

    #[test]
    fn parses_legacy_panic() {
        let out = "thread 'it_works' panicked at 'boom', tests/a.rs:7:5\n";
        let d = parse(out, Path::new(ROOT));
        let list = &d[Path::new("/ws/tests/a.rs")];
        assert_eq!(list[0].range.start, Position::new(6, 4));
        assert!(list[0].message.ends_with("boom"));
    }

    #[test]
    fn parses_compiler_error() {
        let json = r#"{"reason":"compiler-message","package_id":"x 0.1.0 (path+file:///ws)","manifest_path":"/ws/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"x","src_path":"/ws/src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"error[E0308]","$message_type":"diagnostic","children":[{"children":[],"code":null,"level":"help","message":"try this","rendered":null,"spans":[]}],"code":{"code":"E0308","explanation":null},"level":"error","message":"mismatched types","spans":[{"byte_end":10,"byte_start":5,"column_end":12,"column_start":5,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":"expected `u32`","line_end":3,"line_start":3,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
        let d = parse(json, Path::new(ROOT));
        let list = &d[Path::new("/ws/src/lib.rs")];
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(list[0].code, Some(NumberOrString::String("E0308".into())));
        assert_eq!(
            list[0].range,
            Range::new(Position::new(2, 4), Position::new(2, 11))
        );
        assert!(list[0].message.contains("help: try this"));
        assert!(list[0].message.contains("expected `u32`"));
    }

    #[test]
    fn maps_skeptic_snippet_errors_to_markdown() {
        let dir = std::env::temp_dir().join(format!("skeptic-lsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("vec-deque.md"),
            "# T\n\n```rust\nfn main() {\n    let x: u32 = \"a\";\n}\n```\n",
        )
        .unwrap();
        let out = "\
---- vec_deque_sect_t_line_3 stdout ----
error[E0308]: mismatched types
 --> /tmp/rust-skeptic1/test.rs:3:18
  |
3 |     let x: u32 = \"a\";
  |                  ^^^ expected `u32`

thread 'vec_deque_sect_t_line_3' panicked at /x/out/skeptic-tests.rs:4:5:
Command failed:
\"rustc\"
";
        let d = parse(out, &dir);
        let list = &d[&dir.join("vec-deque.md")];
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].range,
            Range::new(Position::new(4, 17), Position::new(4, 20))
        );
        assert_eq!(list[0].code, Some(NumberOrString::String("E0308".into())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn should_panic_snippet_that_ran_to_completion_is_reported() {
        let dir = std::env::temp_dir().join(format!("skeptic-lsp-sp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sp.md"),
            "# T\n\n```rust,should_panic\nfn main() {\n    let x = 1;\n}\n```\n",
        )
        .unwrap();
        let out = "\
---- sp_sect_t_line_3 stdout ----
thread 'sp_sect_t_line_3' (1) panicked at /x/skeptic/src/rt.rs:410:9:
the snippet is marked `should_panic` but ran to completion
";
        let d = parse(out, &dir);
        let list = &d[&dir.join("sp.md")];
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(list[0].message.contains("should_panic"));
        // On `fn main`, inside the fenced block.
        assert_eq!(list[0].range.start.line, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn maps_by_content_when_skeptic_adds_blank_lines() {
        let dir = std::env::temp_dir().join(format!("skeptic-lsp-blank-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.md"),
            "# T\n\n```rust\nfn main() {\n\n    let x = 1;\n}\n```\n",
        )
        .unwrap();
        // Skeptic's generated test doubles the blank line, shifting later lines by one.
        std::fs::write(
            dir.join("skeptic-tests.rs"),
            "#[test] fn main_line_3() {\n    let s = &format!(r####\"\n{}\"####, r####\"fn main() {\n\n\n    let x = 1;\n}\n\"####);\n}\n",
        )
        .unwrap();
        let out = format!(
            "{{\"reason\":\"build-script-executed\",\"package_id\":\"a 0.1.0\",\"linked_libs\":[],\"linked_paths\":[],\"cfgs\":[],\"env\":[],\"out_dir\":\"{}\"}}\n\
---- main_line_3 stdout ----
warning: unused variable: `x`
 --> /tmp/rust-skeptic1/test.rs:5:9
  |
5 |     let x = 1;
  |         ^ help: prefix it
",
            dir.display()
        );
        let d = parse(&out, &dir);
        let list = &d[&dir.join("main.md")];
        assert_eq!(
            list[0].range,
            Range::new(Position::new(5, 8), Position::new(5, 9))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_noise() {
        let d = parse(
            "{\"reason\":\"build-finished\",\"success\":true}\nhello\n",
            Path::new(ROOT),
        );
        assert!(d.is_empty());
    }
}
