# Judge Agents

**Purpose:** Declarative validation agents that replace hardcoded judge logic

---

## Overview

Judge agents are specialized agents whose role is to **evaluate and score** the outputs of other agents. This implements the **Agent-as-Judge** pattern, where validation is no longer hardcoded Rust logic but instead a declarative agent execution.

### Why Agent-as-Judge?

**Before (Hardcoded):**

```rust
// Rust code in ExecutionSupervisor
fn validate_output(output: &str) -> bool {
    output.contains("success")  // ❌ Rigid, not evolvable
}
```

**After (Declarative):**

```yaml
# Judge agent manifest
prompt: "Evaluate this code on a 0.0-1.0 scale..."
validation:
  type: json_schema
  schema: { score: number, reasoning: string }
```

**Benefits:**

- ✅ **Fractal Self-Similarity:** Judges are agents, agents can judge themselves
- ✅ **Evolvability:** Swap judge prompts without recompiling
- ✅ **Transparency:** Judge reasoning is visible to users
- ✅ **Composability:** Judges can call sub-judges (recursive)

---

## Available Judges

### 1. basic-judge.yaml

**Role:** General-purpose code validation  
**Scoring:** 0.0-1.0 gradient scale  
**Max Iterations:** 1 (no refinement)  
**Network:** Denied  
**Filesystem:** Read-only

**Input Context:**

- `task` - Original task description
- `code` - Generated code
- `output` - Execution output
- `exit_code` - Process exit code
- `execution_time_ms` - Runtime duration

**Output Schema:**

```json
{
  "score": 0.85,
  "confidence": 0.92,
  "reasoning": "Detailed explanation...",
  "suggestions": ["Improvement 1", "Improvement 2"],
  "verdict": "pass"
}
```

**Scoring Rubric:**

- `1.0` - Perfect execution, meets all requirements
- `0.8-0.9` - Excellent, minor improvements possible
- `0.6-0.7` - Good, some edge cases missed
- `0.4-0.5` - Fair, partial functionality
- `0.2-0.3` - Poor, major errors
- `0.0-0.1` - Failed, non-functional

**Verdict Logic:**

- `pass` - Score >= 0.70 (transition to completion)
- `refine` - 0.30 < Score < 0.70 (trigger refinement)
- `fail` - Score <= 0.30 (terminal failure)

---

## Usage in Workflows

### Example: 100monkeys-classic Workflow

```yaml
states:
  VALIDATE:
    kind: Agent
    agent: basic-judge  # Use judge agent instead of hardcoded logic
    input:
      task: "{{context.task}}"
      code: "{{state_outputs.GENERATE.code}}"
      output: "{{state_outputs.EXECUTE.stdout}}"
      exit_code: "{{state_outputs.EXECUTE.exit_code}}"
      execution_time_ms: "{{state_outputs.EXECUTE.duration_ms}}"
    
    transitions:
      - condition: score_above
        threshold: 0.95
        target: COMPLETE
      
      - condition: score_between
        min: 0.30
        max: 0.95
        target: REFINE
      
      - condition: score_below
        threshold: 0.30
        target: FAILED
```

### Example: Recursive Judge (Judge-as-Judge)

```yaml
states:
  PRIMARY_JUDGE:
    kind: Agent
    agent: basic-judge
    input:
      task: "{{context.task}}"
      code: "{{state_outputs.GENERATE.code}}"
      output: "{{state_outputs.EXECUTE.stdout}}"
    
    transitions:
      - condition: confidence_below
        threshold: 0.80
        target: META_JUDGE  # Low confidence? Ask another judge!
      
      - condition: confidence_above
        threshold: 0.80
        target: COMPLETE
  
  META_JUDGE:
    kind: Agent
    agent: meta-judge  # Judge that evaluates other judges
    input:
      primary_score: "{{state_outputs.PRIMARY_JUDGE.score}}"
      primary_reasoning: "{{state_outputs.PRIMARY_JUDGE.reasoning}}"
      execution_output: "{{state_outputs.EXECUTE.stdout}}"
    
    transitions:
      - condition: consensus  # Both judges agree
        target: COMPLETE
      
      - condition: always
        target: REFINE
```

