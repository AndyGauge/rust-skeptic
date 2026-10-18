use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::rt::rustc_command;

/// Compiled snippets, kept in `<target>/<profile>/.skeptic-bin/` so that
/// running the tests again only recompiles what changed.
///
/// Only the executable is kept, never its output: a snippet may read the
/// clock, the network or the disk, and has to run every time. An entry is
/// named after everything the compilation depended on: the snippet, the
/// compiler, the edition, the target, and the size and modification time of
/// each library it is linked with. Anything that changes any of them simply
/// misses.
///
/// Opt in with the `binary-cache` feature (the entries add up to hundreds of
/// megabytes in a large project). Then `SKEPTIC_NO_BINARY_CACHE` turns it off
/// for a run, and `SKEPTIC_BINARY_CACHE_MB` (default 1024) bounds the space
/// used; the least recently used entries go first.
pub(crate) struct BinaryCache {
    dir: PathBuf,
    /// Everything that is the same for every snippet.
    stamp: u64,
}

impl BinaryCache {
    pub(crate) fn open(
        target_dir: &Path,
        edition: Option<&str>,
        externs: &[(String, PathBuf)],
        search_dirs: &[PathBuf],
    ) -> Option<BinaryCache> {
        use std::hash::{Hash, Hasher};

        if !cfg!(feature = "binary-cache") || env::var_os("SKEPTIC_NO_BINARY_CACHE").is_some() {
            return None;
        }
        let version = Command::new(rustc_command()).arg("-vV").output().ok()?;
        if !version.status.success() {
            return None;
        }

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        version.stdout.hash(&mut hasher);
        edition.hash(&mut hasher);
        search_dirs.hash(&mut hasher);
        for (name, rlib) in externs {
            // The sibling .rmeta is passed along too, but it is rebuilt
            // together with the rlib.
            let meta = fs::metadata(rlib).ok()?;
            (name, rlib, meta.len(), meta.modified().ok()).hash(&mut hasher);
        }

        let dir = target_dir.join(".skeptic-bin");
        fs::create_dir_all(&dir).ok()?;
        let cache = BinaryCache {
            dir,
            stamp: hasher.finish(),
        };
        cache.prune();
        Some(cache)
    }

    /// 128 bits, so that two snippets never share an entry by accident.
    pub(crate) fn key(&self, target_triple: &str, test_text: &str) -> String {
        use std::hash::{Hash, Hasher};

        let hash = |salt: u8| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (salt, self.stamp, target_triple, test_text).hash(&mut hasher);
            hasher.finish()
        };
        format!("{:016x}{:016x}", hash(0), hash(1))
    }

    /// The cached executable, if there is one. It is run where it is: a
    /// fresh copy would be vetted by the system again (see `execute`).
    pub(crate) fn restore(&self, key: &str) -> Option<PathBuf> {
        let entry = self.dir.join(key);
        // Mark it as recently used.
        let file = fs::File::options().write(true).open(&entry).ok()?;
        let _ = file.set_modified(SystemTime::now());
        Some(entry)
    }

    /// Keep a freshly built executable, returning where it now is.
    pub(crate) fn store(&self, key: &str, binary: &Path) -> Option<PathBuf> {
        // Copy beside the entry, then rename, so a reader never sees half.
        let tmp = tempfile::NamedTempFile::new_in(&self.dir).ok()?;
        fs::copy(binary, tmp.path()).ok()?;
        let entry = self.dir.join(key);
        tmp.persist(&entry).ok()?;
        Some(entry)
    }

    fn prune(&self) {
        let limit = env::var("SKEPTIC_BINARY_CACHE_MB")
            .ok()
            .and_then(|mb| mb.parse::<u64>().ok())
            .unwrap_or(1024)
            * 1024
            * 1024;
        let mut entries: Vec<_> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        entries.sort();
        for (_, len, path) in entries {
            if total <= limit {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path, stamp: u64) -> BinaryCache {
        BinaryCache {
            dir: dir.to_path_buf(),
            stamp,
        }
    }

    #[test]
    fn key_depends_on_everything_it_was_built_from() {
        let dir = tempfile::tempdir().unwrap();
        let base = cache(dir.path(), 1).key("t", "fn main() {}");
        assert_eq!(base, cache(dir.path(), 1).key("t", "fn main() {}"));
        assert_ne!(base, cache(dir.path(), 2).key("t", "fn main() {}"));
        assert_ne!(base, cache(dir.path(), 1).key("u", "fn main() {}"));
        assert_ne!(base, cache(dir.path(), 1).key("t", "fn main() { }"));
    }

    #[test]
    fn stored_binary_is_restored_and_misses_are_misses() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = cache(cache_dir.path(), 1);
        let key = cache.key("t", "x");

        let built = work.path().join("out.exe");
        fs::write(&built, b"binary").unwrap();
        assert!(cache.restore(&key).is_none());

        let stored = cache.store(&key, &built).unwrap();
        let restored = cache.restore(&key).unwrap();
        assert_eq!(restored, stored);
        assert_eq!(fs::read(&restored).unwrap(), b"binary");
        assert!(cache.restore(&cache.key("t", "y")).is_none());
        // Nothing but entries is left behind.
        assert_eq!(fs::read_dir(cache_dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn prune_drops_the_least_recently_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        for path in [&old, &new] {
            fs::write(path, vec![0u8; 700 * 1024]).unwrap();
        }
        let now = SystemTime::now();
        let touch = |path: &Path, ago: u64| {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(ago))
                .unwrap();
        };
        touch(&old, 100);
        touch(&new, 10);

        // 1 MB holds only one of them. (Environment is process-wide, but no
        // other test reads this variable.)
        env::set_var("SKEPTIC_BINARY_CACHE_MB", "1");
        cache(dir.path(), 1).prune();
        env::remove_var("SKEPTIC_BINARY_CACHE_MB");
        assert!(!old.exists());
        assert!(new.exists());
    }
}
