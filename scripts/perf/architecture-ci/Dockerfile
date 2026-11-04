FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 docker-cli git util-linux curl openssl ca-certificates procps \
    libpcap0.8 libelf1 libzstd1 linux-perf binutils \
    && rm -rf /var/lib/apt/lists/*
RUN command -v perf && command -v c++filt && command -v docker
