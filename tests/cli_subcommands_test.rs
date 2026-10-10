//! Integration coverage for `init`, `clean` and the less common `check`
//! flags (`--diff`, `--add-noqa`, `--show-files`, `--statistics`, output
//! formats, `--output-file`, `--isolated`, stdin input).

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;

/// One safe-fixable KIS001 violation (`join` imported from a module).
const BAD: &str = "from os.path import join\n\nprint(join(\"a\", \"b\"))\n";
/// No violations.
const GOOD: &str = "import os\n\nprint(os.sep)\n";

fn konform(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.current_dir(dir);
    cmd
}

/// `konform check --isolated --no-cache <extra...>` in `dir`.
fn check(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = konform(dir);
    cmd.args(["check", "--isolated", "--no-cache"]).args(extra);
    cmd
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

#[test]
fn init_creates_config_and_patterns_in_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    konform(dir.path())
        .arg("init")
        .assert()
        .success()
        .stderr(contains("Created konform.toml"))
        .stderr(contains("Created konform_patterns.toml"));

    assert!(read(dir.path().join("konform.toml")).contains("# konform.toml"));
    assert!(dir.path().join("konform_patterns.toml").is_file());
}

#[test]
fn init_no_patterns_skips_patterns_file() {
    let dir = tempfile::tempdir().unwrap();
    konform(dir.path())
        .args(["init", "--no-patterns"])
        .assert()
        .success()
        .stderr(contains("konform_patterns.toml").not());

    assert!(dir.path().join("konform.toml").is_file());
    assert!(!dir.path().join("konform_patterns.toml").exists());
}

#[test]
fn init_diff_prints_plan_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    konform(dir.path())
        .args(["init", "--diff"])
        .assert()
        .success()
        .stdout(contains("--- /dev/null"))
        .stdout(contains("konform.toml"))
        .stdout(contains("+# konform.toml"))
        .stdout(contains("konform_patterns.toml"));

    let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(entries.is_empty(), "--diff must not write: {entries:?}");
}

#[test]
fn init_does_not_overwrite_existing_konform_toml_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("konform.toml");
    std::fs::write(&cfg, "# mine\n").unwrap();

    // NOTE: documents existing behaviour: an existing config is a note, not
    // an error, so the exit code is 0.
    konform(dir.path())
        .args(["init", "--no-patterns"])
        .assert()
        .success()
        .stderr(contains(
            "konform.toml already exists. Run with --force to overwrite.",
        ));
    assert_eq!(read(&cfg), "# mine\n");
}

#[test]
fn init_force_overwrites_existing_konform_toml() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("konform.toml");
    std::fs::write(&cfg, "# mine\n").unwrap();

    konform(dir.path())
        .args(["init", "--force", "--no-patterns"])
        .assert()
        .success()
        .stderr(contains("Created konform.toml"));
    let after = read(&cfg);
    assert!(after.contains("# konform.toml"), "not overwritten: {after}");
    assert!(!after.contains("# mine"));
}

#[test]
fn init_appends_tool_konform_to_existing_pyproject() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[project]\nname = \"x\"\n").unwrap();

    konform(dir.path())
        .args(["init", "--no-patterns"])
        .assert()
        .success()
        .stderr(contains("Updated pyproject.toml"));

    let after = read(&pyproject);
    assert!(after.starts_with("[project]\nname = \"x\"\n"), "{after}");
    assert!(after.contains("[tool.konform]"), "{after}");
    assert!(!dir.path().join("konform.toml").exists());

    // A second run sees the section and leaves the file alone.
    konform(dir.path())
        .args(["init", "--no-patterns"])
        .assert()
        .success()
        .stderr(contains("[tool.konform] already in pyproject.toml"));
    assert_eq!(read(&pyproject), after);
}

