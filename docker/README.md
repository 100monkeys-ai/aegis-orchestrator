# AEGIS Docker Infrastructure

This directory contains the **build tooling** (Dockerfiles) for the AEGIS runtime images.

> **Looking for the Docker Compose stack to run AEGIS locally?**
> It has moved to [aegis-examples/deploy/](https://github.com/100monkeys-ai/aegis-examples/tree/main/deploy).
> Clone `aegis-examples` and follow the [Getting Started guide](https://docs.aegis.ai/docs/getting-started).

## Dockerfiles

| File | Purpose |
| --- | --- |
| **Dockerfile.runtime** | Self-contained multi-stage build. Compiles Rust inside Docker using BuildKit layer caching. Slower, but requires no local toolchain — good fallback when a local build environment is unavailable. |
| **Dockerfile.runtime-slim** | Copy-only runtime image. Expects a pre-built `aegis` binary at the build context root. Used by CI and the `scripts/docker-build-local.sh` helper for fast image packaging. |

Both produce `ghcr.io/100monkeys-ai/aegis-runtime`.

### glibc Compatibility

The `Dockerfile.runtime-slim` image is based on Ubuntu 24.04 (glibc 2.39). Any binary copied in **must** be built on a system with glibc <= 2.39 to avoid runtime linker errors. CI enforces this by building on Ubuntu 24.04 runners.

### Other Files

- **CORTEX_SERVICES.md** - Notes on the Cortex embedding and Neo4j services

> **Temporal Worker image** moved to its own repo: [aegis-temporal-worker](https://github.com/100monkeys-ai/aegis-temporal-worker).
> Its Docker image (`ghcr.io/100monkeys-ai/aegis-temporal-worker`) is built and published there via GitHub Actions.

These Dockerfiles are used by CI/CD to build and publish images to the GitHub Container Registry. End-users consume the published images via the compose stack in `aegis-examples`.

## CI/CD

Both platform images are automatically built and pushed to `ghcr.io` via GitHub Actions:

| Image | Repo | Workflow | Triggers |
| --- | --- | --- | --- |
| `ghcr.io/100monkeys-ai/aegis-runtime` | `aegis-orchestrator` | `.github/workflows/docker-publish.yml` | Push to `main` (`:latest`, `:sha-<short>`) Semver tags `*.*.*` |
| `ghcr.io/100monkeys-ai/aegis-temporal-worker` | `aegis-temporal-worker` | `.github/workflows/docker-publish.yml` | Push to `main` (`:latest`, `:sha-<short>`) Semver tags `*.*.*` |

CI now builds the `aegis` binary **natively** on the runner (for better cargo/sccache caching), then packages it via `Dockerfile.runtime-slim`. This separates compilation from image assembly, making builds faster and more cacheable.

### Tagging Strategy

- **`:latest`** — updated on every push to `main`
- **`:sha-abc1234`** — short commit SHA for traceability on `main` pushes
- **`:0.1.0`**, **`:0.1`**, **`:0`** — semver tags when `v0.1.0` git tag is pushed

## Building Images Locally

### Recommended: `scripts/docker-build-local.sh`

The helper script builds the binary and packages the image in one step:

```bash
# Auto-detect: uses native cargo if available, falls back to Docker multi-stage
scripts/docker-build-local.sh

# Force native build (fastest — requires local Rust toolchain)
scripts/docker-build-local.sh native

# Force Docker multi-stage build (no local toolchain needed)
scripts/docker-build-local.sh docker
```

**Modes:**

| Mode | What it does |
| --- | --- |
| `native` | Runs `cargo build --release` locally, then packages via `Dockerfile.runtime-slim`. Fastest option. |
| `docker` | Uses `Dockerfile.runtime` to compile Rust inside Docker. No local toolchain required. |
| `auto` (default) | Uses `native` if `cargo` is on `$PATH`, otherwise falls back to `docker`. |

### Manual builds

```bash
# Multi-stage (self-contained, no local toolchain)
docker build -f docker/Dockerfile.runtime -t aegis-runtime:local .

# Slim (pre-built binary — must exist at repo root as ./aegis)
cargo build --release && cp target/release/aegis .
docker build -f docker/Dockerfile.runtime-slim -t aegis-runtime:local .
```

To build the Temporal Worker image locally, clone [aegis-temporal-worker](https://github.com/100monkeys-ai/aegis-temporal-worker) and run `docker build -t aegis-temporal-worker:local .` from that repo root.

## Ports

- **5432** - PostgreSQL
- **7233** - Temporal gRPC (primary endpoint)
- **8233** - Temporal Web UI (<http://localhost:8233>)
- **3000** - Temporal Worker HTTP API
- **8088** - AEGIS Runtime orchestrator API
- **50051** - AEGIS Runtime gRPC (for worker activities)

## Environment Variables

### AEGIS Runtime Service

- **AEGIS_DATABASE_URL**: PostgreSQL connection string (required)
- **RUST_LOG**: Logging level (default: `info,aegis_orchestrator=debug`)
- **TEMPORAL_ADDRESS**: Temporal server address (default: `temporal:7233`)
- **TEMPORAL_WORKER_URL**: Temporal worker HTTP API URL (default: `http://temporal-worker:3000`)

## Development

For development with hot-reload:

- The worker source is mounted from the [aegis-temporal-worker](https://github.com/100monkeys-ai/aegis-temporal-worker) repo (clone it as a sibling directory)
- Changes to TypeScript files will require rebuilding: `docker compose build temporal-worker`

For Rust development:

- Rebuild the runtime: `docker compose build aegis-runtime`

## Health Checks

All services have health checks configured. Check status:

```bash
docker compose ps
```

## Troubleshooting

**Temporal fails to start:**

- Ensure PostgreSQL is healthy first
- Check logs: `docker compose logs temporal`

**Worker can't connect:**

- Verify Temporal is healthy: `docker compose ps temporal`
- Check network connectivity: `docker compose exec temporal-worker ping temporal`

**gRPC connection issues:**

- Ensure aegis-runtime is running: `docker compose ps aegis-runtime`
- Check gRPC port 50051 is accessible from worker

### Docker Socket Permission Issues

**Problem:** Agent execution fails with "Error in the hyper legacy client: client error (Connect)"

**Cause:** Host Docker socket ownership/group can vary across Linux systems.

**Solution:**

The runtime image entrypoint now resolves Docker socket access dynamically:

- Starts container entrypoint as root
- Detects `/var/run/docker.sock` group ID at runtime
- Adds `aegis` user to that detected group
- Drops privileges and execs `aegis-runtime --daemon` as user `aegis`
- Uses `tini` as PID 1 for signal forwarding and process reaping

No manual `DOCKER_GID` export, compose edits, or image rebuild is required.

**Verify:**

```bash
docker compose logs aegis-runtime | tail -n 50
docker compose exec aegis-runtime id
```

**Embedding service unhealthy:**

- Check if the gRPC server is responding: `docker compose logs embedding-service`
- The healthcheck uses the custom `HealthCheck` RPC defined in the service
- May take up to 90 seconds (30s interval × 3 retries) to become healthy on first start

## Testing

Verify the setup using the Testing Guide at `../DEV_TESTING_GUIDE.md`.
