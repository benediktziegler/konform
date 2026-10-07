//! Ruff-style binary violation cache.
//!
//! # Directory layout
//! ```text
//! {cache_dir}/
//! ├── .gitignore       — "*" so the cache is never accidentally committed
//! ├── CACHEDIR.TAG     — Cache Directory Tagging spec file
//! └── {VERSION}/       — one sub-directory per konform release
//!     └── {hash:016x}  — one binary file per (package_root × settings) pair
//! ```
//!
//! # Invalidation strategy
//! Files are re-checked whenever their `mtime` or `permissions` change.
//! This avoids reading the entire file on every run (the trade-off ruff makes).
//!
//! # Wire format
//! Each cache file is a [`bincode`]-encoded [`PackageCache`] struct.

use crate::config::Config;
use crate::rules::Rule;
use anyhow::Result;
use bincode::{Decode, Encode};
use seahash::SeaHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::{fs, io};

/// Cache sub-directory name equals the konform version string, so upgrading
/// automatically uses a fresh sub-directory without explicit migration.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entries not visited within this many days are evicted on [`Cache::persist`].
const EVICT_DAYS: u64 = 30;

// ---------------------------------------------------------------------------
// On-disk data structures
// ---------------------------------------------------------------------------

/// Root on-disk structure; one per *(package_root × settings)* combination.
#[derive(Encode, Decode)]
struct PackageCache {
    /// Canonicalised package root — sanity-checked on open.
    package_root: String,
    /// Absolute-path string → per-file cache entry.
    files: HashMap<String, FileCache>,
}

/// Per-file cache entry.
#[derive(Encode, Decode, Clone)]
struct FileCache {
    /// SeaHash of the file's `FileCacheKey` fields.
    key: u64,
    /// Milliseconds since Unix epoch; used for 30-day LRU eviction.
    last_seen: u64,
    /// Violations found on the last check.  An empty `Vec` means "checked
    /// and clean".  Absence from the map means "not yet cached".
    violations: Vec<CachedViolation>,
}

/// Slim violation record stored in the cache.
#[derive(Encode, Decode, Clone)]
pub struct CachedViolation {
    pub rule: String,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub message: String,
    pub help: String,
    pub level: String,
    pub fixable: bool,
}

impl CachedViolation {
    /// Reconstruct the `serde_json::Value` blob consumed by the output pipeline.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "rule":    self.rule,
            "line":    self.line,
            "message": self.message,
            "level":   self.level,
            "help":    if self.help.is_empty() { serde_json::Value::Null }
                       else { serde_json::Value::String(self.help.clone()) },
            "range": {
                "start_line":      self.line,
                "start_character": self.col,
                "end_line":        self.end_line,
                "end_character":   0u32,
            },
            "fixable": self.fixable,
        })
    }

    /// Build from the `serde_json::Value` produced by `Violation::to_json`.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            rule: v["rule"].as_str()?.to_owned(),
            line: v["line"].as_u64()? as u32,
            col: v["range"]["start_character"].as_u64().unwrap_or(1) as u32,
            end_line: v["range"]["end_line"].as_u64().unwrap_or(1) as u32,
            message: v["message"].as_str().unwrap_or("").to_owned(),
            help: v["help"].as_str().unwrap_or("").to_owned(),
            level: v["level"].as_str().unwrap_or("error").to_owned(),
            fixable: v["fixable"].as_bool().unwrap_or(false),
        })
    }
}

// ---------------------------------------------------------------------------
// File cache key — mtime + permissions
// ---------------------------------------------------------------------------

/// Inputs that determine whether a cached result is still valid for a file.
///
/// Using mtime + permissions avoids reading the entire file on every run.
/// The stored value is the SeaHash of these three fields.
pub struct FileCacheKey {
    mtime_secs: i64,
    mtime_nanos: u32,
    permissions: u32,
}