#[test]
fn init_force_with_pyproject_creates_konform_toml() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[tool.konform]\n").unwrap();

    konform(dir.path())
        .args(["init", "--force", "--no-patterns"])
        .assert()
        .success();
    assert!(dir.path().join("konform.toml").is_file());
    assert_eq!(read(&pyproject), "[tool.konform]\n");
}

#[test]
fn init_registers_konform_codes_as_ruff_external() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[tool.ruff]\nline-length = 100\n").unwrap();

    konform(dir.path())
        .args(["init", "--no-patterns"])
        .assert()
        .success();

    let after = read(&pyproject);
    assert!(after.contains("[tool.ruff.lint]\nexternal = ["), "{after}");
    assert!(after.contains("\"KIS\""), "{after}");
}

// ---------------------------------------------------------------------------
// check --diff
// ---------------------------------------------------------------------------

#[test]
fn check_diff_prints_fix_without_modifying_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    check(dir.path(), &["--diff", "bad.py"])
        .assert()
        .code(1)
        .stdout(contains("--- bad.py"))
        .stdout(contains("-from os.path import join"))
        .stdout(contains("+import os.path"))
        .stdout(contains("+print(os.path.join(\"a\", \"b\"))"));

    assert_eq!(read(dir.path().join("bad.py")), BAD);
}

#[test]
fn check_diff_on_clean_file_exits_zero_with_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("good.py"), GOOD).unwrap();

    check(dir.path(), &["--diff", "good.py"])
        .assert()
        .success()
        .stdout("");
}

// ---------------------------------------------------------------------------
// check --add-noqa
// ---------------------------------------------------------------------------

#[test]
fn add_noqa_annotates_violating_lines_and_merges_existing_codes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(
        &file,
        "from os.path import join\nfrom os.path import sep  # noqa: KPT001\n\nprint(join(\"a\", sep))\n",
    )
    .unwrap();

    // Codes are merged into an existing `# noqa:` list, sorted.
    check(dir.path(), &["--add-noqa", "mod.py"])
        .assert()
        .success();

    assert_eq!(
        read(&file),
        "from os.path import join  # noqa: KIS001\n\
         from os.path import sep  # noqa: KIS001, KPT001\n\
         \n\
         print(join(\"a\", sep))\n"
    );

    // The annotated file is now clean.
    check(dir.path(), &["mod.py"]).assert().success();
}

#[test]
fn add_noqa_from_stdin_writes_annotated_source_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s.py");
    std::fs::write(&file, "from os.path import join\n").unwrap();

    check(dir.path(), &["--add-noqa", "--stdin-filename", "s.py", "-"])
        .write_stdin("from os.path import join\n")
        .assert()
        .success()
        .stdout("from os.path import join  # noqa: KIS001\n");

    assert_eq!(read(&file), "from os.path import join\n");
}

// ---------------------------------------------------------------------------
// check --show-files
// ---------------------------------------------------------------------------

#[test]
fn show_files_lists_python_files_without_linting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();
    std::fs::write(dir.path().join("good.py"), GOOD).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/x.py"), BAD).unwrap();

    let out = check(dir.path(), &["--show-files", "."])
        .assert()
        .success()
        .stderr(contains("KIS001").not())
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    // Paths are printed with the native separator (`sub\x.py` on Windows).
    let files: Vec<String> = stdout.lines().map(|l| l.replace('\\', "/")).collect();
    assert_eq!(files, ["bad.py", "good.py", "sub/x.py"]);
}

#[test]
fn show_files_honours_exclude() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), GOOD).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/x.py"), GOOD).unwrap();

    // Root-relative globs match when walking `.` (the leading `./` is ignored).
    check(dir.path(), &["--show-files", "--exclude", "sub/**", "."])
        .assert()
        .success()
        .stdout("a.py\n");
    check(dir.path(), &["--show-files", "--exclude", "**/sub/**", "."])
        .assert()
        .success()
        .stdout("a.py\n");

    // With explicit file arguments, the root-relative glob does match.
    check(
        dir.path(),
        &["--show-files", "--exclude", "sub/**", "a.py", "sub/x.py"],
    )
    .assert()
    .success()
    .stdout("a.py\n");
}

