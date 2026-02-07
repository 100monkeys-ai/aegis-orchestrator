# Demo Agents

This directory contains example agents demonstrating different capabilities and use cases of AEGIS.

## Available Demo Agents

### Echo Agent

**Path:** `echo/agent.yaml`

**Purpose:** Simple echo agent that returns the input back to the user.

**Use Case:** Testing basic agent deployment and execution.

**Example:**

```bash
aegis agent deploy ./demo-agents/echo/agent.yaml
aegis task execute echo --input "Hello, AEGIS!"
```

**Expected Output:** The input text echoed back.

---

### Greeter Agent

**Path:** `greeter/agent.yaml`

**Purpose:** Personalized greeting agent that creates friendly greetings based on the user's name.

**Use Case:** Demonstrating simple text transformation and personalization.

**Example:**

```bash
aegis agent deploy ./demo-agents/greeter/agent.yaml
aegis task execute greeter --input "Alice"
```

**Expected Output:** A personalized greeting like "Hello Alice, nice to meet you!"

---

### Coder Agent

**Path:** `coder/agent.yaml`

**Purpose:** Technical agent that provides Rust code examples and explanations.

**Use Case:** Code generation and technical assistance.

**Features:**

- Rust programming expertise
- Provides code snippets
- Explains concepts with examples
- Uses modern Rust idioms

**Example:**

```bash
aegis agent deploy ./demo-agents/coder/agent.yaml
aegis task execute coder --input "What are the advantages of using this language?"
aegis task execute coder --input "How do I use async/await in Rust?"
```

**Expected Output:** Concise Rust code examples with brief explanations.

---

### Debater Agent

**Path:** `debater/agent.yaml`

**Purpose:** Argumentative agent that presents both sides of a topic.

**Use Case:** Critical thinking and balanced analysis.

**Features:**

- Presents arguments FOR a topic
- Presents arguments AGAINST a topic
- Provides synthesis and conclusion
- Master debater persona

**Example:**

```bash
aegis agent deploy ./demo-agents/debater/agent.yaml
aegis task execute debater --input "Sushi is delicious."
aegis task execute debater --input "Remote work is better than office work."
```

**Expected Output:** Balanced arguments for and against the statement, with synthesis.

---

### Poet Agent

**Path:** `poet/agent.yaml`

**Purpose:** Creative agent that responds in poetic form.

**Use Case:** Creative writing and artistic text generation.

**Features:**

- Whimsical poet persona
- Responds only in verse
- Short, creative poems
- No prose responses

**Example:**

```bash
aegis agent deploy ./demo-agents/poet/agent.yaml
aegis task execute poet --input "Tell me about the stars"
aegis task execute poet --input "What is love?"
```

**Expected Output:** Creative, rhyming poetry about the topic.

---

### Piglatin Agent

**Path:** `piglatin/agent.yaml`

**Purpose:** Text transformation agent that converts input to Pig Latin.

**Use Case:** Demonstrating text transformation and playful interactions.

**Features:**

- Witty character persona
- Responds in Pig Latin
- Maintains conversational tone

**Example:**

```bash
aegis agent deploy ./demo-agents/piglatin/agent.yaml
aegis task execute piglatin --input "How are you doing today my friend?"
```

**Expected Output:** Input text transformed into Pig Latin.

---

## Quick Start with Demo Agents

### Deploy All Demo Agents

```bash
# Start daemon
aegis daemon start

# Deploy all agents
aegis agent deploy ./demo-agents/echo/agent.yaml
aegis agent deploy ./demo-agents/greeter/agent.yaml
aegis agent deploy ./demo-agents/coder/agent.yaml
aegis agent deploy ./demo-agents/debater/agent.yaml
aegis agent deploy ./demo-agents/poet/agent.yaml
aegis agent deploy ./demo-agents/piglatin/agent.yaml

# Verify deployment
aegis agent list
```

### Test All Demo Agents

```bash
# Echo
aegis task execute echo --input "Hello Daemon"

# Greeter
aegis task execute greeter --input "Jeshua"

# Coder
aegis task execute coder --input "What are the advantages of using this language?"

# Debater
aegis task execute debater --input "Sushi is delicious."

# Poet
aegis task execute poet --input "Tell me about the stars"

# Piglatin
aegis task execute piglatin --input "How are you doing today my friend?"
```

