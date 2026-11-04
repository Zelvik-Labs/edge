FROM edge-syscall-sidecar:20261104
RUN apt-get update && apt-get install -y --no-install-recommends binutils \
    && rm -rf /var/lib/apt/lists/*
RUN strace --version && command -v c++filt
