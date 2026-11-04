#!/usr/bin/env bash
# Owned topology only; all binaries must already be built.
set -euo pipefail
[[ $# -ge 7 && $EUID == 0 ]] || { echo 'usage: sudo unshare --net --mount --propagation private -- bash udp-run.sh edge nginx openresty loadgen output parent-net parent-mount [comparison options]' >&2; exit 2; }
edge_binary=$1
nginx_binary=$2
openresty_binary=$3
loadgen_binary=$4
output=$5
parent_net=$6
parent_mount=$7
shift 7
current_net=$(readlink /proc/self/ns/net)
current_mount=$(readlink /proc/self/ns/mnt)
[[ $parent_net =~ ^net:\[[0-9]+\]$ && $current_net != "$parent_net" ]] || { echo 'network namespace must differ from caller' >&2; exit 2; }
[[ $parent_mount =~ ^mnt:\[[0-9]+\]$ && $current_mount != "$parent_mount" ]] || { echo 'mount namespace must differ from caller' >&2; exit 2; }
mkdir -p "$output"
printf '{"network_namespace":"%s","parent_network_namespace":"%s","mount_namespace":"%s","parent_mount_namespace":"%s"}\n' "$current_net" "$parent_net" "$current_mount" "$parent_mount" > "$output/namespaces.json"
mount --make-rprivate /
mkdir -p /sys/fs/bpf /run/netns
mount -t bpf edge-udp-bpf /sys/fs/bpf
mount -t tmpfs edge-udp-netns /run/netns
ip link set lo up
ip netns add edge-udp-peer
owned=false
cleanup() {
  local result=$?
  local cleaned=true
  trap - EXIT
  if [[ $owned == true ]] && ! ip link delete eup-node; then result=1; cleaned=false; fi
  if ! ip netns delete edge-udp-peer; then result=1; cleaned=false; fi
  printf '{"cleanup_passed":%s,"exit_code":%d}\n' "$cleaned" "$result" > "$output/cleanup.json"
  exit "$result"
}
trap cleanup EXIT
ip link add name eup-node numrxqueues 1 numtxqueues 1 type veth peer name eup-peer numrxqueues 1 numtxqueues 1
owned=true
ip link set eup-peer netns edge-udp-peer
ip addr add 10.203.0.1/24 dev eup-node
ip link set eup-node up
ip netns exec edge-udp-peer ip addr add 10.203.0.2/24 dev eup-peer
ip netns exec edge-udp-peer ip link set eup-peer up
ip netns exec edge-udp-peer ip link set lo up
ulimit -n 65536
export PATH=/usr/bin:/bin:$PATH
python3 scripts/perf/afxdp-node/udp-compare.py --edge "$edge_binary" --nginx "$nginx_binary" --openresty "$openresty_binary" --loadgen "$loadgen_binary" --output "$output/comparison" "$@"
