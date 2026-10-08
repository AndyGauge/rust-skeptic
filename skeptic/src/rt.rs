use std::collections::btree_map::Entry;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use cargo_metadata::Edition;

use once_cell::sync::Lazy;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use walkdir::WalkDir;

// Persistent cache structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentCache {
    pub cache_version: u32, // Defensive versioning to invalidate cache when data format changes
    pub entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    fingerprints: Vec<Fingerprint>,
    cache_key_hash: u64,
    created_at: SystemTime,
}

impl Default for PersistentCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistentCache {
    pub fn new() -> Self {
        Self {
            cache_version: 1,
            entries: HashMap::new(),
        }
    }

    pub fn get_cache_file_path(root_dir: &Path) -> PathBuf {
        root_dir.join(".skeptic-cache")
    }

    pub fn load_from_file(root_dir: &Path) -> Result<Self> {
        let cache_file = Self::get_cache_file_path(root_dir);
        if !cache_file.exists() {
            return Ok(Self::new());
        }

        let data = fs::read(&cache_file)?;
        // A cache written by an older release (binary) fails to parse and is
        // simply rebuilt.
        match serde_json::from_slice::<PersistentCache>(&data) {
            Ok(cache) => {
                if cache.cache_version == 1 {
                    Ok(cache)
                } else {
                    Ok(Self::new())
                }
            }
            Err(_) => Ok(Self::new()),
        }
    }

    pub fn save_to_file(&self, root_dir: &Path) -> Result<()> {
        let cache_file = Self::get_cache_file_path(root_dir);
        let data = serde_json::to_vec(self).map_err(|e| {
            SkepticError::Io(std::io::Error::other(format!("Serialization error: {}", e)))
        })?;
        fs::write(&cache_file, data)?;
        Ok(())
    }

    pub fn generate_cache_key_hash(root_dir: &Path, target_dir: &Path) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // Hash the paths
        root_dir.hash(&mut hasher);
        target_dir.hash(&mut hasher);

        // Hash the Cargo.toml modification time if it exists
        if let Ok(metadata) = fs::metadata(root_dir.join("Cargo.toml")) {
            if let Ok(modified) = metadata.modified() {
                modified.hash(&mut hasher);
            }
        }

        // Hash the Cargo.lock modification time if it exists
        if let Ok(metadata) = fs::metadata(root_dir.join("Cargo.lock")) {
            if let Ok(modified) = metadata.modified() {
                modified.hash(&mut hasher);
            }
        }

        hasher.finish()
    }

    pub fn get(&self, cache_key: &str, expected_hash: u64) -> Option<&Vec<Fingerprint>> {
        if let Some(entry) = self.entries.get(cache_key) {
            if entry.cache_key_hash == expected_hash {
                // Check if cache is not too old (1 hour)
                if let Ok(elapsed) = entry.created_at.elapsed() {
                    if elapsed.as_secs() < 3600 {
                        return Some(&entry.fingerprints);
                    }
                }
            }
        }
        None
    }

    pub fn insert(
        &mut self,
        cache_key: String,
        fingerprints: Vec<Fingerprint>,
        cache_key_hash: u64,
    ) {
        let entry = CacheEntry {
            fingerprints,
            cache_key_hash,
            created_at: SystemTime::now(),
        };
        self.entries.insert(cache_key, entry);
    }
}

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

/// Everything needed to invoke `rustc` that is the same for every snippet of
/// a given project. Resolving it runs `cargo metadata` and walks the
/// fingerprint directory, so it is computed once and shared.
struct Prepared {
    edition: Option<&'static str>,
    target_dir: PathBuf,
    deps_dir: PathBuf,
    /// Directories holding the dependency rlibs (many, in the newer layout).
    search_dirs: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
}

type PreparedKey = (String, String);

static PREPARED: Lazy<Mutex<HashMap<PreparedKey, Arc<Prepared>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Forget everything cached in memory and on disk for `root_dir`, so the next
/// snippet pays the full setup cost. Intended for benchmarks.
#[doc(hidden)]
pub fn clear_caches(root_dir: &str) {
    PREPARED.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let _ = fs::remove_file(PersistentCache::get_cache_file_path(Path::new(root_dir)));
}

