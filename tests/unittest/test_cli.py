"""Tests for the konform CLI — existing behaviour and new flags."""

from __future__ import annotations

import json
import pathlib
import shutil
import subprocess
import sys
import time

import pytest

import konform.__main__ as konform_main
from konform import __version__  # noqa: KIS001

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

BINARY_NAME = "konform.exe" if sys.platform == "win32" else "konform"
PACKAGE_DIR = pathlib.Path(__file__).parent.parent.parent / "python" / "konform"
FIXTURES = pathlib.Path(__file__).parent.parent / "fixtures"


def _run(*args: str, cwd: pathlib.Path | None = None, stdin: str | None = None) -> subprocess.CompletedProcess:  # type: ignore[type-arg]
    """Run konform via the Python entry point and capture output."""
    return subprocess.run(
        [sys.executable, "-m", "konform", *args],
        capture_output=True,
        text=True,
        check=False,
        cwd=cwd,
        input=stdin,
    )


needs_binary = pytest.mark.skipif(
    not (pathlib.Path(sys.prefix) / "bin" / BINARY_NAME).exists()
    and not (pathlib.Path(sys.prefix) / "Scripts" / BINARY_NAME).exists(),
    reason="Rust binary not compiled — run `hatch run develop` first",
)

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

CLEAN_SRC = '"""Module."""\nimport os\nimport sys\n'
DIRTY_SRC = "from __future__ import annotations\nfrom os.path import join\nfrom collections import OrderedDict\n"
NOQA_SRC = "from os.path import join  # noqa: KIS001\n"


# ---------------------------------------------------------------------------
# Smoke / existing behaviour
# ---------------------------------------------------------------------------


@needs_binary
def test_help_exits_zero() -> None:
    result = _run("--help")
    assert result.returncode == 0
    assert "konform" in result.stdout.lower() or "usage" in result.stdout.lower()


@needs_binary
def test_version_flag() -> None:
    result = _run("--version")
    assert result.returncode == 0
    assert any(char.isdigit() for char in result.stdout + result.stderr)


@needs_binary
def test_version_subcommand() -> None:
    result = _run("version")
    assert result.returncode == 0
    assert "konform" in result.stdout


