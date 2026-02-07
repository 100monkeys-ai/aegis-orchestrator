# Agent Development Guide

Learn how to create custom agents for AEGIS using agent manifests.

## Table of Contents

- [Agent Manifest Structure](#agent-manifest-structure)
- [Basic Agent Example](#basic-agent-example)
- [Manifest Fields Reference](#manifest-fields-reference)
- [Task Instructions](#task-instructions)
- [Runtime Configuration](#runtime-configuration)
- [Execution Modes](#execution-modes)
- [Best Practices](#best-practices)
- [Advanced Examples](#advanced-examples)

## Agent Manifest Structure

An agent is defined by a YAML manifest file (`agent.yaml`) that specifies its behavior, runtime, and execution parameters.

### Minimal Example

```yaml
version: "1.1"
agent:
  name: "my-agent"
  runtime: "python:3.11"
task:
  instruction: "Your task instruction here."
```

### Complete Example

```yaml
version: "1.1"
agent:
  name: "my-agent"
  version: "1.0.0"
  description: "A helpful description of what this agent does."
  runtime: "python:3.11"
  autopull: true
  task:
    instruction: "Detailed task instruction for the agent."
  execution:
    max_retries: 3
    timeout_seconds: 120
    mode: "single"
```

## Basic Agent Example

Let's create a simple translation agent:

### `translator/agent.yaml`

```yaml
version: "1.1"
agent:
  name: "translator"
  version: "1.0.0"
  description: "Translates text to different languages."
  runtime: "python:3.11"
  autopull: true
  task:
    instruction: |
      You are a professional translator. When given text and a target language,
      translate the text accurately while preserving tone and context.
      
      Expected input format:
      {
        "text": "Text to translate",
        "target_language": "Spanish"
      }
      
      Respond with only the translated text.
  execution:
    max_retries: 2
    timeout_seconds: 60
    mode: "single"
```

### Deploy and Test

```bash
# Deploy the agent
aegis agent deploy ./translator/agent.yaml

# Execute a translation
aegis task execute translator --input '{"text": "Hello, world!", "target_language": "Spanish"}'
```

## Manifest Fields Reference

### Version

```yaml
version: "1.1"
```

**Required.** Manifest schema version. Current version is `1.1`.

---

### Agent Section

#### `name` (required)

```yaml
agent:
  name: "my-agent"
```

**Type:** String  
**Description:** Unique identifier for the agent. Must be lowercase, alphanumeric, and hyphens only.  
**Example:** `email-summarizer`, `code-reviewer`, `data-analyst`

---

#### `version` (optional)

```yaml
agent:
  version: "1.0.0"
```

**Type:** String (Semantic Versioning)  
**Description:** Agent version for tracking changes.  
**Default:** `"1.0.0"`

---

#### `description` (optional)

```yaml
agent:
  description: "A brief description of the agent's purpose."
```

**Type:** String  
**Description:** Human-readable description of what the agent does.

---

#### `runtime` (required)

```yaml
agent:
  runtime: "python:3.11"
```

**Type:** String  
**Description:** Docker image for the agent runtime.  
**Common values:**

- `python:3.11` - Python 3.11
- `python:3.10-slim` - Python 3.10 (smaller image)
- `node:20` - Node.js 20
- `node:20-alpine` - Node.js 20 (Alpine Linux)

---

#### `autopull` (optional)

```yaml
agent:
  autopull: true
```

**Type:** Boolean  
**Description:** Automatically pull Docker image if not present locally.  
**Default:** `true`

---

### Task Section

#### `instruction` (required)

```yaml
task:
  instruction: "Your detailed task instruction here."
```

**Type:** String (supports multi-line with `|`)  
**Description:** The system prompt or instruction that defines the agent's behavior.

**Best practices:**

- Be specific and clear
- Define expected input/output formats
- Include examples if helpful
- Set the tone and personality
- Specify constraints or limitations

---

### Execution Section

#### `max_retries` (optional)

```yaml
execution:
  max_retries: 3
```

**Type:** Integer  
**Description:** Number of retry attempts on failure.  
**Default:** `0`  
**Range:** `0-10`

---

#### `timeout_seconds` (optional)

```yaml
execution:
  timeout_seconds: 120
```

**Type:** Integer  
**Description:** Maximum execution time in seconds.  
**Default:** `300` (5 minutes)  
**Range:** `1-3600`

---

#### `mode` (optional)

```yaml
execution:
  mode: "single"
```

**Type:** String  
**Description:** Execution mode for the agent.  
**Values:**

- `single` - Execute once and return (default)
- `loop` - Continuous execution loop
- `100monkeys` - Multi-iteration refinement mode

**Default:** `"single"`

---

## Task Instructions

The `task.instruction` field is the most important part of your agent. It defines the agent's behavior.

### Writing Effective Instructions

#### 1. Define the Role

```yaml
task:
  instruction: |
    You are a professional code reviewer with expertise in Rust.
    Your job is to review code for bugs, performance issues, and best practices.
```

#### 2. Specify Input Format

```yaml
task:
  instruction: |
    You are a data analyst.
    
    Expected input:
    {
      "data": [...],
      "analysis_type": "summary" | "trend" | "correlation"
    }
```

#### 3. Define Output Format

```yaml
task:
  instruction: |
    Respond with a JSON object:
    {
      "summary": "Brief summary",
      "details": "Detailed analysis",
      "confidence": 0.0-1.0
    }
```

#### 4. Set Constraints

```yaml
task:
  instruction: |
    Rules:
    - Responses must be under 500 words
    - Use formal tone
    - Cite sources when making claims
    - If uncertain, say "I don't know"
```

#### 5. Provide Examples

```yaml
task:
  instruction: |
    Example input:
    "Explain async/await in Rust"
    
    Example output:
    "Async/await in Rust allows you to write asynchronous code that looks synchronous..."
```

## Runtime Configuration

### Choosing a Runtime

#### Python Runtimes

```yaml
# Full Python 3.11 (includes build tools)
runtime: "python:3.11"

# Slim Python 3.10 (smaller, faster)
runtime: "python:3.10-slim"

# Alpine-based (smallest)
runtime: "python:3.11-alpine"
```

**Use cases:**

- `python:3.11` - Need pip packages with C extensions
- `python:3.10-slim` - Most agents, smaller image
- `python:3.11-alpine` - Minimal footprint

#### Node.js Runtimes

```yaml
# Full Node.js 20
runtime: "node:20"

# Alpine-based (smaller)
runtime: "node:20-alpine"
```

**Use cases:**

- `node:20` - TypeScript agents, npm packages
- `node:20-alpine` - Minimal Node.js agents

### Custom Runtimes

You can use any Docker image:

```yaml
runtime: "rust:1.75"
runtime: "golang:1.21"
runtime: "ubuntu:22.04"
```

## Execution Modes

### Single Mode (Default)

```yaml
execution:
  mode: "single"
```

**Behavior:** Execute once and return the result.

**Use cases:**

- Simple transformations
- One-shot queries
- Stateless operations

---

### Loop Mode

```yaml
execution:
  mode: "loop"
```

**Behavior:** Continuous execution loop until explicitly stopped.

**Use cases:**

- Monitoring tasks
- Periodic data processing
- Event-driven workflows

---

### 100monkeys Mode

```yaml
execution:
  mode: "100monkeys"
```

**Behavior:** Multi-iteration refinement with self-critique.

**Use cases:**

- Creative writing
- Code generation with refinement
- Complex problem-solving

## Best Practices

### 1. Start Simple

Begin with a minimal manifest and iterate:

```yaml
version: "1.1"
agent:
  name: "test-agent"
  runtime: "python:3.11"
task:
  instruction: "Simple instruction to test."
```

### 2. Use Descriptive Names

```yaml
# Good
name: "email-summarizer"
name: "code-reviewer"
name: "data-analyst"

# Avoid
name: "agent1"
name: "test"
name: "my_agent"
```

### 3. Set Appropriate Timeouts

```yaml
# Quick tasks
timeout_seconds: 30

# Standard tasks
timeout_seconds: 120

# Complex tasks
timeout_seconds: 300
```

### 4. Enable Autopull for Convenience

```yaml
autopull: true  # Automatically pull Docker images
```

### 5. Version Your Agents

```yaml
agent:
  version: "1.0.0"  # Initial release
  version: "1.1.0"  # Added feature
  version: "2.0.0"  # Breaking change
```

### 6. Document Expected Input

```yaml
task:
  instruction: |
    Expected input format:
    {
      "field1": "description",
      "field2": "description"
    }
```

### 7. Test Incrementally

```bash
# Deploy
aegis agent deploy ./agent.yaml

# Test with simple input
aegis task execute my-agent --input "test"

# View logs
aegis agent logs my-agent

# Iterate on manifest
# ... edit agent.yaml ...

# Redeploy
aegis agent remove <agent-id>
aegis agent deploy ./agent.yaml
```

## Advanced Examples

### Code Reviewer Agent

```yaml
version: "1.1"
agent:
  name: "code-reviewer"
  version: "1.0.0"
  description: "Reviews code for bugs and best practices."
  runtime: "python:3.11-slim"
  autopull: true
  task:
    instruction: |
      You are an expert code reviewer specializing in Rust.
      
      When given code, analyze it for:
      1. Potential bugs or logic errors
      2. Performance issues
      3. Security vulnerabilities
      4. Best practice violations
      5. Code style and readability
      
      Provide specific, actionable feedback with line references.
      
      Output format:
      {
        "summary": "Overall assessment",
        "issues": [
          {"severity": "high|medium|low", "line": 42, "description": "..."}
        ],
        "suggestions": ["..."]
      }
  execution:
    max_retries: 2
    timeout_seconds: 180
    mode: "single"
```

### Data Analyst Agent

```yaml
version: "1.1"
agent:
  name: "data-analyst"
  version: "1.0.0"
  description: "Analyzes datasets and provides insights."
  runtime: "python:3.11"
  autopull: true
  task:
    instruction: |
      You are a data analyst. Given a dataset, perform statistical analysis.
      
      Input:
      {
        "data": [...],
        "analysis_type": "summary" | "trend" | "correlation"
      }
      
      Provide:
      - Summary statistics
      - Key insights
      - Visualizations (as descriptions)
      - Recommendations
  execution:
    max_retries: 3
    timeout_seconds: 240
    mode: "single"
```

### Creative Writer Agent

```yaml
version: "1.1"
agent:
  name: "creative-writer"
  version: "1.0.0"
  description: "Generates creative content with refinement."
  runtime: "python:3.10-slim"
  autopull: true
  task:
    instruction: |
      You are a creative writer. Generate engaging, original content.
      
      Input: Topic or prompt
      Output: Well-crafted creative piece
      
      Style: Engaging, vivid, original
      Length: 300-500 words
  execution:
    max_retries: 2
    timeout_seconds: 180
    mode: "100monkeys"  # Use refinement mode
```

---

**See Also:**

- [Getting Started Guide](GETTING_STARTED.md)
- [CLI Reference](CLI_REFERENCE.md)
- [Local Testing Guide](LOCAL_TESTING.md)
- [Demo Agents](../demo-agents/README.md)
