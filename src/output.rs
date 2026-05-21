//! Terminal output formatting and Zuul CI artefact writing.
//!
//! [`print_violations`] drives all stderr output for the `check` subcommand.
//! [`write_zuul_return`] serialises violations to the YAML file consumed by
//! the Zuul CI comment-bot.
//!
//! Colour choices are centralised in [`crate::theme`]; change
//! [`crate::theme::ACTIVE_THEME`] to restyle all output at once.

use crate::theme;
use crate::types::{ChangedFiles, Level};
use std::collections::HashMap;
use std::path::Path;

/// Returns `true` when stderr should emit ANSI colour codes.
///
/// Mirrors ruff / ripgrep behaviour:
/// * Strip colours when stderr is not a TTY.
/// * Always strip when `NO_COLOR` is set (<https://no-color.org>).
/// * Always strip when `TERM=dumb`.
pub fn colors_enabled() -> bool {
    use crate::theme::{ColorWhen, COLOR_PREFERENCE};
    match COLOR_PREFERENCE.get().copied().unwrap_or(ColorWhen::Auto) {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            use std::io::IsTerminal;
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
                return false;
            }
            std::io::stderr().is_terminal()
        }
    }
}

/// Output format for violations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Ruff-style output with arrows and help lines (default).
    #[default]
    Full,
    /// Concise single-line: `file:line:col: [RULE] message`.
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
/// Returns `0` when no violation reaches the configured severity threshold,
/// `1` otherwise.
pub fn print_violations(
    reported: &HashMap<String, Vec<serde_json::Value>>,
    changed_files: &ChangedFiles,
    level: Level,
    changed_files_level: Level,
    output_format: OutputFormat,
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
        OutputFormat::Full => {} // fall through to the full renderer below
    }

    // ── Per-violation blocks ──────────────────────────────────────────────
    // Sort files for deterministic output.
    let mut paths: Vec<&String> = reported.keys().collect();
    paths.sort();

    let mut error_count = 0usize;
    let mut warning_count = 0usize;
    let mut fixable_count = 0usize;

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
                fixable_count += 1;
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

            if fixable_count > 0 {
                eprintln!(
                    "[{}] {} fixable with the `--fix` option.",
                    pal.summary_star(),
                    fixable_count,
                );
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
    eprintln!("Wrote Zuul output to {}", output_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Hint helpers
// ---------------------------------------------------------------------------

/// Hint shown after a failing check run to suggest how to auto-fix.
///
/// Translates the current `check` invocation into an equivalent `fix`
/// invocation by replacing the `check` subcommand with `fix` and stripping
/// flags that only exist on `check`.
pub fn format_fix_hint(args: &[String]) -> String {
    // Flags that only exist on the `check` subcommand and have no meaning
    // on `fix`.  Value-taking flags (all except the two booleans) require
    // their following argument to be dropped as well.
    const VALUE_FLAGS: &[&str] = &[
        "--level",
        "--changed-files-level",
        "--output-path",
        "--since-ref",
    ];
    const BOOL_FLAGS: &[&str] = &["--no-cache", "--fix", "--fix-only"];

    let mut fix_argv: Vec<String> =
        vec!["konform".to_owned(), "check".to_owned(), "--fix".to_owned()];
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
                "helpUri": format!("https://github.com/bziegler/konform#rules")
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
                    "informationUri": "https://github.com/bziegler/konform",
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
/// All CI formats render to a string here; `Full` and `Concise` fall back to
/// JSON since they stream to stderr and have no returnable string form.
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
        // Full/Concise stream to stderr — fall back to JSON for file output.
        OutputFormat::Full | OutputFormat::Concise => render_for_file(reported, OutputFormat::Json),
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
        let hint = format_fix_hint(&["check".into(), "--all-files".into(), "src/".into()]);
        assert!(
            hint.contains("konform check --fix --all-files src/"),
            "got: {hint}"
        );
    }

    #[test]
    fn fix_hint_drops_level_flag_and_value() {
        let hint = format_fix_hint(&[
            "check".into(),
            "--level".into(),
            "error".into(),
            "src/".into(),
        ]);
        assert!(!hint.contains("--level"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_drops_level_equals_form() {
        let hint = format_fix_hint(&["check".into(), "--level=error".into(), "src/".into()]);
        assert!(!hint.contains("--level"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_drops_bool_flags() {
        let hint = format_fix_hint(&[
            "check".into(),
            "--no-cache".into(),
            "--fix".into(),
            "src/".into(),
        ]);
        assert!(!hint.contains("--no-cache"), "got: {hint}");
        // --fix is already baked into the prefix; the original --fix arg
        // must be stripped so it is not duplicated.
        assert!(!hint.contains("--fix --fix"), "got: {hint}");
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_preserves_common_flags() {
        let hint = format_fix_hint(&[
            "check".into(),
            "--select".into(),
            "KIS".into(),
            "--all-files".into(),
            "src/".into(),
        ]);
        assert!(hint.contains("--select KIS"), "got: {hint}");
        assert!(hint.contains("--all-files"), "got: {hint}");
        assert!(hint.contains("src/"), "got: {hint}");
    }

    #[test]
    fn fix_hint_no_subcommand_in_raw_args() {
        let hint = format_fix_hint(&["src/".into()]);
        assert!(hint.contains("konform check --fix src/"), "got: {hint}");
    }
}