/// `a.py`, `gen/x.py` and `vendor/y.py` plus a `konform.toml` with `config`.
fn exclude_project(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("konform.toml"), config).unwrap();
    std::fs::write(dir.path().join("a.py"), GOOD).unwrap();
    for sub in ["gen", "vendor"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("gen/x.py"), GOOD).unwrap();
    std::fs::write(dir.path().join("vendor/y.py"), GOOD).unwrap();
    dir
}

fn shown_files(dir: &Path, extra: &[&str]) -> Vec<String> {
    let out = konform(dir)
        .args(["check", "--no-cache", "--show-files"])
        .args(extra)
        .arg(".")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| l.replace('\\', "/"))
        .collect()
}

#[test]
fn config_exclude_and_extend_exclude_skip_files() {
    let dir = exclude_project("exclude = [\"gen/**\"]\nextend-exclude = [\"vendor/**\"]\n");
    assert_eq!(shown_files(dir.path(), &[]), ["a.py"]);
}

#[test]
fn cli_exclude_replaces_config_exclude_but_keeps_extend_exclude() {
    let dir = exclude_project("exclude = [\"gen/**\"]\nextend-exclude = [\"vendor/**\"]\n");
    assert_eq!(
        shown_files(dir.path(), &["--exclude", "a.py"]),
        ["gen/x.py"]
    );
}

#[test]
fn cli_extend_exclude_adds_to_config_exclude() {
    let dir = exclude_project("exclude = [\"gen/**\"]\n");
    assert_eq!(
        shown_files(dir.path(), &["--extend-exclude", "vendor/**"]),
        ["a.py"]
    );
}

#[test]
fn a_bare_directory_name_excludes_everything_below_it() {
    let dir = exclude_project("exclude = [\"vendor\"]\n");
    std::fs::create_dir(dir.path().join("vendor/deep")).unwrap();
    std::fs::write(dir.path().join("vendor/deep/z.py"), GOOD).unwrap();
    assert_eq!(shown_files(dir.path(), &[]), ["a.py", "gen/x.py"]);
    // Also for files named explicitly, as pre-commit does.
    konform(dir.path())
        .args([
            "check",
            "--no-cache",
            "--show-files",
            "a.py",
            "vendor/deep/z.py",
        ])
        .assert()
        .success()
        .stdout("a.py\n");
}

/// pre-commit passes file names explicitly; the config exclude must still apply.
#[test]
fn config_exclude_applies_to_explicit_files() {
    let dir = exclude_project("exclude = [\"gen/**\"]\n");
    konform(dir.path())
        .args(["check", "--no-cache", "--show-files", "a.py", "gen/x.py"])
        .assert()
        .success()
        .stdout("a.py\n");
}

#[test]
fn an_unparsable_config_is_a_hard_error_for_check_and_a_warning_for_rule_list() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform\n").unwrap();
    std::fs::write(dir.path().join("a.py"), BAD).unwrap();
    konform(dir.path())
        .args(["check", "--no-cache", "a.py"])
        .assert()
        .code(2)
        .stderr(contains("invalid TOML"));
    konform(dir.path())
        .args(["rule", "--list"])
        .assert()
        .success()
        .stderr(contains("warning:").and(contains("invalid TOML")));
}

#[test]
fn broken_extend_is_a_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("konform.toml"), "extend = \"nope.toml\"\n").unwrap();
    std::fs::write(dir.path().join("a.py"), GOOD).unwrap();
    konform(dir.path())
        .args(["check", "--no-cache", "a.py"])
        .assert()
        .code(2)
        .stderr(contains("cannot read extended config"));
}

#[test]
fn extend_pulls_settings_from_the_base_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.toml"),
        "[lint]\nignore = [\"KIS001\"]\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("konform.toml"), "extend = \"base.toml\"\n").unwrap();
    std::fs::write(dir.path().join("a.py"), BAD).unwrap();
    konform(dir.path())
        .args(["check", "--no-cache", "a.py"])
        .assert()
        .success()
        .stderr(contains("KIS001").not());
}