impl FileCacheKey {
    /// Read the key from `path` metadata.  Returns `None` on I/O error.
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        let permissions = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode()
            }
            #[cfg(not(unix))]
            {
                u32::from(meta.permissions().readonly())
            }
        };
        Some(Self {
            mtime_secs: mtime.seconds(),
            mtime_nanos: mtime.nanoseconds(),
            permissions,
        })
    }

    /// SeaHash of the three key fields.
    pub fn hash(&self) -> u64 {
        let mut h = SeaHasher::new();
        self.mtime_secs.hash(&mut h);
        self.mtime_nanos.hash(&mut h);
        self.permissions.hash(&mut h);
        h.finish()
    }
}

// ---------------------------------------------------------------------------
// Settings hash — determines the cache file name
// ---------------------------------------------------------------------------

/// Hash of everything that changes lint results besides the file contents
/// and the select/ignore/level settings: rule config tables, per-file
/// ignores, noqa aliases, and each rule's own [`Rule::fingerprint`] (e.g. the
/// content of loaded pattern files).
pub fn rules_fingerprint(config: &Config, rules: &[Box<dyn Rule>]) -> u64 {
    let mut h = SeaHasher::new();

    let mut tables: Vec<_> = config.lint.rules.iter().collect();
    tables.sort_by_key(|(name, _)| name.as_str());
    for (name, value) in tables {
        name.hash(&mut h);
        hash_toml(value, &mut h);
    }

    let mut ignores: Vec<_> = config.lint.per_file_ignores.iter().collect();
    ignores.sort_by_key(|(glob, _)| glob.as_str());
    for (glob, codes) in ignores {
        glob.hash(&mut h);
        let mut codes = codes.clone();
        codes.sort();
        codes.hash(&mut h);
    }

    let mut aliases: Vec<_> = config.lint.noqa_aliases.iter().collect();
    aliases.sort();
    aliases.hash(&mut h);

    for rule in rules {
        rule.code().hash(&mut h);
        rule.fingerprint(config.rule_config(rule.config_name()))
            .hash(&mut h);
    }
    h.finish()
}

/// Feed `value` into `h` with table keys in sorted order, so the result does
/// not depend on map iteration order.
fn hash_toml(value: &toml::Value, h: &mut SeaHasher) {
    use toml::Value;
    match value {
        Value::String(s) => (0u8, s).hash(h),
        Value::Integer(i) => (1u8, i).hash(h),
        Value::Float(f) => (2u8, f.to_bits()).hash(h),
        Value::Boolean(b) => (3u8, b).hash(h),
        Value::Datetime(d) => (4u8, d.to_string()).hash(h),
        Value::Array(a) => {
            5u8.hash(h);
            a.len().hash(h);
            for v in a {
                hash_toml(v, h);
            }
        }
        Value::Table(t) => {
            6u8.hash(h);
            let mut entries: Vec<_> = t.iter().collect();
            entries.sort_by_key(|(k, _)| k.as_str());
            for (k, v) in entries {
                k.hash(h);
                hash_toml(v, h);
            }
        }
    }
}

