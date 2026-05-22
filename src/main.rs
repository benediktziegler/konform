mod cache;
mod cli;
mod config;
mod engine;
mod git;
mod lsp;
mod module_probe;
mod output;
mod rules;
mod theme;
mod types;

use cache::Cache;
use cache::FileCacheKey;
use clap::Parser;
use cli::{CheckArgs, CleanArgs, Cli, Command, InitArgs, RuleArgs};
use config::{load_config, resolve_python};
use engine::CheckInput;
use git::{find_repo_root, get_changed_files};
use ignore::WalkBuilder;
use module_probe::ModuleProbe;
use output::{
    format_fix_hint, print_statistics, print_violations, render_for_file, write_zuul_return,
    OutputFormat,
};
use owo_colors::OwoColorize;
use rayon::prelude::*;
use rules::all_rules;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use types::Level;

// ---------------------------------------------------------------------------
// File walking
// ---------------------------------------------------------------------------

/// Compile a list of glob patterns into an optional [`globset::GlobSet`].
///
/// Returns `None` when `patterns` is empty so callers can short-circuit.
fn build_glob_set(patterns: &[String]) -> Option<globset::GlobSet> {
    use globset::{Glob, GlobSetBuilder};
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    builder.build().ok()
}

fn walk_python_files(path: &Path, exclude: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let exclude_set = build_glob_set(exclude);

    let walker = WalkBuilder::new(path).standard_filters(true).build();
    for entry in walker.flatten() {
        let p = entry.path().to_path_buf();
        if p.extension().is_none_or(|e| e != "py") {
            continue;
        }
        if let Some(gs) = &exclude_set {
            let path_str = p.to_string_lossy();
            if gs.is_match(path_str.as_ref()) || p.file_name().is_some_and(|n| gs.is_match(n)) {
                continue;
            }
        }
        files.push(p);
    }
    files
}

/// Print a unified diff between `original` and `modified` for `path` to stdout.
///
/// Matches the format produced by ruff's `--diff` flag:
/// ```text
/// --- path/to/file
/// +++ path/to/file
/// @@ -1,4 +1,4 @@
///  import os
/// -from os.path import join
/// +import os.path
/// ```
fn print_unified_diff(path: &Path, original: &str, modified: &str) {
    use similar::TextDiff;
    let path_str = path.display().to_string();
    let diff = TextDiff::from_lines(original, modified);
    // unified_diff() emits the full --- / +++ / @@ header and hunks.
    let text = diff.unified_diff().header(&path_str, &path_str).to_string();
    if !text.is_empty() {
        print!("{text}");
    }
}