/// The `target/<profile>` directory, found from a build script's `OUT_DIR`.
/// Cargo has laid that out as both `build/<pkg>-<hash>/out` and
/// `build/<pkg>/<hash>/out`, so look for the outermost `build` directory
/// within reach instead of assuming a depth.
fn profile_dir(out_dir: &Path) -> PathBuf {
    out_dir
        .ancestors()
        .skip(1)
        .take(4)
        .filter(|dir| dir.file_name().is_some_and(|name| name == "build"))
        .last()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            // Unusual layout: fall back to the classic depth.
            let mut dir = out_dir.to_path_buf();
            dir.pop();
            dir.pop();
            dir.pop();
            dir
        })
}

/// Where cargo keeps per-unit fingerprints: `.fingerprint/` in the classic
/// layout, or inside `build/<pkg>/<hash>/fingerprint/` in the newer one.
fn fingerprint_root(target_dir: &Path) -> Option<PathBuf> {
    let classic = target_dir.join(".fingerprint");
    if classic.is_dir() {
        return Some(classic);
    }
    let build = target_dir.join("build");
    if build.is_dir() {
        return Some(build);
    }
    None
}

fn prepare(root_dir: &str, target_dir: &str) -> Arc<Prepared> {
    // Held while computing so concurrent tests wait for one resolution
    // instead of each doing their own.
    let mut cache = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    let key = (root_dir.to_owned(), target_dir.to_owned());
    if let Some(prepared) = cache.get(&key) {
        return Arc::clone(prepared);
    }

    let root_dir = PathBuf::from(root_dir);
    let target_dir = profile_dir(Path::new(target_dir));
    let deps_dir = target_dir.join("deps");

    let metadata_path = root_dir.join("Cargo.toml");
    let metadata = get_cargo_meta(&metadata_path).expect("failed to read Cargo.toml");
    let edition = metadata
        .packages
        .iter()
        .filter_map(|package| edition_str(&package.edition))
        .max()
        .unwrap();
    let edition = if edition != "2015" {
        Some(edition)
    } else {
        None
    };

    let externs: Vec<(String, PathBuf)> = get_rlib_dependencies(root_dir, target_dir.clone())
        .expect("failed to read dependencies")
        .into_iter()
        .map(|dep| (dep.libname, dep.rlib))
        .collect();

    // In the newer layout every unit's rlibs live in their own directory, and
    // rustc must be able to see all of them: a dependency may have been built
    // in several variants, and the one a crate was compiled against is not
    // necessarily the one we pass with --extern.
    let mut search_dirs: Vec<PathBuf> = Vec::new();
    if !target_dir.join(".fingerprint").is_dir() {
        for pkg in fs::read_dir(target_dir.join("build"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for unit in fs::read_dir(pkg.path()).into_iter().flatten().flatten() {
                let out = unit.path().join("out");
                if unit.path().join("fingerprint").is_dir() && out.is_dir() {
                    search_dirs.push(out);
                }
            }
        }
        search_dirs.sort();
    }

    let prepared = Arc::new(Prepared {
        edition,
        target_dir,
        deps_dir,
        search_dirs,
        externs,
    });
    cache.insert(key, Arc::clone(&prepared));
    prepared
}

/// Captured result of running one command.
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    failure: Option<String>,
}

/// Everything a worker produced for one snippet.
struct Outcome {
    steps: Vec<Captured>,
}

struct Job {
    work: Box<dyn FnOnce() -> Outcome + Send>,
    reply: Sender<Outcome>,
}

/// Number of snippets compiled/run at once: `SKEPTIC_JOBS`, else the CPU count.
fn worker_count() -> usize {
    env::var("SKEPTIC_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(num_cpus::get)
}

/// A fixed pool of workers fed by a queue. libtest runs each generated test
/// on its own thread; those threads only enqueue and wait, so the number of
/// concurrent `rustc` processes stays bounded.
static QUEUE: Lazy<Mutex<Sender<Job>>> = Lazy::new(|| {
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..worker_count() {
        let rx = Arc::clone(&rx);
        thread::spawn(move || loop {
            let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                Ok(job) => job,
                Err(_) => return,
            };
            let _ = job.reply.send((job.work)());
        });
    }
    Mutex::new(tx)
});

