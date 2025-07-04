use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use cargo_metadata::Edition;
use error_chain::error_chain;
use walkdir::WalkDir;

pub fn compile_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
    handle_test(
        root_dir,
        out_dir,
        target_triple,
        test_text,
        CompileType::Check,
    );
}

pub fn run_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
    handle_test(
        root_dir,
        out_dir,
        target_triple,
        test_text,
        CompileType::Full,
    );
}

fn handle_test(
    root_dir: &str,
    target_dir: &str,
    target_triple: &str,
    test_text: &str,
    compile_type: CompileType,
) {
    let out_dir = tempfile::Builder::new()
        .prefix("rust-skeptic")
        .tempdir()
        .unwrap();
    let testcase_path = out_dir.path().join("test.rs");
    fs::write(&testcase_path, test_text.as_bytes()).unwrap();

    // OK, here's where a bunch of magic happens using assumptions
    // about cargo internals. We are going to use rustc to compile
    // the examples, but to do that we've got to tell it where to
    // look for the rlibs with the -L flag, and what their names
    // are with the --extern flag. This is going to involve
    // parsing fingerprints out of the lockfile and looking them
    // up in the fingerprint file.

    let root_dir = PathBuf::from(root_dir);
    let mut target_dir = PathBuf::from(target_dir);
    target_dir.pop();
    target_dir.pop();
    target_dir.pop();
    let mut deps_dir = target_dir.clone();
    deps_dir.push("deps");

    let rustc = env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let mut cmd = Command::new(rustc);
    cmd.arg(testcase_path)
        .arg("--verbose")
        .arg("--crate-type=bin");

    // Find the edition

    // This has to come before "-L".
    let metadata_path = root_dir.join("Cargo.toml");
    let metadata = get_cargo_meta(&metadata_path).expect("failed to read Cargo.toml");
    let edition = metadata
        .packages
        .iter()
        .filter_map(|package| edition_str(&package.edition))
        .max()
        .unwrap();
    if edition != "2015" {
        cmd.arg(format!("--edition={}", edition));
    }

    cmd.arg("-L")
        .arg(&target_dir)
        .arg("-L")
        .arg(&deps_dir)
        .arg("--target")
        .arg(target_triple);

    let deps = get_rlib_dependencies(root_dir, target_dir).expect("failed to read dependencies");
    eprintln!("Found {} dependencies:", deps.len());
    for dep in &deps {
        eprintln!("  {} -> {}", dep.libname, dep.rlib.display());
    }
    
    for dep in deps {
        cmd.arg("--extern");
        cmd.arg(format!(
            "{}={}",
            dep.libname,
            dep.rlib.to_str().expect("filename not utf8"),
        ));
    }

    let binary_path = out_dir.path().join("out.exe");
    match compile_type {
        CompileType::Full => cmd.arg("-o").arg(&binary_path),
        CompileType::Check => cmd.arg(format!(
            "--emit=dep-info={0}.d,metadata={0}.m",
            binary_path.display()
        )),
    };

    interpret_output(cmd);

    if let CompileType::Check = compile_type {
        return;
    }

    let mut cmd = Command::new(binary_path);
    cmd.current_dir(out_dir.path());
    interpret_output(cmd);
}

fn interpret_output(mut command: Command) {
    let output = command.output().unwrap();
    print!("{}", String::from_utf8(output.stdout).unwrap());
    eprint!("{}", String::from_utf8(output.stderr).unwrap());
    if !output.status.success() {
        panic!("Command failed:\n{:?}", command);
    }
}

