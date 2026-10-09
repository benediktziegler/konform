use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

#[test]
fn test_help_exits_zero() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("--help").assert().success();
}

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("--version").assert().success();
}

#[test]
fn test_version_subcommand() {
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.arg("version").assert().success();
}

/// KIS002's fix is unsafe: a plain `check` run must call that out
/// separately from any safe fixes, both in the summary line and in the
/// suggested fix command, mirroring Ruff's `--fix` / `--unsafe-fixes` split.
#[test]
fn check_reports_unsafe_only_fixable_violations_distinctly() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(
        &file,
        "from xml import etree as xml_etree\n\nprint(xml_etree)\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated"])
        .arg(&file)
        .assert()
        .stderr(contains("fixable only with the `--unsafe-fixes` option"))
        .stderr(contains("--unsafe-fixes"));
}

/// When a file has both a safe-fixable violation (KIS001) and an
/// unsafe-fixable one (KIS002), the summary must break the two counts out
/// separately rather than lumping them into a single `--fix` count.
#[test]
fn check_reports_combined_safe_and_unsafe_fixable_counts() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(
        &file,
        "from os.path import join\nfrom xml import etree as xml_etree\n\nprint(join(str(xml_etree)))\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated"])
        .arg(&file)
        .assert()
        .stderr(contains(
            "1 fixable with the `--fix` option (1 hidden fix can be enabled with the `--unsafe-fixes` option).",
        ));
}

/// `--fix` alone must not touch KIS002's unsafe fix, and the remaining
/// violation must still be reported after the fix pass, with a hint
/// pointing at `--unsafe-fixes`.
#[test]
fn fix_without_unsafe_fixes_leaves_unsafe_violation_unfixed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    let original = "from xml import etree as xml_etree\n\nprint(xml_etree)\n";
    std::fs::write(&file, original).unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated", "--fix"])
        .arg(&file)
        .assert()
        .stderr(contains("fixable only with the `--unsafe-fixes` option"));

    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        after, original,
        "plain --fix must not apply KIS002's unsafe fix"
    );
}

/// `--fix --unsafe-fixes` together must apply KIS002's fix and leave the
/// file clean.
#[test]
fn fix_with_unsafe_fixes_fixes_kis002() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(
        &file,
        "from xml import etree as xml_etree\n\nprint(xml_etree)\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated", "--fix", "--unsafe-fixes"])
        .arg(&file)
        .assert()
        .success();

    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("from xml import etree") && !after.contains("as xml_etree"),
        "expected the alias to be dropped: {after:?}"
    );
    assert!(after.contains("print(etree)"), "got: {after:?}");
}

/// `# noqa: KIS002,` (trailing comma) lists only KIS002; it must not act as
/// a blanket suppression and hide the KIS001 violation on the same line.
#[test]
fn noqa_with_trailing_comma_does_not_suppress_other_codes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(&file, "from os.path import join  # noqa: KIS002,\n").unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated", "--no-cache"])
        .arg(&file)
        .assert()
        .failure()
        .stderr(contains("KIS001"));
}

/// `# noqa` text inside a string literal is not a suppression comment.
#[test]
fn noqa_inside_string_literal_does_not_suppress() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mod.py");
    std::fs::write(&file, "from os.path import join; s = \"# noqa\"\n").unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.args(["check", "--isolated", "--no-cache"])
        .arg(&file)
        .assert()
        .failure()
        .stderr(contains("KIS001"));
}

/// `--ignore KPT001` must silence only the KPT001 pattern, not every
/// user-defined pattern (which all share the `KPT` category).
#[test]
fn ignore_single_kpt_pattern_id_keeps_other_patterns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        r#"
[[tool.konform.lint.user-defined-patterns.rules]]
id = "KPT001"
message = "no print"
pattern = '^\s*print\('

[[tool.konform.lint.user-defined-patterns.rules]]
id = "KPT002"
message = "no breakpoint"
pattern = '^\s*breakpoint\('
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("a.py"), "print(1)\nbreakpoint()\n").unwrap();

    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.current_dir(dir.path())
        .args(["check", "--no-cache", "--ignore", "KPT001", "a.py"])
        .assert()
        .stderr(contains("warning[KPT002]"))
        .stderr(contains("warning[KPT001]").not());
}

