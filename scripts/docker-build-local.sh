#!/usr/bin/env bash
# Build the aegis-runtime Docker image using either a native Rust build
# (fast, requires local toolchain) or the self-contained Docker multi-stage
# build (slower, no local toolchain needed).
#
# Usage:
#   ./scripts/docker-build-local.sh [native|docker|auto] [image-tag]
#
# Modes:
#   native  — Build binary with local cargo, package into slim Docker image
#   docker  — Full multi-stage Docker build (Dockerfile.runtime)
#   auto    — Detect Rust toolchain; use native if available, else docker
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONTEXT_ROOT="$(cd "$REPO_ROOT/.." && pwd)"

MODE="${1:-auto}"
IMAGE_TAG="${2:-aegis-runtime:local}"
CONTAINER_RUNTIME_GID="${CONTAINER_RUNTIME_GID:-989}"

case "$MODE" in
  native)
    echo "==> Building binary natively..."
    cd "$REPO_ROOT"
    cargo build --release --bin aegis --locked

    echo "==> Packaging into Docker image..."
    cp target/release/aegis "$CONTEXT_ROOT/aegis"
    cd "$CONTEXT_ROOT"
    docker build \
      -f aegis-orchestrator/docker/Dockerfile.runtime-slim \
      --build-arg CONTAINER_RUNTIME_GID="$CONTAINER_RUNTIME_GID" \
      -t "$IMAGE_TAG" .
    rm -f "$CONTEXT_ROOT/aegis"
    echo "==> Built image: $IMAGE_TAG"
    ;;

  docker)
    echo "==> Building entirely in Docker (multi-stage)..."
    cd "$CONTEXT_ROOT"
    docker build \
      -f aegis-orchestrator/docker/Dockerfile.runtime \
      --build-arg CONTAINER_RUNTIME_GID="$CONTAINER_RUNTIME_GID" \
      -t "$IMAGE_TAG" .
    echo "==> Built image: $IMAGE_TAG"
    ;;

  auto)
    if command -v cargo &>/dev/null && command -v rustc &>/dev/null; then
      echo "Rust toolchain detected, using native build..."
      exec "$0" native "$IMAGE_TAG"
    else
      echo "No Rust toolchain found, falling back to Docker build..."
      exec "$0" docker "$IMAGE_TAG"
    fi
    ;;

  *)
    echo "Usage: $0 [native|docker|auto] [image-tag]"
    exit 1
    ;;
esac
