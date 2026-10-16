#!/usr/bin/env bash
# Shared by the host runner, container runner and offline orchestration tests.
# Authentication stays on the host; only Git objects and a pinned tag cross over.
PROTOCOL_URL=https://github.com/Zelvik-Labs/protocol

# Single source of truth for which phases compile the main workspace. The
# pingora phase builds only the independent pingora-main workspace: it has no
# private git dependency and no eBPF prerequisites (nightly, bpf-linker, xtask).
# Host and container runner both ask this one predicate, so a new phase cannot
# be routed through (or away from) the private preflight in only one of them.
phase_builds_main_workspace() {
    case "$1" in
        all|check|test|clippy) return 0 ;;
        pingora) return 1 ;;
        *) printf 'unknown phase: %s\n' "$1" >&2; return 2 ;;
    esac
}

read_protocol_lock() {
    local identity
    identity="$(python3 -I - "$1" "$PROTOCOL_URL" <<'PY'
from pathlib import Path
import re
import sys
from urllib.parse import unquote

text = Path(sys.argv[1]).read_text()
packages = re.split(r"(?m)^\[\[package\]\][ \t]*$", text)
packages = [p for p in packages if re.search(r'(?m)^name = "zelvik-suite"$', p)]
if len(packages) != 1:
    sys.exit("Cargo.lock must contain exactly one zelvik-suite package")
sources = re.findall(r'(?m)^source = "([^"\n]+)"$', packages[0])
match = re.fullmatch(re.escape("git+" + sys.argv[2]) + r"\?tag=([^&#]+)#([0-9a-f]{40})",
                     sources[0]) if len(sources) == 1 else None
if not match:
    sys.exit("zelvik-suite must have a tagged protocol source and full commit in Cargo.lock")
tag = unquote(match[1], errors="strict")
if any(c.isspace() or ord(c) < 32 for c in tag):
    sys.exit("invalid protocol tag in Cargo.lock")
print(tag, match[2])
PY
    )" || return
    read -r PROTOCOL_TAG PROTOCOL_REV <<< "$identity"
    git check-ref-format "refs/tags/$PROTOCOL_TAG" || return
}

prepare_protocol_mirror() {
    local lock="$1" mirror="$2"
    read_protocol_lock "$lock" || return
    git init --bare --template= "$mirror" >/dev/null || return
    # Fetch the locked object, never the current value of a potentially moved tag.
    # No remote is saved, and host credential/helper configuration is not copied.
    if ! GIT_TERMINAL_PROMPT=0 git -C "$mirror" fetch --no-tags "$PROTOCOL_URL" "$PROTOCOL_REV"; then
        printf 'Cannot fetch locked protocol commit %s. Configure host Git access to %s (e.g. gh auth setup-git).\n' \
            "$PROTOCOL_REV" "$PROTOCOL_URL" >&2
        return 1
    fi
    git -C "$mirror" cat-file -e "$PROTOCOL_REV^{commit}" || return
    git -C "$mirror" update-ref "refs/tags/$PROTOCOL_TAG" "$PROTOCOL_REV"
}

configure_protocol_git() {
    local lock="$1" mirror="$2" actual
    read_protocol_lock "$lock" || return
    # Process-scoped configuration: no credentials or config files in the bind.
    export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
    export GIT_CONFIG_COUNT=3
    export GIT_CONFIG_KEY_0=protocol.file.allow GIT_CONFIG_VALUE_0=always
    export GIT_CONFIG_KEY_1="url.file://$mirror.insteadOf" GIT_CONFIG_VALUE_1="$PROTOCOL_URL"
    # Defence for the commands this helper runs and for non-transport git use. It
    # does NOT rescue the `file://` fetch cargo performs: in git 2.39.5 no config
    # source (`-c`, env-injected or file) authorises a foreign-owned `file://`
    # remote, so what makes that fetch work is the bind being owned by the
    # container's own uid — OrbStack maps host-owned files to uid 0 and the test
    # container runs as root. A container that ran as a different user against a
    # root-owned bind would fail here no matter what this line says.
    export GIT_CONFIG_KEY_2=safe.directory GIT_CONFIG_VALUE_2="$mirror"
    export CARGO_NET_GIT_FETCH_WITH_CLI=true
    git --git-dir="$mirror" cat-file -e "$PROTOCOL_REV^{commit}" || return
    actual="$(git --git-dir="$mirror" rev-parse "refs/tags/$PROTOCOL_TAG")" || return
    if [ "$actual" != "$PROTOCOL_REV" ]; then
        printf 'Protocol mirror does not match Cargo.lock (%s).\n' "$PROTOCOL_REV" >&2
        return 1
    fi
}