/// Appends `# noqa: CODE` suppressions to source lines that have violations.
///
/// - Bare `# noqa` suppresses everything → left unchanged.
/// - `# noqa: CODES` → missing codes from the violation set are merged in (sorted).
/// - No `# noqa` → `  # noqa: CODES` is appended.
/// - Returns the modified source, or `None` if no changes were made.
fn add_noqa_to_source(source: &str, violations: &[serde_json::Value]) -> Option<String> {
    use std::collections::BTreeSet;

    if violations.is_empty() {
        return None;
    }

    // Collect rule codes per 1-based line number.
    let mut by_line: HashMap<usize, BTreeSet<String>> = HashMap::new();
    for v in violations {
        let line_no = v.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
        let rule = v.get("rule").and_then(|r| r.as_str()).unwrap_or("");
        if line_no > 0 && !rule.is_empty() {
            by_line.entry(line_no).or_default().insert(rule.to_owned());
        }
    }

    if by_line.is_empty() {
        return None;
    }

    let trailing_newline = source.ends_with('\n');
    let mut out = String::with_capacity(source.len() + violations.len() * 24);
    let mut changed = false;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(new_codes) = by_line.get(&line_no) {
            out.push_str(&merge_noqa(line, new_codes, &mut changed));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    // Restore original trailing-newline state.
    if !trailing_newline {
        out.pop();
    }

    changed.then_some(out)
}

/// Merge `new_codes` into any existing `# noqa` comment on `line`.
///
/// Three cases:
/// 1. Bare `# noqa` (no code list) — suppresses everything; left unchanged.
/// 2. `# noqa: CODES` — codes from `new_codes` absent from `CODES` are appended
///    in sorted order.  Any trailing content after the code list is dropped (it
///    is uncommon and hard to preserve correctly when the list grows).
/// 3. No `# noqa` at all — `  # noqa: CODES` is appended to the line.
fn merge_noqa(
    line: &str,
    new_codes: &std::collections::BTreeSet<String>,
    changed: &mut bool,
) -> String {
    use std::collections::BTreeSet;

    let Some(noqa_pos) = line.find("# noqa") else {
        // No existing noqa — append one.
        let codes_str = new_codes.iter().cloned().collect::<Vec<_>>().join(", ");
        *changed = true;
        return format!("{line}  # noqa: {codes_str}");
    };

    let after = line[noqa_pos + 6..].trim_start();
    if !after.starts_with(':') {
        // Bare `# noqa` — suppresses everything; leave unchanged.
        return line.to_owned();
    }

    // Parse existing codes.  Take only the first whitespace-delimited token of
    // each comma-separated field so that trailing comments ("# noqa: E501  # why")
    // do not end up being treated as a code.
    let existing: BTreeSet<String> = after[1..]
        .split(',')
        .filter_map(|s| s.split_whitespace().next())
        .filter(|s| !s.starts_with('#') && !s.is_empty())
        .map(str::to_owned)
        .collect();

    let missing: Vec<&str> = new_codes
        .iter()
        .map(String::as_str)
        .filter(|c| !existing.contains(*c))
        .collect();

    if missing.is_empty() {
        return line.to_owned(); // All codes already present.
    }

    // Merge all codes (existing + missing) into one sorted list.
    let mut all: BTreeSet<String> = existing;
    all.extend(missing.iter().map(|s| s.to_string()));
    let codes_str = all.into_iter().collect::<Vec<_>>().join(", ");
    *changed = true;
    format!("{}# noqa: {}", &line[..noqa_pos], codes_str)
}

// ---------------------------------------------------------------------------
// Default-subcommand injection
// ---------------------------------------------------------------------------

/// If the first non-flag argument is not a known subcommand, insert `"check"`
/// so that `konform src/` behaves like `konform check src/`.
///
/// Skips leading `--flag value` pairs so that global options such as
/// `--color always` don't confuse the detection.
fn inject_default_subcommand(args: &mut Vec<std::ffi::OsString>) {
    if args.len() < 2 {
        return;
    }

    // Top-level flags that consume the next token as their value.
    const VALUE_FLAGS: &[&str] = &["--color"];

    let mut i = 1usize;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        // --flag=value form: skip but don't consume an extra token.
        if arg.starts_with("--") && arg.contains('=') {
            i += 1;
            continue;
        }
        // --flag value form: skip the flag AND the next token.
        if VALUE_FLAGS.iter().any(|f| arg.as_ref() == *f) {
            i += 2;
            continue;
        }
        break;
    }

    if i >= args.len() {
        return;
    }

    let first = args[i].to_string_lossy();
    let is_subcommand = matches!(
        first.as_ref(),
        "check" | "server" | "rule" | "version" | "clean" | "init" | "help"
    );
    let is_top_level_flag = matches!(first.as_ref(), "--help" | "-h" | "--version" | "-V");
    if !is_subcommand && !is_top_level_flag {
        args.insert(i, "check".into());
    }
}

// ---------------------------------------------------------------------------
// CLI → Config integration
// ---------------------------------------------------------------------------

/// Merge `--select` / `--ignore` CLI flags into `config`.
///
/// `--select` completely overrides the config-file list (explicit intent).
/// `--ignore` extends the config-file list (additive suppression).
fn apply_cli_overrides(
    config: &mut config::Config,
    select: &[String],
    ignore: &[String],
    extend_select: &[String],
    extend_ignore: &[String],
    per_file_ignores: &[String],
    extend_per_file_ignores: &[String],
) {
    // --select fully replaces the config list (explicit override intent).
    if !select.is_empty() {
        config.select = select.to_vec();
    }
    // --extend-select appends to whatever is in the config (or the --select override).
    config.select.extend(extend_select.iter().cloned());
    // Both --ignore and --extend-ignore are additive.
    config.ignore.extend(ignore.iter().cloned());
    config.ignore.extend(extend_ignore.iter().cloned());
    // --per-file-ignores replaces; --extend-per-file-ignores merges.
    if !per_file_ignores.is_empty() {
        config.per_file_ignores = parse_per_file_ignores(per_file_ignores);
    }
    for (glob, codes) in parse_per_file_ignores(extend_per_file_ignores) {
        config
            .per_file_ignores
            .entry(glob)
            .or_default()
            .extend(codes);
    }
}

