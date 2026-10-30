#!/usr/bin/env python3
"""Compare two konform binaries on real-world Python projects.

The CI counterpart of `ruff-ecosystem`: for each project in PROJECTS it runs a
baseline build (the PR's base) and a candidate build (the PR) and reports

* timing: cold runs (``--no-cache``) and warm runs (cache populated), the two
  binaries interleaved so drift on a shared runner hits both equally;
* diagnostics: violations per rule, plus a sample of the ones that appear or
  disappear, so an unintended behaviour change shows up next to the timing.

A Markdown report goes to stdout (and ``--output``). The exit code is 1 when
the candidate's median cold time is more than ``--threshold`` slower than the
baseline's on any project (and by at least ``--min-delta-ms``, so a few
milliseconds of jitter on a small project cannot fail it), 0 otherwise. Warm runs are too short to gate on
reliably on shared runners, so they are informational.

Usage:
    scripts/ecosystem_check.py BASELINE CANDIDATE [--projects-dir DIR]
        [--runs N] [--threshold FRACTION] [--min-delta-ms MS] [--output FILE]

Standard library only.
"""

from __future__ import annotations

import argparse
import collections
import json
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Project:
    name: str
    repo: str
    ref: str


# Pinned so a timing change is the PR's, not upstream's. A mix of sizes: a
# large application, a large library with long files, and a small library
# where process start-up and config discovery dominate.
PROJECTS = [
    Project("django", "https://github.com/django/django", "5.1.4"),
    Project("sympy", "https://github.com/sympy/sympy", "sympy-1.13.3"),
    Project("flask", "https://github.com/pallets/flask", "3.1.0"),
]

# Every rule family, opt-in KNQ rules included. `--isolated` keeps the
# projects' own configuration (if any) out of the comparison.
CHECK_ARGS = [
    "check",
    "--isolated",
    "--select",
    "KIS,KNQ,KPT,KST",
    "--exit-zero",
    "--output-format",
    "json",
]

SAMPLE = 10


def clone(project: Project, projects_dir: Path) -> Path:
    dest = projects_dir / project.name
    if not dest.exists():
        subprocess.run(
            [
                "git",
                "-c",
                "advice.detachedHead=false",
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--branch",
                project.ref,
                project.repo,
                str(dest),
            ],
            check=True,
        )
    return dest


