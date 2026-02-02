# AEGIS Orchestrator

The core runtime and orchestrator for Project AEGIS - a secure, serverless runtime environment for autonomous AI agents.

[![License](https://img.shields.io/badge/license-BSL%201.1-blue.svg)](LICENSE)
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

Command-line tool for local development:

```bash
aegis run agent.yaml      # Run agent locally
aegis deploy agent.yaml   # Deploy to cloud
aegis logs <agent-id>     # View logs
```

### Edge Node (`edge-node/`)

Lightweight binary for hybrid cloud/on-prem deployments.

## Quick Start

### Prerequisites

- Rust 1.75+
- Docker 24.0+
- (Production) Linux with KVM support

### Build

```bash
# Build all components
cargo build --release

# Run tests
cargo test

# Install CLI
cargo install --path cli
```

### Running Locally

```bash
# Start the orchestrator
cargo run --bin aegis-orchestrator

# In another terminal, run an agent
aegis run examples/email-summarizer/agent.yaml
```

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

## Configuration

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

- [Architecture](docs/ARCHITECTURE.md) - System design
- [Security Model](docs/SECURITY.md) - Threat analysis
- [Contributing](CONTRIBUTING.md) - Development guide

## License

Apache License 2.0. See [LICENSE](LICENSE) for details.

## Related Repositories

- [aegis-sdk-python](https://github.com/aent-ai/aegis-sdk-python) - Python SDK
- [aegis-sdk-typescript](https://github.com/aent-ai/aegis-sdk-typescript) - TypeScript SDK
- [aegis-control-plane](https://github.com/aent-ai/aegis-control-plane) - Web dashboard
- [aegis-examples](https://github.com/aent-ai/aegis-examples) - Example agents

---

**Built with Rust for security, performance, and reliability.**
