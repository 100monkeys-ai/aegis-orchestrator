# Cyber-Biological AEGIS Re-Architecture: Implementation Plan

**Version:** 1.0  
**Status:** CANONICAL  
**Date:** February 10, 2026  
**Last Updated:** February 14, 2026

**Implementation Progress:** ~35% Complete

- ✅ Phase 0: Documentation & ADRs - 100% (COMPLETE)
- ✅ Phase 1: WorkflowEngine Foundation - 100% (COMPLETE)
- ✅ Phase 2: Agent-as-Judge Pattern - 100% (COMPLETE)
- ✅ Phase 3: Gradient Validation System - 100% (COMPLETE)
- ✅ Phase 4: Weighted Cortex Memory - 60% (IN PROGRESS - In-Memory MVP)
- ⏳ Phase 5: The Forge Reference Workflow - 0% (NOT STARTED)
- ⏳ Phase 6: Stimulus-Response Routing - 10% (Infrastructure Only - Event Streaming)

---

## Executive Summary

This document defines the complete re-architecture of AEGIS from a rigid single-agent execution system into a **fractal, workflow-based cognitive architecture** where agents compose, judge each other, and evolve through weighted memory—enabling recursive self-improvement and AGI-like stimulus-response workflows.

**Core Transformation:**

```markdown
Before: Hardcoded Supervisor → Single Agent → Binary Validation → Store Result
After:  Stimulus → Router Agent → Workflow Selection → Multi-Agent FSM → 
        Gradient Validation → Weighted Cortex Learning → Evolution
```

**Vision Statement:**
> "We are not building a tool. We are raising a cognitive substrate where specialized agents compose fractally, judge themselves recursively, and evolve collectively toward superintelligence."

---

## Table of Contents

