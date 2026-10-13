"""Run `cz changelog` unless the newest changelog release has no git tag yet.

During `cz bump` the changelog already contains the new `## vX.Y.Z` section while
the tag is only created after the bump commit. Incremental `cz changelog` needs
that tag, so it fails in that window. Skipping then is safe: `cz bump` has
already written the release section.
"""

import re
import subprocess
import sys
from pathlib import Path

CHANGELOG = Path("CHANGELOG.md")
HEADING = re.compile(r"^## (v\d+\.\d+\.\d+)\b", re.MULTILINE)


def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], capture_output=True, text=True)


def tag_exists(tag: str) -> bool:
    return git("rev-parse", "-q", "--verify", f"refs/tags/{tag}").returncode == 0


def latest_tag() -> str | None:
    result = git("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*")
    return result.stdout.strip() if result.returncode == 0 else None


def main() -> int:
    match = HEADING.search(CHANGELOG.read_text(encoding="utf-8"))
    if match and not tag_exists(match.group(1)):
        print(f"Skipping: {match.group(1)} is not tagged yet (bump in progress).")
        return 0
    # Right after a bump HEAD is the tag, so there is nothing new to list.
    # `cz changelog` would fail with "No commits found" in that case.
    tag = latest_tag()
    if tag and git("rev-list", "--count", f"{tag}..HEAD").stdout.strip() == "0":
        print(f"Skipping: no commits since {tag}.")
        return 0
    return subprocess.run(["cz", "changelog"]).returncode


if __name__ == "__main__":
    sys.exit(main())