// Retrieve the exact dependencies for a given build by
// cross-referencing the lockfile with the fingerprint file
fn get_rlib_dependencies(root_dir: PathBuf, target_dir: PathBuf) -> Result<Vec<Fingerprint>> {
    eprintln!("get_rlib_dependencies: root_dir={}, target_dir={}", root_dir.display(), target_dir.display());
    
    let lock = LockedDeps::from_path(root_dir).or_else(|_| {
        // could not find Cargo.lock in $CARGO_MAINFEST_DIR
        // try relative to target_dir
        let mut root_dir = target_dir.clone();
        root_dir.pop();
        root_dir.pop();
        eprintln!("Trying alternative root_dir: {}", root_dir.display());
        LockedDeps::from_path(root_dir)
    })?;

    let fingerprint_dir = target_dir.join(".fingerprint/");
    eprintln!("Fingerprint dir: {}", fingerprint_dir.display());
    eprintln!("Fingerprint dir exists: {}", fingerprint_dir.exists());
    
    let locked_deps: HashMap<String, String> = lock.collect();
    eprintln!("Locked deps: {:?}", locked_deps);
    
    let mut found_deps: HashMap<String, Fingerprint> = HashMap::new();

    for finger in WalkDir::new(fingerprint_dir)
        .into_iter()
        .filter_map(|v| Fingerprint::from_path(v.ok()?.path()).ok())
    {
        let locked_ver = match locked_deps.get(&finger.name()) {
            Some(ver) => ver,
            None => continue,
        };

        // Improved version matching logic
        match (found_deps.entry(finger.name()), finger.version()) {
            (Entry::Occupied(mut e), Some(ver)) => {
                // If we have a version, prefer exact matches with the locked version
                if *locked_ver == ver {
                    // If we already have an entry, only replace if this one is fresher
                    if e.get().mtime < finger.mtime {
                        e.insert(finger);
                    }
                }
                // If versions don't match, keep the existing entry (first one wins)
            }
            (Entry::Vacant(e), Some(ver)) => {
                // For new entries with version, only insert if it matches locked version
                if *locked_ver == ver {
                    e.insert(finger);
                }
            }
            (Entry::Vacant(e), None) => {
                // For unversioned entries, insert them (they might be workspace members)
                e.insert(finger);
            }
            (Entry::Occupied(_), None) => {
                // If we already have an entry and this one is unversioned, skip it
                // This prevents unversioned entries from overriding versioned ones
            }
        }
    }

    Ok(found_deps
        .into_iter()
        .filter_map(|(_, val)| if val.rlib.exists() { Some(val) } else { None })
        .collect())
}

// An iterator over the root dependencies in a lockfile
#[derive(Debug)]
struct LockedDeps {
    dependencies: Vec<(String, String)>,
}

fn get_cargo_meta<P: AsRef<Path> + std::convert::AsRef<std::ffi::OsStr>>(
    path: P,
) -> Result<cargo_metadata::Metadata> {
    Ok(cargo_metadata::MetadataCommand::new()
        .manifest_path(&path)
        .exec()?)
}

// Update LockedDeps to accept a package name
impl LockedDeps {
    fn from_path<P: AsRef<Path>>(path: P) -> Result<LockedDeps> {
        let path = path.as_ref().join("Cargo.toml");
        let metadata = get_cargo_meta(&path)?;
        eprintln!("LockedDeps::from_path: path={}", path.display());
        for pkg in &metadata.packages {
            eprintln!("  package: name={}, manifest_path={}", pkg.name, pkg.manifest_path.as_str());
        }
        let resolve = metadata.resolve.ok_or("Missing dependency metadata")?;
        let all_nodes: std::collections::HashMap<_, _> = resolve
            .nodes
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect();
        // Find the root package (the one matching the manifest path)
        let root_package = metadata.packages.iter().find(|pkg| pkg.manifest_path.as_str() == path.to_str().unwrap()).ok_or("Root package not found")?;
        let root_id = &root_package.id;
        eprintln!("Root package id: {}", root_id.repr);
        for pkg in &metadata.packages {
            eprintln!("  id: {} name: {}", pkg.id.repr, pkg.name);
        }
        // Walk dependencies from the root package
        let mut all_deps = std::collections::HashSet::new();
        if let Some(root_node) = all_nodes.get(root_id) {
            eprintln!("Root node dependencies: {:?}", root_node.dependencies.iter().map(|d| d.repr.clone()).collect::<Vec<_>>());
            all_deps.insert(root_node.id.clone());
            let mut to_visit = root_node.dependencies.clone();
            while let Some(dep_id) = to_visit.pop() {
                if all_deps.insert(dep_id.clone()) {
                    if let Some(dep_node) = all_nodes.get(&dep_id) {
                        to_visit.extend(dep_node.dependencies.clone());
                    }
                }
            }
        }
        // Collect (name, version) pairs for all_deps
        let mut dep_pairs = Vec::new();
        for node_id in &all_deps {
            if let Some(pkg) = metadata.packages.iter().find(|p| &p.id == node_id) {
                dep_pairs.push((pkg.name.replace('-', "_"), pkg.version.to_string()));
            }
        }
        Ok(LockedDeps {
            dependencies: dep_pairs,
        })
    }
}