/// Parse `GLOB:CODE[,CODE,...]` specs into a `HashMap<glob, codes>` map.
///
/// Specs with no `:` separator or an empty glob/code list are silently skipped.
fn parse_per_file_ignores(specs: &[String]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for spec in specs {
        if let Some(colon) = spec.find(':') {
            let glob = spec[..colon].trim().to_owned();
            let codes: Vec<String> = spec[colon + 1..]
                .split(',')
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .collect();
            if !glob.is_empty() && !codes.is_empty() {
                map.entry(glob).or_default().extend(codes);
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Subcommand handlers
// ---------------------------------------------------------------------------

fn run_check(args: CheckArgs, isolated: bool) {
    // Detect stdin mode: "-" in FILE_PATHS.
    let wants_stdin = args.common.file_paths.iter().any(|p| p.as_os_str() == "-");
    let stdin_display = args
        .stdin_filename
        .clone()
        .unwrap_or_else(|| PathBuf::from("<stdin>"));

    // Use the first real (non-stdin) path for config anchor.
    let anchor = args
        .common
        .file_paths
        .iter()
        .find(|p| p.as_os_str() != "-")
        .map(|p| p.as_path());
    let mut config = if isolated {
        config::Config::default()
    } else {
        load_config(anchor, args.common.config.as_deref())
    };
    // --ignore-noqa
    config.ignore_noqa = args.ignore_noqa;
    // --cache-dir override
    if let Some(ref cd) = args.cache_dir {
        config.cache_dir = cd.to_string_lossy().into_owned();
    }
    apply_cli_overrides(
        &mut config,
        &args.common.select,
        &args.common.ignore,
        &args.common.extend_select,
        &args.common.extend_ignore,
        &args.common.per_file_ignores,
        &args.common.extend_per_file_ignores,
    );

    let exclude: Vec<String> = args
        .common
        .exclude
        .iter()
        .chain(args.common.extend_exclude.iter())
        .cloned()
        .collect();

    // Walk only real (non-stdin) paths; "-" is not a filesystem path.
    let all_file_paths: Vec<PathBuf> = args
        .common
        .file_paths
        .iter()
        .filter(|p| p.as_os_str() != "-")
        .flat_map(|p| walk_python_files(p, &exclude))
        .collect();

    // Read stdin once here so every code path below (--show-files, --diff,
    // --fix, check) works from the same in-memory buffer.
    let mut stdin_source: Option<String> = if wants_stdin {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        Some(buf)
    } else {
        None
    };

    let python = resolve_python(&config);
    let probe = Arc::new(ModuleProbe::new(&python));
    let active_rules = all_rules(Arc::clone(&probe), config.config_dir.clone());

    let repo_root = anchor
        .and_then(find_repo_root)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let level_str = args.level.to_string();
    let cache_root = repo_root.join(&config.cache_dir);
    let _ = cache::init(&cache_root);
    let mut cache = Cache::open(
        repo_root.clone(),
        &cache_root,
        args.no_cache || args.ignore_noqa,
        &level_str,
        &config.select,
        &config.ignore,
    );

    let changed_files = get_changed_files();

    // ── --show-files: print resolved file list and exit ──────────────────
    if args.show_files {
        if wants_stdin {
            println!("{}", stdin_display.display());
        }
        let mut targets: Vec<&PathBuf> = all_file_paths.iter().collect();
        targets.sort();
        for p in targets {
            println!("{}", p.display());
        }
        std::process::exit(0);
    }

    // ── --diff: show unified diff of what format would change, then exit ──
    if args.diff {
        let mut has_diff = false;
        // stdin diff: fix in memory and compare.
        if let Some(ref src) = stdin_source {
            let input = CheckInput::new(&stdin_display, src);
            if let Ok(Some(ref fixed)) = engine::run_fix(&input, &active_rules, &config) {
                if *fixed != *src {
                    has_diff = true;
                    print_unified_diff(&stdin_display, src, fixed);
                }
            }
        }
        for file_path in &all_file_paths {
            let source = match std::fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error reading {}: {e}", file_path.display());
                    continue;
                }
            };
            let input = CheckInput::new(file_path, &source);
            if let Ok(Some(fixed)) = engine::run_fix(&input, &active_rules, &config) {
                if fixed != source {
                    has_diff = true;
                    print_unified_diff(file_path, &source, &fixed);
                }
            }
        }
        std::process::exit(if has_diff { 1 } else { 0 });
    }

    // --fix-only implies --fix but suppresses violation reporting.
    let effective_fix = args.fix || args.fix_only;

    // ── Optional auto-fix pass ──────────────────────────────────────────
    let mut fixed_any = false;
    if effective_fix {
        // stdin fix: write result to stdout; update buffer for the check pass.
        if let Some(src) = stdin_source.take() {
            let input = CheckInput::new(&stdin_display, &src);
            match engine::run_fix(&input, &active_rules, &config) {
                Ok(Some(new_source)) => {
                    print!("{new_source}");
                    fixed_any = true;
                    stdin_source = Some(new_source); // check pass sees fixed version
                }
                Ok(None) => {
                    // Nothing to fix; echo the original to stdout so the pipe works.
                    print!("{src}");
                    stdin_source = Some(src);
                }
                Err(e) => {
                    eprintln!(
                        "{} {}: {e}",
                        "fix failed".yellow().bold(),
                        stdin_display.display()
                    );
                    // Echo original on error to avoid breaking the pipe.
                    print!("{src}");
                    stdin_source = Some(src);
                }
            }
        }
        let fix_targets: Vec<&PathBuf> = all_file_paths.iter().collect();
        for file_path in fix_targets {
            let source = match std::fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "{} {}: {e}",
                        "fix failed".yellow().bold(),
                        file_path.display()
                    );
                    continue;
                }
            };
            let input = CheckInput::new(file_path, &source);
            match engine::run_fix(&input, &active_rules, &config) {
                Ok(Some(new_source)) => {
                    if let Err(e) = std::fs::write(file_path, &new_source) {
                        eprintln!(
                            "{} {}: {e}",
                            "fix failed".yellow().bold(),
                            file_path.display()
                        );
                    } else {
                        eprintln!("{} {}", "fixed".green().bold(), file_path.display());
                        cache.invalidate(file_path);
                        fixed_any = true;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "{} {}: {e}",
                        "fix failed".yellow().bold(),
                        file_path.display()
                    );
                }
            }
        }
        if fixed_any {
            eprintln!("\nAuto-fixed files. Run hatch fmt to reformat.\n");
        }
    }

    if fixed_any && args.exit_non_zero_on_fix {
        std::process::exit(1);
    }

    if args.fix_only {
        std::process::exit(0);
    }

    // ── Check pass ────────────────────────────────────────────────────────
    let mut violations: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut uncached: Vec<&PathBuf> = Vec::new();

    for fp in &all_file_paths {
        let file_key = FileCacheKey::from_path(fp);
        let cache_hit = file_key.as_ref().and_then(|k| cache.get(fp, k));
        if let Some(cached) = cache_hit {
            if !cached.is_empty() {
                violations.insert(fp.to_string_lossy().to_string(), cached);
            }
        } else {
            uncached.push(fp);
        }
    }

    if config.workers > 0 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(config.workers)
            .build_global();
    }

    let new_results: Vec<(PathBuf, Vec<serde_json::Value>)> = uncached
        .par_iter()
        .map(|fp| {
            let source = match std::fs::read_to_string(*fp) {
                Ok(s) => s,
                Err(_) => return ((*fp).clone(), vec![]),
            };
            let mut input = CheckInput::new(fp, &source);
            input.ignore_noqa = config.ignore_noqa;
            let json_viols = engine::run_check(&input, &active_rules, &config)
                .into_iter()
                .map(|v| v.to_json())
                .collect();
            ((*fp).clone(), json_viols)
        })
        .collect();

    for (fp, results) in new_results {
        if let Some(key) = FileCacheKey::from_path(&fp) {
            cache.set_linted(&fp, &key, &results);
        }
        if !results.is_empty() {
            violations.insert(fp.to_string_lossy().to_string(), results);
        }
    }

    // stdin: always run uncached — there is no mtime key for stdin.
    if let Some(ref src) = stdin_source {
        let mut input = CheckInput::new(&stdin_display, src);
        input.ignore_noqa = config.ignore_noqa;
        let json_viols: Vec<serde_json::Value> = engine::run_check(&input, &active_rules, &config)
            .into_iter()
            .map(|v| v.to_json())
            .collect();
        if !json_viols.is_empty() {
            violations.insert(stdin_display.to_string_lossy().into_owned(), json_viols);
        }
    }

    let _ = cache.persist();

    // ── --add-noqa: annotate violations in-place, then exit 0 ──────────────────
    if args.add_noqa {
        let stdin_key = stdin_display.to_string_lossy().into_owned();
        for (file_path_str, viols) in &violations {
            if wants_stdin && *file_path_str == stdin_key {
                // stdin: write annotated source to stdout.
                if let Some(ref src) = stdin_source {
                    match add_noqa_to_source(src, viols) {
                        Some(modified) => print!("{modified}"),
                        None => print!("{src}"),
                    }
                }
            } else {
                let path = PathBuf::from(file_path_str);
                let src = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error reading {}: {e}", path.display());
                        continue;
                    }
                };
                if let Some(modified) = add_noqa_to_source(&src, viols) {
                    if let Err(e) = std::fs::write(&path, &modified) {
                        eprintln!("error writing {}: {e}", path.display());
                    }
                }
            }
        }
        std::process::exit(0);
    }

    // ── Reporting ───────────────────────────────────────────────────────────────────
    let reported = violations.clone();

    let changed_files_level = args.changed_files_level.unwrap_or(args.level);
    let exit_code = print_violations(
        &reported,
        &changed_files,
        args.level,
        changed_files_level,
        args.output_format,
    );

    // --output-file: write the formatted output to disk using the selected format.
    if let Some(ref out_path) = args.output_file {
        let content = render_for_file(&reported, args.output_format);
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let _ = std::fs::write(out_path, content);
    }

    // Show hint after violations (suppressed in quiet/silent mode).
    if !reported.is_empty() && !effective_fix && !theme::is_quiet() {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        eprintln!("{}", format_fix_hint(&argv));
    }

    let mut file_comments: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();
    for (path, viols) in &reported {
        if changed_files.contains(path) {
            file_comments.insert(path.clone(), viols.clone());
        } else {
            for v in viols {
                let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
                let line = v.get("line").and_then(|l| l.as_u64()).unwrap_or(0);
                warnings.push(format!("{path}:{line}: {msg}"));
            }
        }
    }
    if let Some(parent) = args.output_path.parent() {
        if parent.exists() || parent.to_str() == Some("") {
            let _ = write_zuul_return(&args.output_path, file_comments, warnings);
        }
    }

    if args.statistics {
        let rule_names: HashMap<String, String> = active_rules
            .iter()
            .map(|r| (r.code().to_owned(), r.name().to_owned()))
            .collect();
        print_statistics(&reported, &rule_names);
    }

    // ── --watch: enter the file-watching loop (never returns) ───────────────
    if args.watch {
        let ctx = RecheckContext {
            active_rules: &active_rules,
            config: &config,
            level: args.level,
            changed_files_level,
            output_format: args.output_format,
            no_cache: args.no_cache || args.ignore_noqa,
        };
        run_watch_loop(&args.common.file_paths, &exclude, &ctx, cache);
    }

    if args.exit_zero {
        std::process::exit(0);
    }
    std::process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// Watch mode
