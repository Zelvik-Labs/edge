ARG BASE_IMAGE
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends linux-perf procps \
    && rm -rf /var/lib/apt/lists/*
RUN command -v perf && command -v pgrep
