//! Ruff-style binary violation cache.
//!
//! # Directory layout
//! ```text
//! {cache_dir}/
//! ├── .gitignore:        "*" so the cache is never accidentally committed
//! ├── CACHEDIR.TAG:      Cache Directory Tagging spec file
//! └── {VERSION}/:        one sub-directory per konform release
//!     └── {hash:016x}:   one binary file per (package_root × settings) pair
//! ```
//!
//! # Invalidation strategy
//! Files are re-checked whenever their `mtime` or `permissions` change.
//! This avoids reading the entire file on every run (the trade-off ruff makes).
//!
//! # Wire format
//! Each cache file starts with a 16-byte header (`MAGIC`, then the
//! little-endian `FORMAT_VERSION`, then zero padding) followed by an
//! [`rkyv`]-archived [`PackageCache`]. The header keeps the payload 16-byte
//! aligned and lets files from any other format (such as the earlier bincode
//! one) be recognised and silently discarded; they are rewritten by the next
//! [`Cache::persist`]. The payload is validated (bytecheck) before use.

use crate::config::Config;
use crate::rules::Rule;
use crate::types::{Level, Violation};
use anyhow::Result;
use owo_colors::OwoColorize;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Deserialize, Serialize};
use seahash::SeaHasher;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

/// Cache sub-directory name equals the konform version string, so upgrading
/// automatically uses a fresh sub-directory without explicit migration.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entries not visited within this many days are evicted on [`Cache::persist`].
const EVICT_DAYS: u64 = 30;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Per-process counter making temp file names unique across calls.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Upper bound on the size of a cache file. Larger files are ignored on
/// read (so a bogus file never causes a huge allocation), and
/// [`Cache::persist`] trims the cache to stay under it (otherwise the written
/// file could never be read back).
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Magic bytes identifying a konform cache file.
const MAGIC: &[u8; 8] = b"KONFCACH";

/// Version of the on-disk layout. Bump when [`PackageCache`] changes shape.
const FORMAT_VERSION: u32 = 2;

/// Header length: magic + format version + padding to rkyv's 16-byte alignment.
const HEADER_LEN: usize = 16;

/// A hit only rewrites `last_seen` (and so the cache file) when the stored
/// value is older than this; otherwise a fully cached run writes nothing.
/// It must stay far below [`EVICT_DAYS`] so in-use entries never expire.
const REFRESH_AFTER_MS: u64 = 24 * 3600 * 1000;

/// Temp files older than this are considered orphaned (crashed run) and swept.
const STALE_TMP_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Ensures the "cache too large" notice is printed at most once per process.
static WARNED_OVERSIZE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn header() -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    h
}

/// Serialise `pkg` into header + rkyv payload.
fn encode(pkg: &PackageCache) -> Result<AlignedVec<16>> {
    let payload = rkyv::to_bytes::<rkyv::rancor::Error>(pkg)?;
    let mut out: AlignedVec<16> = AlignedVec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Result of trying to load a cache file.
enum ReadOutcome {
    /// A valid cache in the current format.
    Loaded(PackageCache),
    /// Nothing usable, and nothing worth reporting: the file is missing, or
    /// is an old-format or foreign file (expected after an upgrade).
    Ignored,
    /// The file is unusable for an abnormal reason (current header but a bad
    /// payload, an I/O error other than "not found", or too large). Carries
    /// a human-readable reason for the warning.
    Corrupt(String),
}

/// Read and validate a cache file; see [`ReadOutcome`] for the cases.
fn read_package(path: &Path, max_bytes: usize) -> ReadOutcome {
    use ReadOutcome::{Corrupt, Ignored, Loaded};
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ignored,
        Err(e) => return Corrupt(format!("could not read: {e}")),
    };
    let len = match f.metadata().map(|m| usize::try_from(m.len())) {
        Ok(Ok(len)) => len,
        Ok(Err(_)) => return Corrupt("file is too large".into()),
        Err(e) => return Corrupt(format!("could not read: {e}")),
    };
    if len > max_bytes {
        return Corrupt(format!("file exceeds the {max_bytes} byte limit"));
    }
    if len < HEADER_LEN {
        return Ignored; // too short to carry a header: not ours
    }
    // AlignedVec guarantees the alignment rkyv needs for access; `take`
    // bounds the read even if the file grows after the size check.
    let mut buf: AlignedVec<16> = AlignedVec::with_capacity(len);
    if let Err(e) = buf.extend_from_reader(&mut (&mut f).take(len as u64)) {
        return Corrupt(format!("could not read: {e}"));
    }
    if buf.len() < HEADER_LEN || buf[..HEADER_LEN] != header() {
        return Ignored; // old bincode file, foreign file, or other format version
    }
    let archived =
        match rkyv::access::<ArchivedPackageCache, rkyv::rancor::Error>(&buf[HEADER_LEN..]) {
            Ok(a) => a,
            Err(e) => return Corrupt(format!("invalid contents: {e}")),
        };
    match rkyv::deserialize::<PackageCache, rkyv::rancor::Error>(archived) {
        Ok(pkg) => Loaded(pkg),
        Err(e) => Corrupt(format!("invalid contents: {e}")),
    }
}