// ---------------------------------------------------------------------------

/// Shared context passed to both [`run_watch_loop`] and [`recheck_batch`].
struct RecheckContext<'a> {
    active_rules: &'a [Box<dyn rules::Rule>],
    config: &'a config::Config,
    level: Level,
    changed_files_level: Level,
    output_format: OutputFormat,
    no_cache: bool,
}

/// Enter the file-watching loop.  Called after the initial check pass when
/// `-w` / `--watch` is set.  This function never returns; the process exits
/// when Ctrl-C delivers SIGINT or the watcher backend disconnects.
fn run_watch_loop(
    watch_paths: &[PathBuf],
    exclude: &[String],
    ctx: &RecheckContext<'_>,
    mut cache: Cache,
) -> ! {
    use notify::{Config as WatchConfig, RecommendedWatcher, RecursiveMode, Watcher};
    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::time::Duration;

    let exclude_set = build_glob_set(exclude);

    // Channel that receives raw notify events.
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        WatchConfig::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: could not create file watcher: {e}");
            std::process::exit(1);
        }
    };

    let mut watched = 0usize;
    for path in watch_paths {
        if path.as_os_str() == "-" {
            continue; // stdin has no path to watch
        }
        match watcher.watch(path, RecursiveMode::Recursive) {
            Ok(()) => watched += 1,
            Err(e) => eprintln!("warning: could not watch {}: {e}", path.display()),
        }
    }
    eprintln!("Watching {watched} path(s) for changes \u{2014} press Ctrl-C to stop");

    // Debounce window: accumulate events for 150 ms, then flush.
    let debounce = Duration::from_millis(150);
    let mut pending: HashSet<PathBuf> = HashSet::new();

    loop {
        match rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                for path in event.paths {
                    if path.extension().is_some_and(|e| e == "py") && path.is_file() {
                        let excluded = exclude_set.as_ref().is_some_and(|gs| {
                            let s = path.to_string_lossy();
                            gs.is_match(s.as_ref())
                                || path.file_name().is_some_and(|n| gs.is_match(n))
                        });
                        if !excluded {
                            pending.insert(path);
                        }
                    }
                }
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !pending.is_empty() {
                    let batch: Vec<PathBuf> = pending.drain().collect();
                    recheck_batch(&batch, ctx, &mut cache);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("File watcher disconnected.");
                std::process::exit(0);
            }
        }
    }
}

