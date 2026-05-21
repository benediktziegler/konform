"""Fixture: Python file that uses only module imports (should pass the checker).

Uses stdlib packages that have proper package directories so the filesystem
probe can verify them.  `os.path` is a special stdlib module implemented via
an alias (posixpath.py) rather than a real os/path.py file, so the probe
cannot verify it statically — it is suppressed with # noqa: IS001.
"""

from __future__ import annotations

# These are real package sub-modules — the filesystem probe can verify them.
# http/client.py, urllib/parse.py and email/mime/ all exist in the stdlib tree.
from email import mime  # noqa: F401
from http import client  # noqa: F401
from urllib import parse  # noqa: F401