fn settings_hash(
    package_root: &Path,
    select: &[String],
    ignore: &[String],
    level: &str,
    env_fingerprint: u64,
    rules_fingerprint: u64,
) -> u64 {
    let mut h = SeaHasher::new();
    for component in package_root.components() {
        format!("{component:?}").hash(&mut h);
    }
    let mut sel = select.to_vec();
    sel.sort();
    let mut ign = ignore.to_vec();
    ign.sort();
    sel.join(",").hash(&mut h);
    ign.join(",").hash(&mut h);
    level.hash(&mut h);
    // Fold in the Python environment fingerprint (see
    // `ModuleProbe::env_fingerprint`) so that installing/upgrading/removing
    // a package routes checks to a fresh cache file instead of silently
    // reusing module-existence results (e.g. KIS001) computed against a
    // now-stale `sys.path`. The old file is simply orphaned and swept up by
    // the existing 30-day eviction/pruning of unused cache directories.
    env_fingerprint.hash(&mut h);
    // Rule config and external rule files (see `rules_fingerprint`).
    rules_fingerprint.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// init() — called once before the first cache access
// ---------------------------------------------------------------------------

/// Create the version sub-directory, `.gitignore`, and `CACHEDIR.TAG`.
///
/// Safe to call on every run — existing files are left untouched.
pub fn init(cache_root: &Path) -> Result<()> {
    fs::create_dir_all(cache_root.join(VERSION))?;

    // .gitignore — skip if already present
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(cache_root.join(".gitignore"))
    {
        Ok(mut f) => write!(f, "# Automatically created by konform.\n*\n")?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }

    // Standard cache-directory tag (backup / sync tools can skip this dir)
    cachedir::ensure_tag(cache_root)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Cache struct
// ---------------------------------------------------------------------------

/// Ruff-style binary violation cache.
pub struct Cache {
    /// Path to the bincode file for this (package_root × settings) pair.
    path: PathBuf,
    no_cache: bool,
    package: PackageCache,
    /// Pending updates applied to disk in [`Cache::persist`].
    pending: Vec<(String, u64, Vec<CachedViolation>)>,
}

impl Cache {
    /// Open (or create fresh) a cache for the given package root + settings.
    ///
    /// `env_fingerprint` should be [`crate::module_probe::ModuleProbe::env_fingerprint`]
    /// so that changes to the Python environment (installed packages) route
    /// checks to a fresh cache file rather than replaying stale
    /// module-existence results. Pass `0` when no probe-dependent rule is in
    /// use (e.g. in tests).
    ///
    /// `rules_fingerprint` should be [`rules_fingerprint`] so that editing a
    /// rule's config table or pattern file starts from a fresh cache file.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        package_root: PathBuf,
        cache_root: &Path,
        no_cache: bool,
        level: &str,
        select: &[String],
        ignore: &[String],
        env_fingerprint: u64,
        rules_fingerprint: u64,
    ) -> Self {
        let hash = settings_hash(
            &package_root,
            select,
            ignore,
            level,
            env_fingerprint,
            rules_fingerprint,
        );
        let path = cache_root.join(VERSION).join(format!("{hash:016x}"));
        let root_str = package_root.to_string_lossy().into_owned();

        if no_cache {
            return Self::empty(path, root_str, no_cache);
        }

        match fs::File::open(&path) {
            Ok(f) => {
                match bincode::decode_from_reader::<PackageCache, _, _>(
                    BufReader::new(f),
                    bincode::config::standard(),
                ) {
                    Ok(mut pkg) => {
                        if pkg.package_root != root_str {
                            // Hash collision (astronomically rare) — start fresh.
                            pkg.files.clear();
                        }
                        Self {
                            path,
                            no_cache,
                            package: pkg,
                            pending: vec![],
                        }
                    }
                    Err(_) => Self::empty(path, root_str, no_cache), // corrupt
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::empty(path, root_str, no_cache),
            Err(_) => Self::empty(path, root_str, no_cache),
        }
    }

    fn empty(path: PathBuf, root_str: String, no_cache: bool) -> Self {
        Self {
            path,
            no_cache,
            package: PackageCache {
                package_root: root_str,
                files: HashMap::new(),
            },
            pending: vec![],
        }
    }

    // ── Public API ────────────────────────────────────────────────────────

    /// Look up a cached result by absolute path + mtime key.
    ///
    /// Returns `None` on a miss (unknown file or stale mtime).
    pub fn get(&self, abs_path: &Path, key: &FileCacheKey) -> Option<Vec<serde_json::Value>> {
        if self.no_cache {
            return None;
        }
        let k = Self::path_key(abs_path);
        let entry = self.package.files.get(&k)?;
        if entry.key != key.hash() {
            return None; // mtime / permissions changed
        }
        Some(entry.violations.iter().map(|v| v.to_json()).collect())
    }

    /// Stage a linted result.  Applied to disk in [`Cache::persist`].
    pub fn set_linted(
        &mut self,
        abs_path: &Path,
        key: &FileCacheKey,
        json_viols: &[serde_json::Value],
    ) {
        let violations: Vec<CachedViolation> = json_viols
            .iter()
            .filter_map(CachedViolation::from_json)
            .collect();
        self.pending
            .push((Self::path_key(abs_path), key.hash(), violations));
    }

    /// Remove `abs_path` from the cache (called after an in-place fix).
    pub fn invalidate(&mut self, abs_path: &Path) {
        let k = Self::path_key(abs_path);
        self.package.files.remove(&k);
        self.pending.retain(|(r, _, _)| r != &k);
    }

    /// Merge pending entries, evict stale ones, and write atomically.
    pub fn persist(&mut self) -> Result<()> {
        if self.no_cache {
            return Ok(());
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for (k, key_hash, violations) in std::mem::take(&mut self.pending) {
            self.package.files.insert(
                k,
                FileCache {
                    key: key_hash,
                    last_seen: now_ms,
                    violations,
                },
            );
        }

        // Evict entries not seen in EVICT_DAYS.
        let cutoff = now_ms.saturating_sub(EVICT_DAYS * 24 * 3600 * 1000);
        self.package.files.retain(|_, e| e.last_seen >= cutoff);

        let bytes = bincode::encode_to_vec(&self.package, bincode::config::standard())?;

        // Atomic write: encode to .tmp, then rename.
        let tmp = self.path.with_extension("tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        drop(f);
        fs::rename(&tmp, &self.path)?;

        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn path_key(abs_path: &Path) -> String {
        abs_path.to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_cache(tmp: &Path, select: &[&str], ignore: &[&str]) -> Cache {
        let sel: Vec<String> = select.iter().map(|s| s.to_string()).collect();
        let ign: Vec<String> = ignore.iter().map(|s| s.to_string()).collect();
        let _ = init(tmp);
        Cache::open(tmp.to_path_buf(), tmp, false, "error", &sel, &ign, 0, 0)
    }

    // ── Environment fingerprint ───────────────────────────────────────────

    #[test]
    fn env_fingerprint_change_routes_to_a_different_cache_file() {
        // Simulate a `pip install` / `uv sync` that changes the resolvable
        // module set without touching any linted source file: same package
        // root/select/ignore/level, different `env_fingerprint`.
        let tmp = tempfile::tempdir().unwrap();
        let sel: Vec<String> = vec![];
        let ign: Vec<String> = vec![];

        let hash_before = settings_hash(tmp.path(), &sel, &ign, "error", 111, 0);
        let hash_after = settings_hash(tmp.path(), &sel, &ign, "error", 222, 0);

        assert_ne!(
            hash_before, hash_after,
            "changing the env fingerprint must select a different cache file, \
             otherwise stale module-existence results (e.g. KIS001) survive \
             a dependency change"
        );
    }

    #[test]
    fn env_fingerprint_stable_for_unchanged_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let sel: Vec<String> = vec![];
        let ign: Vec<String> = vec![];

        let hash_a = settings_hash(tmp.path(), &sel, &ign, "error", 42, 0);
        let hash_b = settings_hash(tmp.path(), &sel, &ign, "error", 42, 0);

        assert_eq!(hash_a, hash_b, "identical inputs must hash identically");
    }

    #[test]
    fn cache_miss_after_env_fingerprint_change() {
        // End-to-end: a violation cached under one env fingerprint must not
        // be visible when the cache is reopened with a different one, even
        // though the linted file itself never changed.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("e.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&file).unwrap();

        let _ = init(tmp.path());
        let mut cache = Cache::open(
            tmp.path().to_path_buf(),
            tmp.path(),
            false,
            "error",
            &[],
            &[],
            111,
            0,
        );
        cache.set_linted(&file, &key, &[]);
        cache.persist().unwrap();
        assert!(
            cache.get(&file, &key).is_some(),
            "sanity: cache hit before env change"
        );

        let cache2 = Cache::open(
            tmp.path().to_path_buf(),
            tmp.path(),
            false,
            "error",
            &[],
            &[],
            222,
            0,
        );
        assert!(
            cache2.get(&file, &key).is_none(),
            "a changed env fingerprint must not reuse the previous cache file"
        );
    }

    // ── FileCacheKey ──────────────────────────────────────────────────────

    #[test]
    fn file_cache_key_from_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&file);
        assert!(key.is_some(), "key must be readable for an existing file");
    }

    #[test]
    fn file_cache_key_missing_path_returns_none() {
        let key = FileCacheKey::from_path(Path::new("/nonexistent/path/z.py"));
        assert!(key.is_none());
    }

    #[test]
    fn file_cache_key_changes_after_write() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("b.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let h1 = FileCacheKey::from_path(&file).unwrap().hash();
        // Small sleep so mtime advances on filesystems with 1s granularity.
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Touch the file with a different mtime via filetime.
        let new_mtime = filetime::FileTime::from_unix_time(9_999_999, 0);
        filetime::set_file_mtime(&file, new_mtime).unwrap();
        let h2 = FileCacheKey::from_path(&file).unwrap().hash();
        assert_ne!(h1, h2, "hash must change when mtime changes");
    }

    // ── Cache hit / miss ──────────────────────────────────────────────────

    #[test]
    fn cache_miss_on_unknown_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("c.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let cache = open_cache(tmp.path(), &[], &[]);
        let key = FileCacheKey::from_path(&file).unwrap();
        assert!(cache.get(&file, &key).is_none());
    }

    #[test]
    fn cache_hit_after_set_and_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("d.py");
        std::fs::write(&file, "x = 1\n").unwrap();

        let viols = vec![
            serde_json::json!({"rule":"KIS001","line":1,"message":"KIS001: m",
            "level":"error","help":null,"range":{"start_line":1,"start_character":1,
            "end_line":1,"end_character":0},"fixable":false}),
        ];

        {
            let mut c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            c.set_linted(&file, &key, &viols);
            c.persist().unwrap();
        }

        let c2 = open_cache(tmp.path(), &[], &[]);
        let key = FileCacheKey::from_path(&file).unwrap();
        let hit = c2.get(&file, &key);
        assert!(hit.is_some(), "should be a cache hit after persist");
        assert_eq!(hit.unwrap().len(), 1);
    }

    #[test]
    fn cache_miss_after_mtime_change() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("e.py");
        std::fs::write(&file, "x = 1\n").unwrap();

        let viols = vec![
            serde_json::json!({"rule":"KIS001","line":1,"message":"KIS001: m",
            "level":"error","help":null,"range":{"start_line":1,"start_character":1,
            "end_line":1,"end_character":0},"fixable":false}),
        ];

        {
            let mut c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            c.set_linted(&file, &key, &viols);
            c.persist().unwrap();
        }

        // Change the file's mtime.
        let new_mtime = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(&file, new_mtime).unwrap();

        let c2 = open_cache(tmp.path(), &[], &[]);
        let key = FileCacheKey::from_path(&file).unwrap();
        assert!(c2.get(&file, &key).is_none(), "stale mtime must be a miss");
    }

    #[test]
    fn cache_miss_on_settings_key_change() {
        // Regression: changing --ignore must not serve stale results.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.py");
        std::fs::write(&file, "x = 1\n").unwrap();

        let viols = vec![
            serde_json::json!({"rule":"KIS001","line":1,"message":"KIS001: m",
            "level":"error","help":null,"range":{"start_line":1,"start_character":1,
            "end_line":1,"end_character":0},"fixable":false}),
        ];

        {
            let mut c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            c.set_linted(&file, &key, &viols);
            c.persist().unwrap();
        }

        // Different settings → different cache file → miss.
        let c2 = open_cache(tmp.path(), &[], &["KIS"]);
        let key = FileCacheKey::from_path(&file).unwrap();
        assert!(
            c2.get(&file, &key).is_none(),
            "different ignore list must be a miss"
        );
    }

    // ── Directory structure ───────────────────────────────────────────────

    #[test]
    fn init_creates_version_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path()).unwrap();
        assert!(
            tmp.path().join(VERSION).is_dir(),
            "version subdir must exist"
        );
    }

    #[test]
    fn init_creates_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path()).unwrap();
        let gi = tmp.path().join(".gitignore");
        assert!(gi.is_file(), ".gitignore must be created");
        assert!(std::fs::read_to_string(gi).unwrap().contains('*'));
    }

    #[test]
    fn init_creates_cachedir_tag() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path()).unwrap();
        assert!(
            tmp.path().join("CACHEDIR.TAG").is_file(),
            "CACHEDIR.TAG must be created"
        );
    }

    // ── LRU eviction ─────────────────────────────────────────────────────

    #[test]
    fn persist_evicts_entries_older_than_30_days() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("old.py");
        std::fs::write(&file, "x = 1\n").unwrap();

        // Manually insert an entry with a very old last_seen timestamp.
        let old_ms: u64 = 0; // Unix epoch = definitely > 30 days old
        {
            let _ = init(tmp.path());
            let sel: Vec<String> = vec![];
            let ign: Vec<String> = vec![];
            let hash = settings_hash(tmp.path(), &sel, &ign, "error", 0, 0);
            let path = tmp.path().join(VERSION).join(format!("{hash:016x}"));
            let mut pkg = PackageCache {
                package_root: tmp.path().to_string_lossy().into_owned(),
                files: HashMap::new(),
            };
            pkg.files.insert(
                file.to_string_lossy().into_owned(),
                FileCache {
                    key: 0,
                    last_seen: old_ms,
                    violations: vec![],
                },
            );
            let bytes = bincode::encode_to_vec(&pkg, bincode::config::standard()).unwrap();
            std::fs::write(&path, bytes).unwrap();
        }

        // Open and persist — the old entry should be evicted.
        let mut c = open_cache(tmp.path(), &[], &[]);
        c.persist().unwrap();

        // Re-open and check the entry is gone.
        let c2 = open_cache(tmp.path(), &[], &[]);
        let key = FileCacheKey::from_path(&file).unwrap();
        assert!(
            c2.get(&file, &key).is_none(),
            "old entry should have been evicted"
        );
    }
}

