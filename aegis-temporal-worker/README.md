# AEGIS Temporal Worker

TypeScript-based Temporal Worker for AEGIS workflow orchestration.

## Overview

This worker dynamically generates Temporal workflow functions from AEGIS YAML workflow definitions. It acts as the infrastructure layer in the DDD architecture, while the Rust orchestrator maintains the domain model.

## Architecture

```markdown
AEGIS YAML → Rust Domain Model → TemporalWorkflowMapper → JSON Definition
                                                               ↓
                                                    TypeScript Worker
                                                               ↓
                                    Dynamic Workflow Generation → Temporal Execution
                                                               ↓
                                                    Activities → Rust gRPC Runtime
```

## Features

- **Dynamic Workflow Generation**: Creates Temporal workflows from JSON at runtime
- **Multi-Worker Coordination**: PostgreSQL-backed workflow registry
- **gRPC Integration**: Activities call Rust runtime services
- **Hot Reload**: Register new workflows without restarting
- **Observability**: Full integration with Temporal UI

## Getting Started

### Prerequisites

- Node.js >= 20.0.0
- PostgreSQL 15+
- Temporal Server running

### Installation

```bash
npm install
```

### Configuration

Copy `.env.example` to `.env` and configure:

```bash
cp .env.example .env
```

Edit `.env` with your settings:

```env
TEMPORAL_ADDRESS=localhost:7233
DATABASE_URL=postgresql://temporal:temporal@localhost:5432/aegis
AEGIS_RUNTIME_GRPC_URL=localhost:50051
```

### Development

Start both HTTP server and Temporal worker:

```bash
npm run dev:all
```

Or start separately:

```bash
# Terminal 1: HTTP server for workflow registration
npm run dev:server

# Terminal 2: Temporal worker for execution
npm run dev:worker
```

### Production

Build and run:

```bash
npm run build
npm start
```

## API Endpoints

### Register Workflow

```bash
POST /register-workflow
Content-Type: application/json

{
  "workflow_id": "uuid",
  "name": "100monkeys-classic",
  "version": "1.0.0",
  "initial_state": "GENERATE",
  "context": {},
  "states": { ... }
}
```

Response:

```json
{
  "status": "registered",
  "workflow_id": "uuid",
  "name": "100monkeys-classic"
}
```

### Get Workflow Definition

```bash
GET /workflows/:id
```

Response:

```json
{
  "workflow_id": "uuid",
  "name": "100monkeys-classic",
  "states": { ... }
}
```

### List All Workflows

```bash
GET /workflows
```

### Delete Workflow

```bash
DELETE /workflows/:id
```

## Workflow Generation

Workflows are dynamically generated from AEGIS definitions:

```typescript
// Input: AEGIS WorkflowState
{
  kind: "Agent",
  agent: "coder-v1",
  input: "{{workflow.task}}",
  transitions: [
    { condition: "always", target: "EXECUTE" }
  ]
}

// Output: Generated Temporal Workflow Code
const result = await executeAgentActivity({
  agentId: "coder-v1",
  input: renderTemplate(workflow.task, blackboard)
});
blackboard.GENERATE = result;
currentState = "EXECUTE";
```

## Testing

```bash
npm test
```

## Docker

Build Docker image:

```bash
docker build -t aegis-temporal-worker .
```

Run container:

```bash
docker run -p 3000:3000 \
  -e TEMPORAL_ADDRESS=temporal:7233 \
  -e DATABASE_URL=postgresql://... \
  aegis-temporal-worker
```

## Project Structure

```markdown
src/
├── index.ts              # Main entry point
├── config.ts             # Configuration loader
├── logger.ts             # Pino logger
├── types.ts              # TypeScript type definitions
├── database.ts           # PostgreSQL client
├── server.ts             # HTTP server for registration API
├── worker.ts             # Temporal worker initialization
├── workflow-registry.ts  # In-memory workflow registry
├── activities/
│   └── index.ts         # Temporal activities (call Rust gRPC)
└── workflows/
    ├── index.ts         # Workflow exports
    └── generated/       # Dynamically generated workflows
```

## Contributing

See [CONTRIBUTING.md](../../CONTRIBUTING.md)

## License

See [LICENSE](../../LICENSE)