@needs_binary
def test_clean_file_exits_zero(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    result = _run(str(f))
    assert result.returncode == 0, result.stderr


@needs_binary
def test_dirty_file_exits_nonzero(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("--level", "error", str(f))
    assert result.returncode != 0, "Expected violations but got none"


@needs_binary
def test_noqa_suppresses_violation(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "noqa.py"
    f.write_text(NOQA_SRC)
    result = _run("--level", "error", str(f))
    assert result.returncode == 0, result.stderr


@needs_binary
def test_default_subcommand_backward_compat(tmp_path: pathlib.Path) -> None:
    """konform <PATH> (no subcommand) should behave like konform check <PATH>."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    result = _run(str(f))
    assert result.returncode == 0, result.stderr


@needs_binary
def test_ignore_flag_suppresses_rule_no_cache(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-n", "--level", "error", "--ignore", "KIS", str(f))
    assert result.returncode == 0, f"--ignore KIS should suppress all KIS: {result.stderr}"


@needs_binary
def test_select_flag_enables_only_matching_rules(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "dirty.py"
    f.write_text("from os.path import join\n")
    result = _run("check", "-n", "--level", "error", "--select", "KIS", str(f))
    assert result.returncode != 0, "KIS rule should flag the violation"


# ---------------------------------------------------------------------------
# Log-level flags  (-q / -s)
# ---------------------------------------------------------------------------


@needs_binary
def test_quiet_shows_violations_but_no_summary(tmp_path: pathlib.Path) -> None:
    """-q: violations are printed but 'Found N errors.' summary is absent."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-q", str(f))
    assert result.returncode != 0
    assert "KIS001" in result.stderr, "violations must still appear in quiet mode"
    assert "Found" not in result.stderr, "summary must be suppressed in quiet mode"


@needs_binary
def test_silent_produces_no_output(tmp_path: pathlib.Path) -> None:
    """-s: no output at all; exit code still reflects violations."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-s", str(f))
    assert result.returncode != 0, "exit code must be 1 when violations exist"
    assert result.stderr == "", f"stderr must be empty in silent mode, got: {result.stderr!r}"


@needs_binary
def test_silent_clean_file_exit_zero(tmp_path: pathlib.Path) -> None:
    """-s: exit 0 and no output when file is clean."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    result = _run("check", "-s", str(f))
    assert result.returncode == 0
    assert result.stderr == ""


# ---------------------------------------------------------------------------
# --ignore-noqa
# ---------------------------------------------------------------------------


@needs_binary
def test_ignore_noqa_reports_suppressed_violation(tmp_path: pathlib.Path) -> None:
    """--ignore-noqa must report violations that are normally suppressed."""
    f = tmp_path / "noqa.py"
    f.write_text(NOQA_SRC)
    # Without flag: suppressed → exit 0
    assert _run("check", str(f)).returncode == 0
    # With flag: violation reported → exit 1
    result = _run("check", "--ignore-noqa", str(f))
    assert result.returncode != 0, "--ignore-noqa should bypass # noqa"
    assert "KIS001" in result.stderr


# ---------------------------------------------------------------------------
# --fix-only
# ---------------------------------------------------------------------------


@needs_binary
def test_fix_only_applies_fixes_and_exits_zero(tmp_path: pathlib.Path) -> None:
    """--fix-only must rewrite the file and exit 0 without reporting violations."""
    f = tmp_path / "dirty.py"
    f.write_text("from os.path import join\n")
    result = _run("check", "--fix-only", str(f))
    assert result.returncode == 0, f"--fix-only should exit 0, got: {result.stderr}"
    # File should have been rewritten.
    assert "from os.path import join" not in f.read_text()
    # No violation output.
    assert "KIS001" not in result.stderr


# ---------------------------------------------------------------------------
# --exit-non-zero-on-fix
# ---------------------------------------------------------------------------


@needs_binary
def test_exit_non_zero_on_fix_when_files_changed(tmp_path: pathlib.Path) -> None:
    """--exit-non-zero-on-fix exits 1 when --fix actually changed files."""
    f = tmp_path / "dirty.py"
    f.write_text("from os.path import join\n")
    result = _run("check", "--fix", "--exit-non-zero-on-fix", str(f))
    assert result.returncode != 0, "--exit-non-zero-on-fix should exit 1 when files were fixed"


@needs_binary
def test_exit_non_zero_on_fix_clean_file_exits_zero(tmp_path: pathlib.Path) -> None:
    """--exit-non-zero-on-fix exits 0 when no files needed fixing."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    result = _run("check", "--fix", "--exit-non-zero-on-fix", str(f))
    assert result.returncode == 0


# ---------------------------------------------------------------------------
# --output-file
# ---------------------------------------------------------------------------


@needs_binary
def test_output_file_writes_json(tmp_path: pathlib.Path) -> None:
    """-o writes a JSON violations file to the given path."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    out = tmp_path / "violations.json"
    _run("check", "-o", str(out), str(f))
    assert out.exists(), "--output-file did not create the file"
    data = json.loads(out.read_text())
    assert isinstance(data, list)
    assert len(data) > 0
    assert data[0]["rule"] == "KIS001"


# ---------------------------------------------------------------------------
# --isolated
# ---------------------------------------------------------------------------


@needs_binary
def test_isolated_ignores_config(tmp_path: pathlib.Path) -> None:
    """--isolated runs without loading pyproject.toml."""
    f = tmp_path / "dirty.py"
    f.write_text("from os.path import join\n")
    # Write a config that ignores KIS entirely.
    (tmp_path / "pyproject.toml").write_text("[tool.konform]\nignore = ['KIS']\n")
    # Without --isolated: config suppresses violations → exit 0.
    result_normal = _run("check", str(f))
    assert result_normal.returncode == 0, "config should suppress KIS"
    # With --isolated: config is ignored → violations reported → exit 1.
    result_isolated = _run("check", "--isolated", str(f))
    assert result_isolated.returncode != 0, "--isolated should ignore the config"


# ---------------------------------------------------------------------------
# --exclude
# ---------------------------------------------------------------------------


@needs_binary
def test_exclude_skips_matching_files(tmp_path: pathlib.Path) -> None:
    """--exclude should prevent matching files from being checked."""
    good = tmp_path / "good.py"
    bad = tmp_path / "skip_me.py"
    good.write_text(CLEAN_SRC)
    bad.write_text(DIRTY_SRC)
    # Without exclude: violations found.
    assert _run("check", str(tmp_path)).returncode != 0
    # With exclude: skip_me.py excluded → clean.
    result = _run("check", "--exclude", "skip_me.py", str(tmp_path))
    assert result.returncode == 0, f"--exclude should skip skip_me.py: {result.stderr}"


# ---------------------------------------------------------------------------
# --cache-dir
# ---------------------------------------------------------------------------


@needs_binary
def test_cache_dir_creates_cache_in_custom_location(tmp_path: pathlib.Path) -> None:
    """--cache-dir should place the cache in the specified directory."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    cache = tmp_path / "my_cache"
    _run("check", "--cache-dir", str(cache), str(f))
    assert cache.exists(), "--cache-dir did not create the directory"


# ---------------------------------------------------------------------------
# konform init
# ---------------------------------------------------------------------------


@needs_binary
def test_init_creates_konform_toml_when_no_config(tmp_path: pathlib.Path) -> None:
    """init creates konform.toml when no config exists."""
    result = _run("init", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert (tmp_path / "konform.toml").is_file()
    content = (tmp_path / "konform.toml").read_text()
    assert "[konform]" in content
    assert "[konform.KIS]" in content
    # Defaults must NOT be written out as TOML values
    assert "select = []" not in content
    assert 'level = "error"' not in content
    assert 'exceptions = ["__future__"' not in content


@needs_binary
def test_init_appends_to_existing_pyproject_toml(tmp_path: pathlib.Path) -> None:
    """init appends [tool.konform] to a pyproject.toml that has none."""
    (tmp_path / "pyproject.toml").write_text("[build-system]\nrequires = []\n")
    result = _run("init", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    content = (tmp_path / "pyproject.toml").read_text()
    assert "[tool.konform]" in content
    assert "[tool.konform.KIS]" in content
    # Original content must be preserved.
    assert "[build-system]" in content
    # Defaults must NOT be written out as TOML values
    assert "select = []" not in content
    assert 'exceptions = ["__future__"' not in content


@needs_binary
def test_init_skips_when_pyproject_already_configured(tmp_path: pathlib.Path) -> None:
    """init exits 0 and prints a note when [tool.konform] already exists."""
    (tmp_path / "pyproject.toml").write_text('[tool.konform]\nlevel = "error"\n')
    result = _run("init", cwd=tmp_path)
    assert result.returncode == 0
    # Content must not be changed.
    assert (tmp_path / "pyproject.toml").read_text().count("[tool.konform]") == 1


@needs_binary
def test_init_skips_when_konform_toml_already_exists(tmp_path: pathlib.Path) -> None:
    """init exits 0 and prints a note when konform.toml already exists."""
    (tmp_path / "konform.toml").write_text("# existing\n")
    result = _run("init", cwd=tmp_path)
    assert result.returncode == 0
    assert (tmp_path / "konform.toml").read_text() == "# existing\n"


@needs_binary
def test_init_force_overwrites_konform_toml(tmp_path: pathlib.Path) -> None:
    """init --force recreates konform.toml even if one already exists."""
    (tmp_path / "konform.toml").write_text("# old config\n")
    result = _run("init", "--force", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    content = (tmp_path / "konform.toml").read_text()
    assert "[konform]" in content
    assert "# old config" not in content


@needs_binary
def test_init_force_creates_konform_toml_alongside_configured_pyproject(
    tmp_path: pathlib.Path,
) -> None:
    """init --force creates konform.toml even when pyproject.toml has [tool.konform]."""
    (tmp_path / "pyproject.toml").write_text('[tool.konform]\nlevel = "error"\n')
    result = _run("init", "--force", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert (tmp_path / "konform.toml").is_file()


@needs_binary
def test_init_creates_patterns_file_by_default(tmp_path: pathlib.Path) -> None:
    """init creates konform_patterns.toml alongside the main config."""
    _run("init", cwd=tmp_path)
    assert (tmp_path / "konform_patterns.toml").is_file()
    content = (tmp_path / "konform_patterns.toml").read_text()
    assert "[[rules]]" in content


@needs_binary
def test_init_no_patterns_skips_patterns_file(tmp_path: pathlib.Path) -> None:
    """init --no-patterns skips konform_patterns.toml."""
    _run("init", "--no-patterns", cwd=tmp_path)
    assert not (tmp_path / "konform_patterns.toml").is_file()


@needs_binary
def test_init_does_not_overwrite_existing_patterns_file(tmp_path: pathlib.Path) -> None:
    """init never overwrites an existing konform_patterns.toml."""
    (tmp_path / "konform_patterns.toml").write_text("# my rules\n")
    _run("init", cwd=tmp_path)
    assert (tmp_path / "konform_patterns.toml").read_text() == "# my rules\n"


@needs_binary
def test_init_diff_shows_changes_without_writing(tmp_path: pathlib.Path) -> None:
    """init --diff prints a unified diff and writes nothing."""
    result = _run("init", "--diff", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    # Nothing written
    assert not (tmp_path / "konform.toml").exists()
    assert not (tmp_path / "konform_patterns.toml").exists()
    # Diff output contains the expected sections
    assert "[konform]" in result.stdout
    assert "[[rules]]" in result.stdout
    # Looks like a unified diff (has +++ and --- headers)
    assert "+++" in result.stdout
    assert "---" in result.stdout


@needs_binary
def test_init_diff_shows_pyproject_append(tmp_path: pathlib.Path) -> None:
    """init --diff against an existing pyproject.toml shows the appended lines."""
    (tmp_path / "pyproject.toml").write_text("[build-system]\nrequires = []\n")
    result = _run("init", "--diff", "--no-patterns", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert not (tmp_path / "konform.toml").exists()
    # pyproject.toml must be unchanged
    assert (tmp_path / "pyproject.toml").read_text() == "[build-system]\nrequires = []\n"
    assert "[tool.konform]" in result.stdout


@needs_binary
def test_init_explicit_path(tmp_path: pathlib.Path) -> None:
    """init <path> initialises the given directory, not cwd."""
    target = tmp_path / "myproject"
    target.mkdir()
    result = _run("init", "--no-patterns", str(target))
    assert result.returncode == 0, result.stderr
    assert (target / "konform.toml").is_file()
    # cwd must be untouched
    assert (
        not (pathlib.Path.cwd() / "konform.toml").exists()
        or (pathlib.Path.cwd() / "konform.toml").read_text() != (target / "konform.toml").read_text()
    )


@needs_binary
def test_init_adds_ruff_external_when_ruff_toml_present(tmp_path: pathlib.Path) -> None:
    """init adds [lint] external to ruff.toml when ruff is configured."""
    (tmp_path / "ruff.toml").write_text("[tool]\nline-length = 88\n")
    _run("init", "--no-patterns", cwd=tmp_path)
    ruff_content = (tmp_path / "ruff.toml").read_text()
    assert "external" in ruff_content
    assert "KIS" in ruff_content
    assert "KPT" in ruff_content


@needs_binary
def test_init_adds_ruff_external_to_pyproject_ruff_section(
    tmp_path: pathlib.Path,
) -> None:
    """init adds [tool.ruff.lint] external to pyproject.toml with a ruff section."""
    (tmp_path / "pyproject.toml").write_text("[tool.ruff]\nline-length = 88\n")
    _run("init", "--no-patterns", cwd=tmp_path)
    content = (tmp_path / "pyproject.toml").read_text()
    assert "[tool.ruff.lint]" in content
    assert "external" in content
    assert "KIS" in content


@needs_binary
def test_init_skips_ruff_external_when_already_set(tmp_path: pathlib.Path) -> None:
    """init does not double-add external when it is already in ruff config."""
    (tmp_path / "ruff.toml").write_text('[lint]\nexternal = ["KIS", "KPT"]\n')
    _run("init", "--no-patterns", cwd=tmp_path)
    content = (tmp_path / "ruff.toml").read_text()
    assert content.count("external") == 1


@needs_binary
def test_init_diff_includes_ruff_external_patch(tmp_path: pathlib.Path) -> None:
    """init --diff shows the ruff external patch without writing it."""
    (tmp_path / "ruff.toml").write_text("[tool]\nline-length = 88\n")
    result = _run("init", "--diff", "--no-patterns", cwd=tmp_path)
    assert result.returncode == 0
    assert "external" in result.stdout
    # ruff.toml must not be modified
    assert "external" not in (tmp_path / "ruff.toml").read_text()


# ---------------------------------------------------------------------------
# konform init — ruff compat
# ---------------------------------------------------------------------------


@needs_binary
def test_init_config_is_usable(tmp_path: pathlib.Path) -> None:
    """The config created by init allows konform check to run successfully."""
    f = tmp_path / "clean.py"
    f.write_text("import os\n")
    result = _run("check", str(f), cwd=tmp_path)
    assert result.returncode == 0, result.stderr


# ---------------------------------------------------------------------------
# CI output formats
# ---------------------------------------------------------------------------

DIRTY_SRC = "from os.path import join\nfrom sys import argv\n"


@needs_binary
def test_output_format_github(tmp_path: pathlib.Path) -> None:
    """--output-format github emits GitHub Actions workflow commands."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "--output-format", "github", str(f))
    assert result.returncode != 0
    assert "::error file=" in result.stdout
    assert "KIS001" in result.stdout
    # Should include file, line, col, title
    assert "line=" in result.stdout
    assert "col=" in result.stdout
    assert "title=KIS001" in result.stdout


@needs_binary
def test_output_format_gitlab(tmp_path: pathlib.Path) -> None:
    """--output-format gitlab emits a valid GitLab Code Quality JSON array."""

    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "--output-format", "gitlab", str(f))
    assert result.returncode != 0
    data = json.loads(result.stdout)
    assert isinstance(data, list)
    assert len(data) > 0
    entry = data[0]
    assert "description" in entry
    assert "fingerprint" in entry
    assert entry["severity"] in {"minor", "major", "critical", "blocker", "info"}
    assert "location" in entry
    assert "path" in entry["location"]


@needs_binary
def test_output_format_sarif(tmp_path: pathlib.Path) -> None:
    """--output-format sarif emits a valid SARIF 2.1.0 JSON document."""

    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "--output-format", "sarif", str(f))
    assert result.returncode != 0
    data = json.loads(result.stdout)
    assert data["version"] == "2.1.0"
    assert len(data["runs"]) == 1
    run = data["runs"][0]
    assert run["tool"]["driver"]["name"] == "konform"
    assert len(run["results"]) > 0
    r = run["results"][0]
    assert r["ruleId"] == "KIS001"
    assert "message" in r
    assert "locations" in r