def run(binary: Path, project_dir: Path, *extra: str) -> tuple[float, bytes]:
    start = time.perf_counter()
    proc = subprocess.run(
        [str(binary), *CHECK_ARGS, *extra, "."],
        cwd=project_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        raise SystemExit(f"{binary} failed on {project_dir} (exit {proc.returncode})")
    return elapsed, proc.stdout


def time_interleaved(
    runs_of: list[tuple[Path, list[str]]], project_dir: Path, runs: int
) -> list[list[float]]:
    """`runs` timings per `(binary, extra args)`, alternating, after one warm-up each."""
    for binary, extra in runs_of:
        run(binary, project_dir, *extra)
    times: list[list[float]] = [[] for _ in runs_of]
    for _ in range(runs):
        for i, (binary, extra) in enumerate(runs_of):
            times[i].append(run(binary, project_dir, *extra)[0])
    return times


def violations(binary: Path, project_dir: Path) -> list[tuple[str, int, int, str, str]]:
    _, stdout = run(binary, project_dir, "--no-cache")
    return sorted(
        (v["filename"], v["line"], v["col"], v["rule"], v["message"])
        for v in json.loads(stdout or b"[]")
    )


def fmt_ms(seconds: float) -> str:
    return f"{seconds * 1000:.0f} ms"


def compare(
    project: Project,
    project_dir: Path,
    baseline: Path,
    candidate: Path,
    args: argparse.Namespace,
) -> tuple[list[str], bool]:
    runs = args.runs
    lines = [f"### {project.name} ({project.ref})", ""]

    cold = time_interleaved(
        [(baseline, ["--no-cache"]), (candidate, ["--no-cache"])], project_dir, runs
    )
    # Each binary gets its own cache: the format may differ between them. The
    # warm-up run of `time_interleaved` populates it.
    with (
        tempfile.TemporaryDirectory() as base_cache,
        tempfile.TemporaryDirectory() as cand_cache,
    ):
        warm = time_interleaved(
            [
                (baseline, ["--cache-dir", base_cache]),
                (candidate, ["--cache-dir", cand_cache]),
            ],
            project_dir,
            runs,
        )

    regressed = False
    lines += [
        "| run | baseline (median) | candidate (median) | change |",
        "| --- | ---: | ---: | ---: |",
    ]
    for label, base_times, cand_times, gated in [
        ("cold", cold[0], cold[1], True),
        ("warm", warm[0], warm[1], False),
    ]:
        base_med = statistics.median(base_times)
        cand_med = statistics.median(cand_times)
        change = cand_med / base_med - 1
        flag = ""
        if (
            gated
            and change > args.threshold
            and (cand_med - base_med) * 1000 >= args.min_delta_ms
        ):
            regressed = True
            flag = " :x:"
        lines.append(
            f"| {label} | {fmt_ms(base_med)} | {fmt_ms(cand_med)} | {change:+.1%}{flag} |"
        )
    lines.append("")

    base_v = violations(baseline, project_dir)
    cand_v = violations(candidate, project_dir)
    if base_v == cand_v:
        lines += [f"Diagnostics: unchanged ({len(base_v)} violations).", ""]
        return lines, regressed

    base_count = collections.Counter(v[3] for v in base_v)
    cand_count = collections.Counter(v[3] for v in cand_v)
    lines += [
        "| rule | baseline | candidate | change |",
        "| --- | ---: | ---: | ---: |",
    ]
    for rule in sorted(base_count.keys() | cand_count.keys()):
        b, c = base_count[rule], cand_count[rule]
        if b != c:
            lines.append(f"| {rule} | {b} | {c} | {c - b:+d} |")
    base_set, cand_set = set(base_v), set(cand_v)
    for sign, title, items in [
        ("+", "new", sorted(cand_set - base_set)),
        ("-", "removed", sorted(base_set - cand_set)),
    ]:
        if items:
            lines += [
                "",
                f"<details><summary>{len(items)} {title} violations</summary>",
                "",
                "```diff",
            ]
            lines += [
                f"{sign} {f}:{line}:{col}: {rule} {msg}"
                for f, line, col, rule, msg in items[:SAMPLE]
            ]
            if len(items) > SAMPLE:
                lines.append(f"  ... {len(items) - SAMPLE} more")
            lines += ["```", "</details>"]
    lines.append("")
    return lines, regressed


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument(
        "--projects-dir",
        type=Path,
        default=Path(tempfile.gettempdir()) / "konform-ecosystem",
    )
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument(
        "--threshold",
        type=float,
        default=0.15,
        help="allowed cold-run slowdown (default 0.15)",
    )
    parser.add_argument(
        "--min-delta-ms",
        type=float,
        default=20,
        help="smallest cold-run slowdown that fails (default 20)",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    baseline, candidate = args.baseline.resolve(), args.candidate.resolve()
    args.projects_dir.mkdir(parents=True, exist_ok=True)

    report = [
        "## Ecosystem check",
        "",
        (
            f"Median of {args.runs} interleaved runs per binary; a cold-run slowdown above "
            f"{args.threshold:.0%} (and {args.min_delta_ms:.0f} ms) fails the check."
        ),
        "",
    ]
    regressions = []
    for project in PROJECTS:
        project_dir = clone(project, args.projects_dir)
        # A project's own cache from an earlier run would make "cold" warm.
        shutil.rmtree(project_dir / ".konform_cache", ignore_errors=True)
        lines, regressed = compare(project, project_dir, baseline, candidate, args)
        report += lines
        if regressed:
            regressions.append(project.name)

    if regressions:
        report.append(f"**Slower than the threshold on: {', '.join(regressions)}.**")
    text = "\n".join(report) + "\n"
    sys.stdout.write(text)
    if args.output:
        args.output.write_text(text, encoding="utf-8")
    return 1 if regressions else 0


if __name__ == "__main__":
    raise SystemExit(main())