// ---------------------------------------------------------------------------
// On-disk data structures
// ---------------------------------------------------------------------------

/// Root on-disk structure; one per *(package_root × settings)* combination.
#[derive(Archive, Serialize, Deserialize)]
struct PackageCache {
    /// Canonicalised package root: sanity-checked on open.
    package_root: String,
    /// Absolute-path string → per-file cache entry.
    files: HashMap<String, FileCache>,
}

/// Per-file cache entry.
#[derive(Archive, Serialize, Deserialize, Clone)]
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
#[derive(Archive, Serialize, Deserialize, Clone)]
pub struct CachedViolation {
    pub rule: String,
    pub line: u32,
    /// 0-based, as in [`Violation::col`].
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub message: String,
    /// Empty when the violation has no help text.
    pub help: String,
    /// `true` for [`Level::Error`], `false` for [`Level::Warning`].
    pub error: bool,
    pub fixable: bool,
}

impl From<&Violation> for CachedViolation {
    fn from(v: &Violation) -> Self {
        Self {
            rule: v.rule.clone(),
            line: v.line as u32,
            col: v.col as u32,
            end_line: v.end_line as u32,
            end_col: v.end_col as u32,
            message: v.message.clone(),
            help: v.help.clone().unwrap_or_default(),
            error: v.level == Level::Error,
            fixable: v.fixable,
        }
    }
}

impl From<&CachedViolation> for Violation {
    fn from(v: &CachedViolation) -> Self {
        Self {
            rule: v.rule.clone(),
            line: v.line as usize,
            col: v.col as usize,
            end_line: v.end_line as usize,
            end_col: v.end_col as usize,
            message: v.message.clone(),
            help: (!v.help.is_empty()).then(|| v.help.clone()),
            level: if v.error {
                Level::Error
            } else {
                Level::Warning
            },
            fixable: v.fixable,
        }
    }
}

// ---------------------------------------------------------------------------
// File cache key: mtime + permissions
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
// Settings hash: determines the cache file name
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

    // `fixable` / `unfixable` decide the cached `fixable` flags.
    config.lint.fixable.hash(&mut h);
    config.lint.unfixable.hash(&mut h);

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
// init(): called once before the first cache access
// ---------------------------------------------------------------------------

/// The directory holding this konform version's cache files.
pub fn version_dir(cache_root: &Path) -> PathBuf {
    cache_root.join(VERSION)
}

/// Create the version sub-directory, `.gitignore`, and `CACHEDIR.TAG`.
///
/// Safe to call on every run: existing files are left untouched.
pub fn init(cache_root: &Path) -> Result<()> {
    fs::create_dir_all(version_dir(cache_root))?;

    // .gitignore: skip if already present
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

    sweep_stale_tmp(&cache_root.join(VERSION), STALE_TMP_AGE);
    prune_old_versions(cache_root, OLD_VERSION_AGE);

    Ok(())
}

/// Version directories of other konform releases untouched for this long are
/// removed by [`init`].
const OLD_VERSION_AGE: std::time::Duration = std::time::Duration::from_secs(EVICT_DAYS * 24 * 3600);