@needs_binary
def test_output_format_junit(tmp_path: pathlib.Path) -> None:
    """--output-format junit emits valid JUnit XML."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "--output-format", "junit", str(f))
    assert result.returncode != 0
    xml = result.stdout
    assert '<?xml version="1.0"' in xml
    assert "<testsuites>" in xml
    assert "<testsuite" in xml
    assert "<testcase" in xml
    assert "<failure" in xml
    assert "KIS001" in xml


@needs_binary
def test_output_format_github_clean_file(tmp_path: pathlib.Path) -> None:
    """--output-format github produces no output for a clean file."""
    f = tmp_path / "clean.py"
    f.write_text('"""Module."""\nimport os\n')
    result = _run("check", "--output-format", "github", str(f))
    assert result.returncode == 0
    assert result.stdout == ""


@needs_binary
def test_output_format_sarif_clean_file(tmp_path: pathlib.Path) -> None:
    """--output-format sarif produces empty results for a clean file."""

    f = tmp_path / "clean.py"
    f.write_text('"""Module."""\nimport os\n')
    result = _run("check", "--output-format", "sarif", str(f))
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["runs"][0]["results"] == []


@needs_binary
def test_output_file_with_sarif_format(tmp_path: pathlib.Path) -> None:
    """-o with --output-format sarif writes SARIF to the given file."""

    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    out = tmp_path / "results.sarif"
    _run("check", "--output-format", "sarif", "-o", str(out), str(f))
    assert out.exists()
    data = json.loads(out.read_text())
    assert data["version"] == "2.1.0"
    assert len(data["runs"][0]["results"]) > 0


@needs_binary
def test_output_file_with_github_format(tmp_path: pathlib.Path) -> None:
    """-o with --output-format github writes annotations to the given file."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    out = tmp_path / "annotations.txt"
    _run("check", "--output-format", "github", "-o", str(out), str(f))
    assert out.exists()
    content = out.read_text()
    assert "::error file=" in content


