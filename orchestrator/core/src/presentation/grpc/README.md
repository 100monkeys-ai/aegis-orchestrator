# AEGIS gRPC Server Implementation

**Created:** February 12, 2026  
**Status:** ✅ Complete (Cortex stubbed for future Vector+RAG implementation)

## Overview

The Rust gRPC server exposes the AEGIS Runtime service, enabling the TypeScript Temporal Worker to call back into Rust domain services for agent execution, validation, and system operations.

## Architecture

```markdown
TypeScript Worker (Temporal Activities)
    ↓ gRPC calls
Rust gRPC Server (AegisRuntimeService)
    ↓ delegates to
Rust Domain Services (ExecutionService, ValidationService)
```

## Implementation Files

### Core Files

- **`src/presentation/grpc/server.rs`** - gRPC service implementation
- **`src/presentation/grpc/mod.rs`** - Module exports
- **`proto/aegis_runtime.proto`** - Protocol Buffer definitions
- **`build.rs`** - Protobuf compilation configuration

### Dependencies Added

- `tonic` = "0.11" - gRPC framework
- `prost` = "0.12" - Protobuf serialization
- `tokio-stream` = "0.1.18" - Stream utilities
- `bytes` - Binary data handling

## RPC Methods Implemented

### 1. ExecuteAgent (Streaming)

**Purpose:** Execute an agent with 100monkeys iterative refinement

**Request:**

```protobuf
message ExecuteAgentRequest {
  string agent_id = 1;
  string input = 2;
  map<string, string> context = 3;
  optional SecurityPolicy security_policy = 4;
  optional string workflow_execution_id = 5;
}
```

**Response:** Stream of `ExecutionEvent` (8 event types)

- `ExecutionStarted`
- `IterationStarted`
- `IterationOutput`
- `IterationCompleted`
- `IterationFailed`
- `RefinementApplied`
- `ExecutionCompleted`
- `ExecutionFailed`

**Implementation:**

- Converts domain `ExecutionEvent` → protobuf `ExecutionEvent`
- Streams real-time execution progress
- Handles client disconnection gracefully

### 2. ExecuteSystemCommand

**Purpose:** Execute shell commands with environment variables and timeout

**Request:**

```protobuf
message ExecuteSystemCommandRequest {
  string command = 1;
  map<string, string> env = 2;
  optional string workdir = 3;
  optional uint32 timeout_seconds = 4;
}
```

**Response:**

```protobuf
message ExecuteSystemCommandResponse {
  int32 exit_code = 1;
  string stdout = 2;
  string stderr = 3;
  uint64 duration_ms = 4;
}
```

**Implementation:**

- Uses `tokio::process::Command` for async execution
- Supports environment variables and working directory
- Enforces timeout with `tokio::time::timeout`
- Default timeout: 300 seconds

### 3. ValidateWithJudges

**Purpose:** Gradient validation with multi-judge consensus

**Request:**

```protobuf
message ValidateRequest {
  string output = 1;
  string task = 2;
  repeated JudgeConfig judges = 3;
  ConsensusConfig consensus = 4;
}
```

**Response:**

```protobuf
message ValidateResponse {
  float score = 1;              // 0.0 - 1.0
  float confidence = 2;         // 0.0 - 1.0
  string reasoning = 3;
  bool binary_valid = 4;
  repeated JudgeResult individual_results = 5;
}
```

**Implementation:**

- Delegates to `ValidationService::validate_with_judges()`
- Aggregates individual judge results
- Returns consensus score and confidence level
- Binary pass/fail based on threshold (0.5)

### 4. QueryCortexPatterns (STUBBED)

**Purpose:** Search for learned error-solution patterns

**Status:** ⚠️ **Stubbed** - Will be implemented with Vector+RAG in future iteration

**Implementation:**

```rust
async fn query_cortex_patterns(...) -> Result<Response<QueryCortexResponse>, Status> {
    tracing::warn!("QueryCortexPatterns called but Cortex is not yet implemented (stubbed)");
    Ok(Response::new(QueryCortexResponse {
        patterns: vec![],  // Empty results
    }))
}
```

### 5. StoreCortexPattern (STUBBED)

**Purpose:** Store new learned patterns from refinements

**Status:** ⚠️ **Stubbed** - Will be implemented with Vector+RAG in future iteration

**Implementation:**

```rust
async fn store_cortex_pattern(...) -> Result<Response<StoreCortexPatternResponse>, Status> {
    tracing::warn!("StoreCortexPattern called but Cortex is not yet implemented (stubbed)");
    Ok(Response::new(StoreCortexPatternResponse {
        pattern_id: uuid::Uuid::new_v4().to_string(),
        deduplicated: false,
        new_frequency: 1,
    }))
}
```

## Event Streaming

The `ExecuteAgent` RPC uses server-side streaming to provide real-time execution updates:

