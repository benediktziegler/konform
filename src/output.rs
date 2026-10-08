//! Terminal output formatting and Zuul CI artefact writing.
//!
//! [`print_violations`] drives all stderr output for the `check` subcommand.
//! [`write_zuul_return`] serialises violations to the YAML file consumed by
//! the Zuul CI comment-bot.
//!
//! Colour choices are centralised in [`crate::theme`]; change
//! [`crate::theme::ACTIVE_THEME`] to restyle all output at once.

use crate::rules::Rule;
use crate::theme;
use crate::types::{ChangedFiles, Level};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;

/// Returns `true` when stderr should emit ANSI colour codes.
///
/// Mirrors ruff / ripgrep behaviour:
/// * Strip colours when stderr is not a TTY.
/// * Always strip when `NO_COLOR` is set (<https://no-color.org>).
/// * Always strip when `TERM=dumb`.
pub fn colors_enabled() -> bool {
    colors_enabled_for(std::io::stderr().is_terminal())
}

/// Like [`colors_enabled`], for output written to stdout.
pub fn stdout_colors_enabled() -> bool {
    colors_enabled_for(std::io::stdout().is_terminal())
}

fn colors_enabled_for(is_terminal: bool) -> bool {
    use crate::theme::{ColorWhen, COLOR_PREFERENCE};
    match COLOR_PREFERENCE.get().copied().unwrap_or(ColorWhen::Auto) {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
                return false;
            }
            is_terminal
        }
    }
}

/// Output format for violations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Ruff-style output with arrows and help lines (default).
    #[default]
    Full,
    /// Concise single-line: `file:line:col: level[RULE] message` (`[*]` marks
    /// a fixable violation).
    Concise,
    /// JSON array written to stdout (machine-readable).
    Json,
    /// GitHub Actions workflow command annotations (written to stdout).
    Github,
    /// GitLab Code Quality JSON report (written to stdout).
    Gitlab,
    /// SARIF 2.1.0 JSON (written to stdout).
    Sarif,
    /// JUnit XML report (written to stdout).
    Junit,
    /// Zuul `zuul_return.yaml` (merged into the file at `--output-path`).
    Zuul,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the alphabetic category prefix from a rule code.
///
/// ```text
/// "KIS001" → "KIS"
/// "PT001"  → "PT"
/// "FMIS001"→ "FMIS"
/// "UNKNOWN"→ "UNKNOWN"
/// ```
#[allow(dead_code)]
pub fn rule_category(code: &str) -> &str {
    let end = code
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(code.len());
    &code[..end]
}

/// Whether the rule that reports `code` marks its fix as unsafe. Falls back
/// to a category-prefix match for rules with dynamic per-pattern codes
/// (e.g. KPT's user-defined ids), mirroring `rule_name` in the LSP handler.
fn rule_is_unsafe_fix(rules: &[Box<dyn Rule>], code: &str) -> bool {
    rules
        .iter()
        .find(|r| r.code() == code)
        .or_else(|| rules.iter().find(|r| code.starts_with(r.category())))
        .is_some_and(|r| r.is_unsafe_fix())
}

/// Whether `reported` contains any violation that's fixable only via
/// `--unsafe-fixes` (`fixable: true` but the owning rule's fix is unsafe).
/// Used to decide whether the `--fix` hint should also suggest
/// `--unsafe-fixes`.
pub fn has_unsafe_fixable(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    rules: &[Box<dyn Rule>],
) -> bool {
    reported.values().flatten().any(|v| {
        v["fixable"].as_bool().unwrap_or(false)
            && rule_is_unsafe_fix(rules, v["rule"].as_str().unwrap_or(""))
    })
}

// ---------------------------------------------------------------------------
// print_violations
// ---------------------------------------------------------------------------