# ---------------------------------------------------------------------------
# Unit tests for the Python wrapper module (no binary required)
# ---------------------------------------------------------------------------


def test_version_importable() -> None:
    assert isinstance(__version__, str)
    assert __version__


def test_main_missing_binary_exits(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(shutil, "which", lambda _name: None)
    with pytest.raises(SystemExit) as exc_info:
        konform_main.main()
    assert exc_info.value.code != 0


# ---------------------------------------------------------------------------
# Step 23 — Stdin support
# ---------------------------------------------------------------------------


@needs_binary
def test_stdin_clean_source_exits_zero() -> None:
    """Piping clean Python via '-' should exit 0."""
    result = _run("check", "-n", "-", stdin=CLEAN_SRC)
    assert result.returncode == 0, result.stderr


@needs_binary
def test_stdin_dirty_source_exits_nonzero() -> None:
    """Piping a file with violations via '-' should exit non-zero."""
    result = _run("check", "-n", "--level", "error", "-", stdin=DIRTY_SRC)
    assert result.returncode != 0, "Expected violations from stdin"


@needs_binary
def test_stdin_reports_violations_in_stderr() -> None:
    """Violations found in stdin appear in stderr output."""
    result = _run("check", "-n", "-", stdin=DIRTY_SRC)
    assert "KIS001" in result.stderr, f"KIS001 not in stderr: {result.stderr!r}"


@needs_binary
def test_stdin_with_stdin_filename_shows_in_output() -> None:
    """--stdin-filename sets the displayed path in violation messages."""
    result = _run("check", "-n", "--stdin-filename", "mymodule.py", "-", stdin=DIRTY_SRC)
    assert "mymodule.py" in result.stderr, f"filename not in stderr: {result.stderr!r}"


@needs_binary
def test_stdin_default_filename_is_stdin_label() -> None:
    """When --stdin-filename is omitted, violations show '<stdin>' as path."""
    result = _run("check", "-n", "-", stdin=DIRTY_SRC)
    assert "<stdin>" in result.stderr, f"<stdin> not in stderr: {result.stderr!r}"


@needs_binary
def test_stdin_fix_only_writes_fixed_source_to_stdout() -> None:
    """'check --fix-only -' pipes the auto-fixed source to stdout."""
    result = _run("check", "-n", "--fix-only", "-", stdin=DIRTY_SRC)
    assert result.returncode == 0, result.stderr
    # Fixed source must appear on stdout and must not contain the original import form.
    assert result.stdout != "", "Expected fixed output on stdout"
    assert "from os.path import" not in result.stdout, "import not rewritten"


@needs_binary
def test_stdin_fix_writes_output_to_stdout() -> None:
    """'check --fix -' pipes fixed source to stdout and reports remaining violations."""
    result = _run("check", "-n", "--fix", "-", stdin=DIRTY_SRC)
    # Fixed source must appear on stdout.
    assert result.stdout != "", "Expected fixed output on stdout"
    assert "from os.path import" not in result.stdout, "import not rewritten"


@needs_binary
def test_stdin_fix_clean_source_echoes_unchanged() -> None:
    """'check --fix-only -' on clean source still echoes the source to stdout."""
    result = _run("check", "-n", "--fix-only", "-", stdin=CLEAN_SRC)
    assert result.returncode == 0
    assert result.stdout == CLEAN_SRC, "clean source should be echoed unchanged"


@needs_binary
def test_stdin_noqa_suppresses_violation() -> None:
    """'# noqa: KIS001' in piped source suppresses the violation."""
    result = _run("check", "-n", "-", stdin=NOQA_SRC)
    assert result.returncode == 0, result.stderr


@needs_binary
def test_stdin_show_files_lists_stdin_filename(tmp_path: pathlib.Path) -> None:
    """--show-files lists the stdin display path alongside any real files."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    result = _run(
        "check",
        "-n",
        "--show-files",
        "--stdin-filename",
        "pipe.py",
        "-",
        str(f),
        stdin=CLEAN_SRC,
    )
    assert result.returncode == 0
    listed = result.stdout.splitlines()
    assert any("pipe.py" in line for line in listed), f"pipe.py not in --show-files: {listed}"
    assert any(str(f) in line for line in listed), f"{f} not in --show-files: {listed}"


# ---------------------------------------------------------------------------
# Step 24 — --add-noqa
# ---------------------------------------------------------------------------


@needs_binary
def test_add_noqa_annotates_violations(tmp_path: pathlib.Path) -> None:
    """--add-noqa appends '# noqa: KIS001' to each violating line."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-n", "--add-noqa", str(f))
    assert result.returncode == 0, result.stderr
    modified = f.read_text()
    assert "# noqa: KIS001" in modified, f"noqa comment missing:\n{modified}"


