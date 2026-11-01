#!/usr/bin/env python3
"""Entrypoint for ``scripts/memory/soak.py`` that works under ``python3 -I``.

Isolated mode removes the current directory from ``sys.path`` and ignores
``PYTHONPATH``, so the package-qualified ``-m scripts.memory.soak`` invocation
fails. This wrapper adds the repository root explicitly, mirroring
``tests/run_tests.py``. All arguments are forwarded unchanged.
"""

from __future__ import annotations

import os
import sys

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

from scripts.memory import soak  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(soak.main())