### View Logs

```bash
# View logs for specific agent
aegis agent logs echo
aegis agent logs greeter
aegis agent logs coder

# Stream logs in real-time
aegis agent logs poet --follow

# View all executions
aegis task list
```

## Learning from Demo Agents

### Simple Agents (Start Here)

1. **Echo** - Minimal agent configuration
2. **Greeter** - Basic task instruction

### Intermediate Agents

1. **Coder** - Specialized domain knowledge
2. **Piglatin** - Text transformation

### Advanced Agents

1. **Debater** - Complex reasoning and structure
2. **Poet** - Creative generation with constraints

## Manifest Comparison

### Minimal Configuration (Echo)

```yaml
version: "1.1"
agent:
  name: "echo"
  runtime: "python:3.11"
task:
  instruction: "Echo the input back to the user."
```

### Full Configuration (Coder)

```yaml
version: "1.1"
agent:
  name: "coder"
  version: "0.1.0"
  description: "A technical agent that provides code snippets."
  runtime: "python:3.10-slim"
  autopull: true
  task:
    instruction: "You are an expert Rust programmer..."
  execution:
    max_retries: 3
    timeout_seconds: 60
    mode: "single"
```

## Creating Your Own Agent

Use these demo agents as templates:

1. **Copy a demo agent:**

   ```bash
   cp -r demo-agents/echo my-agents/my-agent
   ```

2. **Edit the manifest:**

   ```bash
   vim my-agents/my-agent/agent.yaml
   ```

3. **Deploy and test:**

   ```bash
   aegis agent deploy ./my-agents/my-agent/agent.yaml
   aegis task execute my-agent --input "test"
   ```

## Common Patterns

### Pattern 1: Simple Transformation

**Example:** Echo, Piglatin

**Characteristics:**

- Minimal configuration
- Direct input → output
- No complex reasoning

**Template:**

```yaml
version: "1.1"
agent:
  name: "transformer"
  runtime: "python:3.11"
task:
  instruction: "Transform the input by..."
```

---

### Pattern 2: Domain Expert

**Example:** Coder

**Characteristics:**

- Specialized knowledge
- Detailed instructions
- Specific output format

**Template:**

```yaml
version: "1.1"
agent:
  name: "expert"
  runtime: "python:3.10-slim"
  autopull: true
  task:
    instruction: |
      You are an expert in [domain].
      When given [input], provide [output].
      Format: [specific format]
  execution:
    timeout_seconds: 120
```

---

### Pattern 3: Creative Generator

**Example:** Poet, Debater

**Characteristics:**

- Creative persona
- Style constraints
- Longer timeout for generation

**Template:**

```yaml
version: "1.1"
agent:
  name: "creator"
  runtime: "python:3.10-slim"
  autopull: true
  task:
    instruction: |
      You are a [creative role].
      Style: [constraints]
      Rules: [specific rules]
  execution:
    timeout_seconds: 180
```

## Troubleshooting Demo Agents

### Agent Won't Deploy

1. **Check manifest syntax:**

   ```bash
   python -c "import yaml; yaml.safe_load(open('demo-agents/echo/agent.yaml'))"
   ```

2. **Verify daemon is running:**

   ```bash
   aegis daemon status
   ```

### Execution Fails

1. **Check LLM provider:**

   ```bash
   # For Ollama
   curl http://localhost:11434/api/tags
   ```

2. **View agent logs:**

   ```bash
   aegis agent logs <agent-name>
   ```

3. **Check Docker:**

   ```bash
   docker ps
   ```

## Next Steps

- **[Agent Development Guide](../docs/AGENT_DEVELOPMENT.md)** - Create custom agents
- **[CLI Reference](../docs/CLI_REFERENCE.md)** - Complete command documentation
- **[Local Testing Guide](../docs/LOCAL_TESTING.md)** - Build and test workflow
- **[Getting Started](../docs/GETTING_STARTED.md)** - Complete setup guide

---

**Explore, experiment, and build your own agents!**
