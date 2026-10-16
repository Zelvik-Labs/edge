#!/usr/bin/env python3
"""Clean one workspace after its build has exited, preserving mbx objects."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def validate_target(workspace, target, managed_root=None):
    workspace = workspace.resolve()
    resolved = target.resolve()
    if resolved in (Path("/"), Path.home(), workspace) or workspace.is_relative_to(resolved):
        raise ValueError(f"refusing unsafe build target: {resolved}")
    if target.is_symlink():
        if managed_root is None or not resolved.is_relative_to(managed_root.resolve()):
            raise ValueError(f"refusing unmanaged target symlink: {target} -> {resolved}")
        return resolved, True
    if not resolved.is_relative_to(workspace) or resolved.name != "target":
        raise ValueError(f"refusing target outside workspace or not named target: {resolved}")
    return resolved, False


def validate_managed_owner(workspace, target, resolved):
    # mbx clean removes the workspace's recorded default view, not an
    # arbitrary CARGO_TARGET_DIR. Check the record before invoking it.
    if target != workspace / "target":
        raise ValueError(f"refusing non-default managed target: {target}")
    record = resolved.with_suffix(".json")
    if not record.is_file() or record.is_symlink():
        raise ValueError(f"managed target has no safe ownership record: {record}")
    metadata = json.loads(record.read_text())
    if not isinstance(metadata, dict):
        raise ValueError(f"invalid managed target ownership record: {record}")
    owner = metadata.get("workspace_root")
    if (
        metadata.get("version") != 1
        or not isinstance(owner, str)
        or not Path(owner).is_absolute()
        or Path(owner).resolve() != workspace
    ):
        raise ValueError(f"managed target belongs to a different workspace: {record}")


def acquire_build_locks(target):
    """Cargo uses flock on profile .cargo-lock files on Linux and macOS."""
    locks = []
    try:
        for path in sorted(target.rglob(".cargo-lock")):
            if path.is_symlink():
                raise ValueError(f"refusing symlinked build lock: {path}")
            lock = path.open("r+b")
            locks.append(lock)
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise ValueError(f"build is active; target kept: {path}") from error
        return locks
    except Exception:
        for lock in locks:
            lock.close()
        raise


def directory_bytes(target):
    total = 0
    if target.exists():
        for path in target.rglob("*"):
            if path.is_file() and not path.is_symlink():
                total += path.stat().st_size
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    workspace = args.workspace.resolve()
    if not (workspace / "Cargo.toml").is_file():
        raise ValueError(f"not a Cargo workspace: {workspace}")
    target = Path(os.environ.get("CARGO_TARGET_DIR", workspace / "target"))
    if not target.is_absolute():
        target = workspace / target
    mbx = shutil.which("mbx")
    managed_root = None
    if target.is_symlink():
        if mbx is None:
            raise ValueError("mbx is required to clean a managed target")
        if os.environ.get("MBX_TARGET_ROOT"):
            managed_root = Path(os.environ["MBX_TARGET_ROOT"])
        else:
            store = subprocess.run(
                [mbx, "cache", "dir"], cwd=workspace, check=True, capture_output=True, text=True
            ).stdout.strip()
            managed_root = Path(store).parent / "targets"
    resolved, managed = validate_target(workspace, target, managed_root)
    if managed:
        validate_managed_owner(workspace, target, resolved)
    locks = acquire_build_locks(resolved)
    try:
        size = directory_bytes(resolved)
        print(f"target: {target} -> {resolved}; logical bytes: {size}", flush=True)
        if args.dry_run:
            print("dry-run: target kept")
            return
        if managed:
            # mbx clean is workspace-scoped, but does not protect an active
            # managed target itself; the Cargo locks above must stay held.
            subprocess.run([mbx, "clean", str(workspace)], cwd=workspace, check=True)
            if resolved.exists() or target.is_symlink():
                raise ValueError(f"mbx did not remove this workspace's target: {target}")
        elif resolved.exists():
            shutil.rmtree(resolved)
        print(f"cleaned: {resolved}; logical bytes removed: {size}", flush=True)
        # Only opt in for a dedicated store; never evict unrelated host builds.
        if os.environ.get("ZELVIK_EDGE_MBX_GC") == "1":
            if mbx is None:
                raise ValueError("mbx is required for cache collection")
            subprocess.run([mbx, "gc", "--json"], cwd=workspace, check=True)
    finally:
        for lock in locks:
            lock.close()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"cleanup failed: {error}", file=sys.stderr)
        sys.exit(1)
