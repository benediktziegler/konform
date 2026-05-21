"""Entry point for `python -m konform`.

When installed via pip (maturin `bindings = "bin"`), maturin places the
compiled Rust binary in the wheel's `data/scripts/` directory, from which pip
installs it to the Python environment's `bin/` (Linux/macOS) or `Scripts/`
(Windows) directory.  It is therefore always available on PATH after a normal
`pip install konform`.

This module lets the tool also be invoked as `python -m konform` by finding
the binary on PATH via `shutil.which`.

For development (after `maturin develop` or `hatch run develop`), the binary
is placed in the virtual-environment's bin directory, so `shutil.which` still
works.
"""

from __future__ import annotations

import os
import shutil
import sys

_BINARY_NAME = "konform"


def main() -> None:
    """Locate and exec-replace the konform binary."""
    binary = shutil.which(_BINARY_NAME)

    if binary is None:
        sys.exit(
            f"konform: '{_BINARY_NAME}' not found on PATH.\n"
            "If you are developing locally, run:\n"
            "  hatch run develop\n"
            "to compile the Rust binary with maturin and install it into the venv."
        )

    # os.execvp replaces the current process — zero overhead, correct exit code.
    os.execvp(binary, [binary, *sys.argv[1:]])  # noqa: S606


if __name__ == "__main__":
    main()