/// An invalid pattern regex is reported once per run, not once per file.
#[test]
fn invalid_kpt_regex_is_reported_once_for_many_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("konform_patterns.toml"),
        "[[rules]]\nid = \"KPT030\"\nmessage = \"m\"\npattern = \"(unclosed\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    for i in 1..=5 {
        std::fs::write(dir.path().join(format!("f{i}.py")), format!("x = {i}\n")).unwrap();
    }

    let mut cmd = Command::cargo_bin("konform").unwrap();
    let out = cmd
        .current_dir(dir.path())
        .args(["check", "--no-cache", "."])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("invalid regex").count(), 1, "{stderr}");
}

/// `rule --list` / `--explain` must discover the same config as `check`, so
/// user-defined pattern ids are listed and explainable.
#[test]
fn rule_list_and_explain_include_user_patterns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(
        dir.path().join("konform_patterns.toml"),
        "[[rules]]\nid = \"KPT030\"\nmessage = \"no leading x\"\npattern = \"^x\"\n",
    )
    .unwrap();

    let mut list = Command::cargo_bin("konform").unwrap();
    list.current_dir(dir.path())
        .args(["rule", "--list"])
        .assert()
        .success()
        .stdout(contains("KPT030"))
        .stdout(contains("no leading x"))
        .stdout(contains("KIS001"));

    let mut explain = Command::cargo_bin("konform").unwrap();
    explain
        .current_dir(dir.path())
        .args(["rule", "--explain", "KPT030"])
        .assert()
        .success()
        .stdout(contains("konform_patterns.toml"))
        .stdout(contains("^x"));
}

/// Outside a project with patterns, `rule --list` still shows the built-ins
/// and the generic KPT001 entry.
#[test]
fn rule_list_without_config_shows_builtin_rules() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("konform").unwrap();
    cmd.current_dir(dir.path())
        .args(["rule", "--list"])
        .assert()
        .success()
        .stdout(contains("KIS001"))
        .stdout(contains("KIS002"))
        .stdout(contains("KPT001"));
}

fn run_check_cached(dir: &std::path::Path) -> String {
    let out = Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir)
        .args(["check", "--cache-dir", ".kcache", "b.py"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Editing a rule config table must not replay results cached under the old
/// settings.
#[test]
fn editing_rule_config_table_invalidates_cache() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        "[tool.konform.lint.module-only-imports]\nlevel = \"error\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("b.py"), "from os.path import join\n").unwrap();

    assert!(run_check_cached(dir.path()).contains("error[KIS001]"));
    // Second run: served from cache, same output.
    assert!(run_check_cached(dir.path()).contains("error[KIS001]"));

    std::fs::write(
        &pyproject,
        "[tool.konform.lint.module-only-imports]\nlevel = \"warning\"\n",
    )
    .unwrap();
    let after = run_check_cached(dir.path());
    assert!(after.contains("warning[KIS001]"), "stale cache: {after}");
}

/// Editing a pattern file must not replay results cached under the old
/// pattern, even though the checked `.py` file is unchanged.
#[test]
fn editing_pattern_file_invalidates_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(dir.path().join("b.py"), "x = 1\n").unwrap();
    let patterns = dir.path().join("konform_patterns.toml");
    let write = |msg: &str| {
        std::fs::write(
            &patterns,
            format!("[[rules]]\nid = \"KPT050\"\nmessage = \"{msg}\"\npattern = \"^x\"\n"),
        )
        .unwrap();
    };

    write("old message");
    assert!(run_check_cached(dir.path()).contains("old message"));
    assert!(run_check_cached(dir.path()).contains("old message"));

    write("new message");
    let after = run_check_cached(dir.path());
    assert!(after.contains("new message"), "stale cache: {after}");
    assert!(!after.contains("old message"));
}