/// Print all violations to stderr in ruff-style format and return the exit code.
///
/// Each violation is rendered as:
/// ```text
/// error[KIS001][*]: Import 'join' from 'os.path' is not a module.
///   --> src/foo.py:3:1
///   help: Use only module imports …
///
/// ```
/// followed by a summary line:
/// ```text
/// Found 2 errors.
/// [*] 2 fixable with the `--fix` option.
/// ```
///
/// When some of the fixable violations are only fixable via `--unsafe-fixes`
/// (see `Rule::is_unsafe_fix`), that count is broken out separately so the
/// hint doesn't imply a plain `--fix` run would fix them:
/// ```text
/// Found 3 errors.
/// [*] 1 fixable with the `--fix` option (2 hidden fixes can be enabled with the `--unsafe-fixes` option).
/// ```
///
/// Returns `0` when no violation reaches the configured severity threshold,
/// `1` otherwise.
pub fn print_violations(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
    level: Level,
    changed_files_level: Level,
    output_format: OutputFormat,
    rules: &[Box<dyn Rule>],
) -> i32 {
    let pal = theme::palette(colors_enabled());

    // Silent mode: skip all output, just compute and return the exit code.
    if theme::is_silent() {
        return exit_code_for(reported, changed_files, level, changed_files_level);
    }

    // Non-full formats bypass the ruff-style block renderer.
    match output_format {
        OutputFormat::Json => {
            return print_violations_json(reported, changed_files, level, changed_files_level)
        }
        OutputFormat::Concise => {
            return print_violations_concise(
                reported,
                &pal,
                changed_files,
                level,
                changed_files_level,
            )
        }
        OutputFormat::Github => {
            print!("{}", render_github(reported));
            return exit_code_for(reported, changed_files, level, changed_files_level);
        }
        OutputFormat::Gitlab => {
            println!("{}", render_gitlab(reported));
            return exit_code_for(reported, changed_files, level, changed_files_level);
        }
        OutputFormat::Sarif => {
            println!("{}", render_sarif(reported));
            return exit_code_for(reported, changed_files, level, changed_files_level);
        }
        OutputFormat::Junit => {
            print!("{}", render_junit(reported));
            return exit_code_for(reported, changed_files, level, changed_files_level);
        }
        // The report is written to `--output-path` by the caller; nothing to print.
        OutputFormat::Zuul => {
            return exit_code_for(reported, changed_files, level, changed_files_level);
        }
        OutputFormat::Full => {} // fall through to the full renderer below
    }

    // ── Per-violation blocks ──────────────────────────────────────────────
    // Sort files for deterministic output.
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();

    let mut error_count = 0usize;
    let mut warning_count = 0usize;
    let mut safe_fixable_count = 0usize;
    let mut unsafe_fixable_count = 0usize;

    for file_path in &paths {
        let violations = &reported[*file_path];

        // Sort violations by line number within each file.
        let mut sorted: Vec<&serde_json::Value> = violations.iter().collect();
        sorted.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));

        for v in sorted {
            let line = v["line"].as_u64().unwrap_or(0);
            // start_character is 1-based in the wire format.
            let col = v["range"]["start_character"].as_u64().unwrap_or(1);
            let raw_msg = v["message"].as_str().unwrap_or("");
            let help = v["help"].as_str().unwrap_or("");
            let vlevel = v["level"].as_str().unwrap_or("error");
            let rule_code = v["rule"].as_str().unwrap_or("UNKNOWN");
            let fixable = v["fixable"].as_bool().unwrap_or(false);

            let is_error = vlevel == "error";
            if is_error {
                error_count += 1;
            } else {
                warning_count += 1;
            }
            if fixable {
                if rule_is_unsafe_fix(rules, rule_code) {
                    unsafe_fixable_count += 1;
                } else {
                    safe_fixable_count += 1;
                }
            }

            // Strip the leading "RULE_CODE: " prefix from the message — the
            // rule code is already shown in the [brackets] on the same line.
            let msg = raw_msg
                .strip_prefix(&format!("{rule_code}: "))
                .unwrap_or(raw_msg);

            // ── Header: error[KIS001][*]: message ────────────────────────
            let level_str = if is_error {
                pal.error("error")
            } else {
                pal.warning("warning")
            };
            let code_str = pal.rule_brackets(&format!("[{rule_code}]"), is_error);
            // `[*]` badge: `[` and `]` are plain; only `*` is bright-cyan.
            let badge = if fixable {
                format!("[{}]", pal.fixable_star())
            } else {
                String::new()
            };
            eprintln!("{level_str}{code_str}{badge}: {}", pal.message(msg));

            // ── Arrow: --> file:line:col ──────────────────────────────────
            eprintln!("  {} {file_path}:{line}:{col}", pal.arrow("-->"));

            // ── Help (optional) ───────────────────────────────────────────
            if !help.is_empty() {
                // Strip "(fixable)" suffix — the [*] badge already shows it.
                let help_text = help.strip_suffix(" (fixable)").unwrap_or(help);
                eprintln!("  {}: {help_text}", pal.help_label("help"));
            }

            // Blank line between violations (matches ruff spacing).
            eprintln!();
        }
    }

    // ── Summary ───────────────────────────────────────────────────────────
    if !theme::is_quiet() {
        let total = error_count + warning_count;
        if total > 0 {
            let found = match (error_count, warning_count) {
                (e, 0) => format!("Found {} error{}.", e, if e == 1 { "" } else { "s" }),
                (0, w) => format!("Found {} warning{}.", w, if w == 1 { "" } else { "s" }),
                (e, w) => format!(
                    "Found {} error{} and {} warning{}.",
                    e,
                    if e == 1 { "" } else { "s" },
                    w,
                    if w == 1 { "" } else { "s" }
                ),
            };
            eprintln!("{found}");

            match (safe_fixable_count > 0, unsafe_fixable_count > 0) {
                (true, false) => eprintln!(
                    "[{}] {} fixable with the `--fix` option.",
                    pal.summary_star(),
                    safe_fixable_count,
                ),
                (true, true) => eprintln!(
                    "[{}] {} fixable with the `--fix` option ({} hidden fix{} can be enabled with the `--unsafe-fixes` option).",
                    pal.summary_star(),
                    safe_fixable_count,
                    unsafe_fixable_count,
                    if unsafe_fixable_count == 1 { "" } else { "es" },
                ),
                (false, true) => eprintln!(
                    "[{}] {} fixable only with the `--unsafe-fixes` option.",
                    pal.summary_star(),
                    unsafe_fixable_count,
                ),
                (false, false) => {}
            }
        } else {
            eprintln!("All checks passed.");
        }
    }

    exit_code_for(reported, changed_files, level, changed_files_level)
}

// ---------------------------------------------------------------------------
// write_zuul_return
// ---------------------------------------------------------------------------

/// Write the Zuul report for `reported` to `output_path`.
///
/// Violations in files changed in the current git change become inline file
/// comments; all others are listed as warnings (`path:line: message`, in path
/// order so the output is deterministic).
pub fn emit_zuul_return(
    output_path: &Path,
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
) -> anyhow::Result<()> {
    let mut file_comments: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();
    for path in paths {
        let viols = &reported[path];
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
    write_zuul_return(output_path, file_comments, warnings)
}

/// Like [`emit_zuul_return`], but prints the error to stderr; returns whether
/// the report was written.
pub fn emit_zuul_return_or_report(
    output_path: &Path,
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
) -> bool {
    match emit_zuul_return(output_path, reported, changed_files) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "error: failed to write Zuul output to {}: {e}",
                output_path.display()
            );
            false
        }
    }
}

