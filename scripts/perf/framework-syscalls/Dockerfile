FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 strace \
    && rm -rf /var/lib/apt/lists/*
RUN strace --version