@needs_binary
def test_add_noqa_exit_zero_on_violations(tmp_path: pathlib.Path) -> None:
    """--add-noqa exits 0 even when violations are present."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-n", "--add-noqa", str(f))
    assert result.returncode == 0, f"Expected exit 0, got {result.returncode}\n{result.stderr}"


@needs_binary
def test_add_noqa_rerun_finds_no_violations(tmp_path: pathlib.Path) -> None:
    """After --add-noqa, a normal re-check finds no violations."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    _run("check", "-n", "--add-noqa", str(f))
    result = _run("check", "-n", str(f))
    assert result.returncode == 0, f"Violations remain after --add-noqa:\n{result.stderr}"


@needs_binary
def test_add_noqa_ignore_noqa_still_finds_violations(tmp_path: pathlib.Path) -> None:
    """After --add-noqa, running with --ignore-noqa still reports violations."""
    f = tmp_path / "dirty.py"
    f.write_text(DIRTY_SRC)
    _run("check", "-n", "--add-noqa", str(f))
    result = _run("check", "-n", "--ignore-noqa", str(f))
    assert result.returncode != 0, "Expected violations with --ignore-noqa after --add-noqa"
    assert "KIS001" in result.stderr, f"KIS001 not found:\n{result.stderr}"


@needs_binary
def test_add_noqa_skips_existing_noqa(tmp_path: pathlib.Path) -> None:
    """Lines already carrying a # noqa comment are not modified."""
    src = '"""Module."""\nfrom os.path import join  # noqa: KIS001\n'
    f = tmp_path / "noqa.py"
    f.write_text(src)
    _run("check", "-n", "--add-noqa", str(f))
    assert f.read_text() == src, "Line with existing # noqa must not be touched"