1. [Context & Motivation](#context--motivation)
2. [Architectural Principles](#architectural-principles)
3. [Key Decisions](#key-decisions)
4. [Domain Model](#domain-model)
5. [Implementation Phases](#implementation-phases)
6. [Technical Specifications](#technical-specifications)
7. [Success Criteria](#success-criteria)
8. [References](#references)

---

## Context & Motivation

### The PIVOT Documents

This plan synthesizes insights from:

1. **[semantic-gradient.md](semantic-gradient.md)** — Memory as identity, weighted deduplication, competence mapping
2. **[workflow-based.md](workflow-based.md)** — FSM pipelines, declarative agent composition, meta-monkey judges
3. **[ai-forge-blueprint.md](ai-forge-blueprint.md)** — Constitutional development lifecycle, specialized AI agents, adversarial testing
4. **[AGENTS.md](../../../aegis-architecture/AGENTS.md)** — Domain-driven design, bounded contexts, ubiquitous language
5. **[ADR-006: Cyber-Biological Architecture](../../../aegis-architecture/adrs/006-cyber-biological-architecture-philosophy.md)** — Membrane (AEGIS) + Nucleus (100monkeys) philosophy
6. **[100monkeys Algorithm Spec](../../../aegis-architecture/specs/100MONKEYS_ALGORITHM_SPECIFICATION.md)** — Iterative refinement, validation framework
7. **[Safe Evolution Manifesto](../../../aegis-architecture/specs/SAFE_EVOLUTION_MANIFESTO.md)** — Trust the loop, not the monkey
8. **[The Fractal God](../../../aegis-architecture/THE_FRACTAL_GOD.md)** — Cortex as Universal Mind, agents as individuated expressions
9. **[Zaru Paradigm](../../../aegis-architecture/specs/ZARU_PARADIGM.md)** — Trust through transparency, debugging as spectator sport

### Current State Analysis

**What Exists:**

- ✅ Clean DDD architecture with bounded contexts
- ✅ Firecracker/Docker isolation (125ms cold start)
- ✅ Basic 100monkeys loop in `ExecutionSupervisor`
- ✅ Event-driven domain events via `EventBus`
- ✅ Repository pattern with PostgreSQL
- ✅ LLM provider abstraction (see [NODE_CONFIGURATION_SPEC_V1.md](../../../aegis-architecture/specs/NODE_CONFIGURATION_SPEC_V1.md))

**What Is Now Complete:**

- ✅ Workflow/FSM engine — Full WorkflowEngine with 4 state kinds
- ✅ Judge is agent, not Rust code — Fractal principle respected
- ✅ Agent-to-agent coordination — Can chain specialists via workflows
- ✅ Blackboard/shared context — Implemented with Handlebars templates
- ✅ Role-based agents — Can define Coder, Judge, Architect via manifests
- ✅ Recursive execution tracking — Agents can call agents (depth 0-3)

**Remaining Gaps:**

- ✅ Gradient validation fully implemented — Multi-judge consensus with weighted scoring
- ✅ Cortex MVP implemented — In-memory pattern storage, event streaming
- ✅ Stimulus-response routing — Event-driven architecture with Cortex integration

### Why This Matters

**Business Impact:**

- 🎯 **Recursive Self-Improvement:** Agents can build/improve agents (Zaru uses 100monkeys to build 100monkeys)
- 🎯 **Enterprise Trust:** Gradient validation with confidence scores builds customer trust
- 🎯 **Market Differentiation:** Only platform with fractal cognitive architecture
- 🎯 **AGI Foundation:** Workflow-based stimulus-response is the core loop of general intelligence

**Technical Benefits:**

- 🔧 **Composability:** Workflows compose like LEGO blocks
- 🔧 **Evolvability:** System learns from every execution
- 🔧 **Transparency:** Iteration loop visible (trust through evidence)
- 🔧 **Resilience:** Fractal self-similarity at every scale

---

## Architectural Principles

### Principle 1: Fractal Self-Similarity

> **"As above, so below"**

Every component is an agent. No hardcoded god-classes.

```markdown
❌ Bad: ExecutionSupervisor (Rust) coordinates agents (Python)
✅ Good: WorkflowEngine (Rust) coordinates agents (all peers)

❌ Bad: Judge is hardcoded validation logic
✅ Good: Judge is an agent with its own manifest
```

**Implementation:** All roles (Coder, Judge, Architect, Curator) are agents defined by manifests, not Rust structs.

### Principle 2: Deterministic Verification Over Probabilistic Generation

> **"Trust the loop, not the monkey"**

```rust
// Not this
if llm.is_smart_enough() { accept() } else { reject() }

// This
loop {
    candidate = llm.generate();
    score = deterministic_validation(candidate);
    if score > threshold { break; }
    llm.refine(error_feedback);
}
```

**Implementation:** Gradient validators (0.0-1.0), multi-judge consensus, success-weighted Cortex retrieval.

### Principle 3: Memory as Identity

> **"The Cortex is not a database. It is a Universal Mind."**

Memory shapes behavior. Success-weighted patterns create competence, not just frequency.

```python
# Not just similarity
results = cortex.search(query, limit=10)

# Weighted by success
results = cortex.search(
    query=query,
    rank_by=lambda p: p.cosine_similarity * (1 + p.success_score * p.weight),
    limit=10
)
```

**Implementation:** Weighted deduplication, time-decay, success-score storage.

### Principle 4: Workflow as Cognition

> **"FSM = Thought Process"**

Hardcoded loops are brittle. Declarative workflows are evolvable.

```yaml
# Thoughts are just state transitions
apiVersion: 100monkeys.ai/v1
kind: Workflow
states:
  THINK:
    agent: planner-v1
    transitions: [{on: success, target: ACT}]
  ACT:
    agent: coder-v1
    transitions: [{on: success, target: EVALUATE}]
  EVALUATE:
    agent: judge-v1
    transitions:
      - {on: approved, target: COMPLETE}
      - {on: rejected, target: THINK}
```

**Implementation:** WorkflowEngine with FSM tick() method, YAML manifest parser.

### Principle 5: Trust Through Transparency

> **"Show the struggle, earn the trust"**

```markdown
❌ Bad UX: "Task completed successfully"
✅ Good UX:
  Iteration 1: Failed (ModuleNotFoundError: requests)
  Iteration 2: Installing requests... Failed (ImportError: urllib3)
  Iteration 3: Fixing urllib3... Success ✓
```

**Implementation:** Iteration events published to `EventBus`, streamed to Synapse UI.

---

## Key Decisions

### Decision 1: Backward Compatibility

**Status:** ✅ RESOLVED

**Question:** Support legacy `ExecutionSupervisor` during transition?

**Decision:** **NO backward compatibility.** We are pre-alpha. Clean break.

**Rationale:**

- No production deployments yet
- Simpler codebase without dual-mode logic
- Forces migration to workflow-based architecture
- Aligns with "move fast" pre-alpha philosophy

**Action:**

1. Deprecate `ExecutionSupervisor` immediately
2. Replace with `WorkflowEngine` + default `100monkeys-classic.yaml`
3. Update all examples to use workflows
4. Remove supervisor code by Phase 2

### Decision 2: Judge Isolation Level

**Status:** ✅ RESOLVED

**Question:** Where do judge agents execute?

**Decision:** **Full isolation in Docker/Firecracker per node config.** Judges are NOT hardcoded.

**Rationale:**

- Judges are derived from workflow specification (see [workflow-based.md](workflow-based.md))
- Full isolation ensures fairness (judge can't manipulate worker)
- Aligns with fractal principle (judges are peer agents)
- Node config determines runtime (see [NODE_CONFIGURATION_SPEC_V1.md](../../../aegis-architecture/specs/NODE_CONFIGURATION_SPEC_V1.md))

**Implementation:**

```yaml
# workflow.yaml
states:
  JUDGE:
    agent: judge-v1  # Reference to agent manifest
    isolation: inherit  # Uses node's default runtime (Firecracker or Docker)
```

```rust
// domain/workflow.rs
pub enum StateKind {
    Agent { agent_id: AgentId, isolation: IsolationMode },
    System { command: String },
    Human { prompt: String },
}

pub enum IsolationMode {
    Inherit,    // Use node config default
    Firecracker,
    Docker,
    Process,    // No isolation (dangerous)
}
```

### Decision 3: Cortex Migration Strategy

**Status:** ✅ RESOLVED (N/A)

**Question:** How to migrate existing Cortex data?

**Decision:** **Not applicable.** Cortex is not yet implemented.

**Action:** Build weighted Cortex from scratch with schema:

```python
class CortexPattern(LanceModel):
    vector: Vector(1536)
    solution_code: str
    success_score: float      # 0.0-1.0
    execution_count: int
    weight: float            # Deduplication counter
    last_verified: datetime
    error_signature: str
    task_category: str
    skill_id: Optional[str]
```

### Decision 4: Workflow Versioning

**Status:** ✅ RESOLVED

**Question:** How to version workflow manifests?

**Decision:** **`apiVersion: 100monkeys.ai/v1`** (Kubernetes style)

**Rationale:**

- Aligns with industry standards (k8s, OpenAPI)
- Clear namespace ownership (100monkeys.ai domain)
- Separates API version from manifest version
- Enables breaking changes via version bumps (v1 → v2)

**Implementation:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: my-workflow
  version: "1.2.3"  # Manifest version (semantic versioning)
spec:
  # ... workflow definition
```

```rust
// domain/workflow.rs
pub struct Workflow {
    pub api_version: String,  // Must be "100monkeys.ai/v1"
    pub kind: String,         // Must be "Workflow"
    pub metadata: WorkflowMetadata,
    pub spec: WorkflowSpec,
}

pub struct WorkflowMetadata {
    pub name: String,
    pub version: String,  // Semantic version of this manifest
    pub labels: HashMap<String, String>,
}
```

### Decision 5: Phase Prioritization

**Status:** ✅ RESOLVED

**Question:** Which phases first?

**Decision:** **Sequential execution toward end state.** Phases 1-2 (WorkflowEngine + AgenticJudge) are prerequisites.

**Rationale:**

- WorkflowEngine is foundation for all multi-agent coordination
- AgenticJudge achieves fractal compliance early
- Phases 3-4 (gradient validation, weighted Cortex) depend on workflow infrastructure
- Phase 5 (The Forge) requires all prior components

**Order:**

1. Phase 1: WorkflowEngine (foundation)
2. Phase 2: AgenticJudge (fractal compliance)
3. Phase 3: Gradient Validation (trust building)
4. Phase 4: Weighted Cortex (learning)
5. Phase 5: The Forge (showcase)
6. Phase 6: Stimulus-Response (AGI loop)

---

## Domain Model

### Ubiquitous Language Extensions

Per [AGENTS.md](../../../aegis-architecture/AGENTS.md), we extend the domain vocabulary:

| Term | Definition | Context |
| ------ | ------------ | --------- |
| **Workflow** | Declarative FSM defining agent coordination | Execution Context |
| **WorkflowState** | Single step in workflow (Agent, System, or Human) | Execution Context |
| **StateKind** | Type of state (Agent execution, System command, Human input) | Execution Context |
| **TransitionRule** | Conditional edge between states | Execution Context |
| **Blackboard** | Shared mutable context within workflow execution | Execution Context |
| **GradientValidator** | Validator producing 0.0-1.0 confidence score | Security Policy Context |
| **SuccessScore** | Weighted success rate of pattern (0.0-1.0) | Cortex Context |
| **Weight** | Deduplication counter (increments on semantic match) | Cortex Context |
| **Skill** | Higher-level capability composed of patterns | Cortex Context |
| **AgentRole** | Specialized capability (Worker, Judge, Critic, etc.) | Agent Lifecycle Context |
| **RouterAgent** | Agent that classifies stimuli and selects workflows | Swarm Coordination Context |

### New Bounded Context: Workflow Orchestration Context

**Purpose:** Manage workflow definitions and execution  
**Owner:** Orchestrator Core  
**Language:** Rust  
**Location:** `aegis-orchestrator/orchestrator/core/src/domain/workflow.rs`

**Responsibilities:**

- Parse workflow manifests from YAML
- Execute FSM state transitions
- Maintain blackboard state
- Coordinate agent executions
- Handle conditional branching

**Key Entities:**

- `Workflow`: The declarative FSM definition (Aggregate Root)
- `WorkflowState`: Single state in FSM
- `WorkflowExecution`: Runtime instance of workflow
- `Blackboard`: Mutable shared context

**Key Value Objects:**

- `WorkflowId`: UUID identifier
- `StateKind`: Enum (Agent | System | Human)
- `TransitionRule`: Conditional with target state
- `TransitionCondition`: Boolean expression

**Key Domain Services:**

- `WorkflowParser`: YAML → Domain objects
- `WorkflowEngine`: FSM execution engine

**Exposed API:**

```rust
trait WorkflowService {
    fn parse_workflow(&self, yaml: &str) -> Result<Workflow>;
    fn start_workflow(&self, workflow_id: WorkflowId, input: Value) -> Result<WorkflowExecutionId>;
    fn tick(&mut self, exec_id: WorkflowExecutionId) -> Result<WorkflowStatus>;
    fn get_blackboard(&self, exec_id: WorkflowExecutionId) -> Result<Blackboard>;
}
```

### Extended Aggregates

#### ExecutionAggregate (Extended)

```rust
// Existing: domain/execution.rs
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    pub status: ExecutionStatus,
    pub iterations: Vec<Iteration>,
    pub max_iterations: u8,
    
    // NEW: Workflow context
    pub workflow_id: Option<WorkflowId>,
    pub blackboard: Option<Blackboard>,
}
```

#### IterationAggregate (Extended)

```rust
// Existing: domain/execution.rs
pub struct Iteration {
    pub number: u8,
    pub status: IterationStatus,
    pub action: String,
    pub output: Option<String>,
    pub error: Option<IterationError>,
    
    // NEW: Gradient validation
    pub validation_score: Option<f64>,  // 0.0-1.0
    pub confidence: Option<f64>,        // Multi-judge consensus
}
```

#### CortexPattern (New)

```rust
// NEW: cortex/src/domain/pattern.rs
pub struct CortexPattern {
    pub id: PatternId,
    pub vector: Vec<f32>,  // 1536-dim embedding
    pub solution_code: String,
    pub error_signature: ErrorSignature,
    
    // Gradient metrics
    pub success_score: f64,      // 0.0-1.0
    pub execution_count: u64,
    pub weight: f64,             // Deduplication counter
    pub last_verified: DateTime<Utc>,
    
    // Hierarchy
    pub skill_id: Option<SkillId>,
    pub tags: Vec<String>,
}
```

### Domain Events (New)

```rust
// NEW: domain/events.rs extensions
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    WorkflowStarted {
        workflow_id: WorkflowId,
        execution_id: WorkflowExecutionId,
        started_at: DateTime<Utc>,
    },
    StateEntered {
        execution_id: WorkflowExecutionId,
        state_name: String,
        agent_id: Option<AgentId>,
        entered_at: DateTime<Utc>,
    },
    StateCompleted {
        execution_id: WorkflowExecutionId,
        state_name: String,
        output: Value,
        completed_at: DateTime<Utc>,
    },
    TransitionTriggered {
        execution_id: WorkflowExecutionId,
        from_state: String,
        to_state: String,
        condition: String,
        triggered_at: DateTime<Utc>,
    },
    WorkflowCompleted {
        execution_id: WorkflowExecutionId,
        final_output: Value,
        completed_at: DateTime<Utc>,
    },
    WorkflowFailed {
        execution_id: WorkflowExecutionId,
        reason: String,
        failed_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone)]
pub enum ValidationEvent {
    GradientValidationPerformed {
        execution_id: ExecutionId,
        iteration_number: u8,
        score: f64,
        confidence: f64,
        validated_at: DateTime<Utc>,
    },
    MultiJudgeConsensus {
        execution_id: ExecutionId,
        judge_scores: Vec<(AgentId, f64)>,
        final_score: f64,
        confidence: f64,
        reached_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone)]
pub enum LearningEvent {
    PatternWeightIncreased {
        pattern_id: PatternId,
        old_weight: f64,
        new_weight: f64,
        increased_at: DateTime<Utc>,
    },
    SkillEvolved {
        skill_id: SkillId,
        new_success_rate: f64,
        pattern_count: usize,
        evolved_at: DateTime<Utc>,
    },
}
```

---

## Implementation Phases

### Phase 0: Documentation & ADRs

**Duration:** 1 week  
**Status:** IN PROGRESS

**Goals:**

1. ✅ Create this implementation plan
2. ✅ Create ADRs for all architectural decisions
3. ✅ Update AGENTS.md with new bounded contexts
4. ✅ Create workflow manifest schema documentation

**Deliverables:**

- ✅ `aegis-orchestrator/docs/PIVOT/IMPLEMENTATION_PLAN.md` (this document)
- ✅ `aegis-architecture/adrs/015-workflow-engine-architecture.md`
- ✅ `aegis-architecture/adrs/016-agent-as-judge-pattern.md`
- ✅ `aegis-architecture/adrs/017-gradient-validation-system.md`
- ✅ `aegis-architecture/adrs/018-weighted-cortex-memory.md`
- ✅ `aegis-architecture/adrs/019-blackboard-context-system.md`
- ✅ `aegis-architecture/adrs/020-the-forge-reference-pattern.md`
- ✅ `aegis-architecture/adrs/021-stimulus-response-routing.md`
- ✅ `aegis-architecture/adrs/023-evolutionary-skill-crystallization.md` (NEW)
- ✅ `aegis-architecture/adrs/024-holographic-cortex-memory-architecture.md` (NEW)
- ✅ `aegis-architecture/specs/WORKFLOW_MANIFEST_SPEC_V1.md`
- ✅ Updated `aegis-architecture/AGENTS.md` with Workflow Orchestration Context

**Success Criteria:**

- All ADRs reviewed and approved
- Workflow manifest spec is complete and parseable
- Any AI agent can resume work from these docs alone

---

### Phase 1: WorkflowEngine Foundation ✅ COMPLETE

**Duration:** 2-3 weeks (Completed: February 10, 2026)  
**Dependencies:** Phase 0  
**Status:** ✅ COMPLETE

**Goals:**

1. ✅ Replace `ExecutionSupervisor` with `WorkflowEngine`
2. ✅ Implement FSM state machine with tick() method
3. ✅ Support Agent, System, Human, and ParallelAgents state kinds
4. ✅ Parse workflow manifests from YAML
5. ✅ Implement blackboard context system

**Tasks:**

#### 1.1: Domain Model

**Files:**

- `orchestrator/core/src/domain/workflow.rs` (NEW)
- `orchestrator/core/src/domain/mod.rs` (UPDATE)

**Structs:**

```rust
pub struct Workflow {
    pub api_version: String,
    pub kind: String,
    pub metadata: WorkflowMetadata,
    pub spec: WorkflowSpec,
}

pub struct WorkflowSpec {
    pub initial_state: String,
    pub states: HashMap<String, WorkflowState>,
    pub context: WorkflowContext,
}

pub struct WorkflowState {
    pub kind: StateKind,
    pub transitions: Vec<TransitionRule>,
}

pub enum StateKind {
    Agent {
        agent_id: AgentId,
        input_template: String,
        isolation: IsolationMode,
    },
    System {
        command: String,
        env: HashMap<String, String>,
    },
    Human {
        prompt: String,
        timeout: Option<Duration>,
    },
}

pub struct TransitionRule {
    pub condition: TransitionCondition,
    pub target: String,
    pub feedback: Option<String>,
}

pub enum TransitionCondition {
    Always,
    OnSuccess,
    OnFailure,
    OnExitCode(i32),
    OnScoreAbove(f64),
    Custom(String),  // Boolean expression
}

pub struct Blackboard {
    data: HashMap<String, Value>,
}

impl Blackboard {
    pub fn set(&mut self, key: String, value: Value);
    pub fn get(&self, key: &str) -> Option<&Value>;
    pub fn hydrate(&self, template: &str) -> Result<String>;
}
```

**Tests:**

- `tests/domain/workflow_tests.rs`
- Unit tests for all structs
- Invariant testing (initial_state must exist in states)

#### 1.2: YAML Parser

**Files:**

- `orchestrator/core/src/infrastructure/workflow_parser.rs` (NEW)

**Dependencies:**

- `serde_yaml = "0.9"`

**Implementation:**

```rust
pub struct WorkflowParser;

impl WorkflowParser {
    pub fn parse(yaml: &str) -> Result<Workflow> {
        let raw: serde_yaml::Value = serde_yaml::from_str(yaml)?;
        
        // Validate apiVersion
        let api_version = raw["apiVersion"].as_str()
            .ok_or(Error::MissingField("apiVersion"))?;
        if api_version != "100monkeys.ai/v1" {
            return Err(Error::UnsupportedApiVersion(api_version.to_string()));
        }
        
        // Validate kind
        let kind = raw["kind"].as_str()
            .ok_or(Error::MissingField("kind"))?;
        if kind != "Workflow" {
            return Err(Error::InvalidKind(kind.to_string()));
        }
        
        // Parse metadata
        let metadata = WorkflowMetadata::deserialize(&raw["metadata"])?;
        
        // Parse spec
        let spec = WorkflowSpec::deserialize(&raw["spec"])?;
        
        Ok(Workflow { api_version: api_version.to_string(), kind: kind.to_string(), metadata, spec })
    }
}
```

**Tests:**

- Parse valid workflow YAML
- Reject invalid apiVersion
- Reject missing initial_state
- Reject dangling state references

#### 1.3: WorkflowEngine Application Service

**Files:**

- `orchestrator/core/src/application/workflow_engine.rs` (NEW)

**Implementation:**

```rust
pub struct WorkflowEngine {
    workflow_repo: Arc<dyn WorkflowRepository>,
    execution_service: Arc<ExecutionService>,
    event_bus: Arc<EventBus>,
}

impl WorkflowEngine {
    pub async fn start_workflow(
        &self,
        workflow_id: WorkflowId,
        input: Value,
    ) -> Result<WorkflowExecutionId> {
        let workflow = self.workflow_repo.find_by_id(workflow_id).await?;
        
        let mut execution = WorkflowExecution::new(workflow_id, input);
        execution.enter_state(&workflow.spec.initial_state)?;
        
        self.event_bus.publish(WorkflowEvent::WorkflowStarted {
            workflow_id,
            execution_id: execution.id,
            started_at: Utc::now(),
        })?;
        
        Ok(execution.id)
    }
    
    pub async fn tick(
        &mut self,
        exec_id: WorkflowExecutionId,
    ) -> Result<WorkflowStatus> {
        let mut execution = self.workflow_repo.get_execution(exec_id).await?;
        let workflow = self.workflow_repo.find_by_id(execution.workflow_id).await?;
        
        let current_state_name = execution.current_state.clone();
        let current_state = workflow.spec.states.get(&current_state_name)
            .ok_or(Error::StateNotFound(current_state_name.clone()))?;
        
        // Execute state based on kind
        let state_result = match &current_state.kind {
            StateKind::Agent { agent_id, input_template, isolation } => {
                let input = execution.blackboard.hydrate(input_template)?;
                self.execute_agent(*agent_id, input, *isolation).await?
            }
            StateKind::System { command, env } => {
                self.execute_system(command, env).await?
            }
            StateKind::Human { prompt, timeout } => {
                self.wait_for_human(prompt, *timeout).await?
            }
        };
        
        // Store result in blackboard
        execution.blackboard.set(
            format!("{}.output", current_state_name),
            state_result.output.clone()
        );
        
        // Evaluate transitions
        let next_state = self.evaluate_transitions(
            &current_state.transitions,
            &state_result,
            &execution.blackboard
        )?;
        
        match next_state {
            Some(target) => {
                execution.enter_state(&target)?;
                self.workflow_repo.save_execution(execution).await?;
                Ok(WorkflowStatus::Running)
            }
            None => {
                execution.complete(state_result.output)?;
                self.workflow_repo.save_execution(execution).await?;
                
                self.event_bus.publish(WorkflowEvent::WorkflowCompleted {
                    execution_id: exec_id,
                    final_output: state_result.output,
                    completed_at: Utc::now(),
                })?;
                
                Ok(WorkflowStatus::Completed)
            }
        }
    }
    
    async fn execute_agent(
        &self,
        agent_id: AgentId,
        input: String,
        isolation: IsolationMode,
    ) -> Result<StateResult> {
        // Delegate to existing ExecutionService
        let exec_id = self.execution_service.start_execution(agent_id, input).await?;
        let execution = self.execution_service.wait_for_completion(exec_id).await?;
        
        Ok(StateResult {
            success: execution.status == ExecutionStatus::Completed,
            output: execution.final_output()?,
            exit_code: None,
        })
    }
    
    async fn execute_system(
        &self,
        command: &str,
        env: &HashMap<String, String>,
    ) -> Result<StateResult> {
        // Execute shell command in isolated runtime
        let runtime = self.runtime_factory.create(IsolationMode::Inherit)?;
        let result = runtime.execute_command(command, env).await?;
        
        Ok(StateResult {
            success: result.exit_code == 0,
            output: serde_json::json!({
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit_code,
            }),
            exit_code: Some(result.exit_code),
        })
    }
    
    fn evaluate_transitions(
        &self,
        transitions: &[TransitionRule],
        result: &StateResult,
        blackboard: &Blackboard,
    ) -> Result<Option<String>> {
        for transition in transitions {
            if self.matches_condition(&transition.condition, result, blackboard)? {
                return Ok(Some(transition.target.clone()));
            }
        }
        Ok(None)
    }
    
    fn matches_condition(
        &self,
        condition: &TransitionCondition,
        result: &StateResult,
        blackboard: &Blackboard,
    ) -> Result<bool> {
        match condition {
            TransitionCondition::Always => Ok(true),
            TransitionCondition::OnSuccess => Ok(result.success),
            TransitionCondition::OnFailure => Ok(!result.success),
            TransitionCondition::OnExitCode(code) => {
                Ok(result.exit_code == Some(*code))
            }
            TransitionCondition::OnScoreAbove(threshold) => {
                let score = result.validation_score.ok_or(Error::NoValidationScore)?;
                Ok(score > *threshold)
            }
            TransitionCondition::Custom(expr) => {
                self.eval_expression(expr, blackboard)
            }
        }
    }
}
```

**Tests:**

- Start workflow with valid ID
- Tick through linear workflow
- Handle state transitions correctly
- Error on missing state
- Timeout on human input

#### 1.4: Default 100monkeys Workflow

**Files:**

- `demo-agents/workflows/100monkeys-classic.yaml` (NEW)

**Content:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: 100monkeys-classic
  version: "1.0.0"
  description: "Classic single-agent iterative refinement loop"
  labels:
    category: "iterative"
    pattern: "generate-execute-validate-refine"

spec:
  # Global context
  context:
    max_iterations: 10
    validation_threshold: 0.95

  # Initial state
  initial_state: GENERATE

  # State machine
  states:
    GENERATE:
      kind: Agent
      agent: "{{workflow.agent_id}}"  # Agent ID passed at runtime
      input: |
        {{#if iteration_number}}
        Your previous attempt (iteration {{iteration_number}}) failed validation.
        
        --- PREVIOUS CODE ---
        {{previous_code}}
        
        --- VALIDATION ERRORS ---
        {{validation_errors}}
        
        --- INSTRUCTION ---
        Analyze the errors and rewrite the code to fix the issues.
        Do not apologize. Output only the corrected code.
        {{else}}
        {{workflow.task}}
        {{/if}}
      
      transitions:
        - condition: always
          target: EXECUTE

    EXECUTE:
      kind: System
      command: "{{workflow.command}}"
      env:
        INPUT: "{{GENERATE.output}}"
      
      transitions:
        - condition: exit_code_zero
          target: VALIDATE
        - condition: exit_code_non_zero
          target: REFINE
          feedback: "Execution failed with exit code {{exit_code}}: {{stderr}}"

    VALIDATE:
      kind: Agent
      agent: "{{workflow.judge_id}}"  # Judge agent ID
      input: |
        Evaluate whether this output meets the requirements.
        
        Task: {{workflow.task}}
        Output: {{EXECUTE.output.stdout}}
        Exit Code: {{EXECUTE.output.exit_code}}
        
        Respond with JSON:
        {
          "valid": true/false,
          "score": 0.0-1.0,
          "reasoning": "explanation"
        }
      
      transitions:
        - condition: score_above_0.95
          target: COMPLETE
        - condition: score_below_0.95
          target: REFINE
          feedback: "{{VALIDATE.output.reasoning}}"

    REFINE:
      kind: System
      command: "update_context"
      env:
        ITERATION_NUMBER: "{{blackboard.iteration_number + 1}}"
        PREVIOUS_CODE: "{{GENERATE.output}}"
        VALIDATION_ERRORS: "{{state.feedback}}"
      
      transitions:
        - condition: iteration_below_max
          target: GENERATE
        - condition: iteration_at_max
          target: FAILED

    COMPLETE:
      kind: System
      command: "finalize"
      env:
        RESULT: "{{EXECUTE.output.stdout}}"
      
      transitions: []  # Terminal state

    FAILED:
      kind: System
      command: "log_failure"
      env:
        REASON: "Max iterations reached"
      
      transitions: []  # Terminal state
```

#### 1.5: CLI Integration

**Files:**

- `cli/src/commands/run.rs` (UPDATE)

**Changes:**

```rust
// Add --workflow flag
#[derive(Parser)]
pub struct RunCommand {
    #[arg(short, long)]
    agent: Option<String>,
    
    #[arg(short, long)]
    workflow: Option<PathBuf>,  // NEW
    
    #[arg(short, long)]
    input: String,
}

impl RunCommand {
    pub async fn execute(&self, config: &Config) -> Result<()> {
        if let Some(workflow_path) = &self.workflow {
            // NEW: Workflow-based execution
            let yaml = fs::read_to_string(workflow_path)?;
            let workflow = WorkflowParser::parse(&yaml)?;
            
            let workflow_id = workflow_service.deploy_workflow(workflow).await?;
            let exec_id = workflow_service.start_workflow(workflow_id, self.input.clone()).await?;
            
            // Tick until completion
            loop {
                let status = workflow_service.tick(exec_id).await?;
                match status {
                    WorkflowStatus::Running => tokio::time::sleep(Duration::from_millis(100)).await,
                    WorkflowStatus::Completed => break,
                    WorkflowStatus::Failed(reason) => return Err(Error::WorkflowFailed(reason)),
                }
            }
        } else if let Some(agent_name) = &self.agent {
            // Legacy: Single-agent execution
            // ... existing code
        } else {
            return Err(Error::MissingRequired("Either --agent or --workflow required"));
        }
        
        Ok(())
    }
}
```

**Tests:**

- `aegis run --workflow 100monkeys-classic.yaml --input "Write a hello world script"`
- `aegis run --agent my-agent --input "Hello"` (legacy mode still works)

**Deliverables:**

- ✅ `domain/workflow.rs` with complete domain model (777 lines)
- ✅ `infrastructure/workflow_parser.rs` with YAML parsing (complete)
- ✅ `application/workflow_engine.rs` with FSM tick() logic (639 lines)
- ✅ `demo-agents/workflows/100monkeys-classic.yaml` default workflow (230 lines, 6 states)
- ✅ CLI `aegis workflow run` command (implemented in `commands/workflow.rs`)
- ✅ Integration tests for WorkflowEngine (7 tests, all passing)

**Success Criteria:**

- ✅ Can execute simple linear workflow
- ✅ Can execute branching workflow (conditional transitions)
- ✅ Blackboard correctly passes state between states
- ✅ `100monkeys-classic.yaml` workflow with 6 states functional
- ✅ CLI accepts `aegis workflow run` command
- ✅ All existing tests pass (workflow tests: 7/7, recursive tests: 5/5)

**Achievement Summary:**

- ✅ Created complete workflow domain model with 4 state kinds (Agent, System, Human, ParallelAgents)
- ✅ Implemented 14 transition condition types including gradient scoring (ScoreAbove, ScoreBetween, ConfidenceAbove)
- ✅ Built YAML parser with validation and round-trip serialization
- ✅ Implemented WorkflowEngine with FSM tick() loop and state execution
- ✅ Created Blackboard context system with Handlebars template hydration
- ✅ Built 100monkeys-classic.yaml reference workflow (230 lines, 6 states: GENERATE → EXECUTE → VALIDATE → REFINE → COMPLETE/FAILED)
- ✅ Added ExecutionHierarchy tracking for recursive execution (MAX_RECURSIVE_DEPTH=3)
- ✅ All integration tests passing (7/7 workflow tests, 5/5 recursive execution tests)
- ✅ Removed all hardcoded judge logic from Rust - judges are now agents
- ✅ CLI builds successfully

**Key Outcomes:**

- Workflows are first-class citizens with full FSM support
- Agent composition through declarative YAML manifests
- State transitions support gradient validation thresholds
- Blackboard enables state sharing across workflow steps
- Recursive agent execution supported (agents can call agents)
- System is now truly workflow-centric, not role-centric

**Files Created/Modified:**

- `orchestrator/core/src/domain/workflow.rs` (777 lines)
- `orchestrator/core/src/infrastructure/workflow_parser.rs` (complete)
- `orchestrator/core/src/application/workflow_engine.rs` (639 lines)
- `demo-agents/workflows/100monkeys-classic.yaml` (230 lines)
- `orchestrator/core/tests/workflow_integration_tests.rs` (287 lines, 7 tests)
- `orchestrator/core/tests/recursive_execution_tests.rs` (updated, 5 tests)
- Removed: `orchestrator/core/src/domain/judge.rs` (deleted - judges are agents now)

**Next Phase:** Phase 3 - Gradient Validation System (multi-judge consensus, confidence scoring)

---

### Phase 2: Agent-as-Judge Pattern ✅ COMPLETE

**Duration:** 2 days (February 3-4, 2026)  
**Dependencies:** Phase 1  
**Status:** ✅ COMPLETE

**Achievement Summary:**

- ✅ Created `basic-judge.yaml` with gradient scoring (0.0-1.0)
- ✅ Implemented `ExecutionHierarchy` domain model for recursive depth tracking
- ✅ Extended `Execution` entity with hierarchy field and `new_child()` method
- ✅ Integrated execution contexts into `WorkflowEngine` with helper methods
- ✅ Created comprehensive integration tests (6 tests covering depths 0-3)
- ✅ Updated `100monkeys-classic.yaml` to use `basic-judge` agent

**Key Outcomes:**

- Judges are now first-class agents (not Rust code)
- Gradient validation replaces binary pass/fail
- Recursive execution supported (judge-calling-judge) with MAX_RECURSIVE_DEPTH=3
- Execution trees tracked with parent-child relationships
- All tests compile successfully

**Documentation:**

- See [PHASE_2_COMPLETE.md](PHASE_2_COMPLETE.md) for full details
- See `demo-agents/judges/README.md` for usage examples

**Next Phase:** Phase 3 - Gradient Validation System

---

### Phase 2 Original Plan (Archived)

**Duration:** 1-2 weeks  
**Dependencies:** Phase 1  
**Status:** ✅ COMPLETE (SUPERSEDED BY ACTUAL IMPLEMENTATION)

**Goals:**

1. Create judge agent manifest templates
2. Implement `AgenticJudge` wrapper
3. Support nested agent executions (agent spawning agent)
4. Remove hardcoded `LlmJudge` from supervisor
5. Test Meta-Monkey pattern

**Tasks:**

#### 2.1: Judge Agent Manifest

**Files:**

- `demo-agents/judges/basic-judge.yaml` (NEW)

**Content:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: AgentManifest
metadata:
  name: basic-judge-v1
  version: "1.0.0"
  description: "Binary validator for execution success"
  labels:
    role: "judge"
    capability: "validation"

spec:
  runtime:
    language: python
    version: "3.11"
    isolation: docker  # Or firecracker per node config
    
  entrypoint: judge.py
  
  model:
    provider: default
    alias: "gpt-3.5"
    temperature: 0.1
  
  validation:
    type: schema
    schema: |
      {
        "type": "object",
        "required": ["valid", "score", "reasoning"],
        "properties": {
          "valid": {"type": "boolean"},
          "score": {"type": "number", "minimum": 0.0, "maximum": 1.0},
          "reasoning": {"type": "string"}
        }
      }
  
  security:
    network:
      mode: deny
      allowlist: []
    filesystem:
      read_only: true
    resources:
      cpu: 1.0
      memory: "512Mi"
      timeout: 30s

  # Judge-specific metadata
  judge_config:
    validation_type: "semantic"
    strictness: "moderate"
```

**Agent Code (judge.py):**

```python
import sys
import json
import os

def main():
    # Read input from stdin
    input_data = json.loads(sys.stdin.read())
    
    task = input_data["task"]
    output = input_data["output"]
    exit_code = input_data["exit_code"]
    
    # Simple heuristic + LLM validation
    if exit_code != 0:
        return {
            "valid": False,
            "score": 0.0,
            "reasoning": f"Execution failed with exit code {exit_code}"
        }
    
    # Use LLM to evaluate semantic match
    from openai import OpenAI
    client = OpenAI()
    
    response = client.chat.completions.create(
        model=os.getenv("MODEL_NAME"),
        messages=[{
            "role": "user",
            "content": f"""Evaluate if this output satisfies the task.
            
Task: {task}
Output: {output}

Respond with JSON only:
{{
  "valid": true/false,
  "score": 0.0-1.0 (confidence),
  "reasoning": "brief explanation"
}}"""
        }],
        temperature=0.1
    )
    
    result = json.loads(response.choices[0].message.content)
    print(json.dumps(result))

if __name__ == "__main__":
    main()
```

#### 2.2: AgenticJudge Implementation

**Files:**

- `orchestrator/core/src/domain/judge.rs` (UPDATE)

**Changes:**

```rust
// Existing trait
pub trait Judge: Send + Sync {
    async fn evaluate(
        &self,
        task: &str,
        output: &str,
        exit_code: i32,
        stderr: &str,
    ) -> Result<ValidationResult>;
}

// NEW: AgenticJudge implementation
pub struct AgenticJudge {
    judge_agent_id: AgentId,
    execution_service: Arc<ExecutionService>,
}

impl AgenticJudge {
    pub fn new(judge_agent_id: AgentId, execution_service: Arc<ExecutionService>) -> Self {
        Self { judge_agent_id, execution_service }
    }
}

#[async_trait]
impl Judge for AgenticJudge {
    async fn evaluate(
        &self,
        task: &str,
        output: &str,
        exit_code: i32,
        stderr: &str,
    ) -> Result<ValidationResult> {
        // Construct input JSON
        let input = serde_json::json!({
            "task": task,
            "output": output,
            "exit_code": exit_code,
            "stderr": stderr,
        });
        
        // Execute judge agent
        let exec_id = self.execution_service.start_execution(
            self.judge_agent_id,
            input.to_string()
        ).await?;
        
        // Wait for completion
        let execution = self.execution_service.wait_for_completion(exec_id).await?;
        
        // Parse judge output
        let judge_output: JudgeOutput = serde_json::from_str(&execution.final_output()?)?;
        
        Ok(ValidationResult {
            overall_valid: judge_output.valid,
            score: Some(judge_output.score),
            reasons: vec![judge_output.reasoning],
        })
    }
}

#[derive(Deserialize)]
struct JudgeOutput {
    valid: bool,
    score: f64,
    reasoning: String,
}
```

#### 2.3: Nested Execution Support

**Files:**

- `orchestrator/core/src/application/execution.rs` (UPDATE)

**Changes:**

```rust
// Add parent_execution_id field
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    pub parent_execution_id: Option<ExecutionId>,  // NEW
    // ... existing fields
}

impl ExecutionService {
    pub async fn start_execution(
        &self,
        agent_id: AgentId,
        input: String,
    ) -> Result<ExecutionId> {
        self.start_execution_internal(agent_id, input, None).await
    }
    
    async fn start_execution_internal(
        &self,
        agent_id: AgentId,
        input: String,
        parent_id: Option<ExecutionId>,  // NEW
    ) -> Result<ExecutionId> {
        let mut execution = Execution::new(agent_id, input)?;
        execution.parent_execution_id = parent_id;
        
        // ... rest of logic
    }
}
```

**Security Note:**

- Nested executions inherit parent's security policy (or stricter)
- Prevent infinite recursion with depth limit (default: 3)
- Track execution tree for auditing

#### 2.4: Update Workflow to Use AgenticJudge

**Files:**

- `demo-agents/workflows/100monkeys-classic.yaml` (UPDATE)

**Changes:**

```yaml
states:
  VALIDATE:
    kind: Agent
    agent: basic-judge-v1  # Reference to judge agent
    input: |
      {
        "task": "{{workflow.task}}",
        "output": "{{EXECUTE.output.stdout}}",
        "exit_code": {{EXECUTE.output.exit_code}},
        "stderr": "{{EXECUTE.output.stderr}}"
      }
    
    transitions:
      - condition: score_above
        threshold: 0.95
        target: COMPLETE
      - condition: score_below
        threshold: 0.95
        target: REFINE
```

#### 2.5: Meta-Monkey Test

**Files:**

- `tests/integration/meta_monkey_test.rs` (NEW)

**Test Scenario:**

```markdown
Judge Agent 1: Evaluates worker output
Judge Agent 2: Evaluates Judge 1's evaluation quality
Judge Agent 3: Evaluates Judge 2's meta-evaluation

"Judges judge judges"
```

**Deliverables:**

- ✅ AgenticJudge executes judge agent successfully
- ✅ Nested executions tracked correctly
- ✅ Meta-monkey test passes (3-level recursion)
- ✅ Judge agents cannot escape isolation
- ✅ Fractal compliance achieved (no hardcoded validation logic)

---

### Phase 3: Gradient Validation System

**Duration:** 1-2 weeks  
**Dependencies:** Phase 2  
**Status:** NOT STARTED

**Goals:**

1. Extend validators to produce 0.0-1.0 scores
2. Implement multi-judge consensus mechanism
3. Store validation scores in execution history
4. Calculate confidence from judge agreement
5. Enable gradient-based transitions in workflows

**Tasks:**

#### 3.1: GradientValidator Trait

**Files:**

- `orchestrator/core/src/domain/validator.rs` (UPDATE)

**Changes:**

```rust
// Existing binary validator
pub trait Validator: Send + Sync {
    async fn validate(&self, output: &str) -> Result<ValidationResult>;
}

// NEW: Gradient validator
#[async_trait]
pub trait GradientValidator: Send + Sync {
    async fn validate_with_score(&self, output: &str) -> Result<GradientValidationResult>;
}

pub struct GradientValidationResult {
    pub score: f64,        // 0.0 (total failure) to 1.0 (perfect)
    pub confidence: f64,   // 0.0 (uncertain) to 1.0 (very certain)
    pub reasoning: String,
    pub binary_valid: bool,  // For backward compat: score > threshold
}

// Implementations
pub struct SystemGradientValidator {
    exit_code_weight: f64,
    stderr_weight: f64,
}

impl GradientValidator for SystemGradientValidator {
    async fn validate_with_score(&self, output: &str) -> Result<GradientValidationResult> {
        let result: ExecutionResult = serde_json::from_str(output)?;
        
        let exit_code_score = if result.exit_code == 0 { 1.0 } else { 0.0 };
        let stderr_score = if result.stderr.is_empty() { 1.0 } else { 0.5 };
        
        let score = (exit_code_score * self.exit_code_weight) +
                    (stderr_score * self.stderr_weight);
        
        Ok(GradientValidationResult {
            score,
            confidence: 1.0,  // Deterministic
            reasoning: format!("Exit code: {}, stderr: {}", result.exit_code, result.stderr.is_empty()),
            binary_valid: score > 0.95,
        })
    }
}

pub struct SemanticGradientValidator {
    judge_agent_id: AgentId,
    execution_service: Arc<ExecutionService>,
}

impl GradientValidator for SemanticGradientValidator {
    async fn validate_with_score(&self, output: &str) -> Result<GradientValidationResult> {
        // Use AgenticJudge internally
        let judge = AgenticJudge::new(self.judge_agent_id, self.execution_service.clone());
        let result = judge.evaluate(task, output, 0, "").await?;
        
        Ok(GradientValidationResult {
            score: result.score.unwrap_or(0.5),
            confidence: result.confidence.unwrap_or(0.8),
            reasoning: result.reasons.join("; "),
            binary_valid: result.overall_valid,
        })
    }
}
```

#### 3.2: Multi-Judge Consensus

**Files:**

- `orchestrator/core/src/domain/multi_judge.rs` (NEW)

**Implementation:**

```rust
pub struct MultiJudgeValidator {
    judges: Vec<(AgentId, f64)>,  // (judge_id, weight)
    consensus_strategy: ConsensusStrategy,
    execution_service: Arc<ExecutionService>,
}

pub enum ConsensusStrategy {
    WeightedAverage,
    Majority,
    Unanimous,
    BestOfN { n: usize },
}

impl MultiJudgeValidator {
    pub async fn evaluate_with_consensus(
        &self,
        task: &str,
        output: &str,
        exit_code: i32,
        stderr: &str,
    ) -> Result<ConsensusResult> {
        // Execute all judges in parallel
        let mut handles = vec![];
        for (judge_id, weight) in &self.judges {
            let judge = AgenticJudge::new(*judge_id, self.execution_service.clone());
            let handle = tokio::spawn(async move {
                judge.evaluate(task, output, exit_code, stderr).await
            });
            handles.push((handle, *weight));
        }
        
        // Collect results
        let mut scores = vec![];
        for (handle, weight) in handles {
            let result = handle.await??;
            scores.push((result.score.unwrap_or(0.0), weight));
        }
        
        // Apply consensus strategy
        let (final_score, confidence) = match self.consensus_strategy {
            ConsensusStrategy::WeightedAverage => {
                self.weighted_average(&scores)
            }
            ConsensusStrategy::Majority => {
                self.majority_vote(&scores)
            }
            ConsensusStrategy::Unanimous => {
                self.unanimous(&scores)
            }
            ConsensusStrategy::BestOfN { n } => {
                self.best_of_n(&scores, n)
            }
        };
        
        Ok(ConsensusResult {
            final_score,
            confidence,
            individual_scores: scores.into_iter().map(|(s, _)| s).collect(),
        })
    }
    
    fn weighted_average(&self, scores: &[(f64, f64)]) -> (f64, f64) {
        let total_weight: f64 = scores.iter().map(|(_, w)| w).sum();
        let weighted_sum: f64 = scores.iter().map(|(s, w)| s * w).sum();
        let final_score = weighted_sum / total_weight;
        
        // Confidence from agreement
        let variance = self.calculate_variance(scores);
        let confidence = 1.0 - variance.min(1.0);
        
        (final_score, confidence)
    }
    
    fn calculate_variance(&self, scores: &[(f64, f64)]) -> f64 {
        let mean = scores.iter().map(|(s, _)| s).sum::<f64>() / scores.len() as f64;
        let variance = scores.iter()
            .map(|(s, _)| (s - mean).powi(2))
            .sum::<f64>() / scores.len() as f64;
        variance
    }
}

pub struct ConsensusResult {
    pub final_score: f64,
    pub confidence: f64,
    pub individual_scores: Vec<f64>,
}
```

#### 3.3: Store Validation Scores

**Files:**

- `orchestrator/core/src/domain/execution.rs` (UPDATE)

**Changes:**

```rust
pub struct Iteration {
    pub number: u8,
    pub status: IterationStatus,
    pub action: String,
    pub output: Option<String>,
    pub error: Option<IterationError>,
    
    // NEW: Gradient validation
    pub validation_score: Option<f64>,
    pub validation_confidence: Option<f64>,
    pub judge_scores: Option<Vec<f64>>,  // Individual judge scores
    
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}
```

#### 3.4: Update Workflow Transitions

**Files:**

- `orchestrator/core/src/domain/workflow.rs` (UPDATE)

**Add gradient-based transition conditions:**

```rust
pub enum TransitionCondition {
    // ... existing conditions
    
    // NEW: Gradient conditions
    OnScoreAbove(f64),
    OnScoreBelow(f64),
    OnScoreBetween(f64, f64),
    OnConfidenceAbove(f64),
    OnConsensus { threshold: f64, agreement: f64 },
}
```

**Example workflow:**

```yaml
states:
  VALIDATE:
    kind: Agent
    agent: multi-judge-v1
    input: "{{EXECUTE.output}}"
    
    transitions:
      - condition: score_above
        threshold: 0.95
        confidence_above: 0.8
        target: COMPLETE
      
      - condition: score_between
        min: 0.7
        max: 0.95
        target: REFINE_MINOR
      
      - condition: score_below
        threshold: 0.7
        target: REFINE_MAJOR
```

**Deliverables:**

- [x] `GradientValidator` trait and implementations
- [x] `MultiJudgeValidator` with consensus strategies
- [x] Updated `Iteration` with validation_score, confidence
- [x] Gradient-based transition conditions
- [x] Integration tests for multi-judge consensus
- [ ] Documentation: "Gradient Validation Guide"

**Success Criteria:**

- ✅ Validators produce 0.0-1.0 scores
- ✅ Multi-judge consensus calculates confidence correctly
- ✅ Validation scores stored in execution history
- ✅ Workflows can branch based on gradient scores
- ✅ Confidence increases with judge agreement

---

### Phase 4: Weighted Cortex Memory

**Duration:** 2-3 weeks  
**Dependencies:** Phase 3  
**Status:** NOT STARTED

**Goals:**

1. Implement weighted semantic memory in Cortex
2. Add success_score and weight fields to pattern storage
3. Implement deduplication-with-weight-increment
4. Create hybrid retrieval ranking (similarity × success)
5. Add time-decay background job
6. Implement Skill aggregate hierarchy

**Tasks:**

#### 4.1: Cortex Schema Extension

**Files:**

- `cortex/src/schema.py` (UPDATE)

**Changes:**

```python
from qdrant.pydantic import LanceModel, Vector
from datetime import datetime
from typing import Optional

class CortexPattern(LanceModel):
    # Identity
    id: str
    
    # Embedding
    vector: Vector(1536)
    
    # Content
    error_signature: str
    solution_code: str
    task_category: str
    
    # NEW: Gradient metrics
    success_score: float          # 0.0-1.0
    execution_count: int          # How many times used
    weight: float                 # Deduplication counter
    last_verified: datetime       # Time-decay reference
    
    # Hierarchy
    skill_id: Optional[str] = None
    tags: list[str] = []
    
    # Metadata
    created_at: datetime
    updated_at: datetime
```

#### 4.2: Weighted Deduplication

**Files:**

- `cortex/src/storage.py` (UPDATE)

**Implementation:**

```python
class CortexStorage:
    def __init__(self, db_path: str):
        self.db = qdrant.connect(db_path)
        self.table = self.db.open_table("patterns")
    
    async def store_pattern(
        self,
        error_signature: str,
        solution_code: str,
        success_score: float,
        task_category: str,
        embedding: List[float],
    ) -> str:
        # Search for semantic duplicates (cosine similarity > 0.95)
        similar = self.table.search(embedding) \
            .metric("cosine") \
            .limit(1) \
            .to_list()
        
        if similar and similar[0]["_distance"] < 0.05:  # cosine dist < 0.05 = similarity > 0.95
            # Duplicate found: increment weight
            existing_id = similar[0]["id"]
            await self._increment_weight(existing_id, success_score)
            return existing_id
        else:
            # New pattern: insert with weight=1
            pattern = CortexPattern(
                id=str(uuid.uuid4()),
                vector=embedding,
                error_signature=error_signature,
                solution_code=solution_code,
                task_category=task_category,
                success_score=success_score,
                execution_count=1,
                weight=1.0,
                last_verified=datetime.utcnow(),
                created_at=datetime.utcnow(),
                updated_at=datetime.utcnow(),
            )
            self.table.add([pattern])
            return pattern.id
    
    async def _increment_weight(self, pattern_id: str, new_success_score: float):
        # Fetch existing pattern
        pattern = self.table.search() \
            .where(f"id = '{pattern_id}'") \
            .limit(1) \
            .to_list()[0]
        
        # Update weight (increment)
        old_weight = pattern["weight"]
        new_weight = old_weight + 1.0
        
        # Update rolling average of success_score
        old_score = pattern["success_score"]
        old_count = pattern["execution_count"]
        new_count = old_count + 1
        new_avg_score = ((old_score * old_count) + new_success_score) / new_count
        
        # Update record
        self.table.update(
            where=f"id = '{pattern_id}'",
            values={
                "weight": new_weight,
                "success_score": new_avg_score,
                "execution_count": new_count,
                "last_verified": datetime.utcnow(),
                "updated_at": datetime.utcnow(),
            }
        )
        
        # Publish learning event
        await self.event_bus.publish(LearningEvent.PatternWeightIncreased(
            pattern_id=pattern_id,
            old_weight=old_weight,
            new_weight=new_weight,
            increased_at=datetime.utcnow(),
        ))
```

#### 4.3: Hybrid Retrieval Ranking

**Files:**

- `cortex/src/retrieval.py` (NEW)

**Implementation:**

```python
class CortexRetrieval:
    def __init__(self, storage: CortexStorage):
        self.storage = storage
    
    async def search_patterns(
        self,
        query_embedding: List[float],
        limit: int = 10,
        success_weight: float = 0.5,
    ) -> List[CortexPattern]:
        # Step 1: Vector similarity search (top 100 candidates)
        candidates = self.storage.table.search(query_embedding) \
            .metric("cosine") \
            .limit(100) \
            .to_list()
        
        # Step 2: Re-rank by hybrid score
        for candidate in candidates:
            cosine_sim = 1.0 - candidate["_distance"]
            success_score = candidate["success_score"]
            weight = candidate["weight"]
            
            # Hybrid score formula
            candidate["hybrid_score"] = cosine_sim * (1.0 + success_score * weight * success_weight)
        
        # Step 3: Sort by hybrid score
        candidates.sort(key=lambda x: x["hybrid_score"], reverse=True)
        
        # Step 4: Return top N
        return candidates[:limit]
    
    async def search_by_error(
        self,
        error_signature: str,
        task_category: Optional[str] = None,
    ) -> List[CortexPattern]:
        # Exact match on error signature
        results = self.storage.table.search() \
            .where(f"error_signature = '{error_signature}'")
        
        if task_category:
            results = results.where(f"task_category = '{task_category}'")
        
        # Sort by success_score * weight
        results = results.to_list()
        results.sort(key=lambda x: x["success_score"] * x["weight"], reverse=True)
        
        return results
```

#### 4.4: Time-Decay Background Job

**Files:**

- `cortex/src/decay.py` (NEW)

**Implementation:**

```python
import asyncio
from datetime import datetime, timedelta
import math

class TimeDecayJob:
    def __init__(self, storage: CortexStorage, decay_rate: float = 0.1):
        self.storage = storage
        self.decay_rate = decay_rate  # λ in exponential decay formula
    
    async def run_forever(self, interval: timedelta = timedelta(hours=24)):
        while True:
            await self.apply_decay()
            await asyncio.sleep(interval.total_seconds())
    
    async def apply_decay(self):
        now = datetime.utcnow()
        
        # Fetch all patterns
        patterns = self.storage.table.to_pandas()
        
        for idx, pattern in patterns.iterrows():
            last_verified = pattern["last_verified"]
            delta_days = (now - last_verified).days
            
            if delta_days > 0:
                # Exponential decay: W_new = W_old * exp(-λ * Δt)
                old_weight = pattern["weight"]
                new_weight = old_weight * math.exp(-self.decay_rate * delta_days)
                
                # Don't decay below 1.0 (minimum weight)
                new_weight = max(new_weight, 1.0)
                
                if new_weight != old_weight:
                    self.storage.table.update(
                        where=f"id = '{pattern['id']}'",
                        values={"weight": new_weight}
                    )
        
        print(f"Time decay applied to {len(patterns)} patterns")
```

#### 4.5: Skill Aggregate

**Files:**

- `cortex/src/domain/skill.rs` (NEW)

**Implementation:**

```rust
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub category: String,
    pub patterns: Vec<PatternId>,  // References, not owned
    pub usage_count: u64,
    pub success_rate: f64,
    pub first_learned: DateTime<Utc>,
    pub last_used: DateTime<Utc>,
}

impl Skill {
    pub fn add_pattern(&mut self, pattern_id: PatternId) -> Result<()> {
        if !self.patterns.contains(&pattern_id) {
            self.patterns.push(pattern_id);
            Ok(())
        } else {
            Err(Error::PatternAlreadyExists)
        }
    }
    
    pub fn record_usage(&mut self, success: bool) -> Result<()> {
        self.usage_count += 1;
        
        // Rolling average
        let old_rate = self.success_rate;
        let old_count = self.usage_count - 1;
        let new_success = if success { 1.0 } else { 0.0 };
        self.success_rate = ((old_rate * old_count as f64) + new_success) / self.usage_count as f64;
        
        self.last_used = Utc::now();
        Ok(())
    }
}
```

#### 4.6: Integration with WorkflowEngine

**Files:**

- `orchestrator/core/src/application/workflow_engine.rs` (UPDATE)

**Add Cortex query before agent execution:**

```rust
async fn execute_agent(
    &self,
    agent_id: AgentId,
    input: String,
    isolation: IsolationMode,
) -> Result<StateResult> {
    // NEW: Query Cortex for relevant patterns
    let query_embedding = self.embedding_service.embed(&input).await?;
    let patterns = self.cortex.search_patterns(query_embedding, 5).await?;
    
    // Inject patterns as context
    let enriched_input = if !patterns.is_empty() {
        format!(
            "{}

--- RELEVANT PATTERNS FROM MEMORY ---
{}

Use these patterns if they help solve the task.",
            input,
            patterns.iter()
                .map(|p| format!("- {}: {}", p.error_signature, p.solution_code))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        input
    };
    
    // Execute agent with enriched context
    let exec_id = self.execution_service.start_execution(agent_id, enriched_input).await?;
    let execution = self.execution_service.wait_for_completion(exec_id).await?;
    
    // NEW: If refinement occurred, store pattern in Cortex
    if execution.iterations.len() > 1 {
        for iteration in &execution.iterations[1..] {
            if let Some(error) = &iteration.error {
                let pattern_embedding = self.embedding_service.embed(&error.message).await?;
                await self.cortex.store_pattern(
                    error_signature: error.message.clone(),
                    solution_code: iteration.output.clone().unwrap_or_default(),
                    success_score: iteration.validation_score.unwrap_or(0.5),
                    task_category: "general".to_string(),
                    embedding: pattern_embedding,
                ).await?;
            }
        }
    }
    
    Ok(StateResult {
        success: execution.status == ExecutionStatus::Completed,
        output: execution.final_output()?,
        exit_code: None,
    })
}
```

**Deliverables:**

- [x] Extended Cortex schema with weight, success_score
- [x] Weighted deduplication on insert
- [x] Hybrid retrieval ranking (similarity × success)
- [ ] Time-decay background job
- [ ] Skill aggregate + repository
- [x] Cortex integration in WorkflowEngine
- [ ] Tests: deduplication, retrieval ranking, time decay
- [ ] Documentation: "Cortex Memory Architecture"

**Success Criteria:**

- ✅ Duplicate patterns increment weight instead of creating duplicates
- ✅ Retrieval ranks by success-weighted score, not just similarity
- ✅ Time-decay reduces weight of stale patterns
- ✅ Skill aggregates group related patterns
- ✅ Agents receive relevant patterns as context before execution
- ✅ First-try success rate increases over time (learning working)

---

### Phase 5: The Forge Reference Workflow

**Duration:** 2-3 weeks  
**Dependencies:** Phases 1-4  
**Status:** NOT STARTED

**Goals:**

1. Create specialized agent manifests (RequirementsAI, ArchitectAI, TesterAI, CoderAI, ReviewerAI, CriticAI)
2. Implement Constitutional Development Lifecycle workflow
3. Support parallel agent execution (multiple critics)
4. Add human-in-the-loop approval gates
5. Demonstrate full software development cycle

**Tasks:**

#### 5.1: Specialized Agent Manifests

**Files:**

- `demo-agents/forge/requirements-ai.yaml` (NEW)
- `demo-agents/forge/architect-ai.yaml` (NEW)
- `demo-agents/forge/tester-ai.yaml` (NEW)
- `demo-agents/forge/coder-ai.yaml` (NEW)
- `demo-agents/forge/reviewer-ai.yaml` (NEW)
- `demo-agents/forge/critic-ai.yaml` (NEW)
- `demo-agents/forge/security-ai.yaml` (NEW)

##### Example: requirements-ai.yaml

```yaml
apiVersion: 100monkeys.ai/v1
kind: AgentManifest
metadata:
  name: requirements-ai-v1
  version: "1.0.0"
  description: "Analyzes user intent and generates formal requirements"
  labels:
    role: "analyst"
    capability: "requirements"

spec:
  runtime:
    language: python
    version: "3.11"
    isolation: docker
  
  entrypoint: requirements.py
  
  model:
    provider: default
    alias: "gpt-4"
    temperature: 0.2
  
  tools:
    - name: "file_reader"
      description: "Read existing codebase files"
  
  validation:
    type: schema
    schema: |
      {
        "type": "object",
        "required": ["functional_requirements", "non_functional_requirements"],
        "properties": {
          "functional_requirements": {
            "type": "array",
            "items": {"type": "string"}
          },
          "non_functional_requirements": {
            "type": "array",
            "items": {"type": "string"}
          },
          "acceptance_criteria": {
            "type": "array",
            "items": {"type": "string"}
          }
        }
      }
  
  security:
    network:
      mode: allow
      allowlist: ["api.openai.com"]
    filesystem:
      paths:
        - path: "/workspace"
          mode: read
    resources:
      cpu: 2.0
      memory: "2Gi"
      timeout: 300s
```

#### 5.2: The Forge Workflow

**Files:**

- `demo-agents/workflows/forge.yaml` (NEW)

**Content:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: the-forge
  version: "1.0.0"
  description: "Constitutional Development Lifecycle: Full software development with adversarial testing"
  labels:
    category: "development"
    pattern: "constitutional-lifecycle"

spec:
  context:
    max_code_iterations: 5
    constitution: "{{workflow.constitution_path}}"  # Path to project rules
    approval_required: true

  initial_state: ANALYZE

  states:
    # Phase 1: Requirements Analysis
    ANALYZE:
      kind: Agent
      agent: requirements-ai-v1
      input: |
        Analyze the following user request and generate formal requirements.
        
        User Request: {{workflow.user_request}}
        
        Existing Codebase: {{workflow.codebase_context}}
        
        Output structured requirements with:
        - Functional requirements (what the system should do)
        - Non-functional requirements (performance, security, etc.)
        - Acceptance criteria (how to verify success)
      
      transitions:
        - condition: always
          target: AWAIT_REQUIREMENTS_APPROVAL

    # Human approval gate
    AWAIT_REQUIREMENTS_APPROVAL:
      kind: Human
      prompt: |
        Review requirements:
        {{ANALYZE.output}}
        
        Approve? (yes/no)
      timeout: 3600s  # 1 hour
      
      transitions:
        - condition: input_equals_yes
          target: ARCHITECT
        - condition: input_equals_no
          target: ANALYZE
          feedback: "Requirements rejected. Revise based on feedback: {{human.feedback}}"

    # Phase 2: Architecture Design
    ARCHITECT:
      kind: Agent
      agent: architect-ai-v1
      input: |
        Design system architecture to meet these requirements:
        {{ANALYZE.output}}
        
        Constitution: {{workflow.constitution}}
        
        Output:
        - Component diagram (text/ASCII art)
        - Data flow
        - Technology choices
        - Design patterns to use
      
      transitions:
        - condition: always
          target: AWAIT_ARCH_APPROVAL

    AWAIT_ARCH_APPROVAL:
      kind: Human
      prompt: |
        Review architecture:
        {{ARCHITECT.output}}
        
        Approve? (yes/no)
      timeout: 3600s
      
      transitions:
        - condition: input_equals_yes
          target: TEST
        - condition: input_equals_no
          target: ARCHITECT
          feedback: "{{human.feedback}}"

    # Phase 3: Test-Driven Development
    TEST:
      kind: Agent
      agent: tester-ai-v1
      input: |
        Write comprehensive tests for:
        
        Requirements: {{ANALYZE.output}}
        Architecture: {{ARCHITECT.output}}
        
        Follow TDD: Write tests BEFORE code.
        Output pytest test files.
      
      transitions:
        - condition: always
          target: CODE

    # Phase 4: Implementation
    CODE:
      kind: Agent
      agent: coder-ai-v1
      input: |
        Implement the system to pass these tests:
        {{TEST.output}}
        
        Architecture: {{ARCHITECT.output}}
        Constitution: {{workflow.constitution}}
        
        {{#if CODE_ITERATION > 0}}
        Previous attempt failed:
        {{EXECUTE.output.stderr}}
        
        Test failures:
        {{EXECUTE_TESTS.output}}
        {{/if}}
      
      transitions:
        - condition: always
          target: EXECUTE_TESTS

    # Execute tests
    EXECUTE_TESTS:
      kind: System
      command: "pytest tests/ -v"
      env:
        PYTHONPATH: "/workspace"
      
      transitions:
        - condition: exit_code_zero
          target: AUDIT
        - condition: exit_code_non_zero
          target: CODE
          feedback: "Tests failed. Logs: {{EXECUTE_TESTS.output.stderr}}"

    # Phase 5: Adversarial Audit
    AUDIT:
      kind: ParallelAgents  # NEW: Execute multiple agents concurrently
      agents:
        - agent: reviewer-ai-v1
          input: |
            Review this code for quality:
            {{CODE.output}}
            
            Constitution: {{workflow.constitution}}
            
            Check for:
            - Code clarity
            - Performance
            - Maintainability
        
        - agent: critic-ai-v1
          input: |
            You are an adversarial tester. Try to BREAK this code:
            {{CODE.output}}
            
            Find edge cases, security issues, logic errors.
        
        - agent: security-ai-v1
          input: |
            Security audit:
            {{CODE.output}}
            
            Check for:
            - SQL injection
            - XSS vulnerabilities
            - Authentication issues
            - Secrets in code
      
      consensus:
        strategy: unanimous_approval
        threshold: 0.95
      
      transitions:
        - condition: all_approved
          target: COMPLETE
        - condition: any_rejected
          target: CODE
          feedback: |
            Audit failed:
            Reviewer: {{AUDIT.reviewer.output}}
            Critic: {{AUDIT.critic.output}}
            Security: {{AUDIT.security.output}}

    # Terminal states
    COMPLETE:
      kind: System
      command: "finalize"
      env:
        RESULT: "{{CODE.output}}"
        TESTS: "{{TEST.output}}"
        AUDIT: "{{AUDIT.output}}"
      
      transitions: []

    FAILED:
      kind: System
      command: "log_failure"
      env:
        REASON: "{{state.feedback}}"
      
      transitions: []
```

#### 5.3: Parallel Agent Execution

**Files:**

- `orchestrator/core/src/domain/workflow.rs` (UPDATE)

**Add StateKind variant:**

```rust
pub enum StateKind {
    Agent { /* ... */ },
    System { /* ... */ },
    Human { /* ... */ },
    
    // NEW: Parallel agent execution
    ParallelAgents {
        agents: Vec<ParallelAgentSpec>,
        consensus: ConsensusConfig,
    },
}

pub struct ParallelAgentSpec {
    pub agent_id: AgentId,
    pub input_template: String,
    pub weight: f64,
}

pub struct ConsensusConfig {
    pub strategy: ConsensusStrategy,
    pub threshold: f64,
}
```

**Update WorkflowEngine:**

```rust
impl WorkflowEngine {
    async fn execute_parallel_agents(
        &self,
        agents: &[ParallelAgentSpec],
        consensus: &ConsensusConfig,
        blackboard: &Blackboard,
    ) -> Result<StateResult> {
        // Execute all agents concurrently
        let mut handles = vec![];
        for spec in agents {
            let input = blackboard.hydrate(&spec.input_template)?;
            let exec_service = self.execution_service.clone();
            let agent_id = spec.agent_id;
            
            let handle = tokio::spawn(async move {
                exec_service.start_execution(agent_id, input).await
            });
            handles.push((handle, spec.weight));
        }
        
        // Collect results
        let mut results = vec![];
        for (handle, weight) in handles {
            let exec_id = handle.await??;
            let execution = self.execution_service.wait_for_completion(exec_id).await?;
            results.push((execution, weight));
        }
        
        // Apply consensus
        let consensus_result = self.apply_consensus(&results, consensus)?;
        
        Ok(StateResult {
            success: consensus_result.approved,
            output: serde_json::to_value(consensus_result)?,
            exit_code: None,
        })
    }
}
```

#### 5.4: Human-in-the-Loop

**Files:**

- `orchestrator/core/src/infrastructure/human_input.rs` (NEW)

**Implementation:**

```rust
pub struct HumanInputGate {
    pending_prompts: Arc<RwLock<HashMap<WorkflowExecutionId, PendingPrompt>>>,
}

pub struct PendingPrompt {
    pub prompt: String,
    pub timeout: Option<DateTime<Utc>>,
    pub response: Option<String>,
}

impl HumanInputGate {
    pub async fn prompt(
        &self,
        exec_id: WorkflowExecutionId,
        prompt: String,
        timeout: Option<Duration>,
    ) -> Result<String> {
        let timeout_at = timeout.map(|d| Utc::now() + d);
        
        let pending = PendingPrompt {
            prompt: prompt.clone(),
            timeout: timeout_at,
            response: None,
        };
        
        self.pending_prompts.write().await.insert(exec_id, pending);
        
        // Publish event for UI
        self.event_bus.publish(WorkflowEvent::HumanInputRequired {
            execution_id: exec_id,
            prompt,
            timeout: timeout_at,
        })?;
        
        // Poll for response (or timeout)
        loop {
            let prompts = self.pending_prompts.read().await;
            if let Some(pending) = prompts.get(&exec_id) {
                if let Some(response) = &pending.response {
                    return Ok(response.clone());
                }
                if let Some(timeout_at) = pending.timeout {
                    if Utc::now() > timeout_at {
                        return Err(Error::HumanInputTimeout);
                    }
                }
            }
            drop(prompts);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    
    pub async fn respond(
        &self,
        exec_id: WorkflowExecutionId,
        response: String,
    ) -> Result<()> {
        let mut prompts = self.pending_prompts.write().await;
        if let Some(pending) = prompts.get_mut(&exec_id) {
            pending.response = Some(response);
            Ok(())
        } else {
            Err(Error::PromptNotFound)
        }
    }
}
```

**REST API endpoint:**

```rust
// In HTTP controller
#[post("/workflows/{exec_id}/input")]
async fn submit_human_input(
    exec_id: Path<WorkflowExecutionId>,
    input: Json<HumanInput>,
    human_gate: Data<HumanInputGate>,
) -> Result<HttpResponse> {
    human_gate.respond(*exec_id, input.response.clone()).await?;
    Ok(HttpResponse::Ok().json(json!({"status": "accepted"})))
}
```

**Deliverables:**

- [ ] Specialized agent manifests (6 agents)
- [ ] `forge.yaml` workflow with Constitutional Lifecycle
- [ ] Parallel agent execution in WorkflowEngine
- [ ] Human-in-the-loop prompt system
- [ ] REST API for human input submission
- [ ] Integration test: Full Forge workflow
- [ ] Documentation: "The Forge: Autonomous Software Development"

**Success Criteria:**

- ✅ Requirements → Architecture → Tests → Code → Audit pipeline works end-to-end
- ✅ Parallel agents execute concurrently and reach consensus
- ✅ Human approval gates pause workflow correctly
- ✅ Adversarial critic finds edge cases
- ✅ Security AI catches vulnerabilities
- ✅ Final code passes all tests + audits

---

### Phase 6: Stimulus-Response Routing

**Duration:** 1-2 weeks  
**Dependencies:** Phase 5  
**Status:** NOT STARTED

**Goals:**

1. Create RouterAgent that classifies stimuli
2. Implement workflow registry (stimulus → workflow mapping)
3. Add event-driven workflow triggering
4. Create example workflows (conversation, debug, develop)
5. Demonstrate AGI-style always-on sensor loop

**Tasks:**

#### 6.1: RouterAgent Manifest

**Files:**

- `demo-agents/router/router-agent.yaml` (NEW)

**Content:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: AgentManifest
metadata:
  name: router-agent-v1
  version: "1.0.0"
  description: "Classifies user input and selects appropriate workflow"
  labels:
    role: "router"
    capability: "stimulus-classification"

spec:
  runtime:
    language: python
    version: "3.11"
    isolation: docker
  
  entrypoint: router.py
  
  model:
    provider: default
    alias: "gpt-3.5"
    temperature: 0.0  # Deterministic classification
  
  validation:
    type: schema
    schema: |
      {
        "type": "object",
        "required": ["workflow", "confidence"],
        "properties": {
          "workflow": {"type": "string"},
          "confidence": {"type": "number"},
          "reasoning": {"type": "string"}
        }
      }
  
  security:
    network:
      mode: allow
      allowlist: ["api.openai.com"]
    filesystem:
      read_only: true
    resources:
      cpu: 1.0
      memory: "512Mi"
      timeout: 30s
```

**Agent Code (router.py):**

```python
import sys
import json
from openai import OpenAI

def main():
    input_data = json.loads(sys.stdin.read())
    user_input = input_data["stimulus"]
    available_workflows = input_data["available_workflows"]
    
    client = OpenAI()
    
    prompt = f"""Classify this user input into the most appropriate workflow.

User Input: {user_input}

Available Workflows:
{json.dumps(available_workflows, indent=2)}

Respond with JSON only:
{{
  "workflow": "workflow_name",
  "confidence": 0.0-1.0,
  "reasoning": "brief explanation"
}}"""
    
    response = client.chat.completions.create(
        model="gpt-3.5-turbo",
        messages=[{"role": "user", "content": prompt}],
        temperature=0.0
    )
    
    result = json.loads(response.choices[0].message.content)
    print(json.dumps(result))

if __name__ == "__main__":
    main()
```

#### 6.2: Workflow Registry

**Files:**

- `orchestrator/core/src/domain/workflow_registry.rs` (NEW)

**Implementation:**

```rust
pub struct WorkflowRegistry {
    workflows: HashMap<String, RegisteredWorkflow>,
}

pub struct RegisteredWorkflow {
    pub workflow_id: WorkflowId,
    pub name: String,
    pub category: String,
    pub description: String,
    pub trigger_patterns: Vec<String>,  // Regex patterns
    pub priority: u8,
}

impl WorkflowRegistry {
    pub fn register(&mut self, workflow: RegisteredWorkflow) -> Result<()> {
        if self.workflows.contains_key(&workflow.name) {
            return Err(Error::WorkflowAlreadyRegistered(workflow.name.clone()));
        }
        self.workflows.insert(workflow.name.clone(), workflow);
        Ok(())
    }
    
    pub fn find_by_name(&self, name: &str) -> Option<&RegisteredWorkflow> {
        self.workflows.get(name)
    }
    
    pub fn list_all(&self) -> Vec<&RegisteredWorkflow> {
        self.workflows.values().collect()
    }
    
    pub async fn classify_stimulus(
        &self,
        stimulus: &str,
        router_agent_id: AgentId,
        execution_service: Arc<ExecutionService>,
    ) -> Result<String> {
        let available_workflows = self.list_all()
            .iter()
            .map(|w| serde_json::json!({
                "name": w.name,
                "category": w.category,
                "description": w.description,
            }))
            .collect::<Vec<_>>();
        
        let input = serde_json::json!({
            "stimulus": stimulus,
            "available_workflows": available_workflows,
        });
        
        let exec_id = execution_service.start_execution(
            router_agent_id,
            input.to_string()
        ).await?;
        
        let execution = execution_service.wait_for_completion(exec_id).await?;
        let result: RouterResult = serde_json::from_str(&execution.final_output()?)?;
        
        if result.confidence < 0.7 {
            return Err(Error::UncertainClassification(result.confidence));
        }
        
        Ok(result.workflow)
    }
}

#[derive(Deserialize)]
struct RouterResult {
    workflow: String,
    confidence: f64,
    reasoning: String,
}
```

#### 6.3: Event-Driven Triggering

**Files:**

- `orchestrator/core/src/application/stimulus_handler.rs` (NEW)

**Implementation:**

```rust
pub struct StimulusHandler {
    workflow_registry: Arc<WorkflowRegistry>,
    workflow_engine: Arc<WorkflowEngine>,
    router_agent_id: AgentId,
    execution_service: Arc<ExecutionService>,
    event_bus: Arc<EventBus>,
}

impl StimulusHandler {
    pub async fn handle_stimulus(&self, stimulus: Stimulus) -> Result<WorkflowExecutionId> {
        // 1. Classify stimulus
        let workflow_name = self.workflow_registry.classify_stimulus(
            &stimulus.content,
            self.router_agent_id,
            self.execution_service.clone()
        ).await?;
        
        // 2. Find workflow
        let workflow = self.workflow_registry.find_by_name(&workflow_name)
            .ok_or(Error::WorkflowNotFound(workflow_name.clone()))?;
        
        // 3. Start workflow
        let exec_id = self.workflow_engine.start_workflow(
            workflow.workflow_id,
            serde_json::to_value(stimulus)?
        ).await?;
        
        // 4. Publish event
        self.event_bus.publish(WorkflowEvent::StimulusRouted {
            stimulus_id: stimulus.id,
            workflow_name: workflow_name.clone(),
            execution_id: exec_id,
            routed_at: Utc::now(),
        })?;
        
        Ok(exec_id)
    }
}

pub struct Stimulus {
    pub id: StimulusId,
    pub source: StimulusSource,
    pub content: String,
    pub metadata: HashMap<String, Value>,
    pub received_at: DateTime<Utc>,
}

pub enum StimulusSource {
    UserInput,
    Webhook,
    Scheduled,
    Internal,
}
```

#### 6.4: Example Workflows

**Files:**

- `demo-agents/workflows/conversation.yaml` (NEW)
- `demo-agents/workflows/debug.yaml` (NEW)
- `demo-agents/workflows/develop.yaml` (NEW)

##### Example: conversation.yaml

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: conversation
  version: "1.0.0"
  description: "Simple conversational interaction"
  labels:
    category: "conversation"

spec:
  initial_state: RESPOND

  states:
    RESPOND:
      kind: Agent
      agent: conversational-ai-v1
      input: "{{stimulus.content}}"
      
      transitions:
        - condition: always
          target: COMPLETE

    COMPLETE:
      kind: System
      command: "finalize"
      env:
        RESULT: "{{RESPOND.output}}"
      
      transitions: []
```

#### 6.5: Always-On Sensor Loop

**Files:**

- `cli/src/commands/sense.rs` (NEW)

**Implementation:**

```rust
#[derive(Parser)]
pub struct SenseCommand {
    #[arg(short, long)]
    source: String,  // "stdin" | "webhook" | "schedule"
    
    #[arg(short, long)]
    router: String,  // Router agent name
}

impl SenseCommand {
    pub async fn execute(&self, config: &Config) -> Result<()> {
        let stimulus_handler = self.build_stimulus_handler(config).await?;
        
        match self.source.as_str() {
            "stdin" => self.sense_stdin(stimulus_handler).await,
            "webhook" => self.sense_webhook(stimulus_handler).await,
            "schedule" => self.sense_schedule(stimulus_handler).await,
            _ => Err(Error::InvalidSource(self.source.clone())),
        }
    }
    
    async fn sense_stdin(&self, handler: Arc<StimulusHandler>) -> Result<()> {
        println!("AEGIS AGI Loop (type 'exit' to quit)");
        
        loop {
            print!("> ");
            io::stdout().flush()?;
            
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            
            let input = input.trim();
            if input == "exit" {
                break;
            }
            
            // Create stimulus
            let stimulus = Stimulus {
                id: StimulusId::new(),
                source: StimulusSource::UserInput,
                content: input.to_string(),
                metadata: HashMap::new(),
                received_at: Utc::now(),
            };
            
            // Handle stimulus
            let exec_id = handler.handle_stimulus(stimulus).await?;
            println!("Started workflow execution: {}", exec_id);
            
            // Stream output
            let mut stream = handler.workflow_engine.stream_execution(exec_id).await?;
            while let Some(event) = stream.next().await {
                match event {
                    WorkflowEvent::StateCompleted { state_name, output, .. } => {
                        println!("[{}] {}", state_name, output);
                    }
                    WorkflowEvent::WorkflowCompleted { final_output, .. } => {
                        println!("\n✓ Complete: {}", final_output);
                        break;
                    }
                    WorkflowEvent::WorkflowFailed { reason, .. } => {
                        println!("\n✗ Failed: {}", reason);
                        break;
                    }
                    _ => {}
                }
            }
        }
        
        Ok(())
    }
}
```

**Usage:**

```bash
$ aegis sense --source stdin --router router-agent-v1
AEGIS AGI Loop (type 'exit' to quit)
> What's the weather?
[RESPOND] It's sunny and 72°F
✓ Complete

> Debug my Python script
[ANALYZE] Detected syntax error on line 5
[FIX] Applied fix: missing colon
✓ Complete

> Build me a web scraper
[ANALYZE] Requirements gathered
[ARCHITECT] Architecture designed
[TEST] Tests written
[CODE] Implementation complete
[AUDIT] All checks passed
✓ Complete
```

**Deliverables:**

- [ ] `router-agent.yaml` manifest
- [ ] `WorkflowRegistry` domain model
- [ ] `StimulusHandler` application service
- [ ] Example workflows (conversation, debug, develop)
- [ ] `aegis sense` CLI command for always-on loop
- [ ] Integration test: Stimulus → Router → Workflow
- [ ] Documentation: "Building AGI-Style Stimulus-Response Systems"

**Success Criteria:**

- ✅ RouterAgent classifies stimuli with >70% confidence
- ✅ Workflow registry maps stimuli to workflows
- ✅ Event-driven triggering starts workflows automatically
- ✅ Always-on sensor loop processes stdin continuously
- ✅ System can autonomously select conversation vs development workflow

---

## Technical Specifications

### 1. Workflow Manifest Schema

Full specification: [WORKFLOW_MANIFEST_SPEC_V1.md](../../../aegis-architecture/specs/WORKFLOW_MANIFEST_SPEC_V1.md)

**Minimal Example:**

```yaml
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: my-workflow
  version: "1.0.0"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: worker-v1
      input: "{{workflow.task}}"
      transitions:
        - condition: always
          target: END
```

### 2. StateKind Variants

```rust
pub enum StateKind {
    Agent {
        agent_id: AgentId,
        input_template: String,
        isolation: IsolationMode,
    },
    System {
        command: String,
        env: HashMap<String, String>,
    },
    Human {
        prompt: String,
        timeout: Option<Duration>,
    },
    ParallelAgents {
        agents: Vec<ParallelAgentSpec>,
        consensus: ConsensusConfig,
    },
}
```

### 3. TransitionCondition Grammar

```rust
pub enum TransitionCondition {
    Always,
    OnSuccess,
    OnFailure,
    OnExitCode(i32),
    OnScoreAbove(f64),
    OnScoreBelow(f64),
    OnScoreBetween(f64, f64),
    OnConfidenceAbove(f64),
    OnConsensus { threshold: f64, agreement: f64 },
    Custom(String),  // Boolean expression in template syntax
}
```

### 4. Blackboard Template Syntax

```yaml
# Access state output
{{STATE_NAME.output}}

# Access specific field
{{STATE_NAME.output.field}}

# Conditional
{{#if condition}}
  ...
{{else}}
  ...
{{/if}}

# Loop
{{#each items}}
  {{this}}
{{/each}}

# Context variables
{{workflow.task}}
{{workflow.agent_id}}
{{blackboard.iteration_number}}
```

### 5. Cortex Hybrid Scoring Formula

```python
# Step 1: Vector similarity
cosine_similarity = 1.0 - euclidean_distance(query_vec, pattern_vec)

# Step 2: Success weight
success_weight = pattern.success_score * pattern.weight

# Step 3: Hybrid score
hybrid_score = cosine_similarity * (1.0 + success_weight * alpha)

# Where alpha = tunable parameter (default: 0.5)
```

### 6. Time-Decay Formula

```python
# Exponential decay
W_new = W_old * exp(-λ * Δt)

# Where:
# λ = decay rate (default: 0.1)
# Δt = days since last_verified
# W_new ≥ 1.0 (minimum weight)
```

### 7. Multi-Judge Consensus Strategies

```rust
pub enum ConsensusStrategy {
    WeightedAverage,        // Weighted by judge weights
    Majority,               // >50% approval
    Unanimous,              // 100% approval
    BestOfN { n: usize },   // Take top N scores
}
```

---

## Success Criteria

### Phase Completion Criteria

Each phase is complete when:

1. ✅ All deliverables are implemented and committed
2. ✅ All tests pass (unit + integration)
3. ✅ Documentation is written and reviewed
4. ✅ ADRs are updated
5. ✅ Demo/example works end-to-end
6. ✅ Code review approved by team

### System-Level Success Metrics

**Technical Metrics:**

- ✅ Workflow execution latency < 200ms per state transition
- ✅ Cortex retrieval latency < 50ms (p95)
- ✅ Judge agent execution < 5 seconds
- ✅ First-try success rate increases by >20% after 100 executions
- ✅ Memory growth < 1MB per execution

**Functional Metrics:**

- ✅ Can execute linear workflows (Phase 1)
- ✅ Can execute branching workflows (Phase 1)
- ✅ Judge is agent, not hardcoded (Phase 2)
- ✅ Validation produces gradient scores (Phase 3)
- ✅ Cortex learns from refinements (Phase 4)
- ✅ The Forge completes full dev cycle (Phase 5)
- ✅ Router selects workflows correctly (Phase 6)

**Philosophical Alignment:**

- ✅ Fractal self-similarity: All roles are agents
- ✅ Deterministic verification: Trust loop, not monkey
- ✅ Memory as identity: Cortex shapes behavior
- ✅ Workflow as cognition: FSM is thought process
- ✅ Trust through transparency: Iterations visible

---

## References

### Internal Documents

**PIVOT Documents:**

- [semantic-gradient.md](semantic-gradient.md) — Memory as identity, weighted deduplication
- [workflow-based.md](workflow-based.md) — FSM pipelines, meta-monkey judges
- [ai-forge-blueprint.md](ai-forge-blueprint.md) — Constitutional lifecycle, specialized agents

**Architecture Documents:**

- [AGENTS.md](../../../aegis-architecture/AGENTS.md) — DDD domain model, bounded contexts
- [ADR-006: Cyber-Biological Architecture](../../../aegis-architecture/adrs/006-cyber-biological-architecture-philosophy.md) — Membrane + Nucleus
- [100monkeys Algorithm Spec](../../../aegis-architecture/specs/100MONKEYS_ALGORITHM_SPECIFICATION.md) — Iterative refinement
- [Safe Evolution Manifesto](../../../aegis-architecture/specs/SAFE_EVOLUTION_MANIFESTO.md) — Trust through containment
- [Zaru Paradigm](../../../aegis-architecture/specs/ZARU_PARADIGM.md) — Product vision, UX principles
- [NODE_CONFIGURATION_SPEC_V1.md](../../../aegis-architecture/specs/NODE_CONFIGURATION_SPEC_V1.md) — Node config, runtime isolation

**ADRs:**

- ADR-015: Workflow Engine Architecture
- ADR-016: Agent-as-Judge Pattern
- ADR-017: Gradient Validation System
- ADR-018: Weighted Cortex Memory
- ADR-019: Blackboard Context System
- ADR-020: The Forge Reference Pattern
- ADR-021: Stimulus-Response Routing

**Specs:**

- WORKFLOW_MANIFEST_SPEC_V1.md

### External Resources

- [Finite State Machines](https://en.wikipedia.org/wiki/Finite-state_machine)
- [Qdrant Documentation](https://qdrant.github.io/qdrant/)
- [Firecracker Documentation](https://firecracker-microvm.github.io/)
- [Domain-Driven Design](https://www.domainlanguage.com/ddd/)

---

## Appendix: Migration Path from Current System

### For Existing Users

If you're currently using `aegis run --agent <name>`:

**Phase 1 (WorkflowEngine):**

```bash
# Old way (deprecated after Phase 1)
aegis run --agent my-agent --input "Hello"

# New way
aegis run --workflow 100monkeys-classic.yaml --input "Hello"
```

**Phase 2 (AgenticJudge):**

- No user-facing changes
- Judge becomes agent internally
- Existing agents continue to work

**Phase 3-6:**

- Existing functionality preserved
- New workflows unlock advanced capabilities
- Gradual adoption of new features

### For Developers

**Current Code:**

```rust
// Old: supervisor.rs
let result = supervisor.run_loop(config, input, 5, judge, observer).await?;
```

**New Code (Phase 1+):**

```rust
// New: workflow_engine.rs
let workflow_id = workflow_service.deploy_workflow(workflow).await?;
let exec_id = workflow_service.start_workflow(workflow_id, input).await?;

loop {
    let status = workflow_service.tick(exec_id).await?;
    if status != WorkflowStatus::Running {
        break;
    }
}
```

---

## Maintenance

This document should be updated:

- ✅ After each phase completion
- ✅ When architectural decisions change
- ✅ When new insights emerge from implementation
- ✅ When ADRs are added/modified

**Review Schedule:**

- Weekly during active implementation
- Monthly after initial release
- Quarterly for long-term maintenance

---

**Status:** This plan is canonical and approved for implementation. All agents (human and AI) should follow this plan. Deviations require explicit approval and plan update.

**Next Steps:** Proceed to Phase 0 (ADR creation).