impl Iterator for LockedDeps {
    type Item = (String, String);
    fn next(&mut self) -> Option<(String, String)> {
        self.dependencies.pop()
    }
}

#[derive(Debug)]
struct Fingerprint {
    libname: String,
    version: Option<String>, // version might not be present on path or vcs deps
    rlib: PathBuf,
    mtime: SystemTime,
}

fn guess_ext(mut path: PathBuf, exts: &[&str]) -> Result<PathBuf> {
    for ext in exts {
        path.set_extension(ext);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(ErrorKind::Fingerprint.into())
}

fn extract_version_from_fingerprint<P: AsRef<Path>>(path: P) -> Result<Option<String>> {
    let content = fs::read_to_string(path)?;
    
    // Look for version information in the fingerprint content
    // Cargo fingerprint files often contain version info in various formats
    for line in content.lines() {
        // Look for patterns like "version: 1.2.3" or "1.2.3" after certain keywords
        if line.contains("version:") {
            if let Some(version) = line.split("version:").nth(1) {
                let version = version.trim();
                if !version.is_empty() {
                    return Ok(Some(version.to_string()));
                }
            }
        }
        
        // Look for semver patterns (x.y.z)
        if line.contains('.') {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for part in parts {
                if part.matches('.').count() == 2 && part.chars().all(|c| c.is_digit(10) || c == '.') {
                    return Ok(Some(part.to_string()));
                }
            }
        }
    }
    
    Ok(None)
}

impl Fingerprint {
    fn from_path<P: AsRef<Path>>(path: P) -> Result<Fingerprint> {
        let path = path.as_ref();

        // Use the parent path to get libname and hash, replacing - with _
        let mut captures = path
            .parent()
            .and_then(Path::file_stem)
            .and_then(OsStr::to_str)
            .ok_or(ErrorKind::Fingerprint)?
            .rsplit('-');
        let hash = captures.next().ok_or(ErrorKind::Fingerprint)?;
        let mut libname_parts = captures.collect::<Vec<_>>();
        libname_parts.reverse();
        let libname = libname_parts.join("_");

        path.extension()
            .and_then(|e| if e == "json" { Some(e) } else { None })
            .ok_or(ErrorKind::Fingerprint)?;

        let mut rlib = PathBuf::from(path);
        rlib.pop();
        rlib.pop();
        rlib.pop();
        let mut dll = rlib.clone();
        rlib.push(format!("deps/lib{}-{}", libname, hash));
        dll.push(format!("deps/{}-{}", libname, hash));
        rlib = guess_ext(rlib, &["rlib", "so", "dylib"]).or_else(|_| guess_ext(dll, &["dll"]))?;

        // Try to extract version from the fingerprint file content
        let version = extract_version_from_fingerprint(path)?;

        Ok(Fingerprint {
            libname,
            version,
            rlib,
            mtime: fs::metadata(path)?.modified()?,
        })
    }

    fn name(&self) -> String {
        self.libname.clone()
    }

    fn version(&self) -> Option<String> {
        self.version.clone()
    }
}

error_chain! {
    errors { Fingerprint }
    foreign_links {
        Io(std::io::Error);
        Metadata(cargo_metadata::Error);
    }
}

#[derive(Clone, Copy)]
enum CompileType {
    Full,
    Check,
}

fn edition_str(edition: &Edition) -> Option<&'static str> {
    Some(match edition {
        Edition::E2015 => "2015",
        Edition::E2018 => "2018",
        Edition::E2021 => "2021",
        _ => return None,
    })
}

