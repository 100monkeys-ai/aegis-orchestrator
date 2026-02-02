# AEGIS Architecture

This document provides a comprehensive overview of the AEGIS system architecture.

## Table of Contents

1. [System Overview](#system-overview)
2. [Core Components](#core-components)
3. [Runtime Architecture](#runtime-architecture)
4. [Security Model](#security-model)
5. [Memory System (Cortex)](#memory-system-cortex)
6. [Swarm Coordination](#swarm-coordination)
7. [Deployment Architecture](#deployment-architecture)
8. [Data Flow](#data-flow)

## System Overview

AEGIS is built on a layered architecture following Domain-Driven Design (DDD) principles with clear separation of concerns.

```markdown
┌─────────────────────────────────────────────────────────────────┐
│                       Client SDKs                               │
│               (Python, TypeScript, Rust)                        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                     API Gateway (Axum)                          │
│            • Authentication  • Rate Limiting                    │
│            • Request Validation  • Metrics                      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Orchestrator Core                            │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────────┐        │
│  │  Scheduler  │  │ Policy Engine│  │ Swarm Coordinator│        │
│  └─────────────┘  └──────────────┘  └──────────────────┘        │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
    ┌──────────────────┐           ┌──────────────────┐
    │  Docker Runtime  │           │ Firecracker VMs  │
    │   (Development)  │           │   (Production)   │
    └──────────────────┘           └──────────────────┘
              │                               │
              └───────────────┬───────────────┘
                              ▼
                    ┌──────────────────┐
                    │  Agent Execution │
                    │  Environment     │
                    └──────────────────┘
```

## Core Components

### 1. Orchestrator (The Hive)

The orchestrator is the control plane, written in Rust for performance and safety.

**Responsibilities:**

- Request routing and load balancing
- Agent lifecycle management
- Security policy enforcement
- Resource allocation and scheduling
- Billing and metering
- Audit logging

**Key Modules:**

- `orchestrator/core`: Pure domain logic (DDD)
- `orchestrator/api`: HTTP/gRPC API server
- `orchestrator/runtime-docker`: Docker adapter
- `orchestrator/runtime-firecracker`: Firecracker adapter
- `orchestrator/security`: Policy engine
- `orchestrator/memory`: Cortex vector store

### 2. Runtime Abstraction

The `AgentRuntime` trait provides a unified interface for both Docker (development) and Firecracker (production) environments.

```rust
#[async_trait]
pub trait AgentRuntime {
    async fn spawn(&self, config: AgentConfig) -> Result<InstanceId>;
    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput>;
    async fn terminate(&self, id: &InstanceId) -> Result<()>;
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus>;
}
```

**Docker Runtime** (Development):

- Fast iteration cycles
- Cross-platform (macOS, Windows, Linux)
- Namespace isolation (cgroups, network namespaces)

**Firecracker Runtime** (Production):

- Kernel-level isolation (KVM)
- ~125ms cold-start time
- Minimal attack surface
- Linux-only (bare-metal optimized)

### 3. Policy Engine

Enforces security policies declared in `agent.yaml` manifests.

**Enforcement Points:**

- Network egress (DNS/IP allow-listing)
- Filesystem access (read/write path restrictions)
- Resource limits (CPU, memory, execution time)
- Tool invocation permissions

**Default Stance:** Deny all access. Agents must explicitly request permissions.

### 4. Cortex (Memory System)

Persistent semantic memory for agents using vector embeddings.

**Components:**

- Vector store (LanceDB / Sqlite-Vec)
- Embedding service (OpenAI, local models)
- Auto-indexing pipeline
- Query optimizer

**Workflow:**

1. Agent executes a task
2. Session logs and tool outputs are captured
3. Content is embedded and stored in vector DB
4. Future task executions query relevant memories
5. Agent learns from past successes/failures

## Runtime Architecture

### Agent Lifecycle States

```markdown
Cold → Warm → Hot → Terminated
         ↓
      Failed
```

- **Cold**: Definition exists, no runtime instance (zero cost)
- **Warm**: Runtime pre-booted, waiting for tasks (~125ms startup)
- **Hot**: Actively executing a task
- **Failed**: Execution error, instance marked for cleanup
- **Terminated**: Successfully completed, resources released

### Execution Flow

1. **Request Arrives**: Client sends task via API
2. **Authentication**: Verify API key and permissions
3. **Policy Check**: Validate task against security policies
4. **Scheduling**: Orchestrator selects/spawns runtime instance
5. **Injection**: Environment variables and secrets injected
6. **Execution**: Agent runs in isolated environment
7. **Monitoring**: Real-time logging and resource tracking
8. **Memory Update**: Results stored in Cortex
9. **Response**: Output returned to client
10. **Cleanup**: Instance destroyed (ephemeral)

## Security Model

### Defense in Depth

1. **Isolation Layer** (L1)
   - Kernel-level via KVM (Firecracker)
   - Docker namespaces (development)
   - No shared state between instances

2. **Network Layer** (L2)
   - Default-deny firewall rules
   - DNS/IP allow-listing
   - TLS enforcement
   - No direct internet access

3. **Filesystem Layer** (L3)
   - Read-only root filesystem
   - Explicit volume mounts
   - Path-based access control
   - No writes outside allowed paths

4. **Resource Layer** (L4)
   - CPU quota enforcement (cgroups)
   - Memory limits (hard caps)
   - Execution time limits (timeout)
   - Disk I/O throttling

5. **Audit Layer** (L5)
   - Immutable logging (append-only)
   - Every tool call recorded
   - Network requests logged
   - Tamper-evident (cryptographic hashing)

### Threat Model

**Protected Against:**

- Prompt injection attacks (sandboxing)
- Resource exhaustion (limits)
- Data exfiltration (network controls)
- Privilege escalation (isolation)
- Supply chain attacks (verified images)

**Not Protected Against:**

- Zero-day kernel exploits (mitigated via updates)
- Physical access attacks (customer responsibility)
- Social engineering (out of scope)

## Memory System (Cortex)

### Architecture

```markdown
┌─────────────────────────────────────────────────────────┐
│                  Agent Execution                        │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
            ┌───────────────────────┐
            │   Auto-Indexing       │
            │   Pipeline            │
            └───────────┬───────────┘
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   ┌────────┐    ┌──────────┐    ┌──────────┐
   │ Session│    │   Tool   │    │ Feedback │
   │  Logs  │    │  Outputs │    │   Data   │
   └────────┘    └──────────┘    └──────────┘
        │               │               │
        └───────────────┼───────────────┘
                        ▼
            ┌───────────────────────┐
            │  Embedding Service    │
            └───────────┬───────────┘
                        ▼
            ┌───────────────────────┐
            │   Vector Store        │
            │   (LanceDB)           │
            └───────────────────────┘
```

### Query Optimization

- **Recency Bias**: Recent memories weighted higher
- **Relevance Filtering**: Semantic similarity threshold
- **Context Compression**: Summarization for token efficiency
- **Incremental Updates**: Only new data embedded

## Swarm Coordination

### Concurrency Control

**Problem**: Multiple agents accessing shared resources (files, databases, APIs) can cause race conditions.

**Solution**: Distributed locking via the orchestrator.

```rust
// Agent A acquires lock
let lock = aegis.lock("user_database").await?;

// Perform modifications
database.update(user_id, data).await?;

// Release lock
lock.release().await?;
```

**Implementation**:

- In-memory lock registry (orchestrator)
- FIFO queue for lock requests
- Timeout-based deadlock prevention
- Automatic release on agent termination

### Message Bus

Agents communicate via a secure internal pub/sub system (NATS or Redis).

**Use Cases**:

- Parent-child task delegation
- Event broadcasting (e.g., "file uploaded")
- Status updates
- Error propagation

## Deployment Architecture

### Development (Local)

```markdown
Developer Machine
├── Docker Engine
│   ├── Agent Container 1
│   ├── Agent Container 2
│   └── ...
└── AEGIS CLI
    └── Local Orchestrator (Rust)
```

### Production (Cloud)

```markdown
┌─────────────────────────────────────────────────────────┐
│                   Load Balancer                         │
└───────────────────────┬─────────────────────────────────┘
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   ┌─────────┐    ┌─────────┐    ┌─────────┐
   │ API Node│    │ API Node│    │ API Node│
   └────┬────┘    └────┬────┘    └────┬────┘
        │              │              │
        └──────────────┼──────────────┘
                       ▼
           ┌───────────────────────┐
           │   Orchestrator        │
           │   (Rust Cluster)      │
           └───────────┬───────────┘
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   ┌─────────┐   ┌─────────┐   ┌─────────┐
   │ Worker  │   │ Worker  │   │ Worker  │
   │ Node 1  │   │ Node 2  │   │ Node 3  │
   │ (FC VMs)│   │ (FC VMs)│   │ (FC VMs)│
   └─────────┘   └─────────┘   └─────────┘
```

### Edge Vectors (Hybrid)

```markdown
┌─────────────────┐          ┌─────────────────┐
│  AEGIS Cloud    │◄────────►│   Edge Node     │
│  Orchestrator   │   Secure │  (Customer DC)  │
│                 │   Tunnel │                 │
└─────────────────┘          └─────────────────┘
                                     │
                    ┌────────────────┼────────────────┐
                    ▼                ▼                ▼
              ┌──────────┐    ┌──────────┐    ┌──────────┐
              │  Local   │    │   DB     │    │ Browser  │
              │   Files  │    │ Access   │    │ Control  │
              └──────────┘    └──────────┘    └──────────┘
```

**Use Case**: Cloud agents can "teleport" specific tool calls to customer infrastructure while maintaining security.

## Data Flow

### Example: Email Summarization Agent

1. **Client Request**

   ```json
   {
     "agent_id": "email-summarizer",
     "task": {
       "prompt": "Summarize emails from today",
       "context": {}
     }
   }
   ```

2. **Orchestrator Processing**
   - Authenticates request
   - Loads agent manifest
   - Checks security policies
   - Spawns Firecracker VM

3. **Agent Execution**
   - Queries Cortex for similar past tasks
   - Connects to Gmail via MCP (allowed by policy)
   - Fetches emails
   - Calls OpenAI API (using BYOK)
   - Generates summary

4. **Memory Storage**
   - Session log embedded
   - Stored in vector DB
   - Tagged with metadata (date, success)

5. **Response**

   ```json
   {
     "result": {
       "summary": "5 emails received: 2 urgent, 3 newsletters"
     },
     "logs": ["Connected to Gmail", "Fetched 5 emails", "Generated summary"]
   }
   ```

6. **Cleanup**
   - VM terminated
   - Resources released
   - Billing recorded

## Technology Stack

- **Language**: Rust (Edition 2021)
- **Async Runtime**: Tokio
- **Web Framework**: Axum + Tower
- **Database**: PostgreSQL (metadata), Sled (embedded logs)
- **Vector Store**: LanceDB / Sqlite-Vec
- **Containerization**: Bollard (Docker), Firecracker
- **Message Bus**: NATS / Redis
- **Monitoring**: Prometheus + Grafana

## Design Principles

1. **Security by Default**: Deny-all policies, explicit permissions
2. **Fail-Safe**: Errors terminate the agent, not the system
3. **Auditability**: Every action logged immutably
4. **Scalability**: Stateless orchestrator, horizontal scaling
5. **Simplicity**: Complexity in infrastructure, not in agents
6. **Interoperability**: Standards-based (MCP, open formats)

---

For implementation details, see the codebase in `orchestrator/` and refer to [PROJECT_AEGIS_SPEC.md](../PROJECT_AEGIS_SPEC.md).
