# AEGIS Docker Infrastructure

This directory contains the **build tooling** (Dockerfiles) for the AEGIS runtime images.

> **Looking for the Docker Compose stack to run AEGIS locally?**
> It has moved to [aegis-examples/deploy/](https://github.com/100monkeys-ai/aegis-examples/tree/main/deploy).
> Clone `aegis-examples` and follow the [Getting Started guide](https://docs.aegis.ai/docs/getting-started).

## Files

- **Dockerfile.runtime** - Multi-stage build for the AEGIS Rust runtime (`ghcr.io/100monkeys-ai/aegis-runtime`)
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

### Tagging Strategy

- **`:latest`** — updated on every push to `main`
- **`:sha-abc1234`** — short commit SHA for traceability on `main` pushes
- **`:0.1.0`**, **`:0.1`**, **`:0`** — semver tags when `v0.1.0` git tag is pushed
- Rust builds use **BuildKit GHA layer caching** to avoid rebuilding unchanged dependencies

## Building images locally

From the repo root:

```bash
# Build the Rust runtime image
docker build -f docker/Dockerfile.runtime -t aegis-runtime:local .
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

The compose stack now resolves Docker socket access dynamically at container startup:

- Starts `aegis-runtime` as root briefly
- Detects `/var/run/docker.sock` group ID at runtime
- Adds `aegis` user to that detected group
- Drops privileges and launches `aegis-runtime --daemon` as user `aegis`

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