@needs_binary
def test_add_noqa_clean_file_unchanged(tmp_path: pathlib.Path) -> None:
    """--add-noqa on a clean file makes no modifications."""
    f = tmp_path / "clean.py"
    f.write_text(CLEAN_SRC)
    _run("check", "-n", "--add-noqa", str(f))
    assert f.read_text() == CLEAN_SRC, "Clean file must not be modified"


@needs_binary
def test_add_noqa_stdin_writes_to_stdout() -> None:
    """--add-noqa with stdin writes the annotated source to stdout."""
    result = _run("check", "-n", "--add-noqa", "-", stdin=DIRTY_SRC)
    assert result.returncode == 0, result.stderr
    assert "# noqa: KIS001" in result.stdout, f"noqa not in stdout:\n{result.stdout}"


@needs_binary
def test_add_noqa_merges_with_foreign_noqa(tmp_path: pathlib.Path) -> None:
    """A line with '# noqa: E501' gets KIS001 appended: '# noqa: E501, KIS001'."""
    src = '"""Module."""\nfrom os.path import join  # noqa: E501\n'
    f = tmp_path / "mixed.py"
    f.write_text(src)
    result = _run("check", "-n", "--add-noqa", str(f))
    assert result.returncode == 0, result.stderr
    content = f.read_text()
    assert "# noqa: E501, KIS001" in content, f"codes not merged:\n{content!r}"


