# AEGIS Docker Infrastructure

All Docker-related files for AEGIS are organized in this directory.

## Files

- **docker-compose.yml** - Main compose file with all 5 services
- **Dockerfile.runtime** - Multi-stage build for Rust gRPC server
- **Dockerfile.worker** - Multi-stage build for TypeScript Temporal worker

## Services

1. **postgres** - PostgreSQL 15 database for Temporal and AEGIS
2. **temporal** - Temporal server (v1.23.0 with auto-setup)
3. **temporal-ui** - Temporal Web UI (v2.21.3)
4. **temporal-worker** - TypeScript worker for workflow execution
5. **aegis-runtime** - Rust gRPC server providing ExecutionService, ValidationService, etc.

## Usage

Start all services:

```bash
cd docker
docker compose up -d
```

View logs:

```bash
docker compose logs -f
```

Stop all services:

```bash
docker compose down
```

Stop and remove volumes:

```bash
docker compose down -v
```

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
- **AEGIS_ENABLE_DISK_QUOTAS**: Enable/disable Docker disk quotas (default: `true`)
  - Set to `false` on WSL/macOS where XFS+pquota is unavailable
  - Overrides the `spec.runtime.enable_disk_quotas` setting in config file
  - Accepts: `true`, `false`, `1`, `0`, `yes`, `no`, `on`, `off`

## Development

For development with hot-reload:

- The worker source is mounted at `../aegis-temporal-worker/src:/app/src`
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
