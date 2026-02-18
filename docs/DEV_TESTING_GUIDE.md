# AEGIS Temporal Workflow Testing Guide

**Status:** Verification Phase
**Prerequisites:** Docker, Docker Compose, Rust Toolchain

---

## Table of Contents

1. [Overview](#overview)
2. [Architecture Recap](#architecture-recap)
3. [Pre-Flight Checklist](#pre-flight-checklist)
4. [Test Scenarios](#test-scenarios)
   - [Scenario 1: Echo Workflow (Basic Connectivity)](#scenario-1-echo-workflow-basic-connectivity)
   - [Scenario 2: Agent Execution (gRPC Bridge)](#scenario-2-agent-execution-grpc-bridge)
   - [Scenario 3: 100monkeys Classic (Full Integration)](#scenario-3-100monkeys-classic-workflow-full-integration)
   - [Scenario 4: Multi-Judge Consensus Workflow](#scenario-4-multi-judge-consensus-workflow)
   - [Scenario 5: Human-in-the-Loop Workflow](#scenario-5-human-in-the-loop-workflow)
5. [Expected Outcomes](#expected-outcomes)
6. [Troubleshooting](#troubleshooting)
7. [Advanced Testing](#advanced-testing)
8. [Next Steps](#next-steps-after-testing)
9. [Quick Reference Commands](#quick-reference-commands)

> [!NOTE]
> **Future Improvement:** Currently, the application writes to both `workflows` and `workflow_definitions` tables in the same repository method.
> In a future iteration, we should move the `workflow_definitions` insert to a database trigger or an event-driven subscriber that listens to changes on the `workflows` table.
> This will ensure better separation of concerns and robustness.

---

## Overview

This guide walks you through testing the complete Temporal workflow integration for AEGIS. You'll test:

- ✅ Database schema creation
- ✅ Temporal Server connectivity
- ✅ TypeScript Worker registration
- ✅ Rust gRPC Server
- ✅ Dynamic workflow generation
- ✅ Agent execution with 100monkeys refinement
- ✅ Multi-judge validation
- ✅ System command execution

There are two main ways to run the system:

1. **Full Docker Stack** (Recommended): Runs everything in containers.
2. **Hybrid Local/Docker**: Runs Temporal/Postgres/Worker in Docker, but runs the Orchestrator via `cargo run` for debugging.

---

## Architecture Recap

```markdown
┌─────────────────────────────────────────────────────────────────┐
│                         Test Flow                               │
└─────────────────────────────────────────────────────────────────┘

1. YAML Workflow → RegisterWorkflowUseCase (Rust)
                        ↓
2. Parse & Map → TemporalWorkflowMapper
                        ↓
3. Save to DB → PostgreSQL (workflows table)
                        ↓
4. POST → TypeScript Worker HTTP API (:3000/register-workflow)
                        ↓
5. Generate → createWorkflowFromDefinition() (TypeScript)
                        ↓
6. Register → Temporal Worker (.workflows object)
                        ↓
7. Execute → Temporal Client starts workflow
                        ↓
8. Activities → gRPC calls to Rust (:50051)
                        ↓
9. Results → Streamed back through Temporal
```

---

## Pre-Flight Checklist

### 1. Full Docker Stack Setup

```bash
cd /path/to/aegis-orchestrator

# Start all services
cd docker
# Build containers
docker compose build
# Stop containers (if running)
docker compose down -v
# Deploy containers
docker compose up -d

# Verify services are running
docker compose ps

# Expected output:
# postgres          Up      5432/tcp
# temporal          Up      7233/tcp
# temporal-ui       Up      8233/tcp
# temporal-worker   Up      3000/tcp
# aegis-runtime     Up      50051/tcp, 8088/tcp
```

**Service Health Checks:**

- PostgreSQL: `docker exec -it aegis-postgres pg_isready`
- Temporal: `curl http://localhost:8233` (UI should load)
- Worker HTTP: `curl http://localhost:3000/health`
- Rust gRPC: `grpcurl -plaintext localhost:50051 list`

**Note:** All Docker files are now organized in the `docker/` directory. See `docker/README.md` for detailed infrastructure documentation.

### 2. Hybrid Local/Docker Setup (For Debugging)

First, minimize the Orchestrator container so we can run it locally, but keep the rest of the stack (Temporal, Postgres, Worker) running.

```bash
cd docker
# Build all images first to ensure worker is up to date
docker compose build

# Start core infrastructure
docker compose up -d
```

**Verify Services:**

- Temporal UI: [http://localhost:8233](http://localhost:8233)
- Temporal Worker: `docker compose logs -f temporal-worker`

### 3. Database Verification

```bash
# Verify tables
psql -h localhost -U aegis -d aegis -c "\dt"

# Expected tables:
# - workflows
# - workflow_executions
# - agents
# - executions
# - workflow_definitions

# Verify views
psql -h localhost -U aegis -d aegis -c "\dv"

# Expected views:
# - active_workflow_executions
# - agent_success_rates
```

---

## Test Scenarios

Run these commands in a new terminal (when using hybrid setup) or directly against the Docker stack.

### Scenario 1: Echo Workflow (Basic Connectivity)

**Objective:** Test basic workflow registration and execution. Tests the ability to register and execute a generic workflow definition.

#### Step 1.1: Create Simple Workflow

Create `demo-agents/workflows/echo-workflow.yaml`:

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow

metadata:
  name: "echo-test"
  version: "1.0.0"
  description: "Simple echo workflow for testing"

spec:
  context:
    message: "Hello from Temporal!"

  initial_state: ECHO

  states:
    ECHO:
      kind: System
      command: "echo"
      env:
        MESSAGE: "{{workflow.context.message}}"
      timeout: 10s
      transitions:
        - condition: exit_code_zero
          target: COMPLETE

    COMPLETE:
      kind: System
      command: "echo"
      env:
        RESULT: "Workflow completed successfully"
      transitions: []
```

#### Step 1.2: Deploy Workflow

```bash
# Using Rust CLI
cargo run --bin aegis -- --port 8088 workflow deploy demo-agents/workflows/echo-workflow.yaml

# OR using HTTP API
curl -X POST http://localhost:8088/v1/workflows/register \
  -H "Content-Type: application/yaml" \
  --data-binary @demo-agents/workflows/echo-workflow.yaml
```

**Expected Response:**

```json
{
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "name": "echo-test",
  "version": "1.0.0",
  "status": "registered",
  "temporal_workflow_name": "aegis_workflow_echo_test"
}
```

#### Step 1.3: Verify Registration

```bash
# Check database
psql -h localhost -U aegis -d aegis -c \
  "SELECT id, name, version FROM workflows WHERE name = 'echo-test';"

# Check TypeScript worker
curl http://localhost:3000/workflows

# Expected response:
# [
#   {
#     "workflow_id": "...",
#     "name": "echo-test",
#     "registered_at": "2026-02-12T10:30:00Z"
#   }
# ]
```

#### Step 1.4: Run Workflow

```bash
# Using Rust CLI
cargo run --bin aegis -- --port 8088 workflow run echo-test \
  --param message="Hello World!"

# OR using Temporal CLI directly
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_echo_test \
  --workflow-id test-echo-001 \
  --input '{}'

# Watch execution
temporal workflow show --workflow-id test-echo-001
```

**Expected Output:**

```markdown
Workflow Status: COMPLETED
Result: {
  "status": "completed",
  "output": "Workflow completed successfully",
  "iterations": 2,
  "final_state": "COMPLETE"
}
```

#### Step 1.5: Verification

- **Console:** Should show `✓ Workflow execution started! Execution ID: ...`
- **Temporal UI:** Check [http://localhost:8233](http://localhost:8233) for `aegis-workflow` execution.
- **Output:** The workflow details in the UI should show the variable `message` reflected in the state.

```bash
# Check database
psql -h localhost -U aegis -d aegis -c \
  "SELECT * FROM workflow_executions WHERE temporal_workflow_id = 'test-echo-001';"

# Check Temporal UI
open http://localhost:8233/namespaces/default/workflows/test-echo-001
```

---

### Scenario 2: Agent Execution (gRPC Bridge)

**Objective:** Test agent execution via gRPC. Tests the `Orchestrator -> Activity -> gRPC -> Orchestrator` loop.

#### Step 2.1: Deploy Test Agent

Create `demo-agents/greeter/agent.yaml`:

```yaml
version: "1.1"
agent:
  name: "greeter"
  version: "0.1.0"
  description: "An agent that greets users based on their name."
  runtime: "python:3.11"
task:
  instruction: "Greet the user based on their name: \"Hello <name>, nice to meet you!\""
```

```bash
# Deploy agent
cargo run --bin aegis -- --port 8088 agent deploy demo-agents/greeter/agent.yaml

# Expected output:
# Agent deployed: coder (id: ...)
```

### Step 2.2: Execute Test Agent

```bash
cargo run --bin aegis -- --port 8088 task execute greeter --input "Jeshua"
```

#### Step 2.3: Create and Deploy Agent Workflow

Create `demo-agents/workflows/agent-workflow.yaml`:

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow

metadata:
  name: "agent-test"
  version: "1.0.0"
  description: "Test agent execution"

spec:
  context: {}

  initial_state: RUN_AGENT

  states:
    RUN_AGENT:
      kind: Agent
      agent: "greeter"
      input: "Jeshua"
      timeout: 60s
      transitions:
        - condition: on_success
          target: COMPLETE

    COMPLETE:
      kind: System
      command: "echo"
      env:
        AGENT_OUTPUT: "{{RUN_AGENT.output}}"
      transitions: []
```

```bash
# Deploy workflow
cargo run --bin aegis -- --port 8088 workflow deploy demo-agents/workflows/agent-workflow.yaml
```

#### Step 2.4: Run Workflow

```bash
# Using Rust CLI
cargo run --bin aegis -- --port 8088 workflow run agent-test \
  --param input="Hello"

# Execute with Temporal CLI
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_agent_test \
  --workflow-id test-agent-001 \
  --input '{}'

# Watch progress
temporal workflow show --workflow-id test-agent-001 --follow
```

**Expected Events:**

```markdown
1. WorkflowExecutionStarted
2. ActivityTaskScheduled: executeAgentActivity
3. ActivityTaskStarted
4. ExecutionStarted (gRPC stream)
5. IterationStarted (iteration 1)
6. IterationCompleted (iteration 1)
7. ExecutionCompleted
8. ActivityTaskCompleted
9. ActivityTaskScheduled: executeSystemCommandActivity
10. ActivityTaskCompleted
11. WorkflowExecutionCompleted
```

#### Step 2.5: Verification

- **Daemon Logs:** You should see `ExecuteContainerAgent` activity being requested via gRPC.
- **Docker:** A new Docker container for the agent (e.g., `python:3.11`) should spin up briefly.

```bash
# Check executions table
psql -h localhost -U aegis -d aegis -c \
  "SELECT agent_id, status, iterations FROM executions ORDER BY started_at DESC LIMIT 1;"

# Expected:
# agent_id: hello-agent
# status: completed
# iterations: {"1": {"status": "success", ...}}
```

---

### Scenario 3: 100monkeys Classic Workflow (Full Integration)

**Objective:** Test complete iterative refinement loop with the sophisticated refinement loop and Judges.

#### Step 3.1: Deploy Agents

```bash
# Deploy coder agent
cargo run --bin aegis -- --port 8088 agent deploy demo-agents/coder/agent.yaml

# Deploy judge agent
cargo run --bin aegis -- --port 8088 agent deploy demo-agents/judges/basic-judge.yaml

# Verify deployments
psql -h localhost -U aegis -d aegis -c \
  "SELECT name, status FROM agents;"
```

#### Step 3.2: Register 100monkeys Workflow

```bash
# Register the classic workflow using CLI
cargo run --bin aegis -- --port 8088 workflow deploy demo-agents/workflows/100monkeys-classic.yaml

# OR using HTTP API
curl -X POST http://localhost:8088/v1/workflows/register \
  -H "Content-Type: application/yaml" \
  --data-binary @demo-agents/workflows/100monkeys-classic.yaml

# Verify registration
curl http://localhost:3000/workflows | jq '.[] | select(.name == "100monkeys-classic")'
```

#### Step 3.3: Execute Full Refinement Loop

```bash
# Using Rust CLI
cargo run --bin aegis -- --port 8088 workflow run 100monkeys-classic \
  --input '{"agent_id": "coder", "task": "Create a fibonacci function in Python", "command": "python fib.py"}'

# OR start workflow with Temporal CLI directly
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_100monkeys_classic \
  --workflow-id test-100monkeys-001 \
  --input '{"agent_id": "coder", "task": "Create a fibonacci function in Python", "command": "python fib.py"}'

# Follow execution (when using CLI)
cargo run --bin aegis -- --port 8088 workflow logs <EXECUTION_ID> --follow

# OR follow with Temporal CLI
temporal workflow show --workflow-id test-100monkeys-001 --follow
```

**Expected Flow:**

```markdown
┌──────────────────────────────────────────────────────────────┐
│ Iteration 1                                                  │
├──────────────────────────────────────────────────────────────┤
│ 1. GENERATE → Agent generates code                          │
│    Output: Python function (may have bugs)                  │
│                                                              │
│ 2. EXECUTE → Run code                                       │
│    Exit Code: 0 or 1                                        │
│    Output: Execution results                                │
│                                                              │
│ 3. VALIDATE → Judge evaluates                               │
│    Score: 0.0-1.0                                           │
│    Reasoning: "Missing edge case for n=0"                   │
│                                                              │
│ 4. REFINE → Update blackboard                               │
│    Condition: score < 0.70                                  │
│    Target: GENERATE (loop back)                             │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│ Iteration 2                                                  │
├──────────────────────────────────────────────────────────────┤
│ 1. GENERATE → Agent refines code                            │
│    Input includes previous errors                           │
│    Output: Improved Python function                         │
│                                                              │
│ 2. EXECUTE → Run refined code                               │
│    Exit Code: 0                                             │
│    Output: Correct results                                  │
│                                                              │
│ 3. VALIDATE → Judge re-evaluates                            │
│    Score: 0.85                                              │
│    Reasoning: "Good solution, edge cases handled"           │
│                                                              │
│ 4. Transition → score >= 0.70                               │
│    Target: COMPLETE                                         │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│ COMPLETE                                                     │
├──────────────────────────────────────────────────────────────┤
│ Final Code: [refined code]                                  │
│ Final Score: 0.85                                           │
│ Iterations: 2                                               │
│ Status: SUCCESS                                             │
└──────────────────────────────────────────────────────────────┘
```

#### Step 3.4: Verify Full Execution

```bash
# Check workflow execution
psql -h localhost -U aegis -d aegis -c \
  "SELECT we.id, we.status, we.started_at, we.completed_at, w.name
   FROM workflow_executions we
   JOIN workflows w ON we.workflow_id = w.id
   WHERE we.temporal_workflow_id = 'test-100monkeys-001';"

# Check agent executions (should be multiple for refinement loop)
psql -h localhost -U aegis -d aegis -c \
  "SELECT agent_id, status, jsonb_array_length(iterations) as iteration_count
   FROM executions
   WHERE workflow_execution_id = (
     SELECT id FROM workflow_executions 
     WHERE temporal_workflow_id = 'test-100monkeys-001'
   );"

# View detailed results
temporal workflow show --workflow-id test-100monkeys-001
```

---

### Scenario 4: Multi-Judge Consensus Workflow

**Objective:** Test parallel agent execution and consensus validation

#### Step 4.1: Create Multi-Judge Workflow

Create `demo-agents/workflows/multi-judge.yaml`:

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow

metadata:
  name: "multi-judge-test"
  version: "1.0.0"
  description: "Test multi-judge consensus"

spec:
  context:
    test_code: |
      def add(a, b):
          return a + b
      
      print(add(2, 3))

  initial_state: VALIDATE_WITH_PANEL

  states:
    VALIDATE_WITH_PANEL:
      kind: ParallelAgents
      agents:
        - agent: "basic-judge"
          input: "Evaluate this code for correctness: {{workflow.context.test_code}}"
          weight: 1.0
        
        - agent: "basic-judge"
          input: "Evaluate this code for style: {{workflow.context.test_code}}"
          weight: 0.5
        
        - agent: "basic-judge"
          input: "Evaluate this code for security: {{workflow.context.test_code}}"
          weight: 1.5

      consensus:
        strategy: weighted_average
        threshold: 0.7

      timeout: 120s

      transitions:
        - condition: consensus
          threshold: 0.7
          min_agreement: 0.6
          target: APPROVED

        - condition: any_rejected
          target: REJECTED

    APPROVED:
      kind: System
      command: "echo"
      env:
        MESSAGE: "Code approved by consensus"
        FINAL_SCORE: "{{VALIDATE_WITH_PANEL.final_score}}"
      transitions: []

    REJECTED:
      kind: System
      command: "echo"
      env:
        MESSAGE: "Code rejected by consensus"
      transitions: []
```

#### Step 4.2: Execute Multi-Judge Test

```bash
# Register
curl -X POST http://localhost:8088/v1/workflows/register \
  -H "Content-Type: application/yaml" \
  --data-binary @demo-agents/workflows/multi-judge.yaml

# Execute
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_multi_judge_test \
  --workflow-id test-multi-judge-001 \
  --input '{}'

# Watch parallel execution
temporal workflow show --workflow-id test-multi-judge-001 --follow
```

**Expected Behavior:**

1. **Parallel Execution:**
   - 3 judges execute simultaneously
   - Each judge returns `{score, confidence, reasoning}`

2. **Consensus Calculation:**
   - Weighted average: `(1.0 * score1 + 0.5 * score2 + 1.5 * score3) / 3.0`
   - Agreement level: Standard deviation of scores
   - Final decision: consensus >= 0.7 && agreement >= 0.6

3. **Branch Selection:**
   - If consensus met → APPROVED state
   - If any judge strongly rejects → REJECTED state

---

### Scenario 5: Human-in-the-Loop Workflow

**Objective:** Test signal-based human input state

#### Step 5.1: Create Human Approval Workflow

Create `demo-agents/workflows/human-approval.yaml`:

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow

metadata:
  name: "human-approval-test"
  version: "1.0.0"
  description: "Test human approval step"

spec:
  context: {}

  initial_state: REQUEST_APPROVAL

  states:
    REQUEST_APPROVAL:
      kind: Human
      prompt: "Do you approve this deployment? (yes/no)"
      timeout: 300s  # 5 minutes
      default_response: "no"
      transitions:
        - condition: input_equals_yes
          target: DEPLOY

        - condition: input_equals_no
          target: CANCELLED

    DEPLOY:
      kind: System
      command: "echo"
      env:
        MESSAGE: "Deployment approved and started"
      transitions: []

    CANCELLED:
      kind: System
      command: "echo"
      env:
        MESSAGE: "Deployment cancelled by user"
      transitions: []
```

#### Step 5.2: Execute and Respond

```bash
# Start workflow
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_human_approval_test \
  --workflow-id test-human-001 \
  --input '{}'

# Workflow will wait for signal...

# Send approval signal
temporal workflow signal \
  --workflow-id test-human-001 \
  --name humanInput \
  --input '"yes"'

# Check completion
temporal workflow show --workflow-id test-human-001
```

**Expected Flow:**

1. Workflow starts → REQUEST_APPROVAL state
2. Waits for `humanInput` signal (up to 300s)
3. Signal received → evaluates condition
4. `input_equals_yes` → transitions to DEPLOY
5. DEPLOY executes → workflow completes

---

### Scenario 6: Volume Management & File System Handoff (ADR-032)

**Objective:** Test unified storage layer with SeaweedFS-backed volumes for multi-agent workflows

#### Step 6.1: Verify SeaweedFS Infrastructure

```bash
# Check all SeaweedFS containers are running
docker compose ps | grep seaweedfs

# Expected output:
# aegis-seaweedfs-master   Up   9333/tcp, 19333/tcp
# aegis-seaweedfs-volume   Up   8080/tcp
# aegis-seaweedfs-filer    Up   8888/tcp, 18888/tcp, 8333/tcp

# Verify filer health
curl -f http://localhost:8888/ && echo "✓ Filer is healthy"

# Verify master status
curl http://localhost:9333/cluster/status | jq

# Expected response:
# {
#   "IsLeader": true,
#   "Leader": "seaweedfs-master:9333",
#   "Peers": []
# }

# Check seaweedfs database created
psql -h localhost -U aegis -d seaweedfs -c "\dt"

# Expected tables: seaweedfs_meta (filer metadata)
```

#### Step 6.2: Run Volume Migration

```bash
# Apply database migration for volumes table
psql -h localhost -U aegis -d aegis -f cli/migrations/002_create_volumes_table.sql

# Verify table created
psql -h localhost -U aegis -d aegis -c "\d volumes"

# Expected columns:
# - id (UUID)
# - name (TEXT)
# - tenant_id (UUID)
# - storage_class (JSONB)
# - filer_endpoint (JSONB)
# - remote_path (TEXT UNIQUE)
# - size_limit_bytes (BIGINT)
# - status (JSONB)
# - ownership (JSONB)
# - created_at, attached_at, detached_at, expires_at (TIMESTAMPTZ)

# Verify indexes
psql -h localhost -U aegis -d aegis -c "\di volumes*"

# Expected indexes:
# - volumes_pkey (PRIMARY KEY on id)
# - idx_volumes_tenant_id
# - idx_volumes_expires_at
# - idx_volumes_ownership (GIN)
# - idx_volumes_status (GIN)
```

#### Step 6.3: Test Volume Creation via SeaweedFS API

```bash
# Create test directory
curl -X POST "http://localhost:8888/dir/?path=/aegis/volumes/test-tenant/test-volume-001"

# Verify creation
curl "http://localhost:8888/dir/status?path=/aegis/volumes/test-tenant/test-volume-001" | jq

# Expected response:
# {
#   "TotalSize": 0,
#   "FileCount": 0
# }

# Set quota (10MB)
curl -X POST "http://localhost:8888/quota?path=/aegis/volumes/test-tenant/test-volume-001&bytes=10485760"

# Upload test file
echo "Hello from AEGIS volume!" > /tmp/test.txt
curl -F "file=@/tmp/test.txt" "http://localhost:8888/aegis/volumes/test-tenant/test-volume-001/"

# Verify file uploaded
curl "http://localhost:8888/dir/status?path=/aegis/volumes/test-tenant/test-volume-001" | jq

# Expected: TotalSize > 0, FileCount = 1

# Test S3 gateway (optional external access)
aws s3 ls s3://aegis/volumes/ --endpoint-url http://localhost:8333 --no-sign-request

# Delete test directory
curl -X DELETE "http://localhost:8888/dir/?path=/aegis/volumes/test-tenant/test-volume-001&recursive=true"
```

#### Step 6.4: Create Multi-Agent Workflow with Shared Volume

Create `demo-agents/workflows/volume-handoff-test.yaml`:

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow

metadata:
  name: "volume-handoff-test"
  version: "1.0.0"
  description: "Test file system handoff between agents via shared volume"

spec:
  # Shared workspace volume for all states
  storage:
    workspace:
      name: "shared-workspace"
      storage_class:
        type: ephemeral
        ttl: "6h"
      size_limit_mb: 100

  context:
    task: "Create a simple Python calculator function"

  initial_state: CODER

  states:
    CODER:
      kind: Agent
      agent: "python-coder"
      input: |
        Create a Python file 'calculator.py' with functions for add, subtract, multiply, divide.
        Save it to /workspace/calculator.py
      timeout: 120s
      transitions:
        - condition: on_success
          target: TESTER

    TESTER:
      kind: Agent
      agent: "python-tester"
      input: |
        Read /workspace/calculator.py and create comprehensive unit tests.
        Save tests to /workspace/test_calculator.py
        Run the tests using pytest and report results.
      timeout: 120s
      transitions:
        - condition: on_success
          target: COMPLETE
        - condition: on_failure
          target: CODER

    COMPLETE:
      kind: System
      command: "echo"
      env:
        MESSAGE: "Code created and tested successfully!"
        WORKSPACE_PATH: "/aegis/volumes/{{workflow_execution_id}}/workspace"
      transitions: []
```

#### Step 6.5: Deploy and Execute Volume Workflow

```bash
# Deploy workflow
cargo run --bin aegis -- --port 8088 workflow deploy demo-agents/workflows/volume-handoff-test.yaml

# Run workflow
temporal workflow start \
  --task-queue aegis-task-queue \
  --type aegis_workflow_volume_handoff_test \
  --workflow-id test-volume-handoff-001 \
  --input '{}'

# Watch execution
temporal workflow show --workflow-id test-volume-handoff-001 --follow

# Check volume in database during execution
psql -h localhost -U aegis -d aegis -c \
  "SELECT id, name, remote_path, size_limit_bytes, status, expires_at FROM volumes WHERE ownership->>'type' = 'workflowexecution';"

# Inspect volume contents via SeaweedFS
WORKFLOW_EXEC_ID=$(psql -h localhost -U aegis -d aegis -t -c \
  "SELECT id FROM workflow_executions WHERE temporal_workflow_id = 'test-volume-handoff-001';")

curl "http://localhost:8888/aegis/volumes/00000000-0000-0000-0000-000000000001/${WORKFLOW_EXEC_ID}/workspace/" | jq

# Expected files:
# - calculator.py
# - test_calculator.py
# - pytest_output.txt (or similar)
```

#### Step 6.6: Test Volume Lifecycle Events

```bash
# Monitor volume events in database
psql -h localhost -U aegis -d aegis -c \
  "SELECT event_type, event_data->>'volume_id' as volume_id, created_at 
   FROM domain_events 
   WHERE event_type LIKE 'Volume%' 
   ORDER BY created_at DESC 
   LIMIT 10;"

# Expected events:
# - VolumeCreated
# - VolumeAttached (CODER state)
# - VolumeDetached (CODER state)
# - VolumeAttached (TESTER state)
# - VolumeDetached (TESTER state)
# - VolumeDeleted (or VolumeExpired after TTL)
```

#### Step 6.7: Test Volume Quota Enforcement

```bash
# Create workflow that exceeds quota
# (Requires modifying agent to write large files)

# Expected behavior:
# - Agent execution fails with quota exceeded error
# - VolumeQuotaExceeded event published
# - Workflow transitions to failure state
```

#### Step 6.8: Test Ephemeral Volume Cleanup

```bash
# Create volume with 1 minute TTL
# Wait 90 seconds
# Run cleanup job (in production, runs every 60 minutes)

# Check expired volumes
psql -h localhost -U aegis -d aegis -c \
  "SELECT id, name, expires_at, status FROM volumes WHERE expires_at < NOW();"

# Expected: Volumes past expiration timestamp

# Trigger manual cleanup (when VolumeManager is implemented)
# curl -X POST http://localhost:8088/v1/volumes/cleanup

# Verify deletion
psql -h localhost -U aegis -d aegis -c \
  "SELECT COUNT(*) FROM volumes WHERE status->>'type' = 'deleted';"
```

**Expected Flow:**

1. Workflow starts → VolumeManager creates workspace volume
2. Volume metadata saved to PostgreSQL
3. SeaweedFS directory created at `/aegis/volumes/{tenant_id}/{volume_id}`
4. Quota set on SeaweedFS filer
5. CODER agent spawns → Docker container mounts volume via NFS
6. Agent writes files to `/workspace` (mapped to SeaweedFS)
7. CODER completes → Volume detached
8. TESTER agent spawns → Same volume attached
9. TESTER reads files from `/workspace` (sees CODER's output)
10. Workflow completes → Volume marked for cleanup
11. TTL expires → Volume deleted from SeaweedFS and PostgreSQL

**Performance Expectations:**

| Operation | Target | Acceptable |
| ------------------------- | -------- | ---------- |
| Volume creation | < 2s | < 5s |
| Volume attach | < 3s | < 10s |
| File write (1MB) | < 500ms | < 2s |
| File read (1MB) | < 300ms | < 1s |
| Volume detach | < 1s | < 3s |
| Volume deletion | < 5s | < 15s |
| Multi-agent file handoff | < 10s | < 30s |

**Troubleshooting Volume Issues:**

```bash
# Check SeaweedFS master logs
docker logs aegis-seaweedfs-master

# Check filer logs
docker logs aegis-seaweedfs-filer

# Verify NFS mount inside container
docker exec -it <agent-container> df -h

# Check volume server status
curl http://localhost:8080/status | jq

# Inspect filer metadata
psql -h localhost -U aegis -d seaweedfs -c "SELECT * FROM seaweedfs_meta LIMIT 10;"

# List all volumes
curl "http://localhost:8888/dir/?path=/aegis/volumes" | jq
```

---

## Expected Outcomes

### Success Indicators

#### ✅ Infrastructure Level

- All Docker containers running (5 services)
- PostgreSQL accepting connections
- Temporal UI accessible at <http://localhost:8233>
- TypeScript worker logs show "Worker is running"
- Rust gRPC server logs show "gRPC server listening on :50051"

#### ✅ Database Level

- 5 tables created (workflows, workflow_executions, agents, executions, workflow_definitions)
- 2 views created (active_workflow_executions, agent_success_rates)
- Sample workflow registered with non-null `temporal_def_json`
- `workflow_definitions` table has matching entry

#### ✅ Workflow Registration Level

- POST to worker returns `201 Created`
- GET from worker returns workflow definition
- Database shows `registered_at` timestamp
- TypeScript logs show "Workflow function generated and registered"

#### ✅ Workflow Execution Level

- Temporal shows workflow as RUNNING then COMPLETED
- Database shows workflow_execution record
- Blackboard contains state outputs
- `final_state` matches expected terminal state

#### ✅ Agent Execution Level (via gRPC)

- gRPC streaming returns events (ExecutionStarted, IterationStarted, etc.)
- Agent execution completes with status
- Iterations recorded in database
- Output captured in workflow blackboard

#### ✅ Validation Level

- Judge agents execute and return gradient scores
- Consensus calculated correctly
- Transitions based on score thresholds work
- Individual judge results stored

### Performance Benchmarks

| Metric | Target | Acceptable |
| -------- | -------- | ------------ |
| Workflow registration | < 500ms | < 1s |
| Simple workflow execution | < 5s | < 10s |
| Agent execution (1 iteration) | < 30s | < 60s |
| Multi-judge consensus | < 90s | < 180s |
| 100monkeys loop (2 iterations) | < 120s | < 300s |

### Log Examples

**Successful Workflow Registration:**

```markdown
INFO  Worker HTTP server listening on 3000
INFO  POST /register-workflow received
INFO  Workflow definition validated
INFO  Storing workflow in database: multi-judge-test
INFO  Generating TypeScript workflow function
INFO  Workflow registered in memory registry: aegis_workflow_multi_judge_test
INFO  Response: 201 Created
```

**Successful Agent Execution:**

```markdown
INFO  gRPC ExecuteAgent request received: agent_id=coder
INFO  Starting execution for agent coder
INFO  Streaming event: ExecutionStarted
INFO  Streaming event: IterationStarted (iteration=1)
INFO  Streaming event: IterationOutput (iteration=1)
INFO  Streaming event: IterationCompleted (iteration=1)
INFO  Streaming event: ExecutionCompleted (total_iterations=1)
INFO  Agent execution completed successfully
```

---

## Troubleshooting

### Issue: Temporal Worker Not Connecting

**Symptoms:**

- Worker logs show "Connection refused"
- No workflows registered

**Solutions:**

```bash
# Check Temporal server is running
docker ps | grep temporal

# Check connection
telnet localhost 7233

# Restart Temporal (run from docker/ folder)
cd docker
docker compose restart temporal

# Check worker environment
docker exec aegis-temporal-worker env | grep TEMPORAL
```

### Issue: Protobuf Compilation Fails

**Symptoms:**

- Rust build error: "proto file not found"

**Solutions:**

```bash
# Verify proto file exists
ls proto/aegis_runtime.proto

# Rebuild with verbose output
cargo clean
cargo build --verbose

# Check build.rs
cat orchestrator/core/build.rs
```

### Issue: gRPC Connection Refused

**Symptoms:**

- Activity logs show "Connection refused :50051"

**Solutions:**

```bash
# Check gRPC server is running
grpcurl -plaintext localhost:50051 list

# Check Rust logs
docker logs aegis-runtime | grep gRPC

# Test with direct gRPC call
grpcurl -plaintext -d '{"command":"echo test"}' \
  localhost:50051 aegis.runtime.v1.AegisRuntime/ExecuteSystemCommand
```

### Issue: Workflow Registration Returns 500

**Symptoms:**

- POST /register-workflow returns Internal Server Error

**Solutions:**

```bash
# Check database connection
psql -h localhost -U aegis -d aegis -c "SELECT 1;"

# Check worker logs
docker logs aegis-temporal-worker

# Verify schema
psql -h localhost -U aegis -d aegis -c "\dt workflow_definitions"

# Test with minimal workflow
curl -X POST http://localhost:3000/register-workflow \
  -H "Content-Type: application/json" \
  -d '{"workflow_id":"test","name":"test","definition":{}}'
```

### Issue: Agent Execution Times Out

**Symptoms:**

- Activity timeout after 60s
- No ExecutionCompleted event

**Solutions:**

```bash
# Check agent is deployed
psql -h localhost -U aegis -d aegis -c \
  "SELECT name, status FROM agents WHERE name = 'coder';"

# Check execution service
curl http://localhost:8088/health

# Increase activity timeout in workflow
# Edit workflow YAML: timeout: 120s
```

### Issue: Blackboard State Not Persisting

**Symptoms:**

- Workflow context empty
- State outputs not available in later states

**Solutions:**

- Check Handlebars template syntax: `{{STATE_NAME.output}}`
- Verify state completed before accessing output
- Check workflow logs for template rendering errors
- Ensure state has transitions (not immediately terminal)

### Issue: Cortex Methods Called

**Symptoms:**

- Logs show "QueryCortexPatterns called but Cortex is not yet implemented"

**Expected:** This is normal! Cortex is stubbed for future implementation.

**Action:** No action needed - stubbed methods return empty results gracefully.

### "Connection Refused" on Port 8088

- Is the daemon running? (`cargo run --bin aegis -- --daemon --port 8088`)
- Is something else using port 8088?

### Temporal Workflow Stays "Running"

- Check the **Temporal Worker** logs: `docker compose logs -f temporal-worker`
- If the worker is crashing or silent, it might have failed to register the workflow or connect to Temporal.

### "Workflow not found"

- Did you run `workflow deploy`? The worker needs to know about the workflow definition to execute it generic interpreter logic.

---

## Advanced Testing

### Test Coverage Matrix

| Component | Unit Tests | Integration Tests | E2E Tests |
| ----------- | ------------ | ------------------- | ----------- |
| TemporalWorkflowMapper | ✅ Rust | ⏳ | ⏳ |
| Workflow Generator | ⏳ | ⏳ | ⏳ |
| gRPC Server | ⏳ | ⏳ | ✅ Manual |
| Activities | ⏳ | ⏳ | ✅ Manual |
| Database Schema | ✅ SQL | ✅ Docker | ✅ Manual |

### Performance Testing

```bash
# Load test workflow registration
ab -n 100 -c 10 \
  -p test-workflow.json \
  -T application/json \
  http://localhost:3000/register-workflow

# Concurrent workflow executions
for i in {1..10}; do
  temporal workflow start \
    --task-queue aegis-task-queue \
    --type aegis_workflow_echo_test \
    --workflow-id test-concurrent-$i \
    --input '{}' &
done
wait

# Check completion
temporal workflow list --query "WorkflowId STARTS_WITH 'test-concurrent-'"
```

### Chaos Testing

```bash
# Kill TypeScript worker mid-execution (run from docker/ folder)
cd docker
docker compose stop temporal-worker

# Workflow should remain in RUNNING state (durable)

# Restart worker
docker compose start temporal-worker

# Workflow should resume from last completed activity
```

---

### Scenario 6: Cortex Pattern Learning

**Objective:** Verify that the Holographic Cortex captures and streams pattern discovery events during execution.

#### Step 6.1: Run a Validation-Heavy Workflow

Use the `100monkeys-classic` workflow, which triggers the validation loop where Cortex learning occurs.

```bash
# Run workflow (if not already running)
cargo run --bin aegis -- --port 8088 workflow run 100monkeys-classic \
  --input '{
    "agent_id": "coder",
    "task": "Write a Python function to calculate factorial",
    "command": "python factorial.py"
  }'
```

#### Step 6.2: Verify Event Streaming

Watch the console output. You should see "Cortex Event" logs interleaved with execution logs when a pattern is discovered (usually after a successful validation).

```bash
# Watch logs
cargo run --bin aegis -- --port 8088 workflow logs <EXECUTION_ID> --follow
```

**Expected Output:**

```markdown
[2026-02-14 12:00:01] Iteration 1 completed
[2026-02-14 12:00:02] Validation performed (score: 0.95, conf: 0.90)
[2026-02-14 12:00:02] Cortex Event: PatternDiscovered { pattern_id: "...", execution_id: "...", ... }
[2026-02-14 12:00:02] Execution completed
```

#### Step 6.3: Verify via HTTP Stream (Optional)

You can also connect to the raw **agent execution** event stream (not the workflow logs) to see the JSON structure of the Cortex event. Use the agent execution ID associated with the `/v1/executions/...` resource, not the workflow execution ID returned by `/v1/workflows/:name/run`.

```bash
curl -N http://localhost:8088/v1/executions/<AGENT_EXECUTION_ID>/events
```

**Expected JSON:**

```json
{
  "Cortex": {
    "PatternDiscovered": {
      "pattern_id": "...",
      "execution_id": "...",
      "discovered_at": "..."
    }
  }
}
```

---

## Next Steps After Testing

1. **If all tests pass:**
   - ✅ Move to Task 9: Implement RegisterWorkflowUseCase
   - ✅ Wire up HTTP endpoints
   - ✅ Implement YAML parsing
   - ✅ Test end-to-end registration flow

2. **If failures occur:**
   - 📋 Document failure symptoms
   - 🔍 Check logs in troubleshooting section
   - 🐛 File issues with reproduction steps
   - 🔧 Fix root cause before proceeding

3. **Performance optimization:**
   - Profile gRPC call latency
   - Optimize database queries
   - Tune Temporal worker concurrency
   - Implement caching where appropriate

4. **Future enhancements:**
   - Implement Cortex with Vector+RAG
   - Add authentication to gRPC
   - Implement workflow versioning
   - Add monitoring/observability

---

## Quick Reference Commands

```bash
# Start everything (run from docker/ folder)
cd docker
docker compose up -d

# Stop everything
docker compose down

# View logs
docker compose logs -f temporal-worker
docker compose logs -f aegis-runtime

# Database queries
psql -h localhost -U aegis -d aegis

# Temporal CLI
temporal workflow list
temporal workflow show --workflow-id <ID>
temporal workflow signal --workflow-id <ID> --name <SIGNAL> --input <JSON>

# gRPC testing
grpcurl -plaintext localhost:50051 list
grpcurl -plaintext -d '...' localhost:50051 <SERVICE>/<METHOD>

# Worker HTTP API
curl http://localhost:3000/workflows
curl http://localhost:3000/health

# Rust CLI (when implemented)
cargo run --bin aegis-orchestrator -- workflow register <YAML>
cargo run --bin aegis-orchestrator -- agent deploy <YAML>
```

---

> **Happy Testing! 🚀**

If you encounter issues not covered in this guide, check:

- Docker logs: `docker compose logs`
- Database state: `psql` queries
- Temporal UI: <http://localhost:8233>
- gRPC health: `grpcurl` commands

For questions, refer to:

- [ADR-022: Temporal Integration](../../aegis-architecture/adrs/022-temporal-workflow-engine-integration.md)
- [Proto Definitions](../../proto/aegis_runtime.proto)