/// Re-check a batch of changed files and print incremental output to stderr.
fn recheck_batch(files: &[PathBuf], ctx: &RecheckContext<'_>, cache: &mut Cache) {
    let n = files.len();
    let s = if n == 1 { "" } else { "s" };
    eprintln!("\n\u{2500}\u{2500}\u{2500} {n} file{s} changed, rechecking \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");

    let mut violations: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    for file_path in files {
        cache.invalidate(file_path);

        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error reading {}: {e}", file_path.display());
                continue;
            }
        };
        let mut input = CheckInput::new(file_path, &source);
        input.ignore_noqa = ctx.config.ignore_noqa;
        let json_viols: Vec<serde_json::Value> =
            engine::run_check(&input, ctx.active_rules, ctx.config)
                .into_iter()
                .map(|v| v.to_json())
                .collect();

        if !ctx.no_cache {
            if let Some(key) = FileCacheKey::from_path(file_path) {
                cache.set_linted(file_path, &key, &json_viols);
            }
        }
        if !json_viols.is_empty() {
            violations.insert(file_path.to_string_lossy().to_string(), json_viols);
        }
    }

    if !ctx.no_cache {
        let _ = cache.persist();
    }

    let changed_files = get_changed_files();
    print_violations(
        &violations,
        &changed_files,
        ctx.level,
        ctx.changed_files_level,
        ctx.output_format,
    );
    if violations.is_empty() {
        eprintln!("All checked files are clean \u{2713}");
    }
}

