# Contributing to AEGIS

Thank you for your interest in contributing to Project AEGIS! This document provides guidelines for contributing to the project.

## Table of Contents

1. [Code of Conduct](#code-of-conduct)
2. [Development Setup](#development-setup)
3. [Architecture Overview](#architecture-overview)
4. [Contribution Workflow](#contribution-workflow)
5. [Coding Standards](#coding-standards)
6. [Testing](#testing)
7. [Documentation](#documentation)

## Code of Conduct

We are committed to providing a welcoming and inclusive environment. By participating in this project, you agree to abide by our principles:

- **Respect**: Treat all contributors with respect and professionalism
- **Collaboration**: Work together constructively and value diverse perspectives
- **Security First**: Prioritize security in all contributions
- **Quality**: Maintain high standards for code quality and documentation

## Development Setup

### Prerequisites

- **Rust**: 1.75+ (Edition 2021)
- **Docker**: 24.0+
- **Node.js**: 20+
- **Python**: 3.11+
- **Git**: 2.40+

### Initial Setup

1. **Clone the repository**

   ```bash
   git clone https://github.com/100monkeys-ai/aegis-greenfield.git
   cd aegis-greenfield
   ```

2. **Build the Rust workspace**

   ```bash
   cargo build
   ```

3. **Run tests**

   ```bash
   cargo test
   ```

4. **Install pre-commit hooks**

   ```bash
   ./scripts/setup-hooks.sh
   ```

### Development Workflow

1. **Create a feature branch**

   ```bash
   git checkout -b feature/your-feature-name
   ```

2. **Make your changes**
   - Write code following our standards (see below)
   - Add tests for new functionality
   - Update documentation as needed

3. **Run checks locally**

   ```bash
   cargo fmt --check      # Format check
   cargo clippy           # Linting
   cargo test             # Run tests
   ```

4. **Commit your changes**

   ```bash
   git add .
   git commit -m "feat: add new feature"
   ```

5. **Push and create PR**

   ```bash
   git push origin feature/your-feature-name
   ```

## Architecture Overview

AEGIS follows **Domain-Driven Design (DDD)** principles with clear separation of concerns.

### Directory Structure

```markdown
orchestrator/
├── core/           # Pure domain logic (no IO)
│   ├── agent.rs    # Agent entities and value objects
│   ├── runtime.rs  # Runtime trait and types
│   ├── security.rs # Security policies
│   └── swarm.rs    # Swarm coordination
├── api/            # HTTP/gRPC server (adapter)
├── runtime-docker/ # Docker implementation (adapter)
├── runtime-firecracker/ # Firecracker implementation
└── security/       # Policy enforcement
```

### Key Principles

1. **Bounded Contexts**: Clear separation between Execution, Orchestration, Billing, and Identity
2. **Hexagonal Architecture**: Domain core is pure Rust, infrastructure acts as adapters
3. **Ubiquitous Language**: Consistent terminology throughout code, docs, and UI
4. **Rich Domain Models**: Behavior lives in entities, not service scripts

## Contribution Workflow

### Types of Contributions

- **Bug Fixes**: Fix existing issues
- **Features**: Add new functionality
- **Documentation**: Improve docs and examples
- **Performance**: Optimize code for speed or memory
- **Security**: Address security vulnerabilities

### Commit Message Format

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```markdown
<type>(<scope>): <subject>

<body>

<footer>
```

**Types**:

- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes (formatting, etc.)
- `refactor`: Code refactoring
- `test`: Adding or updating tests
- `chore`: Build process or tooling changes

**Example**:

```markdown
feat(runtime): add Firecracker cold-start optimization

Optimize the VM boot sequence to reduce cold-start time from 150ms to 125ms
by pre-loading kernel modules and using minimal init system.

Closes #123
```

### Pull Request Process

1. **Ensure all checks pass**: Tests, linting, formatting
2. **Update documentation**: If you change APIs or add features
3. **Add tests**: All new code must have tests
4. **Request review**: Tag relevant maintainers
5. **Address feedback**: Make requested changes promptly
6. **Squash commits**: Before merging, squash into logical commits

## Coding Standards

### Rust

- **Edition**: 2021
- **Formatting**: Use `cargo fmt` (rustfmt)
- **Linting**: Use `cargo clippy` with strict settings
- **Error Handling**: Use `Result` and `?` operator, avoid `panic!`
- **Documentation**: All public APIs must have doc comments
- **Async**: Use `async-trait` for traits, prefer `tokio` runtime

**Example**:

```rust
/// Execute a task on a running agent instance.
///
/// # Arguments
/// * `id` - The instance identifier
/// * `input` - Task input containing prompt and context
///
/// # Errors
/// Returns `RuntimeError::ExecutionFailed` if the task fails.
///
/// # Example
/// ```
/// let output = runtime.execute(&id, input).await?;
/// ```
pub async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError>;
```

### Python

- **Version**: 3.11+
- **Formatting**: Use `black` (line length: 100)
- **Linting**: Use `ruff`
- **Type Hints**: All functions must have type annotations
- **Documentation**: Docstrings for all public APIs

**Example**:

```python
async def execute_task(self, agent_id: str, task_input: TaskInput) -> TaskOutput:
    """Execute a task on a deployed agent.

    Args:
        agent_id: ID of the deployed agent
        task_input: Task input with prompt and context

    Returns:
        Task output with result and logs

    Raises:
        HTTPError: If the API request fails
    """
```

### TypeScript

- **Version**: 5.3+
- **Formatting**: Use Prettier
- **Linting**: Use ESLint with strict rules
- **Type Safety**: Enable `strict` mode, avoid `any`
- **Documentation**: JSDoc for all public APIs

## Testing

### Test Categories

1. **Unit Tests**: Test individual functions and modules
2. **Integration Tests**: Test component interactions
3. **End-to-End Tests**: Test full workflows
4. **Security Tests**: Test policy enforcement

### Running Tests

```bash
# Rust tests
cargo test

# Python tests
cd sdks/python
pytest

# TypeScript tests
cd sdks/typescript
npm test

# All tests
./scripts/test-all.sh
```

### Writing Tests

- **Coverage**: Aim for >80% code coverage
- **Clarity**: Tests should be self-documenting
- **Isolation**: Tests should not depend on external state
- **Fast**: Unit tests should run in milliseconds

**Example**:

```rust
#[tokio::test]
async fn test_docker_runtime_spawn() {
    let runtime = DockerRuntime::new("assets/bootstrap.py".to_string(), None, None, "http://localhost:8000".to_string(), None, 2049, 2049).unwrap();
    let config = AgentConfig {
        name: "test-agent".to_string(),
        runtime: "python:3.11".to_string(),
        // ...
    };
    
    let instance_id = runtime.spawn(config).await.unwrap();
    assert!(!instance_id.as_str().is_empty());
}
```

## Documentation

### Types of Documentation

1. **API Docs**: Generated from code comments
2. **Architecture Docs**: High-level design (in `docs/`)
3. **Examples**: Working code samples (in `examples/`)
4. **Guides**: Step-by-step tutorials

### Documentation Standards

- **Clarity**: Write for developers unfamiliar with the codebase
- **Completeness**: Cover all public APIs and features
- **Examples**: Include code examples for complex concepts
- **Updates**: Keep docs in sync with code changes

### Generating Docs

```bash
# Rust API docs
cargo doc --open

# Python API docs
cd sdks/python
pdoc aegis --html

# TypeScript API docs
cd sdks/typescript
npm run docs
```

## Security

### Reporting Vulnerabilities

**Do not** open public issues for security vulnerabilities.

Instead, email: <security@100monkeys.ai>

Include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

### Security Review Process

All security-related changes require:

1. Threat model analysis
2. Security team review
3. Penetration testing (for major features)

## Questions?

- **General Questions**: Open a discussion on GitHub
- **Bug Reports**: Open an issue with the `bug` label
- **Feature Requests**: Open an issue with the `enhancement` label

Thank you for contributing to AEGIS!