/// Write violations to a zuul_return.yaml file (create or merge).
pub fn write_zuul_return(
    output_path: &Path,
    file_comments: HashMap<String, Vec<serde_json::Value>>,
    warnings: Vec<String>,
) -> anyhow::Result<()> {
    let mut zuul_data: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

    if !file_comments.is_empty() {
        // Strip "help" key — Zuul doesn't support it; append to message instead.
        let cleaned: HashMap<String, Vec<serde_json::Value>> = file_comments
            .into_iter()
            .map(|(path, comments)| {
                let cleaned_comments = comments
                    .into_iter()
                    .map(|mut c| {
                        if let Some(help) = c.get("help").and_then(|h| h.as_str()).map(String::from)
                        {
                            if let Some(msg) = c.get_mut("message") {
                                if let Some(s) = msg.as_str() {
                                    *msg = serde_json::Value::String(format!("{s} {help}"));
                                }
                            }
                            if let Some(obj) = c.as_object_mut() {
                                obj.remove("help");
                            }
                        }
                        c
                    })
                    .collect();
                (path, cleaned_comments)
            })
            .collect();
        zuul_data.insert("file_comments".to_string(), serde_json::to_value(cleaned)?);
    }

    if !warnings.is_empty() {
        zuul_data.insert("warnings".to_string(), serde_json::to_value(&warnings)?);
    }

    let existing: serde_json::Value = if output_path.is_file() {
        let content = std::fs::read_to_string(output_path)?;
        serde_yaml::from_str(&content).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    };

    let mut root: serde_json::Map<String, serde_json::Value> = match existing {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    let data = root
        .entry("data")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let zuul = data
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("data is not an object"))?
        .entry("zuul")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let zuul_obj = zuul
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("zuul is not an object"))?;

    for (k, v) in zuul_data {
        zuul_obj.insert(k, v);
    }

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let yaml_str = serde_yaml::to_string(&serde_json::Value::Object(root))?;
    std::fs::write(output_path, yaml_str)?;
    if !theme::is_quiet() {
        eprintln!("Wrote Zuul output to {}", output_path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Hint helpers
// ---------------------------------------------------------------------------

/// Hint shown after a failing check run to suggest how to auto-fix.
///
/// Translates the current `check` invocation into an equivalent `fix`
/// invocation by replacing the `check` subcommand with `fix` and stripping
/// flags that only exist on `check`. When `include_unsafe_fixes` is `true`
/// (i.e. some remaining violations are only fixable via `--unsafe-fixes`),
/// that flag is appended too, so the suggested command actually fixes
/// everything it can rather than silently leaving unsafe fixes behind.
pub fn format_fix_hint(args: &[String], include_unsafe_fixes: bool) -> String {
    // Flags that only exist on the `check` subcommand and have no meaning
    // on `fix`.  Value-taking flags (all except the two booleans) require
    // their following argument to be dropped as well.
    const VALUE_FLAGS: &[&str] = &[
        "--level",
        "--changed-files-level",
        "--output-path",
        "--since-ref",
    ];
    const BOOL_FLAGS: &[&str] = &["--no-cache", "--fix", "--fix-only", "--unsafe-fixes"];

    let mut fix_argv: Vec<String> =
        vec!["konform".to_owned(), "check".to_owned(), "--fix".to_owned()];
    if include_unsafe_fixes {
        fix_argv.push("--unsafe-fixes".to_owned());
    }
    let mut skip_next = false;

    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        // Drop "check" — the hint already starts with "konform check --fix".
        if arg == "check" {
            continue;
        }
        if BOOL_FLAGS.contains(&arg.as_str()) {
            continue;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            skip_next = true;
            continue;
        }
        if VALUE_FLAGS
            .iter()
            .any(|f| arg.starts_with(&format!("{f}=")))
        {
            continue;
        }
        fix_argv.push(arg.clone());
    }

    let pal = theme::palette(colors_enabled());
    let fix_str = fix_argv.join(" ");
    format!(
        "{} To auto-fix violations run: {}",
        pal.hint_label("hint:"),
        pal.hint_cmd(&fix_str),
    )
}

// ---------------------------------------------------------------------------
// Alternate output renderers
// ---------------------------------------------------------------------------

/// Concise single-line renderer: `file:line:col: level[RULE] message`.
fn print_violations_concise(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    pal: &theme::Palette,
    changed_files: &ChangedFiles,
    level: Level,
    changed_files_level: Level,
) -> i32 {
    if theme::is_silent() {
        return exit_code_for(reported, changed_files, level, changed_files_level);
    }

    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();

    for file_path in &paths {
        let mut sorted: Vec<&serde_json::Value> = reported[*file_path].iter().collect();
        sorted.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));

        for v in sorted {
            let line = v["line"].as_u64().unwrap_or(0);
            let col = v["range"]["start_character"].as_u64().unwrap_or(1);
            let raw_msg = v["message"].as_str().unwrap_or("");
            let vlevel = v["level"].as_str().unwrap_or("error");
            let rule_code = v["rule"].as_str().unwrap_or("UNKNOWN");
            let fixable = v["fixable"].as_bool().unwrap_or(false);

            let is_error = vlevel == "error";
            let level_str = if is_error {
                pal.error("error")
            } else {
                pal.warning("warning")
            };
            let code_str = pal.rule_brackets(&format!("[{rule_code}]"), is_error);
            let fix_tag = if fixable {
                format!("[{}]", pal.fixable_star())
            } else {
                String::new()
            };
            let msg = raw_msg
                .strip_prefix(&format!("{rule_code}: "))
                .unwrap_or(raw_msg);

            eprintln!("{file_path}:{line}:{col}: {level_str}{code_str}{fix_tag} {msg}");
        }
    }

    exit_code_for(reported, changed_files, level, changed_files_level)
}

/// JSON renderer — writes a JSON array to **stdout**.
fn print_violations_json(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
    level: Level,
    changed_files_level: Level,
) -> i32 {
    if theme::is_silent() {
        return exit_code_for(reported, changed_files, level, changed_files_level);
    }

    let mut entries: Vec<serde_json::Value> = Vec::new();

    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();

    for file_path in &paths {
        let mut sorted: Vec<&serde_json::Value> = reported[*file_path].iter().collect();
        sorted.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));

        for v in sorted {
            entries.push(serde_json::json!({
                "filename": file_path,
                "line":     v["line"],
                "col":      v["range"]["start_character"].as_u64().unwrap_or(1),
                "end_line": v["range"]["end_line"],
                "rule":     v["rule"],
                "message":  v["message"].as_str().unwrap_or("")
                                .strip_prefix(&format!("{}: ", v["rule"].as_str().unwrap_or("")))
                                .unwrap_or(v["message"].as_str().unwrap_or("")),
                "level":    v["level"],
                "fixable":  v["fixable"],
            }));
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    );
    exit_code_for(reported, changed_files, level, changed_files_level)
}

