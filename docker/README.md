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
|-------|------|----------|----------|
| `ghcr.io/100monkeys-ai/aegis-runtime` | `aegis-orchestrator` | `.github/workflows/docker-publish.yml` | Push to `main` (`:latest`, `:sha-<short>`), semver tags `v*.*.*` |
| `ghcr.io/100monkeys-ai/aegis-temporal-worker` | `aegis-temporal-worker` | `.github/workflows/docker-publish.yml` | Push to `main` (`:latest`, `:sha-<short>`), semver tags `v*.*.*` |

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

**Cause:** The `aegis-runtime` container runs as non-root user `aegis` (UID 10000) and cannot access the Docker socket mounted from the host.

**Solution:**

The container is configured to add the `aegis` user to the `docker` group (GID 999 by default). If your host uses a different GID for the docker group:

1. **Find your host's docker GID:**

   ```bash
   getent group docker | cut -d: -f3
   # Example output: 998
   ```

2. **Update docker-compose.yml with the correct GID:**

   ```yaml
   aegis-runtime:
     build:
       context: ..
       dockerfile: docker/Dockerfile.runtime
       args:
         DOCKER_GID: 998  # Use your host's GID here
   ```

3. **Rebuild the container:**

   ```bash
   docker compose build aegis-runtime
   docker compose up -d aegis-runtime
   ```

4. **Verify Docker access inside the container:**

   ```bash
   docker compose exec aegis-runtime id
   # Should show: uid=10000(aegis) gid=10000(aegis) groups=10000(aegis),999(docker)
   ```

**Alternative (less secure):** If you encounter persistent issues, you can run as root by commenting out the `USER aegis` line in `Dockerfile.runtime`, but this is not recommended for production.

**Embedding service unhealthy:**

- Check if the gRPC server is responding: `docker compose logs embedding-service`
- The healthcheck uses the custom `HealthCheck` RPC defined in the service
- May take up to 90 seconds (30s interval × 3 retries) to become healthy on first start

## Testing

Verify the setup using the Testing Guide at `../DEV_TESTING_GUIDE.md`.
