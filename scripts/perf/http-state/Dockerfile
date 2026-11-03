FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 docker.io curl libcurl4-openssl-dev build-essential pkg-config \
    openssl ca-certificates procps libpcap0.8 libelf1 libzstd1 \
    && rm -rf /var/lib/apt/lists/*