/// Shared exit-code computation used by all renderers.
fn exit_code_for(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
    level: Level,
    changed_files_level: Level,
) -> i32 {
    let mut code = 0i32;
    for (file_path, violations) in reported {
        let threshold = if changed_files.contains(file_path) {
            changed_files_level
        } else {
            level
        };
        for v in violations {
            let vl: Level = v
                .get("level")
                .and_then(|l| l.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(Level::Warning);
            if vl >= threshold {
                code = 1;
            }
        }
    }
    code
}

/// Print a per-rule violation count table (shown with `--statistics`).
///
/// `rule_names` maps rule code → human-readable name.
/// Render GitHub Actions workflow-command annotations.
///
/// Each violation becomes a `::error` or `::warning` command that GitHub
/// Actions picks up as a PR annotation.  Written to **stdout**.
pub fn render_github(reported: &HashMap<String, Vec<serde_json::Value>>) -> String {
    let mut out = String::new();
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();
    for path in paths {
        let mut viols: Vec<&serde_json::Value> = reported[path].iter().collect();
        viols.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));
        for v in viols {
            let line = v["line"].as_u64().unwrap_or(1);
            let col = v["range"]["start_character"].as_u64().unwrap_or(1);
            let rule = v["rule"].as_str().unwrap_or("UNKNOWN");
            let raw_msg = v["message"].as_str().unwrap_or("");
            let msg = raw_msg
                .strip_prefix(&format!("{rule}: "))
                .unwrap_or(raw_msg);
            let level = if v["level"].as_str().unwrap_or("error") == "warning" {
                "warning"
            } else {
                "error"
            };
            out.push_str(&format!(
                "::{level} file={path},line={line},col={col},title={rule}::{msg}\n"
            ));
        }
    }
    out
}

/// Render a GitLab Code Quality JSON report.
///
/// See <https://docs.gitlab.com/ee/ci/testing/code_quality.html#implement-a-custom-tool>.
/// Written to **stdout**.
pub fn render_gitlab(reported: &HashMap<String, Vec<serde_json::Value>>) -> String {
    use seahash::SeaHasher;
    use std::hash::{Hash, Hasher};

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();
    for path in paths {
        let mut viols: Vec<&serde_json::Value> = reported[path].iter().collect();
        viols.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));
        for v in viols {
            let line = v["line"].as_u64().unwrap_or(1);
            let rule = v["rule"].as_str().unwrap_or("UNKNOWN");
            let raw_msg = v["message"].as_str().unwrap_or("");
            let msg = raw_msg
                .strip_prefix(&format!("{rule}: "))
                .unwrap_or(raw_msg);
            let severity = if v["level"].as_str().unwrap_or("error") == "warning" {
                "minor"
            } else {
                "critical"
            };
            // Stable fingerprint: SeaHash of path + rule + line.
            let mut h = SeaHasher::new();
            path.hash(&mut h);
            rule.hash(&mut h);
            line.hash(&mut h);
            let fingerprint = format!("{:016x}", h.finish());
            entries.push(serde_json::json!({
                "description": msg,
                "fingerprint": fingerprint,
                "severity": severity,
                "location": {
                    "path": path,
                    "lines": { "begin": line }
                }
            }));
        }
    }
    serde_json::to_string_pretty(&entries).unwrap_or_default()
}

/// Render a SARIF 2.1.0 JSON report.
///
/// See <https://docs.oasis-open.org/sarif/sarif/v2.1.0/sarif-v2.1.0.html>.
/// Written to **stdout**.
pub fn render_sarif(reported: &HashMap<String, Vec<serde_json::Value>>) -> String {
    use std::collections::BTreeSet;

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut rule_ids: BTreeSet<String> = BTreeSet::new();
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();
    for path in paths {
        let mut viols: Vec<&serde_json::Value> = reported[path].iter().collect();
        viols.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));
        for v in viols {
            let line = v["line"].as_u64().unwrap_or(1);
            let col = v["range"]["start_character"].as_u64().unwrap_or(1);
            let rule = v["rule"].as_str().unwrap_or("UNKNOWN");
            let raw_msg = v["message"].as_str().unwrap_or("");
            let msg = raw_msg
                .strip_prefix(&format!("{rule}: "))
                .unwrap_or(raw_msg);
            let level = v["level"].as_str().unwrap_or("error");
            rule_ids.insert(rule.to_owned());
            results.push(serde_json::json!({
                "ruleId": rule,
                "level": level,
                "message": { "text": msg },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": path, "uriBaseId": "%SRCROOT%" },
                        "region": { "startLine": line, "startColumn": col }
                    }
                }]
            }));
        }
    }
    let rules: Vec<serde_json::Value> = rule_ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "shortDescription": { "text": format!("konform {id}") },
                "helpUri": concat!(env!("CARGO_PKG_REPOSITORY"), "#rules")
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "konform",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": env!("CARGO_PKG_REPOSITORY"),
                    "rules": rules
                }
            },
            "results": results
        }]
    }))
    .unwrap_or_default()
}

/// Render a JUnit XML report.
///
/// Each violation becomes a `<testcase>` with a `<failure>` child.
/// Written to **stdout**.
pub fn render_junit(reported: &HashMap<String, Vec<serde_json::Value>>) -> String {
    let total: usize = reported.values().map(|v| v.len()).sum();
    let mut cases = String::new();
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();
    for path in paths {
        let mut viols: Vec<&serde_json::Value> = reported[path].iter().collect();
        viols.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));
        for v in viols {
            let line = v["line"].as_u64().unwrap_or(1);
            let col = v["range"]["start_character"].as_u64().unwrap_or(1);
            let rule = v["rule"].as_str().unwrap_or("UNKNOWN");
            let raw_msg = v["message"].as_str().unwrap_or("");
            let msg = raw_msg
                .strip_prefix(&format!("{rule}: "))
                .unwrap_or(raw_msg);
            let classname = path.replace('/', ".").trim_matches('.').to_owned();
            let msg_esc = xml_escape(msg);
            cases.push_str(&format!(
                "    <testcase name=\"{path}:{line}:{col}\" classname=\"{classname}\">\n\
                       <failure message=\"{msg_esc}\" type=\"{rule}\"/>\n\
                     </testcase>\n"
            ));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <testsuites>\n\
           <testsuite name=\"konform\" tests=\"{total}\" failures=\"{total}\" errors=\"0\">\n\
         {cases}\
           </testsuite>\n\
         </testsuites>\n"
    )
}

