#!/usr/bin/env bash
# Requires a fresh, owned network namespace; never touches host interfaces.
set -euo pipefail
if [[ $# != 3 || $EUID != 0 ]]; then
  echo 'usage: sudo unshare --net -- bash afxdp-wake.sh absolute-probe-path output-directory parent-netns-id' >&2
  exit 2
fi
probe=$1
output=$2
current_net=$(readlink /proc/self/ns/net)
parent_net=$3
if [[ ! $parent_net =~ ^net:\[[0-9]+\]$ || $current_net == "$parent_net" ]]; then
  echo 'refusing to create veths without a distinct parent network namespace' >&2
  exit 2
fi
mkdir -p "$output"
printf '{"network_namespace":"%s","parent_network_namespace":"%s"}\n' "$current_net" "$parent_net" > "$output/wake-namespace.json"
if ip link show ew-rx >/dev/null 2>&1 || ip link show ew-peer >/dev/null 2>&1; then
  echo 'probe interface names already exist; refusing to replace them' >&2
  exit 2
fi
owned=false
cleanup() {
  local result=$?
  trap - EXIT
  if [[ $owned == true ]]; then
    if ! ip link delete ew-rx; then
      echo '{"interface_cleanup":false}' > "$output/wake-cleanup.json"
      exit 1
    fi
  fi
  echo '{"interface_cleanup":true}' > "$output/wake-cleanup.json"
  exit "$result"
}
trap cleanup EXIT
ip link add ew-rx type veth peer name ew-peer
owned=true
ip link set dev ew-rx address 02:00:00:00:00:01
ip link set dev ew-peer address 02:00:00:00:00:02
ip link set ew-rx up
ip link set ew-peer up
for cycle in 1 2 3; do
  "$probe" ew-rx ew-peer > "$output/wake-$cycle.json" 2> "$output/wake-$cycle.log"
done
