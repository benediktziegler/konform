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
