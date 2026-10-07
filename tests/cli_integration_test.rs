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

const FIXTURE_SRC: &str = "import pytest


@pytest.fixture
def my_fixture():
    # do some setup
    a = 1  # dummy
    assert a == 3  # <- rule: no asserts in fixtures
";

const NO_ASSERT_RULE: &str = "[[rules]]
id = \"KST001\"
message = \"no assert in pytest fixtures\"
level = \"error\"
match = { kind = \"assert\", inside = { kind = \"function\", decorated_with = \"pytest.fixture\" } }
";

/// The motivating KST example, end to end: a rule from `konform_rules.toml`
/// flags `assert` inside a `@pytest.fixture` and fails the run.
#[test]
fn kst_flags_assert_in_pytest_fixture() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(dir.path().join("konform_rules.toml"), NO_ASSERT_RULE).unwrap();
    std::fs::write(dir.path().join("conftest.py"), FIXTURE_SRC).unwrap();
    std::fs::write(dir.path().join("ok.py"), "def helper():\n    assert True\n").unwrap();

    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["check", "--no-cache", "."])
        .assert()
        .failure()
        .stderr(contains("error[KST001]"))
        .stderr(contains("conftest.py"))
        .stderr(contains("ok.py").not());
}

/// Inline config works too, and `--ignore KST001` / `# noqa` silence it.
#[test]
fn kst_inline_rule_respects_ignore_and_noqa() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[[tool.konform.lint.structural-rules.rules]]
id = \"KST001\"
message = \"no assert in pytest fixtures\"
match = { kind = \"assert\", inside = { decorated_with = \"pytest.fixture\" } }
",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.py"), FIXTURE_SRC).unwrap();
    std::fs::write(
        dir.path().join("b.py"),
        "import pytest\n\n@pytest.fixture\ndef f():\n    assert 1  # noqa: KST001\n",
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let out = Command::cargo_bin("konform")
            .unwrap()
            .current_dir(dir.path())
            .args(["check", "--no-cache"])
            .args(extra)
            .arg(".")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let stderr = run(&[]);
    assert!(stderr.contains("warning[KST001]"), "{stderr}");
    assert!(
        stderr.contains("a.py") && !stderr.contains("b.py"),
        "{stderr}"
    );
    assert!(!run(&["--ignore", "KST001"]).contains("warning[KST001]"));
    assert!(!run(&["--select", "KIS"]).contains("warning[KST001]"));
}

/// A broken rule is a hard error (exit 2): skipping it would be a false green.
#[test]
fn kst_invalid_rule_is_a_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(
        dir.path().join("konform_rules.toml"),
        format!(
            "{NO_ASSERT_RULE}\n[[rules]]\nid = \"KST002\"\nmessage = \"m\"\nmatch = {{ kind = \"banana\" }}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("a.py"), FIXTURE_SRC).unwrap();

    for args in [
        &["check", "--no-cache", "a.py"][..],
        &["check", "--fix", "a.py"],
    ] {
        let out = Command::cargo_bin("konform")
            .unwrap()
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("rule 'KST002'"), "{stderr}");
        assert!(stderr.contains("unknown kind 'banana'"), "{stderr}");
        assert!(!stderr.contains("KST001]"), "nothing must run: {stderr}");
    }
}

#[test]
fn rule_test_runs_embedded_snippets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    let run = |rules: &str| {
        std::fs::write(dir.path().join("konform_rules.toml"), rules).unwrap();
        Command::cargo_bin("konform")
            .unwrap()
            .current_dir(dir.path())
            .args(["rule", "--test"])
            .assert()
    };
    let passing = format!(
        "{NO_ASSERT_RULE}\n[rules.test]\nvalid = [\"x = 1\\n\"]\ninvalid = [\"import pytest\\n@pytest.fixture\\ndef f():\\n    assert 1\\n\"]\n"
    );
    run(&passing)
        .success()
        .stdout(contains("KST001").and(contains("2 passed")));

    let failing = passing.replace(
        "valid = [\"x = 1\\n\"]",
        "valid = [\"import pytest\\n@pytest.fixture\\ndef f():\\n    assert 1\\n\"]",
    );
    run(&failing)
        .code(1)
        .stdout(contains("FAIL").and(contains("valid[0]: expected no violation")));

    run(NO_ASSERT_RULE)
        .success()
        .stdout(contains("no test.valid"));

    run(&format!("{NO_ASSERT_RULE}\n[rules.test]\nbogus = 1\n"))
        .code(2)
        .stderr(contains("unknown field `bogus`"));
}

#[test]
fn rule_schema_prints_json_schema() {
    let out = Command::cargo_bin("konform")
        .unwrap()
        .args(["rule", "--schema"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(schema["properties"]["rules"]["type"], "array");
    assert!(schema["$defs"]["matcher"]["properties"]["decorated_with"].is_object());
}

#[test]
fn ast_prints_kinds_and_resolved_names() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), FIXTURE_SRC).unwrap();
    std::fs::write(dir.path().join("bad.py"), "def (:\n").unwrap();

    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["ast", "a.py"])
        .assert()
        .success()
        .stdout(contains(
            "function 5:5  name=my_fixture  decorators=[pytest.fixture]",
        ))
        .stdout(contains("  assert 8:5"));
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["ast", "bad.py"])
        .assert()
        .code(1)
        .stderr(contains("syntax error"));
    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["ast", "missing.py"])
        .assert()
        .code(2);
}

#[test]
fn kst_rules_are_listed_and_explainable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(dir.path().join("konform_rules.toml"), NO_ASSERT_RULE).unwrap();

    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["rule", "--list"])
        .assert()
        .success()
        .stdout(contains("KST001"))
        .stdout(contains("no assert in pytest fixtures"));

    Command::cargo_bin("konform")
        .unwrap()
        .current_dir(dir.path())
        .args(["rule", "--explain", "KST001"])
        .assert()
        .success()
        .stdout(contains("konform_rules.toml"))
        .stdout(contains("pytest.fixture"))
        .stdout(contains("decorated_with"));
}

/// Editing `konform_rules.toml` must not replay results cached under the old
/// rule, even though the checked `.py` file is unchanged.
#[test]
fn editing_kst_rules_file_invalidates_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), "[tool.konform]\n").unwrap();
    std::fs::write(dir.path().join("b.py"), FIXTURE_SRC).unwrap();
    let rules = dir.path().join("konform_rules.toml");
    std::fs::write(&rules, NO_ASSERT_RULE).unwrap();

    assert!(run_check_cached(dir.path()).contains("error[KST001]"));
    assert!(run_check_cached(dir.path()).contains("error[KST001]"));

    std::fs::write(
        &rules,
        NO_ASSERT_RULE.replace("level = \"error\"", "level = \"warning\""),
    )
    .unwrap();
    let after = run_check_cached(dir.path());
    assert!(after.contains("warning[KST001]"), "stale cache: {after}");
}