/// `docs/rules/` is generated from the rule definitions; regenerate with
/// `cargo run -- rule --generate-docs docs/rules` when this fails.
#[test]
fn generated_rule_docs_are_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("konform")
        .unwrap()
        .args(["rule", "--generate-docs"])
        .arg(dir.path())
        .assert()
        .success();

    let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/rules");
    let names = |p: &std::path::Path| {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(dir.path()),
        names(&committed),
        "docs/rules file set differs"
    );
    for name in names(dir.path()) {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&name)).unwrap(),
            std::fs::read_to_string(committed.join(&name)).unwrap(),
            "docs/rules/{} is stale",
            name.to_string_lossy(),
        );
    }
}

fn run_init(dir: &std::path::Path) {
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir)
        .args(["init", "--no-patterns"])
        .assert()
        .success();
}

/// `init` in a directory with a `pyproject.toml` appends `[tool.konform]` to
/// it, separated from the existing content by exactly one blank line.
#[test]
fn init_appends_to_existing_pyproject_with_blank_line_before() {
    for existing in [
        "[project]\nname = \"x\"\n",
        "[project]\nname = \"x\"",
        "[project]\nname = \"x\"\n\n\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pyproject = dir.path().join("pyproject.toml");
        std::fs::write(&pyproject, existing).unwrap();
        run_init(dir.path());

        let out = std::fs::read_to_string(&pyproject).unwrap();
        assert!(
            out.starts_with("[project]\nname = \"x\"\n\n[tool.konform]\n"),
            "{out:?}"
        );
        assert!(out.ends_with('\n') && !out.ends_with("\n\n"), "{out:?}");
        assert!(!dir.path().join("konform.toml").exists());
    }
}

/// An existing `[tool.konform.lint]` table counts as already configured.
#[test]
fn init_leaves_pyproject_with_konform_subtable_alone() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    let before = "[tool.konform.lint]\nselect = [\"KIS\"]\n";
    std::fs::write(&pyproject, before).unwrap();
    run_init(dir.path());
    assert_eq!(std::fs::read_to_string(&pyproject).unwrap(), before);
}

/// Without any config, `init` creates `konform.toml`.
#[test]
fn init_creates_konform_toml_in_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    run_init(dir.path());
    assert!(dir.path().join("konform.toml").is_file());
}

/// With a ruff config in `pyproject.toml`, `init` puts konform's section (and
/// ruff's `external`) after the last ruff block, not at the end of the file.
#[test]
fn init_inserts_after_all_ruff_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        "[tool.ruff]\nline-length = 100\n\n[tool.ruff.format]\nquote-style = \"double\"\n\n\
         [tool.pytest.ini_options]\naddopts = \"-q\"\n",
    )
    .unwrap();
    run_init(dir.path());

    let out = std::fs::read_to_string(&pyproject).unwrap();
    let pos = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {out}"))
    };
    assert!(pos("[tool.ruff.format]") < pos("[tool.ruff.lint]"));
    assert!(pos("[tool.ruff.lint]") < pos("[tool.konform]"));
    assert!(pos("[tool.konform]") < pos("[tool.pytest.ini_options]"));
    assert!(
        out.contains("quote-style = \"double\"\n\n[tool.ruff.lint]\nexternal = [\"KIS\", \"KPT\"]\n\n[tool.konform]\n"),
        "{out}"
    );
    assert!(
        out.contains("# rules_file = \"konform_patterns.toml\"\n\n[tool.pytest.ini_options]\n"),
        "{out}"
    );
}

fn init_cmd(dir: &std::path::Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir)
        .arg("init")
        .args(args)
        .assert()
        .success()
}

/// `init --diff` prints what it would do and writes nothing.
#[test]
fn init_diff_prints_changes_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    let before = "[tool.ruff]\nline-length = 100\n";
    std::fs::write(&pyproject, before).unwrap();

    init_cmd(dir.path(), &["--diff"])
        .stdout(contains("+[tool.konform]"))
        .stdout(contains("+[tool.ruff.lint]"))
        .stdout(contains("+++ "));
    assert_eq!(std::fs::read_to_string(&pyproject).unwrap(), before);
    assert!(!dir.path().join("konform_patterns.toml").exists());
    assert!(!dir.path().join("konform.toml").exists());
}