# ---------------------------------------------------------------------------
# Step 25 — --per-file-ignores / --extend-per-file-ignores
# ---------------------------------------------------------------------------


@needs_binary
def test_per_file_ignores_cli_suppresses_matching_file(tmp_path: pathlib.Path) -> None:
    """--per-file-ignores 'tests/**:KIS001' silences violations inside tests/."""
    sub = tmp_path / "tests"
    sub.mkdir()
    f = sub / "test_foo.py"
    f.write_text(DIRTY_SRC)
    # Run from tmp_path so the relative path 'tests/test_foo.py' is used.
    result = _run("check", "-n", "--per-file-ignores", "tests/**:KIS001", "tests/test_foo.py", cwd=tmp_path)
    assert result.returncode == 0, f"Expected no violations: {result.stderr}"


@needs_binary
def test_per_file_ignores_non_matching_file_still_reports(tmp_path: pathlib.Path) -> None:
    """Files that do not match the glob are unaffected."""
    sub = tmp_path / "src"
    sub.mkdir()
    f = sub / "foo.py"
    f.write_text(DIRTY_SRC)
    result = _run(
        "check", "-n", "--level", "error", "--per-file-ignores", "tests/**:KIS001", "src/foo.py", cwd=tmp_path
    )
    assert result.returncode != 0, "src/foo.py should still be checked"


@needs_binary
def test_per_file_ignores_comma_separated_codes(tmp_path: pathlib.Path) -> None:
    """Multiple codes in one spec: 'tests/**:KIS001,KPT001' are all suppressed."""
    sub = tmp_path / "tests"
    sub.mkdir()
    f = sub / "test_bar.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-n", "--per-file-ignores", "tests/**:KIS001,KPT001", "tests/test_bar.py", cwd=tmp_path)
    assert result.returncode == 0, f"Expected no violations: {result.stderr}"