fn handle_test(
    root_dir: &str,
    target_dir: &str,
    target_triple: &str,
    test_text: &str,
    compile_type: CompileType,
) {
    let prepared = prepare(root_dir, target_dir);
    let target_triple = target_triple.to_owned();
    let test_text = test_text.to_owned();

    let (reply, result) = mpsc::channel();
    let job = Job {
        work: Box::new(move || execute(&prepared, &target_triple, &test_text, compile_type)),
        reply,
    };
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .expect("skeptic worker pool is gone");
    let outcome = result.recv().expect("skeptic worker panicked");

    // Report from the test's own thread so libtest attributes output and
    // panics (including `should_panic`) to the right test.
    for step in outcome.steps {
        print!("{}", String::from_utf8(step.stdout).unwrap());
        eprint!("{}", String::from_utf8(step.stderr).unwrap());
        if let Some(failure) = step.failure {
            panic!("{}", failure);
        }
    }
}

fn execute(
    prepared: &Prepared,
    target_triple: &str,
    test_text: &str,
    compile_type: CompileType,
) -> Outcome {
    let out_dir = tempfile::Builder::new()
        .prefix("rust-skeptic")
        .tempdir()
        .unwrap();
    let testcase_path = out_dir.path().join("test.rs");
    fs::write(&testcase_path, test_text.as_bytes()).unwrap();

    // We use rustc directly, telling it where the rlibs are with -L and
    // their names with --extern (resolved once in `prepare`).
    let rustc = env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let mut cmd = Command::new(rustc);
    cmd.arg(testcase_path)
        .arg("--verbose")
        .arg("--crate-type=bin");

    // This has to come before "-L".
    if let Some(edition) = prepared.edition {
        cmd.arg(format!("--edition={}", edition));
    }

    cmd.arg("-L")
        .arg(&prepared.target_dir)
        .arg("-L")
        .arg(&prepared.deps_dir)
        .arg("--target")
        .arg(target_triple);

    for dir in &prepared.search_dirs {
        let mut arg = std::ffi::OsString::from("dependency=");
        arg.push(dir);
        cmd.arg("-L").arg(arg);
    }

    for (libname, rlib) in &prepared.externs {
        cmd.arg("--extern");
        cmd.arg(format!(
            "{}={}",
            libname,
            rlib.to_str().expect("filename not utf8"),
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

    let mut steps = vec![capture(cmd)];

    if let CompileType::Full = compile_type {
        if steps[0].failure.is_none() {
            let mut cmd = Command::new(binary_path);
            cmd.current_dir(out_dir.path());
            steps.push(capture(cmd));
        }
    }

    Outcome { steps }
}

fn capture(mut command: Command) -> Captured {
    let output = command.output().unwrap();
    let failure = if output.status.success() {
        None
    } else {
        Some(format!("Command failed:\n{:?}", command))
    };
    Captured {
        stdout: output.stdout,
        stderr: output.stderr,
        failure,
    }
}

// Retrieve the exact dependencies for a given build by
// cross-referencing the lockfile with the fingerprint file
fn get_rlib_dependencies(root_dir: PathBuf, target_dir: PathBuf) -> Result<Vec<Fingerprint>> {
    let cache_key_str = format!("{}:{}", root_dir.display(), target_dir.display());
    let cache_key_hash = PersistentCache::generate_cache_key_hash(&root_dir, &target_dir);

    let mut persistent_cache = PersistentCache::load_from_file(&root_dir)?;

    let lock = LockedDeps::from_path(root_dir.clone()).or_else(|_| {
        // could not find Cargo.lock in $CARGO_MAINFEST_DIR
        // try relative to target_dir
        let mut root_dir = target_dir.clone();
        root_dir.pop();
        root_dir.pop();
        LockedDeps::from_path(root_dir)
    })?;

    // Get direct dependencies first before consuming the lock
    let direct_deps = lock.get_direct_dependencies().clone();
    let locked_deps: HashMap<String, String> = lock.collect();

    // Check if cached dependencies are still valid by ensuring key dependencies are present
    // This helps detect when new dependencies have been added to Cargo.lock
    if let Some(cached_deps) = persistent_cache.get(&cache_key_str, cache_key_hash) {
        let cached_dep_names: HashSet<String> =
            cached_deps.iter().map(|d| d.name().to_string()).collect();

        // Check if all locked dependencies are represented in the cache
        let missing_deps: Vec<&String> = locked_deps
            .keys()
            .filter(|dep_name| !cached_dep_names.contains(*dep_name))
            .collect();

        if missing_deps.is_empty() {
            // Cache is valid - all expected dependencies are present
            return Ok(cached_deps.clone());
        }
    }

    let fingerprint_dir =
        fingerprint_root(&target_dir).unwrap_or_else(|| target_dir.join(".fingerprint"));

    // Get cargo metadata once and reuse it for all fingerprints
    let metadata_path = root_dir.join("Cargo.toml");
    let metadata = get_cargo_meta(&metadata_path).ok();

    let mut found_deps: std::collections::BTreeMap<String, Fingerprint> =
        std::collections::BTreeMap::new();

    // Collect all fingerprint paths first and sort for deterministic behavior
    // (`build/<pkg>/<hash>/fingerprint/<file>` is four levels down in the
    // newer layout; the classic one is two.)
    let mut fingerprint_paths: Vec<_> = WalkDir::new(fingerprint_dir)
        .max_depth(4)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().to_owned())
        .collect();

    fingerprint_paths.sort();

    let fingerprints: Vec<_> = fingerprint_paths
        .iter()
        .filter_map(|path| Fingerprint::from_path(path, metadata.as_ref()).ok())
        .collect();

    for finger in fingerprints {
        let locked_ver = match locked_deps.get(&finger.name()) {
            Some(ver) => ver,
            None => continue,
        };

        // Check if this is a direct dependency that requires strict version matching
        let is_direct_dep = direct_deps.contains_key(&finger.name());
        let required_version = if is_direct_dep {
            Some(&direct_deps[&finger.name()])
        } else {
            None
        };
        match (found_deps.entry(finger.name()), finger.version()) {
            (Entry::Occupied(mut e), Some(ver)) => {
                // For direct dependencies, require exact version match
                if let Some(req_ver) = required_version {
                    if *req_ver == ver {
                        e.insert(finger);
                    }
                } else {
                    // For transitive dependencies, use the existing logic
                    // First try exact version match (highest priority)
                    if *locked_ver == ver {
                        e.insert(finger);
                    } else {
                        // Then try semantic version matching
                        if let (Ok(req), Ok(version)) =
                            (VersionReq::parse(locked_ver), Version::parse(&ver))
                        {
                            if req.matches(&version) {
                                // Only replace if we don't have an exact match already
                                let current = e.get();
                                if let Some(current_ver) = &current.version {
                                    if *locked_ver != *current_ver {
                                        // If current is not an exact match, replace with this one
                                        e.insert(finger);
                                    }
                                } else {
                                    e.insert(finger);
                                }
                            }
                        } else {
                            // Fallback: try to parse the locked version as a semver requirement
                            // This handles cases where the project specifies "0.8" but Cargo resolves to "0.8.5"
                            let req_str = if locked_ver.matches('.').count() == 1 {
                                // If it's like "0.8", convert to "^0.8.0"
                                format!("^{}.0", locked_ver)
                            } else {
                                // If it's already a full version like "0.8.5", convert to "^0.8.5"
                                format!("^{}", locked_ver)
                            };

                            if let (Ok(req), Ok(version)) =
                                (VersionReq::parse(&req_str), Version::parse(&ver))
                            {
                                if req.matches(&version) {
                                    let current = e.get();
                                    if let Some(current_ver) = &current.version {
                                        if *locked_ver != *current_ver {
                                            e.insert(finger);
                                        }
                                    } else {
                                        e.insert(finger);
                                    }
                                }
                            } else {
                                // Final fallback to exact match if parsing fails
                                if *locked_ver == ver && e.get().mtime < finger.mtime {
                                    e.insert(finger);
                                }
                            }
                        }
                    }
                }
            }
            (Entry::Vacant(e), Some(ver)) => {
                // For direct dependencies, require exact version match
                if let Some(req_ver) = required_version {
                    if *req_ver == ver {
                        e.insert(finger);
                    }
                } else if *locked_ver == ver {
                    e.insert(finger);
                } else if let (Ok(req), Ok(version)) =
                    (VersionReq::parse(locked_ver), Version::parse(&ver))
                {
                    if req.matches(&version) {
                        e.insert(finger);
                    }
                } else {
                    let req_str = if locked_ver.matches('.').count() == 1 {
                        format!("^{}.0", locked_ver)
                    } else {
                        format!("^{}", locked_ver)
                    };

                    if let (Ok(req), Ok(version)) =
                        (VersionReq::parse(&req_str), Version::parse(&ver))
                    {
                        if req.matches(&version) {
                            e.insert(finger);
                        }
                    } else if *locked_ver == ver {
                        e.insert(finger);
                    }
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

    let result: Vec<Fingerprint> = found_deps
        .into_iter()
        .filter_map(|(name, val)| {
            if val.rlib.exists() {
                Some(val)
            } else {
                eprintln!(
                    "Warning: rlib does not exist for {}: {}",
                    name,
                    val.rlib.display()
                );
                None
            }
        })
        .collect();

    // Save to persistent cache
    persistent_cache.insert(cache_key_str, result.clone(), cache_key_hash);
    let _ = persistent_cache.save_to_file(&root_dir);

    Ok(result)
}

/// Populate the cache during the build phase to make test runs faster
pub fn populate_cache_during_build(root_dir: &Path, target_triple: &str) -> Result<()> {
    fn populate_cache_for_target(root_dir: &Path, target_dir: &Path) -> Result<bool> {
        if !target_dir.exists() || fingerprint_root(target_dir).is_none() {
            return Ok(false);
        }

        let cache_key_str = format!("{}:{}", root_dir.display(), target_dir.display());
        let cache_key_hash = PersistentCache::generate_cache_key_hash(root_dir, target_dir);

        let persistent_cache = PersistentCache::load_from_file(root_dir)?;
        if persistent_cache
            .get(&cache_key_str, cache_key_hash)
            .is_some()
        {
            return Ok(true);
        }

        let _ = get_rlib_dependencies(root_dir.to_path_buf(), target_dir.to_path_buf())?;
        Ok(true)
    }

    // Try to find target directory from OUT_DIR during build script execution
    if let Ok(out_dir) = env::var("OUT_DIR") {
        let out_path = PathBuf::from(&out_dir);
        let mut target_dir = out_path.clone();

        // Go up from out_dir to find the target directory
        // OUT_DIR structure: target/{profile}/build/{package}-{hash}/out
        for _ in 0..4 {
            target_dir.pop();
            if populate_cache_for_target(root_dir, &target_dir)? {
                return Ok(());
            }
        }
    }

    // Fallback: try the traditional locations
    let potential_target_dirs = [
        root_dir.join("target").join(target_triple).join("debug"),
        root_dir.join("target").join(target_triple).join("release"),
        root_dir.join("target").join("debug"),
        root_dir.join("target").join("release"),
    ];

    for target_dir in &potential_target_dirs {
        if populate_cache_for_target(root_dir, target_dir)? {
            return Ok(());
        }
    }

    Ok(())
}

// An iterator over the root dependencies in a lockfile
#[derive(Debug)]
pub struct LockedDeps {
    dependencies: Vec<(String, String)>,
    direct_dependencies: HashMap<String, String>,
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
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<LockedDeps> {
        let path = path.as_ref().join("Cargo.toml");
        let metadata = get_cargo_meta(&path)?;
        let resolve = metadata
            .resolve
            .ok_or(SkepticError::MissingDependencyMetadata)?;
        let all_nodes: std::collections::HashMap<_, _> = resolve
            .nodes
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect();
        // Find the root package (the one matching the manifest path)
        let root_package = metadata
            .packages
            .iter()
            .find(|pkg| pkg.manifest_path.as_str() == path.to_str().unwrap())
            .ok_or(SkepticError::RootPackageNotFound)?;
        let root_id = &root_package.id;

        // First, collect direct dependencies with their versions
        let mut direct_deps = std::collections::HashMap::new();
        if let Some(root_node) = all_nodes.get(root_id) {
            // Collect direct dependencies first
            for dep_id in &root_node.dependencies {
                if let Some(pkg) = metadata.packages.iter().find(|p| &p.id == dep_id) {
                    let name = pkg.name.replace('-', "_");
                    direct_deps.insert(name.clone(), pkg.version.to_string());
                }
            }

            // Then walk transitive dependencies, but don't override direct dependencies
            let mut all_deps = std::collections::HashSet::new();
            all_deps.insert(root_node.id.clone());
            let mut to_visit = root_node.dependencies.clone();
            while let Some(dep_id) = to_visit.pop() {
                if all_deps.insert(dep_id.clone()) {
                    if let Some(dep_node) = all_nodes.get(&dep_id) {
                        to_visit.extend(dep_node.dependencies.clone());
                    }
                }
            }

            // Collect all dependencies, prioritizing direct ones
            let mut dep_pairs = Vec::new();
            for node_id in &all_deps {
                if let Some(pkg) = metadata.packages.iter().find(|p| &p.id == node_id) {
                    let name = pkg.name.replace('-', "_");
                    let version = pkg.version.to_string();

                    // If this is a direct dependency, use it
                    if direct_deps.contains_key(&name) {
                        dep_pairs.push((name.clone(), direct_deps[&name].clone()));
                    } else {
                        // Otherwise, use the transitive dependency version
                        dep_pairs.push((name, version));
                    }
                }
            }

            Ok(LockedDeps {
                dependencies: dep_pairs,
                direct_dependencies: direct_deps,
            })
        } else {
            Err(SkepticError::RootPackageNotFound)
        }
    }

    pub fn get_direct_dependencies(&self) -> &HashMap<String, String> {
        &self.direct_dependencies
    }
}

impl Iterator for LockedDeps {
    type Item = (String, String);
    fn next(&mut self) -> Option<(String, String)> {
        self.dependencies.pop()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub libname: String,
    pub version: Option<String>, // version might not be present on path or vcs deps
    pub rlib: PathBuf,
    pub mtime: SystemTime,
}

fn guess_ext(mut path: PathBuf, exts: &[&str]) -> Result<PathBuf> {
    for ext in exts {
        path.set_extension(ext);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(SkepticError::Fingerprint)
}

/// The crate name and unit hash a fingerprint file belongs to.
///
/// Classic layout: `.fingerprint/<lib>-<hash>/lib-<lib>.json`.
/// Newer layout: `build/<pkg>/<hash>/fingerprint/lib-<lib>.json`.
fn fingerprint_identity(path: &Path) -> Option<(String, String)> {
    let unit_dir = path.parent()?;
    let dir_name = unit_dir.file_name()?.to_str()?;

    if dir_name == "fingerprint" {
        let hash = unit_dir.parent()?.file_name()?.to_str()?;
        let stem = path.file_stem()?.to_str()?;
        let lib = stem.strip_prefix("lib-")?;
        return Some((lib.replace('-', "_"), hash.to_owned()));
    }

    let mut captures = dir_name.rsplit('-');
    let hash = captures.next()?;
    let mut name_parts = captures.collect::<Vec<_>>();
    name_parts.reverse();
    Some((name_parts.join("_"), hash.to_owned()))
}

fn extract_version_from_fingerprint<P: AsRef<Path>>(
    path: P,
    metadata: Option<&cargo_metadata::Metadata>,
) -> Result<Option<String>> {
    let path = path.as_ref();

    // First, try to extract version from the directory name using cached metadata
    if let Some(metadata) = metadata {
        if let Some(parent) = path.parent() {
            if let Some((lib_name, _)) = fingerprint_identity(path) {
                // Find all packages with this name
                let matching_packages: Vec<_> = metadata
                    .packages
                    .iter()
                    .filter(|pkg| pkg.name.replace('-', "_") == lib_name)
                    .collect();

                match matching_packages.len() {
                    1 => {
                        // Only one version, use it
                        return Ok(Some(matching_packages[0].version.to_string()));
                    }
                    n if n > 1 => {
                        // Multiple versions - need to read fingerprint JSON to determine which one
                        let json_path =
                            parent.join(format!("lib-{}.json", lib_name.replace('_', "-")));
                        if json_path.exists() {
                            if let Ok(json_content) = std::fs::read_to_string(&json_path) {
                                if let Ok(fingerprint_data) =
                                    serde_json::from_str::<serde_json::Value>(&json_content)
                                {
                                    if let Some(features) = fingerprint_data
                                        .get("features")
                                        .and_then(|f| f.as_str())
                                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                                    {
                                        // Try to match features against package versions
                                        for pkg in &matching_packages {
                                            // Check if this package's features match the fingerprint
                                            if lib_name == "rand" {
                                                // For rand, use thread_rng as a distinguishing feature
                                                let has_thread_rng =
                                                    features.contains(&"thread_rng".to_string());
                                                let pkg_has_thread_rng =
                                                    pkg.features.contains_key("thread_rng");

                                                if has_thread_rng == pkg_has_thread_rng {
                                                    return Ok(Some(pkg.version.to_string()));
                                                }
                                            } else {
                                                // For other packages, use a more general approach
                                                // This is a simplified check - in practice, you might need more sophisticated matching
                                                let feature_set: std::collections::HashSet<_> =
                                                    features.iter().collect();
                                                let pkg_feature_set: std::collections::HashSet<_> =
                                                    pkg.features.keys().collect();

                                                // Check if there's significant overlap
                                                let intersection_count = feature_set
                                                    .intersection(&pkg_feature_set)
                                                    .count();
                                                if intersection_count > 0 {
                                                    return Ok(Some(pkg.version.to_string()));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // If we can't determine from features, fall back to highest version
                        let mut sorted_packages = matching_packages;
                        sorted_packages.sort_by(|a, b| b.version.cmp(&a.version)); // Sort descending
                        return Ok(Some(sorted_packages[0].version.to_string()));
                    }
                    _ => {
                        // No matching packages - continue to fallback logic
                    }
                }
            }
        }
    }

    // Fallback to the original logic - try to find version in the fingerprint file
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            if line.contains("version:") {
                if let Some(version) = line.split("version:").nth(1) {
                    let version = version.trim();
                    if !version.is_empty() {
                        return Ok(Some(version.to_string()));
                    }
                }
            }
        }
    }

    Ok(None)
}

impl Fingerprint {
    pub fn from_path<P: AsRef<Path>>(
        path: P,
        metadata: Option<&cargo_metadata::Metadata>,
    ) -> Result<Fingerprint> {
        let path = path.as_ref();

        let (libname, hash) = fingerprint_identity(path).ok_or(SkepticError::Fingerprint)?;

        path.extension()
            .filter(|&e| e == "json")
            .ok_or(SkepticError::Fingerprint)?;

        // Classic layout: <profile>/.fingerprint/<lib>-<hash>/<file>.json with
        // the rlib in <profile>/deps. Newer layout:
        // <profile>/build/<pkg>/<hash>/fingerprint/<file>.json with the rlib in
        // the sibling `out` directory.
        let unit_dir = path.parent().ok_or(SkepticError::Fingerprint)?;
        let (rlib, dll) = if unit_dir.file_name().is_some_and(|n| n == "fingerprint") {
            let out = unit_dir.with_file_name("out");
            (
                out.join(format!("lib{}-{}", libname, hash)),
                out.join(format!("{}-{}", libname, hash)),
            )
        } else {
            let profile = unit_dir
                .parent()
                .and_then(Path::parent)
                .ok_or(SkepticError::Fingerprint)?;
            (
                profile.join(format!("deps/lib{}-{}", libname, hash)),
                profile.join(format!("deps/{}-{}", libname, hash)),
            )
        };
        let rlib =
            guess_ext(rlib, &["rlib", "so", "dylib"]).or_else(|_| guess_ext(dll, &["dll"]))?;

        // Try to extract version from the fingerprint file content
        let version = extract_version_from_fingerprint(path, metadata)?;

        Ok(Fingerprint {
            libname,
            version,
            rlib,
            mtime: fs::metadata(path)?.modified()?,
        })
    }

    pub fn name(&self) -> String {
        self.libname
            .split('-')
            .next()
            .unwrap_or(&self.libname)
            .to_string()
    }

    pub fn version(&self) -> Option<String> {
        self.version.clone()
    }
}

#[derive(Debug, Error)]
pub enum SkepticError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Cargo metadata error: {0}")]
    Metadata(#[from] cargo_metadata::Error),
    #[error("Fingerprint error")]
    Fingerprint,
    #[error("Root package not found")]
    RootPackageNotFound,
    #[error("Missing dependency metadata")]
    MissingDependencyMetadata,
}

type Result<T> = std::result::Result<T, SkepticError>;

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

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn profile_dir_classic_layout() {
        let out = Path::new("/w/target/debug/build/testing-0123abcd/out");
        assert_eq!(profile_dir(out), Path::new("/w/target/debug"));
    }

    #[test]
    fn profile_dir_new_layout() {
        let out = Path::new("/w/target/debug/build/testing/0123abcd/out");
        assert_eq!(profile_dir(out), Path::new("/w/target/debug"));
    }

    #[test]
    fn profile_dir_package_named_build() {
        let out = Path::new("/w/target/debug/build/build/0123abcd/out");
        assert_eq!(profile_dir(out), Path::new("/w/target/debug"));
    }

    #[test]
    fn profile_dir_ignores_build_dirs_above_target() {
        let out = Path::new("/home/me/build/proj/target/release/build/x-1/out");
        assert_eq!(
            profile_dir(out),
            Path::new("/home/me/build/proj/target/release")
        );
    }

    #[test]
    fn fingerprint_identity_classic() {
        let path = Path::new("/w/target/debug/.fingerprint/rand_core-0123abcd/lib-rand_core.json");
        assert_eq!(
            fingerprint_identity(path),
            Some(("rand_core".to_owned(), "0123abcd".to_owned()))
        );
    }

    #[test]
    fn fingerprint_identity_new_layout() {
        let path =
            Path::new("/w/target/debug/build/rand_core/0123abcd/fingerprint/lib-rand_core.json");
        assert_eq!(
            fingerprint_identity(path),
            Some(("rand_core".to_owned(), "0123abcd".to_owned()))
        );
    }

    #[test]
    fn fingerprint_identity_new_layout_rejects_non_lib_units() {
        let path = Path::new("/w/target/debug/build/foo/0123abcd/fingerprint/run-build-script-build-script-build.json");
        assert_eq!(fingerprint_identity(path), None);
    }
}

#[cfg(test)]
mod cache_format_tests {
    use super::*;

    #[test]
    fn unreadable_cache_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        // What an older, binary-format cache looks like to a JSON reader.
        fs::write(
            PersistentCache::get_cache_file_path(dir.path()),
            [0u8, 1, 0, 0, 0, 0, 0, 0, 0, 255, 254],
        )
        .unwrap();
        let cache = PersistentCache::load_from_file(dir.path()).unwrap();
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn cache_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = PersistentCache::new();
        let fingerprint = Fingerprint {
            libname: "serde".to_owned(),
            version: Some("1.0.0".to_owned()),
            rlib: PathBuf::from("/t/deps/libserde-abc.rlib"),
            mtime: SystemTime::now(),
        };
        cache.insert("k".to_owned(), vec![fingerprint], 42);
        cache.save_to_file(dir.path()).unwrap();

        let loaded = PersistentCache::load_from_file(dir.path()).unwrap();
        let entry = &loaded.entries["k"];
        assert_eq!(entry.cache_key_hash, 42);
        assert_eq!(entry.fingerprints[0].libname, "serde");
        assert_eq!(
            entry.fingerprints[0].rlib,
            Path::new("/t/deps/libserde-abc.rlib")
        );
    }
}