#[cfg(test)]
mod rules_fingerprint_tests {
    use super::*;
    use crate::module_probe::ModuleProbe;
    use crate::rules::all_rules;
    use std::sync::Arc;

    fn fp(config: &Config) -> u64 {
        let rules = all_rules(Arc::new(ModuleProbe::default()), config.config_dir.clone());
        rules_fingerprint(config, &rules)
    }

    fn table(src: &str) -> toml::Value {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn stable_for_identical_config() {
        assert_eq!(fp(&Config::default()), fp(&Config::default()));
    }

    #[test]
    fn rule_table_edit_changes_fingerprint() {
        let mut a = Config::default();
        a.lint
            .rules
            .insert("module-only-imports".into(), table("level = \"error\""));
        let mut b = a.clone();
        b.lint
            .rules
            .insert("module-only-imports".into(), table("level = \"warning\""));
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn table_key_order_does_not_change_fingerprint() {
        let mut a = Config::default();
        a.lint.rules.insert("t".into(), table("a = 1\nb = 2"));
        let mut b = Config::default();
        b.lint.rules.insert("t".into(), table("b = 2\na = 1"));
        assert_eq!(fp(&a), fp(&b));
    }

    #[test]
    fn per_file_ignores_edit_changes_fingerprint() {
        let a = Config::default();
        let mut b = Config::default();
        b.lint
            .per_file_ignores
            .insert("tests/**".into(), vec!["KIS001".into()]);
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn noqa_aliases_edit_changes_fingerprint() {
        let a = Config::default();
        let mut b = Config::default();
        b.lint.noqa_aliases.insert("IS001".into(), "KIS001".into());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn pattern_file_edit_changes_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("konform_patterns.toml");
        let config = Config {
            config_dir: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let write = |msg: &str| {
            std::fs::write(
                &file,
                format!("[[rules]]\nid = \"KPT050\"\nmessage = \"{msg}\"\npattern = 'x'\n"),
            )
            .unwrap();
        };
        write("one");
        let before = fp(&config);
        assert_eq!(before, fp(&config), "unchanged file => unchanged hash");
        write("two");
        assert_ne!(before, fp(&config));
    }
}