fn run_rule(args: RuleArgs) {
    let probe = Arc::new(ModuleProbe::default());
    let all = all_rules(probe, None);

    if args.list {
        for rule in &all {
            println!(
                "{:8}  {:<10}  {}  — {}",
                rule.code(),
                rule.category(),
                rule.name(),
                rule.description(),
            );
        }
        std::process::exit(0);
    }

    if let Some(code) = &args.explain {
        match all.iter().find(|r| r.code() == code.as_str()) {
            Some(rule) => {
                println!("{}", rule.explain());
                std::process::exit(0);
            }
            None => {
                eprintln!("Unknown rule: {code}");
                eprintln!("Run `konform rule --list` to see all available rules.");
                std::process::exit(2);
            }
        }
    }

    eprintln!("Use --list to list all rules or --explain <CODE> to explain one.");
    std::process::exit(1);
}

fn run_version() {
    println!("konform {}", env!("CARGO_PKG_VERSION"));
}

// ---------------------------------------------------------------------------
// init templates
// ---------------------------------------------------------------------------

/// Rule category prefixes registered with external linters (e.g. ruff `external`).
const RULE_CATEGORIES: &[&str] = &["KIS", "KPT"];

/// Default content written to a new `konform.toml`.
/// Only non-default settings are included; everything else is left as a comment.
const KONFORM_TOML: &str = r#"# konform.toml — project linting configuration
# Run `konform rule --list` to see available rules.
# Run `konform rule --explain KIS001` for detailed documentation.

[konform]

[konform.KIS]
# Extend the built-in exceptions (__future__, typing, typing_extensions,
# collections.abc) if your project has additional exempted modules:
# exceptions = ["mycompany.compat"]

[konform.KPT]
# rules_file = "konform_patterns.toml"
"#;

/// Section appended to an existing `pyproject.toml` that has no `[tool.konform]`.
const PYPROJECT_APPEND: &str = r#"
[tool.konform]

[tool.konform.KIS]
# Extend the built-in exceptions (__future__, typing, typing_extensions,
# collections.abc) if your project has additional exempted modules:
# exceptions = ["mycompany.compat"]

[tool.konform.KPT]
# rules_file = "konform_patterns.toml"
"#;

/// Default `konform_patterns.toml` with commented-out example patterns.
const PATTERNS_TOML: &str = r#"# konform_patterns.toml — user-defined KPT pattern rules
# Auto-discovered when placed alongside pyproject.toml / konform.toml.
# Run `konform rule --explain KPT001` for full documentation.

# ── Debugging artefacts ─────────────────────────────────────────────────────

[[rules]]
id      = "KPT001"
message = "Remove bare print() — use the project logger instead."
pattern = '^\\s*print\\s*\\('
files   = ["src/**/*.py"]
level   = "warning"

[[rules]]
id      = "KPT002"
message = "Remove breakpoint() — debugging artefact must not be committed."
pattern = '^\\s*breakpoint\\s*\\(\\s*\\)'
level   = "error"

# ── Code hygiene ────────────────────────────────────────────────────────────

[[rules]]
id      = "KPT010"
message = "Resolve or ticket this TODO before merging."
pattern = '#\\s*TODO'
files   = ["src/**/*.py"]
level   = "warning"
"#;

// ---------------------------------------------------------------------------
// run_init
// ---------------------------------------------------------------------------

fn run_init(args: InitArgs) {
    let dir = args.path.canonicalize().unwrap_or(args.path.clone());
    init_config(&dir, args.force, args.diff);
    if !args.no_patterns {
        init_patterns(&dir, args.diff);
    }
    init_ruff_compat(&dir, args.diff);
}

