#!/usr/bin/env python3
"""Run the Issue 34 harness unit tests from the repository root.

``python3 -I`` removes the current directory from ``sys.path``; this runner
adds the repository root explicitly so ``scripts.memory`` imports resolve.
"""

from __future__ import annotations

import os
import sys
import unittest

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

if __name__ == "__main__":
    loader = unittest.TestLoader()
    suite = loader.discover(os.path.dirname(__file__), pattern="test_*.py", top_level_dir=REPO_ROOT)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
