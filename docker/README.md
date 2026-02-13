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
- **8080** - AEGIS Runtime orchestrator API
- **50051** - AEGIS Runtime gRPC (for worker activities)

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

## Testing

Verify the setup using the Testing Guide at `../TEMPORAL_TESTING_GUIDE.md`.