@needs_binary
def test_per_file_ignores_category_prefix(tmp_path: pathlib.Path) -> None:
    """Category prefix 'KIS' suppresses all KIS* rules."""
    sub = tmp_path / "tests"
    sub.mkdir()
    f = sub / "test_baz.py"
    f.write_text(DIRTY_SRC)
    result = _run("check", "-n", "--per-file-ignores", "tests/**:KIS", "tests/test_baz.py", cwd=tmp_path)
    assert result.returncode == 0, f"Expected no violations: {result.stderr}"


@needs_binary
def test_per_file_ignores_config_file(tmp_path: pathlib.Path) -> None:
    """per_file_ignores in pyproject.toml is respected."""
    sub = tmp_path / "tests"
    sub.mkdir()
    f = sub / "test_cfg.py"
    f.write_text(DIRTY_SRC)
    (tmp_path / "pyproject.toml").write_text('[tool.konform]\nper_file_ignores = {"tests/**" = ["KIS001"]}\n')
    result = _run("check", "-n", "tests/test_cfg.py", cwd=tmp_path)
    assert result.returncode == 0, f"Config per_file_ignores not applied: {result.stderr}"


@needs_binary
def test_extend_per_file_ignores_merges_with_config(tmp_path: pathlib.Path) -> None:
    """--extend-per-file-ignores adds on top of the config's per_file_ignores."""
    sub = tmp_path / "tests"
    sub.mkdir()
    f = sub / "test_ext.py"
    f.write_text(DIRTY_SRC)
    # Config has no per_file_ignores; CLI adds the suppression via --extend.
    result = _run("check", "-n", "--extend-per-file-ignores", "tests/**:KIS001", "tests/test_ext.py", cwd=tmp_path)
    assert result.returncode == 0, f"Expected no violations: {result.stderr}"


# ---------------------------------------------------------------------------
# Step 26 — --watch / -w
# ---------------------------------------------------------------------------


def _watch_proc(tmp_path: pathlib.Path, *extra_args: str) -> subprocess.Popen[str]:
    """Start konform in watch mode and return the Popen handle."""
    return subprocess.Popen(
        [sys.executable, "-m", "konform", "check", "-n", "--watch", *extra_args],
        stderr=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        cwd=tmp_path,
    )


def _collect(proc: subprocess.Popen[str], timeout: float = 3.0) -> tuple[str, str]:
    """Terminate the process and collect its output."""
    proc.terminate()
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        stdout, stderr = proc.communicate()
    return stdout, stderr


@needs_binary
def test_watch_initial_check_reports_violations(tmp_path: pathlib.Path) -> None:
    """The initial check pass in --watch mode reports violations before the loop."""
    f = tmp_path / "test.py"
    f.write_text(DIRTY_SRC)
    proc = _watch_proc(tmp_path, str(f))
    try:
        time.sleep(1.5)
    finally:
        _, stderr = _collect(proc)
    assert "KIS001" in stderr, f"Initial violations missing: {stderr!r}"
    assert "Watching" in stderr, f"Watch banner missing: {stderr!r}"


@needs_binary
def test_watch_clean_initial_no_violations(tmp_path: pathlib.Path) -> None:
    """--watch on a clean file prints the watch banner but no violations."""
    f = tmp_path / "test.py"
    f.write_text(CLEAN_SRC)
    proc = _watch_proc(tmp_path, str(f))
    try:
        time.sleep(1.5)
    finally:
        _, stderr = _collect(proc)
    assert "KIS001" not in stderr, f"Unexpected violations: {stderr!r}"
    assert "Watching" in stderr, f"Watch banner missing: {stderr!r}"


@needs_binary
def test_watch_detects_file_change(tmp_path: pathlib.Path) -> None:
    """--watch re-checks and reports new violations when a file changes."""
    f = tmp_path / "test.py"
    f.write_text(CLEAN_SRC)
    proc = _watch_proc(tmp_path, str(f))
    try:
        time.sleep(0.5)  # let the watcher start
        f.write_text(DIRTY_SRC)  # inject a violation
        time.sleep(2.0)  # wait for event + 150 ms debounce + recheck
    finally:
        _, stderr = _collect(proc)
    assert "KIS001" in stderr, f"Expected KIS001 after file change: {stderr!r}"
    assert "changed, rechecking" in stderr, f"Recheck banner missing: {stderr!r}"