```rust
type ExecuteAgentStream = ReceiverStream<Result<ExecutionEvent, Status>>;

async fn execute_agent(&self, request) -> Result<Response<Self::ExecuteAgentStream>, Status> {
    let (tx, rx) = mpsc::channel(100);
    
    // Spawn task to stream events
    tokio::spawn(async move {
        // Send ExecutionStarted
        tx.send(Ok(ExecutionEvent { ... })).await;
        
        // Stream execution events from domain
        while let Some(event) = stream.next().await {
            let pb_event = convert_domain_event_to_proto(event);
            tx.send(Ok(pb_event)).await;
        }
    });
    
    Ok(Response::new(ReceiverStream::new(rx)))
}
```

## Domain Event Conversion

The `convert_domain_event_to_proto()` function maps domain events to protobuf:

| Domain Event | Protobuf Event |
| -------------- | ---------------- |
| `IterationStarted` | `ExecutionEvent::IterationStarted` |
| `ConsoleOutput` | `ExecutionEvent::IterationOutput` |
| `IterationCompleted` | `ExecutionEvent::IterationCompleted` |
| `IterationFailed` | `ExecutionEvent::IterationFailed` |
| `RefinementApplied` | `ExecutionEvent::RefinementApplied` |
| `ExecutionCompleted` | `ExecutionEvent::ExecutionCompleted` |
| `ExecutionFailed` | `ExecutionEvent::ExecutionFailed` |

## Starting the Server

```rust
use aegis_core::presentation::grpc::server::start_grpc_server;

let addr = "0.0.0.0:50051".parse()?;
start_grpc_server(
    addr,
    execution_service,
    validation_service
).await?;
```

The server will log:

```markdown
Starting AEGIS gRPC server on 0.0.0.0:50051
```

## Error Handling

All RPC methods return `tonic::Status` errors:

- `Status::invalid_argument()` - Invalid agent_id or request parameters
- `Status::internal()` - Execution or validation failures
- `Status::deadline_exceeded()` - Command timeout

Example:

```rust
AgentId::from_string(&req.agent_id)
    .map_err(|e| Status::invalid_argument(format!("Invalid agent_id: {}", e)))?
```

## Docker Integration

The gRPC server is configured in `docker-compose.temporal.yml`:

```yaml
aegis-runtime:
  build:
    context: ./orchestrator
    dockerfile: Dockerfile
  ports:
    - "50051:50051"  # gRPC
    - "8080:8080"    # HTTP (health checks)
  environment:
    - GRPC_PORT=50051
```

## Next Steps

### Immediate (Task 9)

- ✅ Implement `RegisterWorkflowUseCase`
- ✅ Wire up HTTP POST to TypeScript worker
- ✅ Validate workflow registration

### Future (Cortex Iteration)

- ⏳ Implement Vector+RAG for Cortex
- ⏳ Replace stubbed `QueryCortexPatterns` with vector search
- ⏳ Replace stubbed `StoreCortexPattern` with embedding generation
- ⏳ Add pgvector extension to PostgreSQL
- ⏳ Integrate with embedding models (OpenAI, Cohere, etc.)

## Testing

### Manual Testing with grpcurl

```bash
# List services
grpcurl -plaintext localhost:50051 list

# Execute agent
grpcurl -plaintext -d '{
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "input": "Hello world",
  "context": {}
}' localhost:50051 aegis.runtime.v1.AegisRuntime/ExecuteAgent

# Execute system command
grpcurl -plaintext -d '{
  "command": "echo Hello",
  "env": {},
  "timeout_seconds": 10
}' localhost:50051 aegis.runtime.v1.AegisRuntime/ExecuteSystemCommand

# Query Cortex (stubbed)
grpcurl -plaintext -d '{
  "error_signature": "TypeError: undefined is not a function"
}' localhost:50051 aegis.runtime.v1.AegisRuntime/QueryCortexPatterns
```

### Integration Testing

The TypeScript worker activities will automatically test these endpoints when workflows execute.

## Security Considerations

- **No authentication** implemented yet (coming in security iteration)
- **Plain gRPC** (no TLS) - acceptable for internal Docker network
- **Command execution** uses `sh -c` - requires trusted input
- **Timeout enforcement** prevents hung commands

## Performance Notes

- **Streaming:** Minimal memory overhead with `mpsc::channel(100)` buffering
- **Async execution:** All operations use Tokio async runtime
- **Parallel validation:** Multiple judges execute concurrently
- **No blocking:** All I/O is non-blocking

## References

- [ADR-022: Temporal Workflow Engine Integration](../../aegis-architecture/adrs/022-temporal-workflow-engine-integration.md)
- [Proto Definitions](../../proto/aegis_runtime.proto)
- [TypeScript gRPC Client](../aegis-temporal-worker/src/grpc/client.ts)
- [Tonic Documentation](https://github.com/hyperium/tonic)

---

**Cortex Status:** Stubbed - awaiting Vector+RAG implementation in dedicated iteration  
**gRPC Server:** ✅ Complete and ready for integration testing