/// Escape special XML characters.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Return the rendered output for `--output-file`, using the selected format.
///
/// All CI formats render to a string here; `Full`, `Concise` and `Zuul` fall
/// back to JSON since they have no returnable string form.
pub fn render_for_file(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    format: OutputFormat,
) -> String {
    match format {
        OutputFormat::Json => {
            // Reuse the same JSON shape as print_violations_json.
            let entries: Vec<serde_json::Value> = {
                let mut paths: Vec<&String> = reported.keys().collect();
                paths.sort();
                paths
                    .into_iter()
                    .flat_map(|path| {
                        let mut viols: Vec<&serde_json::Value> = reported[path].iter().collect();
                        viols.sort_by_key(|v| v["line"].as_u64().unwrap_or(0));
                        viols.into_iter().map(move |v| {
                            let rule = v["rule"].as_str().unwrap_or("");
                            let raw = v["message"].as_str().unwrap_or("");
                            let msg = raw.strip_prefix(&format!("{rule}: ")).unwrap_or(raw);
                            serde_json::json!({
                                "filename": path,
                                "line":     v["line"],
                                "col":      v["range"]["start_character"].as_u64().unwrap_or(1),
                                "rule":     rule,
                                "message":  msg,
                                "level":    v["level"],
                                "fixable":  v["fixable"],
                            })
                        })
                    })
                    .collect()
            };
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        }
        OutputFormat::Github => render_github(reported),
        OutputFormat::Gitlab => render_gitlab(reported),
        OutputFormat::Sarif => render_sarif(reported),
        OutputFormat::Junit => render_junit(reported),
        // Full/Concise stream to stderr and Zuul is written to `--output-path`
        // (it needs the changed-file routing) — fall back to JSON for file output.
        OutputFormat::Full | OutputFormat::Concise | OutputFormat::Zuul => {
            render_for_file(reported, OutputFormat::Json)
        }
    }
}

