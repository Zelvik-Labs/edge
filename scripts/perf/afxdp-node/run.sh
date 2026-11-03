#!/usr/bin/env bash
# Full-node functional A/B, using prebuilt matching binaries in private namespaces.
set -euo pipefail
if [[ $# != 4 || $EUID != 0 ]]; then
  echo 'usage: sudo unshare --net --mount --propagation private -- bash run.sh target-dir output parent-netns-id parent-mountns-id' >&2
  exit 2
fi
export CARGO_TARGET_DIR=$1
output=$2
parent=$3
parent_mount=$4
current=$(readlink /proc/self/ns/net)
if [[ ! $parent =~ ^net:\[[0-9]+\]$ || $current == "$parent" ]]; then
  echo 'refusing full-node tests outside a distinct network namespace' >&2
  exit 2
fi
if [[ ! $parent_mount =~ ^mnt:\[[0-9]+\]$ || $(readlink /proc/self/ns/mnt) == "$parent_mount" ]]; then
  echo 'refusing tests without a distinct parent mount namespace' >&2
  exit 2
fi
mkdir -p "$output"
# Fail before tests if HTTP/2 would otherwise be skipped by the legacy smoke.
/usr/bin/curl --version > "$output/curl.log"
curl_version=$(/usr/bin/curl --version)
if [[ $curl_version != *HTTP2* ]]; then
  echo 'curl must support HTTP/2; protocol checks cannot be skipped' >&2
  exit 1
fi
# bpffs and /run/netns are shared across network namespaces unless separately
# mounted. Require a private mount namespace before replacing either mount.
python3 - "$output" "$parent" "$current" "$parent_mount" <<'PY'
import hashlib, json, os, pathlib, platform, subprocess, sys
output = pathlib.Path(sys.argv[1])
mount = os.readlink('/proc/self/ns/mnt')
metadata = {'network_namespace': sys.argv[3], 'parent_network_namespace': sys.argv[2],
            'mount_namespace': mount, 'parent_mount_namespace': sys.argv[4],
            'kernel': platform.uname()._asdict(), 'cpus': sorted(os.sched_getaffinity(0)),
            'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
            'scope': 'full-node copy-mode veth correctness, IPv4/IPv6 and reload; no throughput claim'}
metadata['binaries'] = {}
for name in ['Zelvik-Edge', 'h3-probe']:
    with pathlib.Path(os.environ['CARGO_TARGET_DIR'], 'debug', name).open('rb') as source:
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
        metadata['binaries'][name] = digest.hexdigest()
(output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
PY
mount --make-rprivate /
mkdir -p /sys/fs/bpf /run/netns
mount -t bpf edge-ci-bpf /sys/fs/bpf
mount -t tmpfs edge-ci-netns /run/netns
export ZELVIK_EDGE_XDP_PIN_DIR=/sys/fs/bpf/edge-ci
export XDP_SMOKE_SKIP_BUILD=1 XDP_SMOKE_PROFILE=debug
export XDP_SMOKE_NODE_BIN="$CARGO_TARGET_DIR/debug/Zelvik-Edge"
# Prefer the distro curl with HTTP/2 over hosted-runner /usr/local/bin/curl.
export PATH=/usr/bin:/bin:$PATH
ip link set lo up
result=0
for item in 1-worker 1-readable 2-readable 2-worker; do
  mode=${item#*-}
  started=$(date +%s)
  if XDP_SMOKE_WAKE="$mode" bash scripts/xdp-netns-smoke.sh > "$output/$item.log" 2>&1; then
    rc=0
  else
    rc=$?
    result=1
  fi
  if ! python3 - "$output" "$item" "$mode" "$rc" "$started" <<'PY'
import json, pathlib, subprocess, sys, time
root, label, mode, code, started = sys.argv[1:]
links = json.loads(subprocess.check_output(['ip', '-j', 'link', 'show'], text=True))
namespaces = subprocess.check_output(['ip', 'netns', 'list'], text=True).strip()
cleanup_ok = {link['ifname'] for link in links} == {'lo'} and not namespaces
pathlib.Path(root, label + '.json').write_text(json.dumps({
    'label': label, 'wake': mode, 'exit_code': int(code), 'passed': int(code) == 0 and cleanup_ok,
    'network_cleanup_ok': cleanup_ok, 'remaining_links': links, 'remaining_namespaces': namespaces,
    'elapsed_seconds': time.time() - int(started),
    'timing_kind': 'functional suite wall time, not throughput or latency'}, indent=2) + '\n')
sys.exit(0 if cleanup_ok else 1)
PY
  then
    echo "fixture cleanup failed after $item" >&2
    result=1
  fi
  # A failure stays a failure. Continue only to retain the other independent
  # baseline/candidate evidence; no fallback or successful retry replaces it.
done
python3 - "$output" <<'PY'
import hashlib, json, os, pathlib, sys
root = pathlib.Path(sys.argv[1])
cases = [json.loads(root.joinpath(label + '.json').read_text()) for label in
         ['1-worker', '1-readable', '2-readable', '2-worker']]
binaries = {}
for name in ['Zelvik-Edge', 'h3-probe']:
    with pathlib.Path(os.environ['CARGO_TARGET_DIR'], 'debug', name).open('rb') as source:
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
        binaries[name] = digest.hexdigest()
unchanged = binaries == json.loads(root.joinpath('metadata.json').read_text())['binaries']
passed = all(c['passed'] and c['network_cleanup_ok'] for c in cases) and unchanged
root.joinpath('summary.json').write_text(json.dumps({'passed': passed, 'binaries_unchanged': unchanged,
    'cases': cases, 'binaries': binaries, 'scope': 'correctness only; physical NIC and long soak untested'}, indent=2) + '\n')
sys.exit(0 if passed else 1)
PY
exit "$result"