/// Write or append the konform configuration (or show a diff when `dry_run`).
fn init_config(dir: &std::path::Path, force: bool, dry_run: bool) {
    let pyproject = dir.join("pyproject.toml");
    let konform_toml = dir.join("konform.toml");

    // konform.toml already exists
    if konform_toml.is_file() && !force {
        eprintln!("note: konform.toml already exists. Run with --force to overwrite.");
        return;
    }

    if pyproject.is_file() && !force {
        let content = std::fs::read_to_string(&pyproject).unwrap_or_default();
        if content.contains("[tool.konform]") {
            eprintln!(
                "note: [tool.konform] already in pyproject.toml. Run with --force to create konform.toml."
            );
            return;
        }
        let updated = format!("{}{PYPROJECT_APPEND}", content.trim_end());
        if dry_run {
            print_file_diff(&pyproject, &content, &updated);
        } else {
            if let Err(e) = std::fs::write(&pyproject, &updated) {
                eprintln!("error: failed to update pyproject.toml: {e}");
                std::process::exit(1);
            }
            eprintln!("Updated pyproject.toml — added [tool.konform]");
        }
        return;
    }

    // Create (or overwrite) konform.toml.
    if dry_run {
        print_file_diff(&konform_toml, "", KONFORM_TOML);
    } else {
        if let Err(e) = std::fs::write(&konform_toml, KONFORM_TOML) {
            eprintln!("error: failed to create konform.toml: {e}");
            std::process::exit(1);
        }
        eprintln!("Created konform.toml");
    }
}

/// Create `konform_patterns.toml` in `dir` (or show a diff when `dry_run`).
fn init_patterns(dir: &std::path::Path, dry_run: bool) {
    let file = dir.join("konform_patterns.toml");
    if file.is_file() {
        return; // never overwrite existing patterns
    }
    if dry_run {
        print_file_diff(&file, "", PATTERNS_TOML);
    } else if let Err(e) = std::fs::write(&file, PATTERNS_TOML) {
        eprintln!("error: failed to create konform_patterns.toml: {e}");
    } else {
        eprintln!("Created konform_patterns.toml");
    }
}

/// Print a unified diff between `before` and `after` labelled with `path`.
fn print_file_diff(path: &std::path::Path, before: &str, after: &str) {
    use similar::TextDiff;
    let label = path.display().to_string();
    let before_label = if before.is_empty() {
        "/dev/null".to_owned()
    } else {
        label.clone()
    };
    let diff = TextDiff::from_lines(before, after);
    let text = diff
        .unified_diff()
        .header(&before_label, &label)
        .to_string();
    if !text.is_empty() {
        print!("{text}");
    }
}

/// Detect ruff configuration and add konform rule categories to its `external`
/// list so that `# noqa: KIS001` comments are not stripped by `ruff --fix`.
fn init_ruff_compat(dir: &std::path::Path, dry_run: bool) {
    let ext_value = {
        let quoted: Vec<String> = RULE_CATEGORIES.iter().map(|c| format!("\"{c}\"")).collect();
        format!("[{}]", quoted.join(", "))
    };
    let ext_line = format!("external = {ext_value}");

    // Check pyproject.toml for a ruff section.
    let pyproject = dir.join("pyproject.toml");
    if pyproject.is_file() {
        let content = std::fs::read_to_string(&pyproject).unwrap_or_default();
        if content.contains("[tool.ruff]") || content.contains("[tool.ruff.") {
            patch_ruff_config(&pyproject, &content, &ext_line, "[tool.ruff.lint]", dry_run);
            return;
        }
    }

    // Check standalone ruff.toml / .ruff.toml.
    for name in &["ruff.toml", ".ruff.toml"] {
        let path = dir.join(name);
        if path.is_file() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            patch_ruff_config(&path, &content, &ext_line, "[lint]", dry_run);
            return;
        }
    }
    // No ruff config found — nothing to do.
}

/// Attempt to add `external = ["KIS", "KPT"]` to a ruff config file.
///
/// Strategy:
/// * If `external` is already present → skip (already configured).
/// * If `lint_section` header is present → the section exists with other settings;
///   inserting mid-section is unsafe without a TOML parser, so print a note.
/// * Otherwise → append the complete `lint_section` block with `external`.
fn patch_ruff_config(
    path: &std::path::Path,
    content: &str,
    ext_line: &str,
    lint_section: &str,
    dry_run: bool,
) {
    if content.contains("external") {
        // Already configured — nothing to add.
        return;
    }

    if content.contains(lint_section) {
        // Section exists but without `external`; unsafe to insert without a
        // TOML parser — tell the user what to add manually.
        eprintln!(
            "note: add to {lint_section} in {}: {ext_line}",
            path.display()
        );
        return;
    }

    // Safe to append a new section.
    let append = format!("\n{lint_section}\n{ext_line}\n");
    let updated = format!("{}{append}", content.trim_end());
    if dry_run {
        print_file_diff(path, content, &updated);
    } else {
        if let Err(e) = std::fs::write(path, &updated) {
            eprintln!("error: failed to update {}: {e}", path.display());
        } else {
            eprintln!(
                "Updated {} — added {lint_section} external for konform codes",
                path.display()
            );
        }
    }
}

