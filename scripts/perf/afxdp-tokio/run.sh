#!/usr/bin/env bash
# Create only private veths, then execute all cases after compilation is over.
set -euo pipefail
if [[ $# != 4 || $EUID != 0 ]]; then
  echo 'usage: sudo unshare --net -- bash run.sh binary generated-worker output parent-netns-id' >&2
  exit 2
fi
binary=$1
generated=$2
output=$3
parent=$4
current=$(readlink /proc/self/ns/net)
if [[ ! $parent =~ ^net:\[[0-9]+\]$ || $current == "$parent" ]]; then
  echo 'refusing to create veths without a distinct parent netns' >&2
  exit 2
fi
mkdir -p "$output"
printf '{"network_namespace":"%s","parent_network_namespace":"%s"}\n' "$current" "$parent" > "$output/namespace.json"
if ip link show ew-rx >/dev/null 2>&1 || ip link show ew-peer >/dev/null 2>&1; then
  echo 'probe interface names already exist' >&2
  exit 2
fi
owned=false
cleanup() {
  local result=$?
  trap - EXIT
  if [[ $owned == true ]] && ! ip link delete ew-rx; then
    echo '{"interface_cleanup":false}' > "$output/cleanup.json"
    exit 1
  fi
  echo '{"interface_cleanup":true}' > "$output/cleanup.json"
  exit "$result"
}
trap cleanup EXIT
ip link add ew-rx type veth peer name ew-peer
owned=true
ip link set dev ew-rx address 02:00:00:00:00:01
ip link set dev ew-peer address 02:00:00:00:00:02
ip link set ew-rx up
ip link set ew-peer up
python3 scripts/perf/afxdp-tokio/compare.py --binary "$binary" --generated-worker "$generated" --output "$output/comparison"