#[test]
fn show_settings_prints_the_merged_settings_as_toml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.toml"),
        "[lint]\nselect = [\"KIS\"]\nunfixable = [\"KIS002\"]\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("konform.toml"), "extend = \"base.toml\"\n").unwrap();
    let out = konform(dir.path())
        .args(["check", "--show-settings", "--ignore", "KIS002", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc: toml::Value = toml::from_str(&String::from_utf8(out).unwrap()).unwrap();
    let resolved = &doc["resolved"];
    assert!(resolved["config-file"]
        .as_str()
        .unwrap()
        .ends_with("konform.toml"));
    assert!(resolved["extends"][0]
        .as_str()
        .unwrap()
        .ends_with("base.toml"));
    assert_eq!(resolved["enabled-rules"], toml::Value::from(vec!["KIS001"]));
    let lint = &doc["konform"]["lint"];
    assert_eq!(lint["select"], toml::Value::from(vec!["KIS"]));
    assert_eq!(lint["ignore"], toml::Value::from(vec!["KIS002"]));
    assert_eq!(lint["unfixable"], toml::Value::from(vec!["KIS002"]));
}

#[test]
fn add_noqa_warns_about_lines_that_cannot_take_a_comment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("konform.toml"),
        "[lint.user-defined-patterns]\n[[lint.user-defined-patterns.rules]]\n\
         id = \"KPT901\"\nmessage = \"m\"\npattern = 'secret'\n",
    )
    .unwrap();
    let src = "x = \"\"\"secret\nend\"\"\"\nsecret = 1\n";
    std::fs::write(dir.path().join("a.py"), src).unwrap();
    konform(dir.path())
        .args([
            "check",
            "--no-cache",
            "--select",
            "KPT901",
            "--add-noqa",
            "a.py",
        ])
        .assert()
        .success()
        .stderr(contains("a.py:1: cannot add `# noqa` here"));
    assert_eq!(
        read(dir.path().join("a.py")),
        "x = \"\"\"secret\nend\"\"\"\nsecret = 1  # noqa: KPT901\n"
    );
}

// ---------------------------------------------------------------------------
// check --statistics / --exit-zero / output formats / --output-file
// ---------------------------------------------------------------------------

#[test]
fn statistics_prints_per_rule_counts_after_summary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.py"),
        "from os.path import join\nfrom os.path import sep\n\nprint(join(sep))\n",
    )
    .unwrap();

    let out = check(dir.path(), &["--statistics", "bad.py"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(out).unwrap();
    let summary = stderr.find("Found 2 errors.").expect(&stderr);
    let stats = stderr
        .lines()
        .position(|l| {
            let f: Vec<_> = l.split_whitespace().collect();
            f.first() == Some(&"2") && f.get(1) == Some(&"KIS001")
        })
        .expect(&stderr);
    let stats_offset: usize = stderr.lines().take(stats).map(|l| l.len() + 1).sum();
    assert!(
        stats_offset > summary,
        "stats must follow summary: {stderr}"
    );
}

#[test]
fn exit_zero_exits_zero_despite_violations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    check(dir.path(), &["--exit-zero", "bad.py"])
        .assert()
        .success()
        .stderr(contains("KIS001"));
    check(dir.path(), &["-e", "bad.py"]).assert().success();
}

#[test]
fn concise_output_is_one_line_per_violation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    // Shape: `file:line:col: level[RULE][*] message`.
    check(dir.path(), &["--output-format", "concise", "bad.py"])
        .assert()
        .code(1)
        .stderr(contains(
            "bad.py:1:1: error[KIS001][*] Import 'join' from 'os.path' is not a module.",
        ))
        .stderr(contains("-->").not());
}