pub fn print_statistics(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    rule_names: &HashMap<String, String>,
) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for vs in reported.values() {
        for v in vs {
            let code = v["rule"].as_str().unwrap_or("UNKNOWN").to_owned();
            *counts.entry(code).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return;
    }
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    eprintln!();
    for (code, count) in &sorted {
        let name = rule_names
            .get(code.as_str())
            .map(String::as_str)
            .unwrap_or("");
        eprintln!("{count:>4}  {code:<10}  {name}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Zuul output ────────────────────────────────────────────────────────

    fn zuul_viol(line: u64, message: &str) -> serde_json::Value {
        serde_json::json!({"line": line, "message": message, "help": "do better"})
    }

    fn zuul_reported() -> HashMap<String, Vec<serde_json::Value>> {
        HashMap::from([
            ("changed.py".to_owned(), vec![zuul_viol(3, "bad import")]),
            (
                "other.py".to_owned(),
                vec![zuul_viol(7, "old issue"), zuul_viol(9, "older")],
            ),
        ])
    }

    fn changed(files: &[&str]) -> ChangedFiles {
        ChangedFiles {
            files: files.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    fn read_zuul(path: &Path) -> serde_json::Value {
        let yaml = std::fs::read_to_string(path).unwrap();
        serde_yaml::from_str::<serde_json::Value>(&yaml).unwrap()["data"]["zuul"].clone()
    }

    #[test]
    fn zuul_routes_changed_files_to_comments_and_the_rest_to_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        emit_zuul_return(&out, &zuul_reported(), &changed(&["changed.py"])).unwrap();

        let zuul = read_zuul(&out);
        let comments = &zuul["file_comments"];
        assert_eq!(comments.as_object().unwrap().len(), 1);
        // `help` is folded into the message, Zuul has no such field.
        assert_eq!(comments["changed.py"][0]["message"], "bad import do better");
        assert!(comments["changed.py"][0].get("help").is_none());
        assert_eq!(
            zuul["warnings"],
            serde_json::json!(["other.py:7: old issue", "other.py:9: older"])
        );
    }

    #[test]
    fn zuul_without_changed_files_only_warns() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested/dir/zuul_return.yaml");
        emit_zuul_return(&out, &zuul_reported(), &changed(&[])).unwrap();

        let zuul = read_zuul(&out);
        assert!(zuul.get("file_comments").is_none());
        assert_eq!(zuul["warnings"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn zuul_merges_into_existing_content_and_replaces_its_own_keys() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        std::fs::write(
            &out,
            "data:\n  other: keep\n  zuul:\n    warnings: [stale]\n    extra: keep\n",
        )
        .unwrap();
        emit_zuul_return(&out, &zuul_reported(), &changed(&["changed.py"])).unwrap();

        let yaml: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(yaml["data"]["other"], "keep");
        assert_eq!(yaml["data"]["zuul"]["extra"], "keep");
        assert_eq!(
            yaml["data"]["zuul"]["warnings"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn zuul_with_no_violations_writes_an_empty_zuul_table() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        emit_zuul_return(&out, &HashMap::new(), &changed(&[])).unwrap();
        assert_eq!(read_zuul(&out), serde_json::json!({}));
    }

    #[test]
    fn zuul_rejects_a_non_object_data_section() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        std::fs::write(&out, "data: 3\n").unwrap();
        assert!(emit_zuul_return(&out, &zuul_reported(), &changed(&[])).is_err());
    }

    #[test]
    fn zuul_rejects_a_non_object_zuul_section() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        std::fs::write(&out, "data:\n  zuul: 3\n").unwrap();
        assert!(emit_zuul_return(&out, &zuul_reported(), &changed(&[])).is_err());
    }

    #[test]
    fn zuul_or_report_returns_whether_the_report_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("zuul_return.yaml");
        assert!(emit_zuul_return_or_report(
            &ok,
            &zuul_reported(),
            &changed(&[])
        ));
        // A directory can't be written as a file.
        assert!(!emit_zuul_return_or_report(
            dir.path(),
            &zuul_reported(),
            &changed(&[])
        ));
    }

    #[test]
    fn zuul_format_prints_nothing_and_follows_the_level_for_the_exit_code() {
        let reported = HashMap::from([(
            "a.py".to_owned(),
            vec![
                serde_json::json!({"line": 1, "message": "m", "rule": "KIS001", "level": "error"}),
            ],
        )]);
        let code = print_violations(
            &reported,
            &changed(&[]),
            Level::Error,
            Level::Error,
            OutputFormat::Zuul,
            &[],
        );
        assert_eq!(code, 1);
        // `--output-file` falls back to JSON for this format.
        assert!(render_for_file(&reported, OutputFormat::Zuul).contains("\"filename\""));
    }

    #[test]
    fn colours_are_off_without_a_terminal() {
        // Not a TTY: never coloured under the default (auto) preference.
        assert!(!colors_enabled_for(false));
        // Depends on how the tests are run (captured vs. a terminal).
        let _ = stdout_colors_enabled();
    }

    // ── rule_category ──────────────────────────────────────────────────────

    #[test]
    fn rule_category_strips_digits() {
        assert_eq!(rule_category("KIS001"), "KIS");
        assert_eq!(rule_category("PT001"), "PT");
        assert_eq!(rule_category("FMIS001"), "FMIS");
    }

    #[test]
    fn rule_category_all_alpha_unchanged() {
        assert_eq!(rule_category("UNKNOWN"), "UNKNOWN");
        assert_eq!(rule_category("KIS"), "KIS");
    }

    // ── format_fix_hint ────────────────────────────────────────────────────

    #[test]
    fn fix_hint_replaces_check_subcommand() {
        let hint = format_fix_hint(
            &["check".into(), "--all-files".into(), "src/".into()],
            false,
        );
        assert!(
            hint.contains("konform check --fix --all-files src/"),
            "got: {hint}"
        );
    }

    #[test]
    fn fix_hint_drops_level_flag_and_value() {
        let hint = format_fix_hint(
            &[
                "check".into(),
                "--level".into(),
                "error".into(),
                "src/".into(),
            ],
            false,
        );
        assert!(!hint.contains("--level"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_drops_level_equals_form() {
        let hint = format_fix_hint(
            &["check".into(), "--level=error".into(), "src/".into()],
            false,
        );
        assert!(!hint.contains("--level"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_drops_bool_flags() {
        let hint = format_fix_hint(
            &[
                "check".into(),
                "--no-cache".into(),
                "--fix".into(),
                "src/".into(),
            ],
            false,
        );
        assert!(!hint.contains("--no-cache"), "got: {hint}");
        // --fix is already baked into the prefix; the original --fix arg
        // must be stripped so it is not duplicated.
        assert!(!hint.contains("--fix --fix"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_preserves_common_flags() {
        let hint = format_fix_hint(
            &[
                "check".into(),
                "--select".into(),
                "KIS".into(),
                "--all-files".into(),
                "src/".into(),
            ],
            false,
        );
        assert!(hint.contains("--select KIS"), "got: {hint}");
        assert!(hint.contains("--all-files"), "got: {hint}");
        assert!(hint.contains("src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_no_subcommand_in_raw_args() {
        let hint = format_fix_hint(&["src/".into()], false);
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_appends_unsafe_fixes_when_requested() {
        let hint = format_fix_hint(&["check".into(), "src/".into()], true);
        assert!(
            hint.contains("konform check --fix --unsafe-fixes src/"),
            "got: {hint}"
        );
    }

    #[test]
    fn fix_hint_does_not_duplicate_unsafe_fixes_flag() {
        let hint = format_fix_hint(
            &["check".into(), "--unsafe-fixes".into(), "src/".into()],
            true,
        );
        assert_eq!(hint.matches("--unsafe-fixes").count(), 1, "got: {hint}");
    }

    // ── Machine-readable writers: shared fixtures ──────────────────────────

    use crate::types::Violation;
    use std::collections::HashSet;

    /// Build a violation. `col` is 0-based like `Violation::col`; the wire
    /// format (`to_json`) exposes it 1-based as `range.start_character`.
    fn viol(file: &str, line: usize, col: usize, code: &str, level: Level, msg: &str) -> Violation {
        // `file` is only used by `reported()` to key the map; keep it in the
        // signature so call sites read naturally.
        let _ = file;
        Violation {
            rule: code.to_owned(),
            line,
            col,
            end_line: line,
            end_col: col + 1,
            message: format!("{code}: {msg}"),
            help: None,
            level,
            fixable: false,
        }
    }

    /// Group violations by file into the `reported` map the writers consume.
    fn reported(items: &[(&str, Violation)]) -> HashMap<String, Vec<serde_json::Value>> {
        let mut map: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        for (file, v) in items {
            map.entry((*file).to_owned()).or_default().push(v.to_json());
        }
        map
    }

    fn sample() -> HashMap<String, Vec<serde_json::Value>> {
        reported(&[
            (
                "src/a.py",
                viol("src/a.py", 3, 4, "KIS001", Level::Error, "bad import"),
            ),
            (
                "src/a.py",
                viol(
                    "src/a.py",
                    10,
                    0,
                    "KIS001",
                    Level::Error,
                    "another bad import",
                ),
            ),
            (
                "src/b.py",
                viol("src/b.py", 7, 2, "KPT002", Level::Warning, "pattern hit"),
            ),
        ])
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("invalid JSON ({e}):\n{s}"))
    }

    // ── SARIF ──────────────────────────────────────────────────────────────

    #[test]
    fn sarif_header_and_driver_metadata() {
        let doc = parse(&render_sarif(&sample()));
        assert_eq!(doc["version"], "2.1.0");
        assert_eq!(doc["$schema"], "https://json.schemastore.org/sarif-2.1.0");
        let driver = &doc["runs"][0]["tool"]["driver"];
        assert_eq!(driver["name"], "konform");
        assert_eq!(driver["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(driver["informationUri"], env!("CARGO_PKG_REPOSITORY"));
    }

    #[test]
    fn sarif_help_uris_point_at_package_repository() {
        let doc = parse(&render_sarif(&sample()));
        let rules = doc["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap();
        assert!(!rules.is_empty());
        for r in rules {
            let uri = r["helpUri"].as_str().unwrap();
            assert!(
                uri.starts_with(env!("CARGO_PKG_REPOSITORY")),
                "helpUri {uri} does not start with the package repository"
            );
        }
    }

    #[test]
    fn sarif_results_carry_location() {
        let doc = parse(&render_sarif(&sample()));
        let results = doc["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 3, "one result per violation");

        // Sorted by path, then line.
        let first = &results[0];
        assert_eq!(first["ruleId"], "KIS001");
        assert_eq!(first["level"], "error");
        assert_eq!(
            first["message"]["text"], "bad import",
            "rule prefix stripped"
        );
        let loc = &first["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "src/a.py");
        assert_eq!(loc["region"]["startLine"], 3);
        assert_eq!(loc["region"]["startColumn"], 5, "SARIF columns are 1-based");

        let last = &results[2];
        assert_eq!(last["ruleId"], "KPT002");
        assert_eq!(last["level"], "warning");
        assert_eq!(
            last["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/b.py"
        );
    }

    #[test]
    fn sarif_rules_deduplicated_per_rule_id() {
        let doc = parse(&render_sarif(&sample()));
        let ids: Vec<&str> = doc["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["KIS001", "KPT002"]);
    }

    #[test]
    fn sarif_empty_input_has_empty_results() {
        let doc = parse(&render_sarif(&HashMap::new()));
        assert_eq!(doc["version"], "2.1.0");
        assert_eq!(doc["runs"][0]["results"], serde_json::json!([]));
        assert_eq!(
            doc["runs"][0]["tool"]["driver"]["rules"],
            serde_json::json!([])
        );
    }

    // ── JUnit ──────────────────────────────────────────────────────────────

    /// Minimal well-formedness check (no XML crate available): every
    /// element is closed in order and every `&` starts a known entity.
    fn assert_well_formed_xml(xml: &str) {
        let body = xml
            .strip_prefix("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")
            .expect("XML declaration");
        let mut stack: Vec<String> = Vec::new();
        let mut rest = body;
        while let Some(start) = rest.find('<') {
            let end = rest[start..].find('>').expect("unterminated tag") + start;
            let tag = &rest[start + 1..end];
            assert!(!tag.contains('<'), "raw '<' inside tag: {tag}");
            if let Some(name) = tag.strip_prefix('/') {
                assert_eq!(stack.pop().as_deref(), Some(name), "mismatched </{name}>");
            } else if !tag.ends_with('/') {
                stack.push(tag.split_whitespace().next().unwrap().to_owned());
            }
            rest = &rest[end + 1..];
        }
        assert!(stack.is_empty(), "unclosed elements: {stack:?}");
        for (i, _) in xml.match_indices('&') {
            let tail = &xml[i..];
            assert!(
                ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
                    .iter()
                    .any(|e| tail.starts_with(e)),
                "bare '&' at byte {i}"
            );
        }
    }

    #[test]
    fn junit_is_well_formed_with_matching_counts() {
        let xml = render_junit(&sample());
        assert_well_formed_xml(&xml);
        assert!(
            xml.contains("<testsuite name=\"konform\" tests=\"3\" failures=\"3\" errors=\"0\">")
        );
        assert_eq!(xml.matches("<testcase ").count(), 3);
        assert_eq!(xml.matches("<failure ").count(), 3);
    }

    #[test]
    fn junit_testcase_carries_location_and_message() {
        let xml = render_junit(&sample());
        assert!(xml.contains("<testcase name=\"src/a.py:3:5\" classname=\"src.a.py\">"));
        assert!(xml.contains("<failure message=\"bad import\" type=\"KIS001\"/>"));
        assert!(xml.contains("name=\"src/b.py:7:3\""));
    }

    #[test]
    fn junit_escapes_xml_special_chars_in_message() {
        let r = reported(&[(
            "x.py",
            viol("x.py", 1, 0, "KIS001", Level::Error, r#"a < b & "c" > 'd'"#),
        )]);
        let xml = render_junit(&r);
        assert_well_formed_xml(&xml);
        assert!(
            xml.contains("message=\"a &lt; b &amp; &quot;c&quot; &gt; &apos;d&apos;\""),
            "got:\n{xml}"
        );
    }

    #[test]
    fn junit_empty_input_is_well_formed() {
        let xml = render_junit(&HashMap::new());
        assert_well_formed_xml(&xml);
        assert!(xml.contains("tests=\"0\" failures=\"0\""));
        assert!(!xml.contains("<testcase"));
    }

    #[test]
    fn junit_path_is_not_escaped() {
        // NOTE: documents existing behaviour; see report.
        // Only the message goes through `xml_escape`; the file path is
        // interpolated raw into the `name`/`classname` attributes, so a path
        // containing `&` yields malformed XML.
        let r = reported(&[("a&b.py", viol("a&b.py", 1, 0, "KIS001", Level::Error, "m"))]);
        let xml = render_junit(&r);
        assert!(xml.contains("name=\"a&b.py:1:1\""), "got:\n{xml}");
    }

    // ── GitLab Code Quality ────────────────────────────────────────────────

    #[test]
    fn gitlab_entries_have_required_fields() {
        let doc = parse(&render_gitlab(&sample()));
        let entries = doc.as_array().expect("top-level array");
        assert_eq!(entries.len(), 3);
        let e = &entries[0];
        assert_eq!(e["description"], "bad import");
        assert_eq!(e["location"]["path"], "src/a.py");
        assert_eq!(e["location"]["lines"]["begin"], 3);
        assert_eq!(e["fingerprint"].as_str().unwrap().len(), 16);
    }

    #[test]
    fn gitlab_has_no_check_name() {
        // NOTE: documents existing behaviour; see report.
        // GitLab's Code Quality schema lists `check_name` as required; the
        // writer does not emit it, so the rule code is absent from entries.
        let doc = parse(&render_gitlab(&sample()));
        for e in doc.as_array().unwrap() {
            assert!(e.get("check_name").is_none());
        }
    }

    #[test]
    fn gitlab_severity_mapping() {
        let doc = parse(&render_gitlab(&sample()));
        let sev: Vec<&str> = doc
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["severity"].as_str().unwrap())
            .collect();
        // error → "critical", warning → "minor".
        assert_eq!(sev, vec!["critical", "critical", "minor"]);
    }

    #[test]
    fn gitlab_fingerprints_unique_and_stable() {
        let first = parse(&render_gitlab(&sample()));
        let second = parse(&render_gitlab(&sample()));
        let fps = |d: &serde_json::Value| -> Vec<String> {
            d.as_array()
                .unwrap()
                .iter()
                .map(|e| e["fingerprint"].as_str().unwrap().to_owned())
                .collect()
        };
        let a = fps(&first);
        assert_eq!(a, fps(&second), "fingerprints must be stable across runs");
        let unique: HashSet<&String> = a.iter().collect();
        assert_eq!(unique.len(), a.len(), "fingerprints must differ: {a:?}");
    }

    #[test]
    fn gitlab_fingerprint_collides_on_same_line_and_rule() {
        // NOTE: documents existing behaviour; see report.
        // The fingerprint hashes only path + rule + line, so two hits of the
        // same rule on one line (different columns/messages) collide and
        // GitLab will de-duplicate them into one finding.
        let r = reported(&[
            ("x.py", viol("x.py", 5, 0, "KIS001", Level::Error, "first")),
            (
                "x.py",
                viol("x.py", 5, 20, "KIS001", Level::Error, "second"),
            ),
        ]);
        let doc = parse(&render_gitlab(&r));
        let e = doc.as_array().unwrap();
        assert_eq!(e[0]["fingerprint"], e[1]["fingerprint"]);
    }

    #[test]
    fn gitlab_empty_input_is_empty_array() {
        assert_eq!(
            parse(&render_gitlab(&HashMap::new())),
            serde_json::json!([])
        );
    }

    // ── GitHub Actions annotations ─────────────────────────────────────────

    #[test]
    fn github_one_annotation_per_violation() {
        let out = render_github(&sample());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec![
                "::error file=src/a.py,line=3,col=5,title=KIS001::bad import",
                "::error file=src/a.py,line=10,col=1,title=KIS001::another bad import",
                "::warning file=src/b.py,line=7,col=3,title=KPT002::pattern hit",
            ]
        );
    }

    #[test]
    fn github_empty_input_renders_nothing() {
        assert_eq!(render_github(&HashMap::new()), "");
    }

    #[test]
    fn github_message_newlines_not_escaped() {
        // NOTE: documents existing behaviour; see report.
        // GitHub workflow commands require `%`, `\r`, `\n` in the message to
        // be encoded as `%25`, `%0D`, `%0A`; the writer passes them through,
        // so a multi-line message truncates the annotation.
        let r = reported(&[(
            "x.py",
            viol("x.py", 1, 0, "KIS001", Level::Error, "line1\nline2 100%"),
        )]);
        let out = render_github(&r);
        assert_eq!(out.lines().count(), 2, "got: {out:?}");
        assert!(out.contains("100%"));
    }

    // ── JSON ───────────────────────────────────────────────────────────────

    #[test]
    fn json_entries_have_expected_keys() {
        let doc = parse(&render_for_file(&sample(), OutputFormat::Json));
        let entries = doc.as_array().expect("top-level array");
        assert_eq!(entries.len(), 3);
        let e = entries[0].as_object().unwrap();
        let mut keys: Vec<&str> = e.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["col", "filename", "fixable", "level", "line", "message", "rule"]
        );
        assert_eq!(e["filename"], "src/a.py");
        assert_eq!(e["line"], 3);
        assert_eq!(e["col"], 5);
        assert_eq!(e["rule"], "KIS001");
        assert_eq!(e["message"], "bad import");
        assert_eq!(e["level"], "error");
        assert_eq!(e["fixable"], false);
    }

    #[test]
    fn json_empty_input_is_empty_array() {
        let out = render_for_file(&HashMap::new(), OutputFormat::Json);
        assert_eq!(out, "[]");
    }

    #[test]
    fn json_stdout_writer_exit_code() {
        // `print_violations_json` streams to stdout; only its exit code is
        // observable here.
        let changed = ChangedFiles {
            files: HashSet::new(),
        };
        assert_eq!(
            print_violations_json(&HashMap::new(), &changed, Level::Error, Level::Error),
            0
        );
        assert_eq!(
            print_violations_json(&sample(), &changed, Level::Error, Level::Error),
            1
        );
    }

    #[test]
    fn concise_and_full_fall_back_to_json_for_file_output() {
        let json = render_for_file(&sample(), OutputFormat::Json);
        assert_eq!(render_for_file(&sample(), OutputFormat::Concise), json);
        assert_eq!(render_for_file(&sample(), OutputFormat::Full), json);
    }

    // ── Zuul ───────────────────────────────────────────────────────────────

    fn read_yaml(path: &Path) -> serde_json::Value {
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn zuul_writes_file_comments_and_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested/zuul_return.yaml");
        write_zuul_return(&out, sample(), vec!["heads up".into()]).unwrap();

        let doc = read_yaml(&out);
        let zuul = &doc["data"]["zuul"];
        assert_eq!(zuul["warnings"], serde_json::json!(["heads up"]));
        let a = zuul["file_comments"]["src/a.py"].as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["line"], 3);
        assert_eq!(a[0]["message"], "KIS001: bad import");
        assert_eq!(zuul["file_comments"]["src/b.py"][0]["level"], "warning");
    }

    #[test]
    fn zuul_appends_help_to_message() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        let mut v = viol("x.py", 1, 0, "KIS001", Level::Error, "msg");
        v.help = Some("do this".into());
        write_zuul_return(&out, reported(&[("x.py", v)]), vec![]).unwrap();

        let c = &read_yaml(&out)["data"]["zuul"]["file_comments"]["x.py"][0];
        assert_eq!(c["message"], "KIS001: msg do this");
        assert!(c.get("help").is_none(), "help key stripped");
    }

    #[test]
    fn zuul_null_help_key_is_kept() {
        // NOTE: documents existing behaviour; see report.
        // `Violation::to_json` always emits `"help": null`; the writer only
        // strips string-valued help, so `help: null` leaks into the Zuul file.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        write_zuul_return(&out, sample(), vec![]).unwrap();
        let c = &read_yaml(&out)["data"]["zuul"]["file_comments"]["src/b.py"][0];
        assert_eq!(c.get("help"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn zuul_merges_into_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        std::fs::write(
            &out,
            "top: keep\n\
             data:\n  other: keep\n  zuul:\n    pause: true\n    \
             file_comments:\n      old.py: []\n",
        )
        .unwrap();

        write_zuul_return(&out, sample(), vec![]).unwrap();

        let doc = read_yaml(&out);
        assert_eq!(doc["top"], "keep", "unrelated top-level key preserved");
        assert_eq!(doc["data"]["other"], "keep", "unrelated data key preserved");
        assert_eq!(
            doc["data"]["zuul"]["pause"], true,
            "unrelated zuul key kept"
        );
        // `file_comments` is replaced wholesale, not merged per file.
        let fc = doc["data"]["zuul"]["file_comments"].as_object().unwrap();
        assert!(fc.get("old.py").is_none());
        assert!(fc.contains_key("src/a.py") && fc.contains_key("src/b.py"));
        assert!(doc["data"]["zuul"].get("warnings").is_none());
    }

    #[test]
    fn zuul_empty_input_leaves_existing_comments() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zuul_return.yaml");
        std::fs::write(
            &out,
            "data:\n  zuul:\n    file_comments:\n      old.py: []\n",
        )
        .unwrap();

        write_zuul_return(&out, HashMap::new(), vec![]).unwrap();

        let doc = read_yaml(&out);
        assert!(doc["data"]["zuul"]["file_comments"]
            .as_object()
            .unwrap()
            .contains_key("old.py"));
    }
}
