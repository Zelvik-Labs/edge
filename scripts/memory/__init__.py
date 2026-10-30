"""Issue 34 memory validation harness.

This package is intentionally dependency-free (Python standard library plus
the system ``curl``/``openssl`` binaries) so it can run on a stock GitHub
hosted Ubuntu runner without a package install step.

Every Linux statistic is read strictly: a missing file, a missing field or an
unparsable counter raises :class:`StatError`. Nothing in this package turns a
missing statistic into ``0``.
"""
