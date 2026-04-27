# Unified multi-stage Dockerfile for aegis-orchestrator.
# Stage 1: Build the release binary in Ubuntu 22.04 (glibc 2.35).
# Stage 2: Slim runtime image with only the binary + runtime deps.

# ---------------------------------------------------------------------------
# Stage 1 — Builder
# ---------------------------------------------------------------------------
FROM ubuntu:24.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    curl build-essential pkg-config \
    libfuse3-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build

# Copy manifests first for dependency caching
COPY aegis-orchestrator/Cargo.toml aegis-orchestrator/Cargo.lock ./aegis-orchestrator/
COPY aegis-orchestrator/cli/Cargo.toml ./aegis-orchestrator/cli/
COPY aegis-orchestrator/orchestrator/core/Cargo.toml ./aegis-orchestrator/orchestrator/core/
COPY aegis-orchestrator/orchestrator/swarm/Cargo.toml ./aegis-orchestrator/orchestrator/swarm/
COPY aegis-orchestrator/sdks/Cargo.toml ./aegis-orchestrator/sdks/
COPY aegis-proto ./aegis-proto

# Copy vendored bollard-stubs in full (patched for Podman "stopping" state);
# it must be present before any cargo commands so bollard compiles against the
# real stubs, not an empty placeholder.
COPY aegis-orchestrator/orchestrator/vendor/bollard-stubs ./aegis-orchestrator/orchestrator/vendor/bollard-stubs

# Create stub source files so cargo can resolve the workspace manifests.
# These are throwaway — the real source is copied after cargo fetch.
RUN mkdir -p aegis-orchestrator/cli/src && echo "fn main() {}" > aegis-orchestrator/cli/src/main.rs \
    && mkdir -p aegis-orchestrator/orchestrator/core/src && touch aegis-orchestrator/orchestrator/core/src/lib.rs \
    && mkdir -p aegis-orchestrator/orchestrator/swarm/src && touch aegis-orchestrator/orchestrator/swarm/src/lib.rs \
    && mkdir -p aegis-orchestrator/sdks/src && touch aegis-orchestrator/sdks/src/lib.rs

# Download all crates (cached if Cargo.lock is unchanged)
RUN cd aegis-orchestrator && cargo fetch --locked

# Now copy the full source tree
COPY aegis-orchestrator ./aegis-orchestrator
COPY aegis-proto ./aegis-proto

# Build for real
RUN cd aegis-orchestrator && cargo build --release --bin aegis --locked

# ---------------------------------------------------------------------------
# Stage 2 — Runtime
# ---------------------------------------------------------------------------
FROM ubuntu:24.04 AS runtime

# Install runtime dependencies (rustls -- no libssl needed)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libfuse3-3 \
    fuse3 \
    curl \
    tini \
    && rm -rf /var/lib/apt/lists/* \
    && echo "user_allow_other" >> /etc/fuse.conf

WORKDIR /app

# Copy the release binary from the builder stage
COPY --from=builder /build/aegis-orchestrator/target/release/aegis /usr/local/bin/aegis-runtime
COPY aegis-orchestrator/docker/runtime-entrypoint.sh /usr/local/bin/runtime-entrypoint.sh

# Copy bootstrap script for mounting into agent containers
COPY aegis-orchestrator/assets ./assets

# Expose gRPC ports
EXPOSE 8088 50051

# Run as non-root user with container runtime socket access
# Create container-runtime group (use build arg for custom GID) and add aegis user to it
# Default 989 matches the docker/podman group GID on many Linux distros; override at build time
# (CONTAINER_RUNTIME_GID=<host-gid>) so the container's group aligns with the host's.
ARG CONTAINER_RUNTIME_GID=989
RUN groupadd -g ${CONTAINER_RUNTIME_GID} container-runtime || true && \
    useradd -m -u 10000 -G container-runtime aegis && \
    chown -R aegis:aegis /app && \
    chmod +x /usr/local/bin/runtime-entrypoint.sh

# Use tini for proper signal forwarding/reaping and run startup glue from
# entrypoint, then exec runtime as unprivileged user in foreground.
ENTRYPOINT ["tini", "--", "/usr/local/bin/runtime-entrypoint.sh"]
CMD ["/usr/local/bin/aegis-runtime", "--daemon"]
