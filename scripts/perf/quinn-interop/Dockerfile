FROM edge-http-state-fixture:20261103
RUN apt-get update && apt-get install -y --no-install-recommends \
    libcurl3t64-gnutls libcurl4t64 libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*
