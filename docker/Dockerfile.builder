# Builder image for aegis-orchestrator.
# Ubuntu 22.04 for glibc 2.35 compatibility.
# Cached by docker/build-push-action with GHA cache.

FROM ubuntu:22.04 AS builder

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

# Download all crates (cached if Cargo.lock is unchanged)
RUN cd aegis-orchestrator && cargo fetch --locked

# Now copy the full source tree
COPY aegis-orchestrator ./aegis-orchestrator
COPY aegis-proto ./aegis-proto

# Build for real
RUN cd aegis-orchestrator && cargo build --release --bin aegis --locked

# Output: /build/aegis-orchestrator/target/release/aegis
