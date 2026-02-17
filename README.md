# AEGIS Orchestrator

The core runtime and orchestrator for Project AEGIS - a secure, serverless runtime environment for autonomous AI agents.

[![License](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

## Overview

The AEGIS Orchestrator is the control plane that manages agent lifecycle, enforces security policies, and provides runtime isolation through Docker (development) and Firecracker (production) micro-VMs.

## Architecture

```markdown
┌─────────────────────────────────────────────┐
│         AEGIS Orchestrator (Rust)            │
│  • Scheduling  • Security  • State Mgmt     │
└─────────────────────────────────────────────┘
                    │
    ┌───────────────┴───────────────┐
    ▼                               ▼
┌─────────┐                   ┌─────────┐
│ Docker  │                   │Firecracker│
│ Runtime │                   │  Runtime  │
└─────────┘                   └─────────┘
```

## Components

### Core (`core/`)

Pure domain logic implementing:

- Agent lifecycle management
- Runtime trait abstraction
- Security policy engine
- Swarm coordination
- Memory system (Cortex)

### API (`api/`)

HTTP/gRPC server built with Axum for:

- Agent deployment
- Task execution
- Status monitoring
- Management operations

### Runtimes

- **Docker** (`runtime-docker/`): Development runtime using containers
- **Firecracker** (`runtime-firecracker/`): Production runtime with micro-VMs

### CLI (`cli/`)

Command-line tool for local development and agent management:

```bash
# Daemon management
aegis daemon start                    # Start daemon
aegis daemon stop                     # Stop daemon
aegis daemon status                   # Check status

# Agent management
aegis agent deploy agent.yaml         # Deploy agent
aegis agent list                      # List agents
aegis agent logs <agent-name>         # Stream agent logs
aegis agent remove <agent-id>         # Remove agent

# Task execution
aegis task execute <agent-name>       # Execute task
aegis task list                       # List executions
aegis task logs <execution-id>        # View execution logs
aegis task cancel <execution-id>      # Cancel execution
```

See [CLI Reference](docs/CLI_REFERENCE.md) for complete documentation.

### Edge Node (`edge-node/`)

Lightweight binary for hybrid cloud/on-prem deployments.

## Quick Start

### Prerequisites

- Rust 1.75+
- Docker 24.0+
- Ollama (for local LLM) or OpenAI API key
- (Production) Linux with KVM support

### Build

```bash
# Build the CLI and orchestrator
cargo build -p aegis-cli

# Or build in release mode
cargo build --release -p aegis-cli
```

### Configuration

Create or edit `aegis-config.yaml`:

```yaml
node:
  id: "my-node-001"
  type: "edge"
  name: "my-aegis-node"

llm_providers:
  - name: "local"
    type: "ollama"
    endpoint: "http://localhost:11434"
    models:
      - alias: "default"
        model: "phi3:mini"
        capabilities: ["code", "reasoning"]
        context_window: 4096
```

See [aegis-config.yaml](aegis-config.yaml) for a complete example.

### Debugging and Logging

The orchestrator uses structured logging via the `tracing` crate. Log levels: `trace`, `debug`, `info`, `warn`, `error`.

**Set Log Level:**

```bash
# Via environment variable (recommended for development)
export RUST_LOG=debug
cargo run -p aegis-cli -- daemon start

# Via CLI flag
cargo run -p aegis-cli -- daemon start --log-level debug

# Via config file (aegis-config.yaml)
spec:
  observability:
    logging:
      level: "debug"  # trace, debug, info, warn, error
```

**Bootstrap.py Debugging:**

When running at `debug` level, the orchestrator automatically:

- Logs all stdout from `bootstrap.py` (the Python script inside agent containers)
- Logs all stderr from `bootstrap.py` as warnings
- Enables verbose mode in `bootstrap.py` (via `AEGIS_BOOTSTRAP_DEBUG=true` environment variable)

This is useful for tracing LLM connectivity issues, prompt delivery, or agent execution failures.

**Example Debug Output:**

```bash
# Start with debug logging
RUST_LOG=debug cargo run -p aegis-cli -- daemon start

# In another terminal, execute an agent
cargo run -p aegis-cli -- task execute my-agent --input "test"

# You'll see in the orchestrator logs:
# DEBUG aegis_core::infrastructure::runtime: Starting bootstrap.py execution container_id="abc123"
# DEBUG aegis_core::infrastructure::runtime: Bootstrap output: "Attempting to connect to Orchestrator at http://host.docker.internal:8000..."
# DEBUG aegis_core::infrastructure::runtime: Bootstrap output: "[BOOTSTRAP DEBUG] Bootstrap starting - execution_id=xxx, iteration=1"
# DEBUG aegis_core::infrastructure::runtime: Bootstrap output: "[BOOTSTRAP DEBUG] Received prompt (1234 chars)"
```

**Troubleshooting Bootstrap Issues:**

If agents fail to execute or you see connection errors:

1. Enable debug logging: `RUST_LOG=debug`
2. Check bootstrap.py output in orchestrator logs
3. Verify `AEGIS_ORCHESTRATOR_URL` is reachable from inside containers

### Running Locally

```bash
# Start the daemon
target/debug/aegis daemon start

# Check daemon status
target/debug/aegis daemon status

# Deploy demo agents
target/debug/aegis agent deploy ./demo-agents/echo/agent.yaml
target/debug/aegis agent deploy ./demo-agents/greeter/agent.yaml

# List deployed agents
target/debug/aegis agent list

# Execute a task
target/debug/aegis task execute echo --input "Hello Daemon"

# View agent logs
target/debug/aegis agent logs echo

# Stop the daemon
target/debug/aegis daemon stop
```

For detailed instructions, see [Getting Started Guide](docs/GETTING_STARTED.md).

## Development

### Project Structure

```markdown
aegis-orchestrator/
├── core/              # Domain logic (DDD)
├── api/               # HTTP/gRPC server
├── runtime-docker/    # Docker adapter
├── runtime-firecracker/ # Firecracker adapter
├── memory/            # Cortex vector store
├── security/          # Policy enforcement
├── cli/               # CLI tool
├── edge-node/         # Edge node binary
└── tests/             # Integration tests
```

### Architecture Principles

- **Domain-Driven Design**: Clear bounded contexts
- **Hexagonal Architecture**: Pure domain core with infrastructure adapters
- **Type Safety**: Leverage Rust's type system
- **Security First**: Default-deny policies

### Running Tests

```bash
# Unit tests
cargo test --lib

# Integration tests
cargo test --test '*'

# Specific component
cargo test -p aegis-core
```

## Configuration Reference

See [`examples/`](examples/) for sample configurations.

## Security

The orchestrator enforces:

- **Isolation**: Kernel-level (Firecracker) or namespace-based (Docker)
- **Network Control**: DNS/IP allow-listing
- **Resource Limits**: CPU, memory, execution time
- **Audit Trail**: Immutable logging

For details, see [SECURITY.md](SECURITY.md).

## Performance

- **Cold Start**: <125ms (Firecracker)
- **Throughput**: 1,000+ agents/second (target)
- **Memory**: ~128MB per Firecracker VM

## Documentation

### Getting Started

- [Getting Started Guide](docs/GETTING_STARTED.md) - Complete setup walkthrough
- [Local Testing Guide](docs/LOCAL_TESTING.md) - Build and test workflow
- [CLI Reference](docs/CLI_REFERENCE.md) - Complete command documentation
- [Troubleshooting](docs/TROUBLESHOOTING.md) - Common issues and solutions

### Development Guides

- [Agent Development Guide](docs/AGENT_DEVELOPMENT.md) - Creating custom agents
- [Architecture](docs/ARCHITECTURE.md) - System design
- [Contributing](docs/CONTRIBUTING.md) - Development guide

### Security & Operations

- [Security Model](docs/SECURITY.md) - Threat analysis
- [Roadmap](docs/ROADMAP.md) - Future plans

### References

- Enable CUDA for containers
  - <https://learn.microsoft.com/en-us/windows/ai/directml/gpu-cuda-in-wsl>
  - <https://docs.nvidia.com/cuda/wsl-user-guide/index.html#getting-started-with-cuda-on-wsl-2>
  - <https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html>
  - `sudo nvidia-ctk runtime configure --runtime=docker`
  - `sudo systemctl restart docker`

## License

AGPL-3.0. See [LICENSE](LICENSE) for details.

## Related Repositories

- [aegis-sdk-python](https://github.com/100monkeys-ai/aegis-sdk-python) - Python SDK
- [aegis-sdk-typescript](https://github.com/100monkeys-ai/aegis-sdk-typescript) - TypeScript SDK
- [aegis-control-plane](https://github.com/100monkeys-ai/aegis-control-plane) - Web dashboard
- [aegis-examples](https://github.com/100monkeys-ai/aegis-examples) - Example agents

---

**Built with Rust for security, performance, and reliability.**