/// `--force` creates `konform.toml` even though a `pyproject.toml` exists, and
/// leaves the `pyproject.toml` alone.
#[test]
fn init_force_creates_konform_toml_next_to_pyproject() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    let before = "[project]\nname = \"x\"\n";
    std::fs::write(&pyproject, before).unwrap();

    init_cmd(dir.path(), &["--force", "--no-patterns"]);
    assert!(dir.path().join("konform.toml").is_file());
    assert_eq!(std::fs::read_to_string(&pyproject).unwrap(), before);
}

/// An existing `konform.toml` is kept (with a note) unless `--force` is given.
#[test]
fn init_keeps_existing_konform_toml() {
    let dir = tempfile::tempdir().unwrap();
    let konform_toml = dir.path().join("konform.toml");
    std::fs::write(&konform_toml, "[konform]\n# mine\n").unwrap();

    init_cmd(dir.path(), &["--no-patterns"]).stderr(contains("konform.toml already exists"));
    assert_eq!(
        std::fs::read_to_string(&konform_toml).unwrap(),
        "[konform]\n# mine\n"
    );
}

/// A standalone `ruff.toml` gets `[lint] external` appended after a blank line;
/// konform itself still gets its own `konform.toml`.
#[test]
fn init_patches_standalone_ruff_toml() {
    let dir = tempfile::tempdir().unwrap();
    let ruff = dir.path().join("ruff.toml");
    std::fs::write(&ruff, "line-length = 100\n").unwrap();

    init_cmd(dir.path(), &["--no-patterns"]);
    assert_eq!(
        std::fs::read_to_string(&ruff).unwrap(),
        "line-length = 100\n\n[lint]\nexternal = [\"KIS\", \"KPT\"]\n"
    );
    assert!(dir.path().join("konform.toml").is_file());
}

/// A ruff config that already sets `external` is not touched again.
#[test]
fn init_does_not_touch_ruff_that_already_has_external() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[tool.ruff]\nexternal = [\"X\"]\n").unwrap();

    run_init(dir.path());
    let out = std::fs::read_to_string(&pyproject).unwrap();
    assert_eq!(out.matches("external").count(), 1, "{out}");
    assert!(!out.contains("[tool.ruff.lint]"), "{out}");
    assert!(out.contains("\n\n[tool.konform]\n"), "{out}");
}

/// An existing `[tool.ruff.lint]` without `external` is not edited blindly;
/// `init` prints what to add instead.
#[test]
fn init_only_notes_when_ruff_lint_table_exists() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[tool.ruff.lint]\nselect = [\"E\"]\n").unwrap();

    init_cmd(dir.path(), &["--no-patterns"]).stderr(contains("add to [tool.ruff.lint]"));
    let out = std::fs::read_to_string(&pyproject).unwrap();
    assert!(!out.contains("external"), "{out}");
    assert!(
        out.contains("select = [\"E\"]\n\n[tool.konform]\n"),
        "{out}"
    );
}

/// A second `init` changes nothing.
#[test]
fn init_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let pyproject = dir.path().join("pyproject.toml");
    std::fs::write(&pyproject, "[tool.ruff]\nline-length = 100\n").unwrap();

    run_init(dir.path());
    let first = std::fs::read_to_string(&pyproject).unwrap();
    run_init(dir.path());
    assert_eq!(std::fs::read_to_string(&pyproject).unwrap(), first);
}

/// `konform_patterns.toml` is created, never overwritten, and skipped with
/// `--no-patterns`.
#[test]
fn init_creates_but_never_overwrites_patterns_file() {
    let dir = tempfile::tempdir().unwrap();
    let patterns = dir.path().join("konform_patterns.toml");

    init_cmd(dir.path(), &["--no-patterns"]);
    assert!(!patterns.exists());

    init_cmd(dir.path(), &[]);
    assert!(std::fs::read_to_string(&patterns)
        .unwrap()
        .contains("KPT001"));

    std::fs::write(&patterns, "# mine\n").unwrap();
    init_cmd(dir.path(), &["--force"]);
    assert_eq!(std::fs::read_to_string(&patterns).unwrap(), "# mine\n");
}