/// `true` for plain release version names such as `0.4.0`: two or more
/// non-empty, all-digit components separated by dots. Anything else
/// (pre-releases, dotfiles, files someone placed there) is never pruned.
fn is_version_dir_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Best-effort removal of sibling version directories (from other konform
/// releases) that have not been modified within `max_age`. Only real
/// directories (not symlinks) with a plain version name are touched.
fn prune_old_versions(cache_root: &Path, max_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == VERSION || !is_version_dir_name(name) {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Best-effort removal of orphaned `<hash>.<pid>.<n>.tmp` files (left behind
/// by a crashed run) older than `max_age`.
fn sweep_stale_tmp(dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".tmp")) else {
            continue;
        };
        // `<hash>.<pid>.<n>`
        let parts: Vec<&str> = stem.split('.').collect();
        let is_ours = parts.len() == 3
            && parts[0].len() == 16
            && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
            && parts[1..]
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !is_ours {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Cache struct
// ---------------------------------------------------------------------------

/// Ruff-style binary violation cache.
pub struct Cache {
    /// Path to the cache file for this (package_root × settings) pair.
    path: PathBuf,
    no_cache: bool,
    package: PackageCache,
    /// Pending updates applied to disk in [`Cache::persist`].
    pending: Vec<(String, u64, Vec<CachedViolation>)>,
    /// Keys of entries hit via [`Cache::get`]; their `last_seen` is refreshed
    /// in [`Cache::persist`].
    touched: RefCell<HashSet<String>>,
    /// The in-memory cache differs from the file on disk (entry removed, or
    /// a corrupt file that should be replaced). Pending entries are tracked
    /// separately in `pending`.
    dirty: bool,
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
        Self::open_with_warn(
            package_root,
            cache_root,
            no_cache,
            level,
            select,
            ignore,
            env_fingerprint,
            rules_fingerprint,
            &mut |msg| eprintln!("{} {msg}", "warning:".yellow().bold()),
        )
    }

    /// [`Cache::open`] with an injectable warning sink. A warning is emitted
    /// (once) only for a cache file that exists but is abnormal: right
    /// header with an invalid payload, unreadable for a reason other than
    /// "not found", or over the size limit. Missing, old-format and foreign
    /// files stay silent. Either way the result is an empty cache.
    #[allow(clippy::too_many_arguments)]
    fn open_with_warn(
        package_root: PathBuf,
        cache_root: &Path,
        no_cache: bool,
        level: &str,
        select: &[String],
        ignore: &[String],
        env_fingerprint: u64,
        rules_fingerprint: u64,
        warn: &mut dyn FnMut(&str),
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

        match read_package(&path, MAX_CACHE_BYTES) {
            ReadOutcome::Loaded(mut pkg) => {
                if pkg.package_root != root_str {
                    // Hash collision (astronomically rare): start fresh.
                    pkg.files.clear();
                }
                Self {
                    path,
                    no_cache,
                    package: pkg,
                    pending: vec![],
                    touched: RefCell::new(HashSet::new()),
                    dirty: false,
                }
            }
            ReadOutcome::Ignored => Self::empty(path, root_str, no_cache),
            ReadOutcome::Corrupt(reason) => {
                warn(&format!(
                    "ignoring konform cache file {} ({reason}); it will be rebuilt",
                    path.display()
                ));
                let mut c = Self::empty(path, root_str, no_cache);
                // Replace the bad file on the next persist so the warning
                // does not repeat on every run.
                c.dirty = true;
                c
            }
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
            touched: RefCell::new(HashSet::new()),
            dirty: false,
        }
    }

    // ── Public API ────────────────────────────────────────────────────────

    /// Look up a cached result by absolute path + mtime key.
    ///
    /// Returns `None` on a miss (unknown file or stale mtime).
    pub fn get(&self, abs_path: &Path, key: &FileCacheKey) -> Option<Vec<Violation>> {
        if self.no_cache {
            return None;
        }
        let k = Self::path_key(abs_path);
        let entry = self.package.files.get(&k)?;
        if entry.key != key.hash() {
            return None; // mtime / permissions changed
        }
        // Record the hit so persist() refreshes last_seen. `get` is only
        // called from the sequential cache-lookup loop (never from the
        // parallel lint workers), so interior mutability without a lock is
        // enough; this makes `Cache` `!Sync` on purpose.
        self.touched.borrow_mut().insert(k);
        Some(entry.violations.iter().map(Violation::from).collect())
    }

    /// Stage a linted result.  Applied to disk in [`Cache::persist`].
    pub fn set_linted(&mut self, abs_path: &Path, key: &FileCacheKey, violations: &[Violation]) {
        let violations: Vec<CachedViolation> =
            violations.iter().map(CachedViolation::from).collect();
        self.pending
            .push((Self::path_key(abs_path), key.hash(), violations));
    }

    /// Remove `abs_path` from the cache (called after an in-place fix).
    pub fn invalidate(&mut self, abs_path: &Path) {
        let k = Self::path_key(abs_path);
        if self.package.files.remove(&k).is_some() {
            self.dirty = true;
        }
        self.pending.retain(|(r, _, _)| r != &k);
    }

    /// Merge pending entries, evict stale ones, and write atomically.
    ///
    /// Returns `Ok(true)` if the file was written and `Ok(false)` if there was
    /// nothing to write: no new, removed or replaced entries, and no hit
    /// whose `last_seen` is older than [`REFRESH_AFTER_MS`]. Eviction only
    /// happens as part of a write.
    pub fn persist(&mut self) -> Result<bool> {
        self.persist_at(now_ms(), MAX_CACHE_BYTES)
    }

    fn persist_at(&mut self, now_ms: u64, max_bytes: usize) -> Result<bool> {
        if self.no_cache {
            return Ok(false);
        }

        let touched = std::mem::take(self.touched.get_mut());
        let stale_hit = touched.iter().any(|k| {
            self.package
                .files
                .get(k)
                .is_some_and(|e| now_ms.saturating_sub(e.last_seen) > REFRESH_AFTER_MS)
        });
        if !(self.dirty || !self.pending.is_empty() || stale_hit) {
            return Ok(false);
        }

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

        // Cache hits count as "seen": refresh them before eviction.
        for k in touched {
            if let Some(e) = self.package.files.get_mut(&k) {
                e.last_seen = now_ms;
            }
        }

        // Evict entries not seen in EVICT_DAYS.
        let cutoff = now_ms.saturating_sub(EVICT_DAYS * 24 * 3600 * 1000);
        self.package.files.retain(|_, e| e.last_seen >= cutoff);

        let mut bytes = encode(&self.package)?;

        // A file above the decode limit would be rejected on the next run and
        // leave the cache permanently cold, so drop the oldest half of the
        // entries until it fits.
        while bytes.len() > max_bytes && !self.package.files.is_empty() {
            if !WARNED_OVERSIZE.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "warning: konform cache exceeds {max_bytes} bytes; dropping oldest entries"
                );
            }
            let mut by_age: Vec<(u64, String)> = self
                .package
                .files
                .iter()
                .map(|(k, e)| (e.last_seen, k.clone()))
                .collect();
            by_age.sort();
            let drop_n = by_age.len().div_ceil(2);
            for (_, k) in by_age.into_iter().take(drop_n) {
                self.package.files.remove(&k);
            }
            bytes = encode(&self.package)?;
        }

        // Atomic write: encode to a unique temp file, then rename.
        let tmp = self.temp_path();
        let result = (|| -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            drop(f);
            fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp); // best effort
            return Err(e.into());
        }

        self.dirty = false;
        Ok(true)
    }

    /// Temp file name unique per process and per call, so concurrent
    /// persists never clobber each other's partial writes.
    fn temp_path(&self) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.path
            .with_extension(format!("{}.{n}.tmp", std::process::id()))
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

        let viols = vec![Violation::for_test("KIS001", 1, "KIS001: m")];

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
    fn cached_violations_round_trip_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("g.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let mut warning = Violation::for_test("KPT001", 4, "KPT001: w");
        warning.col = 2;
        warning.end_line = 5;
        warning.end_col = 7;
        warning.help = Some("h".to_owned());
        warning.level = Level::Warning;
        warning.fixable = true;
        let viols = vec![Violation::for_test("KIS001", 1, "KIS001: m"), warning];

        {
            let mut c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            c.set_linted(&file, &key, &viols);
            c.persist().unwrap();
        }

        let c2 = open_cache(tmp.path(), &[], &[]);
        let key = FileCacheKey::from_path(&file).unwrap();
        let hit = c2.get(&file, &key).unwrap();
        assert_eq!(format!("{hit:?}"), format!("{viols:?}"));
    }

    #[test]
    fn cache_miss_after_mtime_change() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("e.py");
        std::fs::write(&file, "x = 1\n").unwrap();

        let viols = vec![Violation::for_test("KIS001", 1, "KIS001: m")];

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

        let viols = vec![Violation::for_test("KIS001", 1, "KIS001: m")];

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

    // ── Old version directories ──────────────────────────────────────────

    #[test]
    fn init_prunes_only_old_plain_version_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
        let mk = |name: &str, aged: bool| {
            let d = tmp.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("abc"), b"x").unwrap();
            if aged {
                filetime::set_file_mtime(&d, filetime::FileTime::from_system_time(old)).unwrap();
            }
            d
        };
        let stale = mk("0.0.1", true);
        let recent = mk("0.0.2", false);
        let prerelease = mk("0.0.3-rc1", true);
        let unrelated = mk("notes", true);
        let dotted_junk = mk("1..2", true);
        let file = tmp.path().join("0.0.4");
        std::fs::write(&file, b"x").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old)).unwrap();
        #[cfg(unix)]
        let link = {
            let target = mk("target", false);
            let link = tmp.path().join("0.0.5");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            link
        };

        init(tmp.path()).unwrap();

        assert!(!stale.exists(), "old plain version dir is pruned");
        assert!(recent.exists());
        assert!(prerelease.exists());
        assert!(unrelated.exists());
        assert!(dotted_junk.exists());
        assert!(file.exists(), "plain files are never removed");
        #[cfg(unix)]
        assert!(link.exists(), "symlinks are never followed or removed");
        assert!(tmp.path().join(VERSION).is_dir());
    }

    #[test]
    fn init_never_prunes_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path()).unwrap();
        let cur = tmp.path().join(VERSION);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 24 * 3600);
        filetime::set_file_mtime(&cur, filetime::FileTime::from_system_time(old)).unwrap();
        init(tmp.path()).unwrap();
        assert!(cur.is_dir());
    }

    // ── LRU eviction ─────────────────────────────────────────────────────

    /// Path of the cache file `open_cache(tmp, &[], &[])` reads and writes.
    fn cache_file(tmp: &Path) -> PathBuf {
        let _ = init(tmp);
        let hash = settings_hash(tmp, &[], &[], "error", 0, 0);
        tmp.join(VERSION).join(format!("{hash:016x}"))
    }

    /// Persist one entry, then assert it is readable: proves the new format
    /// was written and round-trips.
    fn assert_next_persist_round_trips(tmp: &Path, file: &Path) {
        let key = FileCacheKey::from_path(file).unwrap();
        let mut c = open_cache(tmp, &[], &[]);
        assert!(c.get(file, &key).is_none());
        c.set_linted(file, &key, &[]);
        c.persist().unwrap();
        let bytes = std::fs::read(cache_file(tmp)).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        let c2 = open_cache(tmp, &[], &[]);
        assert!(c2.get(file, &key).is_some());
    }

    #[test]
    fn corrupt_data_yields_empty_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let path = cache_file(tmp.path());

        // Right header, garbage payload; truncated header; empty file.
        let mut garbage = header().to_vec();
        garbage.extend_from_slice(&[0xFF; 64]);
        let mut truncated = encode(&PackageCache {
            package_root: tmp.path().to_string_lossy().into_owned(),
            files: HashMap::new(),
        })
        .unwrap()
        .to_vec();
        truncated.truncate(HEADER_LEN + 3);
        for bytes in [garbage, truncated, MAGIC[..4].to_vec(), vec![]] {
            std::fs::write(&path, bytes).unwrap();
            let c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            assert!(c.get(&file, &key).is_none());
        }
        assert_next_persist_round_trips(tmp.path(), &file);
    }

    #[test]
    fn old_bincode_files_are_discarded_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let path = cache_file(tmp.path());

        // Old bincode layout: varint-length package_root, then a map length.
        // First a hostile length prefix (varint marker 0xFD, ~2^63 entries)...
        let mut huge = vec![1u8, b'x', 0xFD];
        huge.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        // ...then a small, well-formed legacy payload with one entry.
        let root = tmp.path().to_string_lossy().into_owned();
        let mut legacy = vec![root.len() as u8];
        legacy.extend_from_slice(root.as_bytes());
        legacy.push(1); // one file
        legacy.push(1); // path "f"
        legacy.push(b'f');
        legacy.extend_from_slice(&[7, 1, 0]); // key, last_seen, no violations

        for bytes in [huge, legacy] {
            std::fs::write(&path, bytes).unwrap();
            let c = open_cache(tmp.path(), &[], &[]);
            let key = FileCacheKey::from_path(&file).unwrap();
            assert!(c.get(&file, &key).is_none());
            assert!(c.package.files.is_empty());
            assert_next_persist_round_trips(tmp.path(), &file);
        }
    }

    #[test]
    fn other_format_version_is_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let path = cache_file(tmp.path());
        let mut bytes = encode(&PackageCache {
            package_root: tmp.path().to_string_lossy().into_owned(),
            files: HashMap::new(),
        })
        .unwrap()
        .to_vec();
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            read_package(&path, MAX_CACHE_BYTES),
            ReadOutcome::Ignored
        ));
        assert_next_persist_round_trips(tmp.path(), &file);
    }

    #[test]
    fn oversized_file_is_rejected_on_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = cache_file(tmp.path());
        let pkg = PackageCache {
            package_root: "r".into(),
            files: HashMap::new(),
        };
        let bytes = encode(&pkg).unwrap();
        std::fs::write(&path, &bytes[..]).unwrap();
        assert!(matches!(
            read_package(&path, bytes.len()),
            ReadOutcome::Loaded(_)
        ));
        assert!(matches!(
            read_package(&path, bytes.len() - 1),
            ReadOutcome::Corrupt(_)
        ));
        assert!(matches!(
            read_package(&tmp.path().join("missing"), 1000),
            ReadOutcome::Ignored
        ));
    }

    #[test]
    fn oversized_cache_is_trimmed_not_written_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = open_cache(tmp.path(), &[], &[]);
        let mut files = Vec::new();
        for i in 0..40 {
            let f = tmp.path().join(format!("f{i}.py"));
            std::fs::write(&f, "x = 1\n").unwrap();
            let key = FileCacheKey::from_path(&f).unwrap();
            c.set_linted(&f, &key, &[]);
            files.push((f, key));
        }
        c.persist_at(now_ms(), 300).unwrap();
        assert!(std::fs::metadata(&c.path).unwrap().len() <= 300);

        // Still readable, and some (but not all) entries survived.
        let c2 = open_cache(tmp.path(), &[], &[]);
        let hits = files.iter().filter(|(f, k)| c2.get(f, k).is_some()).count();
        assert!(hits > 0 && hits < 40, "hits = {hits}");

        // A limit too small for even an empty cache ends with an empty cache.
        c.dirty = true; // force a write: nothing is pending any more
        c.persist_at(now_ms(), 1).unwrap();
        let c3 = open_cache(tmp.path(), &[], &[]);
        assert!(files.iter().all(|(f, k)| c3.get(f, k).is_none()));
    }

    #[test]
    fn large_cache_under_limit_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = open_cache(tmp.path(), &[], &[]);
        let mut viol = Violation::for_test("KIS001", 3, &"m".repeat(200));
        viol.help = Some("h".repeat(200));
        let f = tmp.path().join("a.py");
        std::fs::write(&f, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&f).unwrap();
        let viols = vec![viol; 2000];
        for i in 0..8 {
            c.set_linted(&tmp.path().join(format!("p{i}.py")), &key, &viols);
        }
        c.persist().unwrap();
        let size = std::fs::metadata(&c.path).unwrap().len() as usize;
        assert!(size > 1_000_000 && size < MAX_CACHE_BYTES, "size = {size}");

        let c2 = open_cache(tmp.path(), &[], &[]);
        for i in 0..8 {
            let got = c2.get(&tmp.path().join(format!("p{i}.py")), &key).unwrap();
            assert_eq!(got.len(), 2000);
        }
    }

    #[test]
    fn invalidate_after_hit_does_not_resurrect_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.py");
        std::fs::write(&f, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&f).unwrap();
        let mut c = open_cache(tmp.path(), &[], &[]);
        c.set_linted(&f, &key, &[]);
        c.persist().unwrap();

        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(c.get(&f, &key).is_some());
        c.invalidate(&f);
        c.persist().unwrap();

        let c = open_cache(tmp.path(), &[], &[]);
        assert!(c.get(&f, &key).is_none());
    }

    #[test]
    fn init_sweeps_only_stale_cache_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = init(tmp.path());
        let dir = tmp.path().join(VERSION);
        let old = dir.join("0123456789abcdef.123.4.tmp");
        let fresh = dir.join("0123456789abcdef.123.5.tmp");
        let foreign = dir.join("notes.tmp");
        for p in [&old, &fresh, &foreign] {
            std::fs::write(p, "x").unwrap();
        }
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for p in [&old, &foreign] {
            filetime::set_file_mtime(p, filetime::FileTime::from_system_time(two_hours_ago))
                .unwrap();
        }
        init(tmp.path()).unwrap();
        assert!(!old.exists(), "stale temp file must be swept");
        assert!(fresh.exists(), "recent temp file may belong to a live run");
        assert!(foreign.exists(), "non-matching names are left alone");
    }

    #[test]
    fn temp_paths_are_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_cache(tmp.path(), &[], &[]);
        let a = c.temp_path();
        let b = c.temp_path();
        assert_ne!(a, b);
        assert_ne!(a, c.path);
        assert_eq!(a.parent(), c.path.parent());
    }

    #[test]
    fn persist_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = open_cache(tmp.path(), &[], &[]);
        c.persist().unwrap();
        let dir = c.path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn cache_hit_refreshes_last_seen_so_entry_survives_eviction() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("hit.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&file).unwrap();

        let _ = init(tmp.path());
        let hash = settings_hash(tmp.path(), &[], &[], "error", 0, 0);
        let path = tmp.path().join(VERSION).join(format!("{hash:016x}"));
        let mut pkg = PackageCache {
            package_root: tmp.path().to_string_lossy().into_owned(),
            files: HashMap::new(),
        };
        // Matching key but ancient last_seen (would be evicted without a hit).
        pkg.files.insert(
            file.to_string_lossy().into_owned(),
            FileCache {
                key: key.hash(),
                last_seen: 0,
                violations: vec![],
            },
        );
        std::fs::write(&path, encode(&pkg).unwrap()).unwrap();

        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(c.get(&file, &key).is_some());
        c.persist().unwrap();

        let c2 = open_cache(tmp.path(), &[], &[]);
        assert!(
            c2.get(&file, &key).is_some(),
            "hit entry should survive eviction"
        );
    }

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
            std::fs::write(&path, encode(&pkg).unwrap()).unwrap();
        }

        // Open and persist: the old entry should be evicted.
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

    // ── Write throttling and corruption warnings ─────────────────────────

    const DAY_MS: u64 = 24 * 3600 * 1000;

    /// Write a cache file holding one entry for `file` with the given `last_seen`.
    fn seed_entry(tmp: &Path, file: &Path, last_seen: u64) {
        let key = FileCacheKey::from_path(file).unwrap();
        let mut pkg = PackageCache {
            package_root: tmp.to_string_lossy().into_owned(),
            files: HashMap::new(),
        };
        pkg.files.insert(
            file.to_string_lossy().into_owned(),
            FileCache {
                key: key.hash(),
                last_seen,
                violations: vec![],
            },
        );
        std::fs::write(cache_file(tmp), encode(&pkg).unwrap()).unwrap();
    }

    fn py_file(tmp: &Path) -> (PathBuf, FileCacheKey) {
        let file = tmp.join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let key = FileCacheKey::from_path(&file).unwrap();
        (file, key)
    }

    /// Open `tmp`'s cache, collecting warnings instead of printing them.
    fn open_collecting(tmp: &Path) -> (Cache, Vec<String>) {
        let mut warnings = Vec::new();
        let c = Cache::open_with_warn(
            tmp.to_path_buf(),
            tmp,
            false,
            "error",
            &[],
            &[],
            0,
            0,
            &mut |m| warnings.push(m.to_string()),
        );
        (c, warnings)
    }

    #[test]
    fn pure_hit_run_does_not_rewrite_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, key) = py_file(tmp.path());
        let now = now_ms();
        seed_entry(tmp.path(), &file, now - 1000);
        let before = std::fs::read(cache_file(tmp.path())).unwrap();

        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(c.get(&file, &key).is_some());
        assert!(!c.persist_at(now, MAX_CACHE_BYTES).unwrap());
        assert_eq!(std::fs::read(cache_file(tmp.path())).unwrap(), before);
    }

    #[test]
    fn stale_last_seen_triggers_a_refresh_write() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, key) = py_file(tmp.path());
        let now = now_ms();
        seed_entry(tmp.path(), &file, now - REFRESH_AFTER_MS - 1000);

        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(c.get(&file, &key).is_some());
        assert!(c.persist_at(now, MAX_CACHE_BYTES).unwrap());
        assert_eq!(c.package.files.values().next().unwrap().last_seen, now);
        // The next hit-only run finds a fresh entry and writes nothing.
        let mut c2 = open_cache(tmp.path(), &[], &[]);
        assert!(c2.get(&file, &key).is_some());
        assert!(!c2.persist_at(now + 1000, MAX_CACHE_BYTES).unwrap());
    }

    #[test]
    fn untouched_stale_entry_does_not_trigger_a_write() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, _) = py_file(tmp.path());
        let now = now_ms();
        seed_entry(tmp.path(), &file, now - 10 * DAY_MS);
        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(!c.persist_at(now, MAX_CACHE_BYTES).unwrap());
    }

    #[test]
    fn added_or_invalidated_entry_triggers_a_write() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, key) = py_file(tmp.path());
        let now = now_ms();
        seed_entry(tmp.path(), &file, now);

        let mut c = open_cache(tmp.path(), &[], &[]);
        c.set_linted(&tmp.path().join("b.py"), &key, &[]);
        assert!(c.persist_at(now, MAX_CACHE_BYTES).unwrap());

        let mut c = open_cache(tmp.path(), &[], &[]);
        c.invalidate(&tmp.path().join("unknown.py")); // not cached: no change
        assert!(!c.persist_at(now, MAX_CACHE_BYTES).unwrap());
        c.invalidate(&file);
        assert!(c.persist_at(now, MAX_CACHE_BYTES).unwrap());
        assert!(c.get(&file, &key).is_none());
    }

    #[test]
    fn eviction_runs_when_a_write_happens() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, key) = py_file(tmp.path());
        let now = now_ms();
        seed_entry(tmp.path(), &file, now - 40 * DAY_MS);
        let mut c = open_cache(tmp.path(), &[], &[]);
        c.set_linted(&tmp.path().join("b.py"), &key, &[]);
        assert!(c.persist_at(now, MAX_CACHE_BYTES).unwrap());
        assert!(open_cache(tmp.path(), &[], &[]).get(&file, &key).is_none());
    }

    #[test]
    fn empty_cache_with_nothing_to_store_creates_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = open_cache(tmp.path(), &[], &[]);
        assert!(!c.persist().unwrap());
        assert!(!cache_file(tmp.path()).exists());

        // A missing file with something to store is written.
        let (file, key) = py_file(tmp.path());
        c.set_linted(&file, &key, &[]);
        assert!(c.persist().unwrap());
        assert!(cache_file(tmp.path()).exists());
    }

    #[test]
    fn no_cache_never_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Cache::open(
            tmp.path().to_path_buf(),
            tmp.path(),
            true,
            "error",
            &[],
            &[],
            0,
            0,
        );
        let (file, key) = py_file(tmp.path());
        c.set_linted(&file, &key, &[]);
        assert!(!c.persist().unwrap());
    }

    #[test]
    fn corrupt_payload_warns_once_and_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, key) = py_file(tmp.path());
        let path = cache_file(tmp.path());
        let mut garbage = header().to_vec();
        garbage.extend_from_slice(&[0xFF; 64]);
        std::fs::write(&path, garbage).unwrap();

        let (mut c, warnings) = open_collecting(tmp.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(&path.display().to_string()));
        assert!(c.get(&file, &key).is_none());
        // Replaced even with nothing to store, so the warning does not repeat.
        assert!(c.persist().unwrap());
        let (_, warnings) = open_collecting(tmp.path());
        assert!(warnings.is_empty());
    }

    #[test]
    fn oversized_and_unreadable_files_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let path = cache_file(tmp.path());
        // A directory where the file should be: open succeeds, read fails
        // with an error other than NotFound.
        std::fs::create_dir(&path).unwrap();
        let (_, warnings) = open_collecting(tmp.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(&path, vec![0u8; 100]).unwrap();
        assert!(matches!(read_package(&path, 50), ReadOutcome::Corrupt(_)));
    }

    #[test]
    fn expected_unusable_files_stay_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = cache_file(tmp.path());

        let (_, warnings) = open_collecting(tmp.path()); // missing
        assert!(warnings.is_empty());

        let mut other_version = encode(&PackageCache {
            package_root: String::new(),
            files: HashMap::new(),
        })
        .unwrap()
        .to_vec();
        other_version[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let samples: [Vec<u8>; 5] = [
            vec![
                1, b'x', 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F, 0, 0, 0, 0, 0,
            ], // bincode-like
            b"not a konform cache file at all".to_vec(), // foreign
            other_version,
            MAGIC[..4].to_vec(), // shorter than a header
            vec![],
        ];
        for bytes in samples {
            std::fs::write(&path, bytes).unwrap();
            let (_, warnings) = open_collecting(tmp.path());
            assert!(warnings.is_empty(), "{warnings:?}");
        }
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
