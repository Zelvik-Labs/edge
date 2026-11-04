# Archived branch history — 2026-11-04

This archive preserves 35 retired remote branch heads and their complete original history. The index is branches.json. Five parent roots retain all commits; no old implementation is merged into main. The published v26.11.1.1 tag remains unchanged.

Fetch this archive and recover a branch from the head recorded in branches.json:

    git fetch origin tag archive/branches-2026-11-04
    git show archive/branches-2026-11-04:branches.json
    git switch -c ORIGINAL_BRANCH RECORDED_COMMIT

For performance evidence replay, fetching this one tag also retrieves every original harness and measured-source commit. The archived fix/issue34-memory branch is preserved as historical divergent work; this archive does not claim those commits were merged into main.