/// The `[PATH]` argument selects the directory to initialise.
#[test]
fn init_path_argument_selects_directory() {
    let cwd = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();

    init_cmd(cwd.path(), &[target.path().to_str().unwrap()]);
    assert!(target.path().join("konform.toml").is_file());
    assert!(target.path().join("konform_patterns.toml").is_file());
    assert!(!cwd.path().join("konform.toml").exists());
}

fn violating_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "from os.path import join\n").unwrap();
    dir
}

/// Without `--output-format zuul`, no `zuul_return.yaml` is written anywhere.
#[test]
fn zuul_file_is_not_written_by_default() {
    let dir = violating_dir();
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["check", "--isolated", "app.py"])
        .assert()
        .code(1);
    assert!(!dir.path().join("tmp").exists());

    let explicit = dir.path().join("out.yaml");
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["check", "--isolated", "--output-path"])
        .arg(&explicit)
        .arg("app.py")
        .assert()
        .code(1);
    assert!(!explicit.exists(), "--output-path alone must not write");
}

/// `--output-format zuul` writes (creating directories) and merges into the
/// file at `--output-path`, keeping unrelated content.
#[test]
fn zuul_output_format_writes_and_merges_zuul_return() {
    let dir = violating_dir();
    let out = dir.path().join("nested/dir/zuul_return.yaml");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, "data:\n  other: keep\n").unwrap();

    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "check",
            "--isolated",
            "--output-format",
            "zuul",
            "--output-path",
        ])
        .arg(&out)
        .arg("app.py")
        .assert()
        .code(1)
        .stderr(contains("Wrote Zuul output"));

    let yaml = std::fs::read_to_string(&out).unwrap();
    assert!(yaml.contains("other: keep"), "{yaml}");
    assert!(yaml.contains("zuul:"), "{yaml}");
    // Not in a git repo, so the file isn't a "changed file" -> reported as a warning.
    assert!(
        yaml.contains("warnings:") && yaml.contains("app.py:1"),
        "{yaml}"
    );
}

/// A new nested output directory is created.
#[test]
fn zuul_output_format_creates_missing_directories() {
    let dir = violating_dir();
    let out = dir.path().join("tmp/zuul/zuul_return.yaml");
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["check", "--isolated", "--output-format", "zuul", "app.py"])
        .assert()
        .code(1);
    assert!(out.is_file());
}

/// `rule --explain` renders the Markdown on a colour terminal and leaves it
/// raw when the output is piped, so it can be saved as-is.
#[test]
fn explain_renders_markdown_only_when_colour_is_on() {
    let rendered = Command::cargo_bin("konform")
        .unwrap()
        .args(["--color", "always", "rule", "--explain", "KIS001"])
        .assert()
        .success();
    let rendered = String::from_utf8(rendered.get_output().stdout.clone()).unwrap();
    assert!(rendered.contains('\u{1b}'), "expected ANSI styling");
    assert!(
        !rendered.contains("## "),
        "headings should be rendered: {rendered}"
    );
    assert!(
        !rendered.contains("```"),
        "code fences should be rendered: {rendered}"
    );
    assert!(rendered.contains("What it does"));

    // Piped (not a TTY) with default colour settings: raw Markdown.
    Command::cargo_bin("konform")
        .unwrap()
        .args(["rule", "--explain", "KIS001"])
        .assert()
        .success()
        .stdout(contains("## What it does"))
        .stdout(contains("```python"));
}

/// Rendered `rule --explain` prose and tables never exceed the terminal width,
/// even for the wide options tables. Code blocks are deliberately left
/// unwrapped so they can be copied verbatim.
#[test]
fn explain_output_fits_the_terminal_width() {
    let ansi = regex::Regex::new("\u{1b}\\[[0-9;]*m").unwrap();
    for width in [50usize, 70, 100, 140] {
        let out = Command::cargo_bin("konform")
            .unwrap()
            .env("COLUMNS", width.to_string())
            .args(["--color", "always", "rule", "--explain", "KIS001"])
            .assert()
            .success();
        let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
        for line in ansi.replace_all(&text, "").lines() {
            if line.starts_with("  │ ") {
                continue; // code block line
            }
            assert!(
                line.chars().count() <= width,
                "width {width}: {line:?} is {} columns",
                line.chars().count()
            );
        }
    }
}
