# Local Testing Guide

This guide covers building, testing, and verifying AEGIS locally using the CLI.

## Table of Contents

- [Quick Build and Test](#quick-build-and-test)
- [Build Process](#build-process)
- [Testing Demo Agents](#testing-demo-agents)
- [Automated Test Script](#automated-test-script)
- [Debugging](#debugging)
- [Performance Testing](#performance-testing)
- [Integration Testing](#integration-testing)

## Quick Build and Test

### One-Line Build and Test

```bash
pkill aegis || true && \
cargo build -p aegis-cli && \
target/debug/aegis daemon start && \
target/debug/aegis daemon status && \
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml && \
target/debug/aegis task execute echo --input "Hello Daemon" && \
target/debug/aegis agent logs echo
```

This command:

1. Kills any existing aegis processes
2. Builds the CLI
3. Starts the daemon
4. Verifies daemon status
5. Deploys the echo agent
6. Executes a test task
7. Views the agent logs

## Build Process

### Clean Build

```bash
# Clean previous builds
cargo clean

# Build CLI in debug mode (faster compilation)
cargo build -p aegis-cli

# Build in release mode (optimized)
cargo build --release -p aegis-cli
```

### Build All Components

```bash
# Build all workspace crates
cargo build

# Build specific components
cargo build -p aegis-core
cargo build -p aegis-sdk
cargo build -p cortex
```

### Check for Compilation Errors

```bash
# Check without building
cargo check

# Check with all features
cargo check --all-features

# Check specific package
cargo check -p aegis-cli
```

## Testing Demo Agents

### Deploy All Demo Agents

```bash
# Start daemon first
target/debug/aegis daemon start
target/debug/aegis daemon status

# Deploy all demo agents
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml
target/debug/aegis agent deploy ./demo-agents/greeter/agent.yaml
target/debug/aegis agent deploy ./demo-agents/coder/agent.yaml
target/debug/aegis agent deploy ./demo-agents/debater/agent.yaml
target/debug/aegis agent deploy ./demo-agents/poet/agent.yaml
target/debug/aegis agent deploy ./demo-agents/piglatin/agent.yaml

# Verify all agents deployed
target/debug/aegis agent list
```

### Test Each Agent

#### Echo Agent

```bash
target/debug/aegis task execute echo --input "Hello Daemon"
target/debug/aegis agent logs echo
```

**Expected behavior:** Echoes the input back.

---

#### Greeter Agent

```bash
target/debug/aegis task execute greeter --input "Jeshua"
target/debug/aegis agent logs greeter
```

**Expected behavior:** Personalized greeting with the provided name.

---

#### Coder Agent

```bash
target/debug/aegis task execute coder --input "What are the advantages of using this language?"
target/debug/aegis agent logs coder
```

**Expected behavior:** Provides Rust code examples and explanations.

---

#### Debater Agent

```bash
target/debug/aegis task execute debater --input "Sushi is delicious."
target/debug/aegis agent logs debater
```

**Expected behavior:** Provides a counterargument or debate response.

---

#### Poet Agent

```bash
target/debug/aegis task execute poet --input "Tell me about the stars"
target/debug/aegis agent logs poet
```

**Expected behavior:** Generates creative, poetic text.

---

#### Piglatin Agent

```bash
target/debug/aegis task execute piglatin --input "How are you doing today my friend?"
target/debug/aegis agent logs piglatin
```

**Expected behavior:** Translates text to Pig Latin.

---

### View All Executions

```bash
# List all task executions
target/debug/aegis task list

# View specific execution logs
target/debug/aegis task logs <execution-id>
```

## Automated Test Script

Create a test script to automate the full workflow:

### `test-aegis.sh` (Linux/macOS)

```bash
#!/bin/bash
set -e

echo "=== AEGIS Build and Test ==="

# 1. Clean up existing processes
echo "Stopping existing daemon..."
pkill aegis || true
sleep 2

# 2. Build
echo "Building aegis-cli..."
cargo build -p aegis-cli

# 3. Start daemon
echo "Starting daemon..."
target/debug/aegis daemon start
sleep 3

# 4. Verify daemon
echo "Checking daemon status..."
target/debug/aegis daemon status

# 5. Deploy agents
echo "Deploying demo agents..."
for agent in echo greeter coder debater poet piglatin; do
    echo "  - Deploying $agent..."
    target/debug/aegis agent deploy ./demo-agents/$agent/agent.yaml
done

# 6. List agents
echo "Listing deployed agents..."
target/debug/aegis agent list

# 7. Test agents
echo "Testing agents..."

echo "  - Testing echo..."
target/debug/aegis task execute echo --input "Hello Daemon"

echo "  - Testing greeter..."
target/debug/aegis task execute greeter --input "Jeshua"

echo "  - Testing coder..."
target/debug/aegis task execute coder --input "What are the advantages of using this language?"

echo "  - Testing debater..."
target/debug/aegis task execute debater --input "Sushi is delicious."

echo "  - Testing poet..."
target/debug/aegis task execute poet --input "Tell me about the stars"

echo "  - Testing piglatin..."
target/debug/aegis task execute piglatin --input "How are you doing today my friend?"

# 8. View logs
echo "Viewing agent logs..."
for agent in echo greeter coder debater poet piglatin; do
    echo "  - Logs for $agent:"
    target/debug/aegis agent logs $agent | head -20
done

# 9. List executions
echo "Listing all executions..."
target/debug/aegis task list

echo "=== Test Complete ==="
```

Make it executable:

```bash
chmod +x test-aegis.sh
./test-aegis.sh
```

### `test-aegis.ps1` (Windows PowerShell)

```powershell
Write-Host "=== AEGIS Build and Test ===" -ForegroundColor Green

# 1. Clean up
Write-Host "Stopping existing daemon..." -ForegroundColor Yellow
Get-Process aegis -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# 2. Build
Write-Host "Building aegis-cli..." -ForegroundColor Yellow
cargo build -p aegis-cli

# 3. Start daemon
Write-Host "Starting daemon..." -ForegroundColor Yellow
.\target\debug\aegis.exe daemon start
Start-Sleep -Seconds 3

# 4. Verify daemon
Write-Host "Checking daemon status..." -ForegroundColor Yellow
.\target\debug\aegis.exe daemon status

# 5. Deploy agents
Write-Host "Deploying demo agents..." -ForegroundColor Yellow
$agents = @("echo", "greeter", "coder", "debater", "poet", "piglatin")
foreach ($agent in $agents) {
    Write-Host "  - Deploying $agent..." -ForegroundColor Cyan
    .\target\debug\aegis.exe agent deploy ".\demo-agents\$agent\agent.yaml"
}

# 6. List agents
Write-Host "Listing deployed agents..." -ForegroundColor Yellow
.\target\debug\aegis.exe agent list

# 7. Test agents
Write-Host "Testing agents..." -ForegroundColor Yellow

Write-Host "  - Testing echo..." -ForegroundColor Cyan
.\target\debug\aegis.exe task execute echo --input "Hello Daemon"

Write-Host "  - Testing greeter..." -ForegroundColor Cyan
.\target\debug\aegis.exe task execute greeter --input "Jeshua"

Write-Host "  - Testing coder..." -ForegroundColor Cyan
.\target\debug\aegis.exe task execute coder --input "What are the advantages of using this language?"

# 8. List executions
Write-Host "Listing all executions..." -ForegroundColor Yellow
.\target\debug\aegis.exe task list

Write-Host "=== Test Complete ===" -ForegroundColor Green
```

Run it:

```powershell
.\test-aegis.ps1
```

## Debugging

### View Daemon Logs

```bash
# Linux/macOS
tail -f /tmp/aegis.out
tail -f /tmp/aegis.err

# Windows PowerShell
Get-Content $env:TEMP\aegis.out -Wait
Get-Content $env:TEMP\aegis.err -Wait
```

### Enable Debug Logging

```bash
# Start daemon with debug logging
target/debug/aegis --log-level debug daemon start

# Or set environment variable
export AEGIS_LOG_LEVEL=debug
target/debug/aegis daemon start
```

### Check Agent Execution Logs

```bash
# View logs for specific agent
target/debug/aegis agent logs <agent-name> --follow

# View logs for specific execution
target/debug/aegis task logs <execution-id> --follow

# View errors only
target/debug/aegis agent logs <agent-name> --errors
```

### Verify Configuration

```bash
# Show current configuration
target/debug/aegis config show

# Validate configuration
target/debug/aegis config validate
```

### Check Docker Containers

```bash
# List running containers
docker ps

# View container logs
docker logs <container-id>

# Inspect container
docker inspect <container-id>
```

## Performance Testing

### Measure Cold Start Time

```bash
# Deploy agent
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml

# Measure execution time
time target/debug/aegis task execute echo --input "test"
```

### Concurrent Executions

```bash
# Execute multiple tasks in parallel
for i in {1..10}; do
    target/debug/aegis task execute echo --input "Test $i" &
done
wait

# Check all executions
target/debug/aegis task list --limit 20
```

### Memory Usage

```bash
# Monitor daemon memory
ps aux | grep aegis

# Or use htop
htop -p $(pgrep aegis)
```

## Integration Testing

### Test with Different Runtimes

```yaml
# agent.yaml with different runtime
agent:
  name: "test-agent"
  runtime: "python:3.11"      # Test Python 3.11
  # runtime: "python:3.10-slim" # Test Python 3.10 slim
  # runtime: "node:20"          # Test Node.js
```

### Test Error Handling

```bash
# Test with invalid input
target/debug/aegis task execute echo --input '{"invalid": json'

# Test with non-existent agent
target/debug/aegis task execute nonexistent --input "test"

# Test cancellation
EXEC_ID=$(target/debug/aegis task execute poet --input "Write a long poem" | grep -oP 'Execution started: \K[a-f0-9-]+')
sleep 1
target/debug/aegis task cancel $EXEC_ID
```

### Test Configuration Changes

```bash
# Test with custom config
target/debug/aegis --config ./test-config.yaml daemon start

# Test with different port
target/debug/aegis --port 9000 daemon start
```

## Cleanup

### Stop Daemon and Clean Up

```bash
# Stop daemon
target/debug/aegis daemon stop

# Remove all agents (if needed)
for agent_id in $(target/debug/aegis agent list | grep -oP '^[a-f0-9-]{36}'); do
    target/debug/aegis agent remove $agent_id
done

# Clean build artifacts
cargo clean
```

---

**See Also:**

- [Getting Started Guide](GETTING_STARTED.md)
- [CLI Reference](CLI_REFERENCE.md)
- [Troubleshooting](TROUBLESHOOTING.md)