---

## Creating Custom Judges

### Step 1: Define Your Judge Manifest

```yaml
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: my-custom-judge
  labels:
    role: judge

spec:
  runtime:
    language: python
    timeout: 30s
  
  security:
    network:
      mode: deny-all
    resources:
      max_iterations: 1
  
  prompt: |
    You are evaluating {{domain}}-specific code.
    
    Score the output from 0.0 to 1.0 based on:
    - Correctness
    - Efficiency
    - Security
    
    Output JSON: {"score": 0.85, "reasoning": "..."}
  
  validation:
    type: json_schema
    schema:
      type: object
      required: [score, reasoning]
```

### Step 2: Deploy Your Judge

```bash
aegis agent deploy judges/my-custom-judge.yaml
```

### Step 3: Use in Workflow

```yaml
states:
  VALIDATE:
    kind: Agent
    agent: my-custom-judge
    input:
      code: "{{state_outputs.GENERATE.code}}"
```

---

## Judge Categories

### 1. **Correctness Judges**

- Validate functional requirements
- Check output against expected results
- Example: `basic-judge.yaml`

### 2. **Security Judges**

- Scan for vulnerabilities
- Check for unsafe patterns
- Example: `security-judge.yaml` (TODO)

### 3. **Performance Judges**

- Measure execution time
- Analyze complexity
- Example: `performance-judge.yaml` (TODO)

### 4. **Style Judges**

- Code formatting
- Documentation quality
- Example: `style-judge.yaml` (TODO)

### 5. **Meta-Judges**

- Evaluate other judges
- Consensus building
- Example: `meta-judge.yaml` (TODO)

---

## Best Practices

### 1. Keep Judges Stateless

Judges should not have side effects. They observe and score only.

### 2. Use Gradient Scores

Prefer 0.0-1.0 scores over binary pass/fail. This enables nuanced feedback.

### 3. Include Reasoning

Always output detailed reasoning. This builds trust and enables learning.

### 4. Set Max Iterations = 1

Judges should not refine themselves. They execute once and return a verdict.

### 5. Deny Network Access

Judges evaluate local execution artifacts. They don't need external access.

---

## Recursive Depth Limits

To prevent infinite recursion:

- **Max Depth:** 3 levels (primary → meta → super-meta)
- **Tracked Via:** `parent_execution_id` field in `Execution` entity
- **Enforced By:** WorkflowEngine checks depth before spawning nested agents

```rust
// In WorkflowEngine::execute_state()
if execution.depth >= MAX_RECURSIVE_DEPTH {
    return Err(Error::MaxDepthExceeded);
}
```

---

## Future Enhancements

### Multi-Judge Consensus (Phase 3)

Run multiple judges in parallel and aggregate scores:

```yaml
states:
  VALIDATE:
    kind: ParallelAgents
    agents:
      - basic-judge
      - security-judge
      - performance-judge
    
    consensus:
      method: weighted_average
      weights:
        basic-judge: 0.5
        security-judge: 0.3
        performance-judge: 0.2
      min_agreement: 0.70
```

### Adaptive Judge Selection (Phase 6)

Use stimulus-response routing to select judges based on task type:

```yaml
# Stimulus: Python code validation
# Response: Select python-judge + basic-judge

# Stimulus: Security audit
# Response: Select security-judge + compliance-judge
```

---

## References

- **ADR-016:** Agent-as-Judge Pattern
- **ADR-017:** Gradient Validation System
- **WORKFLOW_MANIFEST_SPEC.md:** StateKind definitions
- **AGENTS.md:** Domain-Driven Design principles
