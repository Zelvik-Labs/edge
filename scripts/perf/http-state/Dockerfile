FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 docker-cli git util-linux curl libcurl4-openssl-dev build-essential pkg-config \
    openssl ca-certificates procps libpcap0.8 libelf1 libzstd1 \
    && rm -rf /var/lib/apt/lists/*
RUN command -v docker && command -v git && command -v taskset \
    && curl --version | grep -q HTTP3
