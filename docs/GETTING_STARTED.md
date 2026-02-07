# Getting Started with AEGIS

This guide will walk you through setting up AEGIS, deploying your first agent, and understanding the core workflows.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Configuration](#configuration)
- [Starting the Daemon](#starting-the-daemon)
- [Your First Agent](#your-first-agent)
- [Understanding Daemon vs Embedded Mode](#understanding-daemon-vs-embedded-mode)
- [Common Workflows](#common-workflows)
- [Next Steps](#next-steps)

## Prerequisites

### Required

- **Rust 1.75+** - [Install Rust](https://rustup.rs/)
- **Docker 24.0+** - [Install Docker](https://docs.docker.com/get-docker/)
- **Git** - For cloning the repository

### LLM Provider (Choose One)

#### Option 1: Local LLM (Recommended for Development)

- **Ollama** - [Install Ollama](https://ollama.ai/)
- Pull a model: `ollama pull phi3:mini`

#### Option 2: Cloud LLM

- OpenAI API key
- Anthropic API key
- Or other compatible provider

## Installation

### 1. Clone the Repository

```bash
git clone https://github.com/100monkeys-ai/aegis-orchestrator.git
cd aegis-orchestrator
```

### 2. Build the CLI

```bash
# Build in debug mode (faster compilation)
cargo build -p aegis-cli

# Or build in release mode (optimized binary)
cargo build --release -p aegis-cli
```

The binary will be located at:

- Debug: `target/debug/aegis` (or `aegis.exe` on Windows)
- Release: `target/release/aegis` (or `aegis.exe` on Windows)

### 3. Verify Installation

```bash
target/debug/aegis --version
```

## Configuration

### 1. Create Configuration File

Copy the example configuration:

```bash
cp aegis-config.yaml my-config.yaml
```

### 2. Edit Configuration

Open `my-config.yaml` and configure your setup:

```yaml
node:
  id: "dev-node-001"              # Unique node identifier
  type: "edge"                     # Node type: edge, cloud, or hybrid
  name: "my-development-node"      # Human-readable name

llm_providers:
  - name: "local"
    type: "ollama"
    endpoint: "http://localhost:11434"
    models:
      - alias: "default"
        model: "phi3:mini"
        capabilities: ["code", "reasoning"]
        context_window: 4096
      - alias: "fast"
        model: "qwen2.5-coder:7b"
        capabilities: ["code"]
        context_window: 4096

llm_selection:
  strategy: "prefer-local"         # prefer-local, prefer-cloud, or round-robin
  default_provider: "local"
  max_retries: 3
  retry_delay_ms: 1000

observability:
  logging:
    level: "info"                  # trace, debug, info, warn, error
```

### 3. Verify Configuration

```bash
target/debug/aegis config show
```

## Starting the Daemon

The AEGIS daemon is the background service that manages agents and executes tasks.

### 1. Start the Daemon

```bash
target/debug/aegis daemon start
```

You should see:

```bash
✓ Daemon starting (PID: 12345)
Check status with: aegis daemon status
```

### 2. Check Daemon Status

```bash
target/debug/aegis daemon status
```

Expected output:

```bash
✓ Daemon is running
  PID: 12345
  Uptime: 2m
```

### 3. View Daemon Logs

Logs are written to:

- **Linux/macOS**: `/tmp/aegis.out` and `/tmp/aegis.err`
- **Windows**: `%TEMP%\aegis.out` and `%TEMP%\aegis.err`

```bash
# View logs (Linux/macOS)
tail -f /tmp/aegis.out

# View logs (Windows PowerShell)
Get-Content $env:TEMP\aegis.out -Wait
```

## Your First Agent

Let's deploy and run the `echo` demo agent.

### 1. Deploy the Agent

```bash
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml
```

Output:

```bash
Deploying agent: echo
✓ Agent deployed: a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

### 2. List Deployed Agents

```bash
target/debug/aegis agent list
```

Output:

```bash
1 agents found:
ID                                   NAME                 VERSION    STATUS
a1b2c3d4-e5f6-7890-abcd-ef1234567890 echo                 1.1        ready
```

### 3. Execute a Task

```bash
target/debug/aegis task execute echo --input "Hello, AEGIS!"
```

Output:

```bash
Executing agent a1b2c3d4-e5f6-7890-abcd-ef1234567890...
✓ Execution started: b2c3d4e5-f6a7-8901-bcde-f12345678901
```

### 4. View Agent Logs

```bash
target/debug/aegis agent logs echo
```

You'll see the agent's execution history, including the echo response.

### 5. View Execution Logs

```bash
target/debug/aegis task logs b2c3d4e5-f6a7-8901-bcde-f12345678901
```

### 6. List All Executions

```bash
target/debug/aegis task list
```

## Understanding Daemon vs Embedded Mode

AEGIS supports two execution modes:

### Daemon Mode (Recommended)

**How it works:**

- Background service runs continuously
- Manages agents and executions
- Provides HTTP API on port 8000 (default)
- Persists state across CLI invocations

**When to use:**

- Development and testing
- Running multiple agents
- Long-running workflows
- Production deployments

**Commands:**

```bash
aegis daemon start    # Start daemon
aegis daemon stop     # Stop daemon
aegis daemon status   # Check status
```

### Embedded Mode (Fallback)

**How it works:**

- CLI embeds the orchestrator directly
- No background service required
- State only exists during command execution

**When to use:**

- Daemon won't start
- Quick one-off tasks
- Troubleshooting

**Automatic fallback:**
If the daemon isn't running, most commands automatically fall back to embedded mode.

## Common Workflows

### Deploy Multiple Agents

```bash
# Deploy all demo agents
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml
target/debug/aegis agent deploy ./demo-agents/greeter/agent.yaml
target/debug/aegis agent deploy ./demo-agents/coder/agent.yaml
target/debug/aegis agent deploy ./demo-agents/poet/agent.yaml

# List all agents
target/debug/aegis agent list
```

### Execute with Follow Mode

Follow mode streams logs in real-time:

```bash
target/debug/aegis task execute greeter --input "Alice" --follow
```

### Execute with JSON Input

```bash
target/debug/aegis task execute coder --input '{"question": "How do I use async/await in Rust?"}'
```

### Execute with Input from File

```bash
echo '{"topic": "stars"}' > input.json
target/debug/aegis task execute poet --input @input.json
```

### Stream Agent Logs

```bash
# Stream all logs for an agent
target/debug/aegis agent logs echo --follow

# Stream only errors
target/debug/aegis agent logs echo --follow --errors
```

### Cancel Running Execution

```bash
target/debug/aegis task cancel <execution-id>
```

### Remove an Agent

```bash
target/debug/aegis agent remove <agent-id>
```

### Stop the Daemon

```bash
# Graceful shutdown
target/debug/aegis daemon stop

# Force kill if needed
target/debug/aegis daemon stop --force
```

## Next Steps

### Learn More

- **[CLI Reference](CLI_REFERENCE.md)** - Complete command documentation
- **[Agent Development Guide](AGENT_DEVELOPMENT.md)** - Create custom agents
- **[Local Testing Guide](LOCAL_TESTING.md)** - Build and test workflow
- **[Architecture](ARCHITECTURE.md)** - System design deep dive

### Try Demo Agents

Explore the demo agents in `demo-agents/`:

- **echo** - Simple echo agent
- **greeter** - Personalized greetings
- **coder** - Rust code examples
- **poet** - Creative writing
- **debater** - Argumentative responses
- **piglatin** - Text transformation

### Create Your First Agent

See the [Agent Development Guide](AGENT_DEVELOPMENT.md) to create your own agent.

### Troubleshooting

If you encounter issues, see the [Troubleshooting Guide](TROUBLESHOOTING.md).

---

**Need Help?** Check the [documentation](../README.md#documentation) or open an issue on GitHub.