fn run_clean(args: CleanArgs) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = load_config(Some(&cwd), args.config.as_deref());
    let cache_path = std::path::Path::new(&config.cache_dir);
    if cache_path.exists() {
        match std::fs::remove_dir_all(cache_path) {
            Ok(()) => eprintln!("Removed cache directory: {}", config.cache_dir),
            Err(e) => {
                eprintln!("error: failed to remove cache: {e}");
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("Cache directory not found: {}", config.cache_dir);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    inject_default_subcommand(&mut args);
    let cli = Cli::parse_from(args);

    // Initialise colour and log-level preferences before any output.
    theme::init_colors(cli.color);
    let log_level = if cli.silent {
        theme::LogLevel::Silent
    } else if cli.quiet {
        theme::LogLevel::Quiet
    } else if cli.verbose {
        theme::LogLevel::Verbose
    } else {
        theme::LogLevel::Default
    };
    theme::init_log_level(log_level);

    // Expose --isolated for config loading.
    let isolated = cli.isolated;

    match cli.command {
        Some(Command::Server) | None => lsp::run(),
        Some(Command::Check(a)) => run_check(*a, isolated),
        Some(Command::Rule(a)) => run_rule(a),
        Some(Command::Version) => run_version(),
        Some(Command::Clean(a)) => run_clean(a),
        Some(Command::Init(a)) => run_init(a),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for add_noqa helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod noqa_tests {
    use super::*;

    fn viol(rule: &str, line: u64) -> serde_json::Value {
        serde_json::json!({"rule": rule, "line": line, "col": 1, "message": "test"})
    }

    // merge_noqa ---------------------------------------------------------------

    #[test]
    fn merge_noqa_no_existing_appends_comment() {
        let mut changed = false;
        let codes = ["KIS001".to_owned()].into_iter().collect();
        let result = merge_noqa("from os.path import join", &codes, &mut changed);
        assert_eq!(result, "from os.path import join  # noqa: KIS001");
        assert!(changed);
    }

    #[test]
    fn merge_noqa_foreign_code_merges_sorted() {
        let mut changed = false;
        let codes = ["KIS001".to_owned()].into_iter().collect();
        let result = merge_noqa("code()  # noqa: E501", &codes, &mut changed);
        assert_eq!(result, "code()  # noqa: E501, KIS001");
        assert!(changed);
    }

    #[test]
    fn merge_noqa_code_already_present_unchanged() {
        let mut changed = false;
        let codes = ["KIS001".to_owned()].into_iter().collect();
        let result = merge_noqa("code()  # noqa: KIS001", &codes, &mut changed);
        assert_eq!(result, "code()  # noqa: KIS001");
        assert!(!changed);
    }

    #[test]
    fn merge_noqa_bare_noqa_unchanged() {
        let mut changed = false;
        let codes = ["KIS001".to_owned()].into_iter().collect();
        let result = merge_noqa("code()  # noqa", &codes, &mut changed);
        assert_eq!(result, "code()  # noqa");
        assert!(!changed);
    }

    #[test]
    fn merge_noqa_multiple_codes_sorted() {
        let mut changed = false;
        let codes = ["KPT001".to_owned(), "KIS001".to_owned()]
            .into_iter()
            .collect();
        let result = merge_noqa("code()", &codes, &mut changed);
        assert_eq!(result, "code()  # noqa: KIS001, KPT001");
        assert!(changed);
    }

    #[test]
    fn merge_noqa_trailing_comment_after_codes_preserved_prefix() {
        // Trailing commentary after the code list is dropped when merging.
        let mut changed = false;
        let codes = ["KIS001".to_owned()].into_iter().collect();
        let result = merge_noqa("code()  # noqa: E501  # intentional", &codes, &mut changed);
        assert_eq!(result, "code()  # noqa: E501, KIS001");
        assert!(changed);
    }

    // add_noqa_to_source -------------------------------------------------------

    #[test]
    fn add_noqa_source_no_violations_returns_none() {
        assert!(add_noqa_to_source("import os\n", &[]).is_none());
    }

    #[test]
    fn add_noqa_source_appends_comment() {
        let viols = vec![viol("KIS001", 1)];
        let result = add_noqa_to_source("from os.path import join\n", &viols).unwrap();
        assert_eq!(result, "from os.path import join  # noqa: KIS001\n");
    }

    #[test]
    fn add_noqa_source_merges_foreign_noqa() {
        let viols = vec![viol("KIS001", 1)];
        let src = "from os.path import join  # noqa: E501\n";
        let result = add_noqa_to_source(src, &viols).unwrap();
        assert_eq!(result, "from os.path import join  # noqa: E501, KIS001\n");
    }

    #[test]
    fn add_noqa_source_preserves_trailing_newline() {
        let viols = vec![viol("KIS001", 1)];
        let result = add_noqa_to_source("from os.path import join\n", &viols).unwrap();
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn add_noqa_source_no_trailing_newline() {
        let viols = vec![viol("KIS001", 1)];
        let result = add_noqa_to_source("from os.path import join", &viols).unwrap();
        assert!(!result.ends_with('\n'));
    }
}
