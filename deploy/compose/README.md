# Zelvik-Edge single-node Compose

The Edge data plane runs in `network_mode: host` so it can bind 80/443, attach
XDP/eBPF and keep source IP for the proxy. It is part of the cross-component
Compose at:

    ../../docs/docs/deployment/compose/docker-compose.yml

This repo provides the Edge image build:

    deploy/Dockerfile.edge

Edge runtime env for standalone compose:

- `CLOUD_NODE_MODE=standalone`
- `CLOUD_NODE_HOME=/var/lib/zelvik/edge`
- runtime config: `./edge/runtime.yaml` mounted to `/etc/zelvik/edge/runtime.yaml`

Capabilities added at compose runtime: `NET_BIND_SERVICE`, `NET_RAW`,
`NET_ADMIN`. For production, deploy Edge on dedicated nodes (docs/08); see the
systemd path in `deploy/systemd/zelvik-edge.service` and `deploy/install.sh`.