#[test]
fn json_output_is_array_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    let out = check(dir.path(), &["--output-format", "json", "bad.py"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let arr = v.as_array().expect("JSON array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["rule"], "KIS001");
    assert_eq!(arr[0]["filename"], "bad.py");
    assert_eq!(arr[0]["line"], 1);
    assert_eq!(arr[0]["fixable"], true);
}

#[test]
fn sarif_output_is_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    let out = check(dir.path(), &["--output-format", "sarif", "bad.py"])
        .assert()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["version"], "2.1.0");
    assert_eq!(v["runs"][0]["results"][0]["ruleId"], "KIS001");
}

#[test]
fn output_file_receives_report() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    check(
        dir.path(),
        &["--output-format", "json", "-o", "out/report.json", "bad.py"],
    )
    .assert()
    .code(1);
    let v: serde_json::Value =
        serde_json::from_str(&read(dir.path().join("out/report.json"))).unwrap();
    assert_eq!(v[0]["rule"], "KIS001");
    assert_eq!(v[0]["filename"], "bad.py");

    // NOTE: documents existing behaviour: `--help` says the report goes to
    // the file *instead of* stderr, but the full report is still printed to
    // stderr, and for `full`/`concise` the file receives JSON, not text.
    check(dir.path(), &["--output-file", "report.txt", "bad.py"])
        .assert()
        .code(1)
        .stderr(contains("error[KIS001]"));
    let v: serde_json::Value = serde_json::from_str(&read(dir.path().join("report.txt"))).unwrap();
    assert_eq!(v[0]["rule"], "KIS001");
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

#[test]
fn clean_removes_cache_dir_created_by_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[tool.konform]\ncache-dir = \".kcache\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("good.py"), GOOD).unwrap();

    konform(dir.path())
        .args(["check", "good.py"])
        .assert()
        .success();
    let cache = dir.path().join(".kcache");
    assert!(cache.is_dir(), "check should have created the cache dir");

    konform(dir.path())
        .arg("clean")
        .assert()
        .success()
        .stderr(contains("Removed cache directory: .kcache"));
    assert!(!cache.exists());

    // Nothing left to clean is not an error.
    konform(dir.path())
        .arg("clean")
        .assert()
        .success()
        .stderr(contains("Cache directory not found"));
}

// ---------------------------------------------------------------------------
// --isolated / stdin
// ---------------------------------------------------------------------------

#[test]
fn isolated_ignores_pyproject_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[tool.konform.lint]\nignore = [\"KIS\"]\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("bad.py"), BAD).unwrap();

    konform(dir.path())
        .args(["check", "--no-cache", "bad.py"])
        .assert()
        .success();
    konform(dir.path())
        .args(["check", "--no-cache", "--isolated", "bad.py"])
        .assert()
        .code(1)
        .stderr(contains("KIS001"));
}

#[test]
fn stdin_reports_violations_under_stdin_filename() {
    let dir = tempfile::tempdir().unwrap();

    check(dir.path(), &["--stdin-filename", "pkg/mod.py", "-"])
        .write_stdin("from os.path import join\n")
        .assert()
        .code(1)
        .stderr(contains("error[KIS001]"))
        .stderr(contains("--> pkg/mod.py:1:1"));

    check(dir.path(), &["--output-format", "concise", "-"])
        .write_stdin("from os.path import join\n")
        .assert()
        .code(1)
        .stderr(contains("<stdin>:1:1: error[KIS001]"));

    check(dir.path(), &["--stdin-filename", "pkg/mod.py", "-"])
        .write_stdin(GOOD)
        .assert()
        .success();
}

#[test]
fn completions_prints_a_script_per_shell() {
    let dir = tempfile::tempdir().unwrap();

    konform(dir.path())
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(contains("_konform"))
        .stdout(contains("--add-noqa"));

    konform(dir.path())
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(contains("#compdef konform"));

    konform(dir.path())
        .args(["completions", "nushell"])
        .assert()
        .failure();
}
