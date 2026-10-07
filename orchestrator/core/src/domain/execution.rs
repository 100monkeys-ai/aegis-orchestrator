// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Execution Domain Aggregate (BC-2, ADR-005)
//!
//! Defines the `Execution` aggregate root and the `Iteration` entity that
//! together implement the **100monkeys Algorithm** iterative refinement loop:
//!
//! ```text
//! start_execution
//!   ▼
//! Iteration 1: generate → execute → evaluate
//!   ├─ success → ExecutionCompleted
//!   └─ failure → RefinementApplied → Iteration 2 → … → Iteration N
//!                                                    └─ max_iterations reached → ExecutionFailed
//! ```
//!
//! ## Aggregate Invariants (see AGENTS.md §Execution Aggregate)
//!
//! - An `Execution` must have at least 1 `Iteration`.
//! - At most `max_iterations` iterations (default 10).
//! - Only one `Iteration` may be `Running` at a time.
//! - Iteration numbers are sequential starting from 1.
//!
//! ## Recursive Execution
//!
//! Agents may spawn child agents. `Execution.depth` tracks the nesting level;
//! `MAX_RECURSIVE_DEPTH` (defined in this module) prevents infinite recursion.
//!
//! See ADR-005 (Iterative Execution Strategy), AGENTS.md §Execution Context.

use crate::domain::agent::AgentId;
use crate::domain::tenant::TenantId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ============================================================================
// Execution Hierarchy (Recursive Execution Tracking)
// ============================================================================

/// Maximum recursive depth for nested agent executions
///
/// This prevents infinite recursion when agents call other agents.
/// Example execution tree:
///
/// ```text
/// Depth 0: User Agent (generates code)
/// Depth 1: ├─ Validation Agent (validates code)
/// Depth 2: │  └─ Meta-Validation Agent (validates validator's reasoning)
/// Depth 3: │     └─ Super-Meta Agent (validates meta-validator) [MAX DEPTH]
/// ```
pub const MAX_RECURSIVE_DEPTH: u8 = 3;

/// Recursive execution tracking
///
/// Tracks parent-child execution relationships for nested agent calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionHierarchy {
    /// Parent execution ID (None for root executions)
    pub parent_execution_id: Option<ExecutionId>,

    /// Recursive depth (0 for root, 1 for child, 2 for grandchild, etc.)
    pub depth: u8,

    /// Execution path from root (list of execution IDs)
    pub path: Vec<ExecutionId>,

    /// Optional swarm ID linking this execution to a multi-agent swarm (ADR-039).
    /// Uses raw UUID to avoid circular dependency with the swarm crate.
    #[serde(default)]
    pub swarm_id: Option<uuid::Uuid>,
}

impl Default for ExecutionHierarchy {
    fn default() -> Self {
        // Use a synthetic root ID for default initialization.
        let temp_id = ExecutionId::new();
        Self::root(temp_id)
    }
}

impl ExecutionHierarchy {
    /// Create a root execution hierarchy (no parent)
    pub fn root(execution_id: ExecutionId) -> Self {
        Self {
            parent_execution_id: None,
            depth: 0,
            path: vec![execution_id],
            swarm_id: None,
        }
    }

    /// Create a child execution hierarchy
    pub fn child(
        parent: &ExecutionHierarchy,
        child_execution_id: ExecutionId,
    ) -> Result<Self, String> {
        let new_depth = parent.depth + 1;

        if new_depth > MAX_RECURSIVE_DEPTH {
            return Err(format!(
                "Maximum recursive depth ({MAX_RECURSIVE_DEPTH}) exceeded. Cannot create child execution."
            ));
        }

        let mut path = parent.path.clone();
        path.push(child_execution_id);

        Ok(Self {
            parent_execution_id: Some(parent.path[parent.path.len() - 1]),
            depth: new_depth,
            path,
            swarm_id: parent.swarm_id,
        })
    }

    /// Check if this execution can spawn a child
    pub fn can_spawn_child(&self) -> bool {
        self.depth < MAX_RECURSIVE_DEPTH
    }

    /// Get the root execution ID
    pub fn root_id(&self) -> ExecutionId {
        self.path[0]
    }

    /// Get the immediate parent execution ID
    pub fn parent_id(&self) -> Option<ExecutionId> {
        self.parent_execution_id
    }

    /// Associate this hierarchy with a swarm.
    pub fn with_swarm_id(mut self, swarm_id: uuid::Uuid) -> Self {
        self.swarm_id = Some(swarm_id);
        self
    }
}

// ============================================================================
// Execution Entity
// ============================================================================

pub use crate::domain::shared_kernel::ExecutionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    #[serde(default)]
    pub tenant_id: TenantId,
    pub status: ExecutionStatus,
    pub iterations: Vec<Iteration>,
    pub max_iterations: u8,
    pub input: ExecutionInput,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,

    /// Hierarchical execution tracking for nested agent calls
    /// Enables judge-calling-judge and agent composition patterns
    #[serde(default)]
    pub hierarchy: ExecutionHierarchy,

    /// Container user ID for permission squashing (ADR-036)
    /// Default: 1000 (standard non-root user)
    #[serde(default = "default_container_uid")]
    pub container_uid: u32,

    /// Container group ID for permission squashing (ADR-036)
    /// Default: 1000 (standard non-root group)
    #[serde(default = "default_container_gid")]
    pub container_gid: u32,

    /// Security context name governing tool access for this execution (ADR-083).
    #[serde(default = "default_security_context_name")]
    pub security_context_name: String,

    /// The `sub` claim of the user who initiated this execution, if available.
    /// Used by the dispatch gateway to reconstruct a minimal `UserIdentity` for
    /// user-scoped rate limiting (ADR-072) when the agent runtime calls back in.
    #[serde(default)]
    pub initiating_user_sub: Option<String>,

    /// The supervisor's bound on the whole execution, in seconds: the agent's
    /// `spec.security.resources.timeout`, or the supervisor's default when the
    /// manifest gives none. Recorded when the execution starts, so the
    /// container reaper can tell an execution that has outlived its bound
    /// without reading the agent's manifest again (ADR-040, Update of
    /// 2026-10-04). `None` until the execution starts.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

fn default_container_uid() -> u32 {
    1000
}

fn default_container_gid() -> u32 {
    1000
}

fn default_security_context_name() -> String {
    "aegis-system-operator".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInput {
    /// Optional free-text override used by the natural-language dispatch path.
    /// Steers the LLM prompt directly. Complementary to `input`, not an
    /// alternative — when an agent declares `input_schema`, callers pass typed
    /// data via `input`; `intent` may be omitted or used alongside it.
    pub intent: Option<String>,
    /// Typed input data for the agent, validated against the agent's
    /// `input_schema` when one is declared. Supplies structured data to the
    /// prompt template context via `{{input}}` / `{{input.KEY}}` dot-notation
    /// (ADR-092).
    pub input: serde_json::Value,
    /// Workspace volume ID provisioned by the workflow orchestrator. When set,
    /// the runtime registers this pre-existing volume against the execution so
    /// fs.* tools can write to the shared workspace (ADR-087).
    pub workspace_volume_id: Option<crate::domain::shared_kernel::VolumeId>,
    /// Mount path for the workspace volume. Defaults to /workspace.
    pub workspace_volume_mount_path: Option<std::path::PathBuf>,
    /// NFS remote path for the workspace volume in SeaweedFS.
    /// When set, used directly for NFS registration instead of constructing a path.
    pub workspace_remote_path: Option<String>,
    /// Workflow execution UUID that owns the workspace volume. When set,
    /// `persist_external_volume` uses `VolumeOwnership::WorkflowExecution` instead
    /// of `VolumeOwnership::Execution`, and the NFS volume registration carries
    /// this ID so FSAL `authorize()` can match it without DB ownership mutations.
    pub workflow_execution_id: Option<uuid::Uuid>,
    /// Structured references to files attached at dispatch time (ADR-113).
    /// Each ref points to a file in a tenant-scoped volume that the agent may
    /// read via the `aegis.attachment.read` tool. Empty when the dispatch
    /// carries no attachments. The shape mirrors `workspace_volume_id` —
    /// structured, not an opaque fileId — so attachments are deterministic at
    /// dispatch time per ADR-092 and never LLM-mediated.
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
}

/// The reserved key of an execution's `input` that carries the person's
/// choice of credential bindings for each remote tool server, as
/// `{"<server>": "<binding id>" | ["<binding id>", ...] | null}` (Zaru
/// ADR-0055 D9, D14; AEGIS ADR-132 Update (13) S11a). Like the caller's
/// `outputs`, it is the platform's and never the agent's: the input schema
/// and the rendered prompt never see it.
pub const CONTEXTS_INPUT_KEY: &str = "contexts";

/// The refusal of a `contexts` value of any shape but S11a's, before
/// anything starts or is called (AEGIS ADR-132 Update (13) S11a).
pub const CONTEXTS_SHAPE: &str = "'contexts' must be an object naming, for each server, a binding id, a list of binding ids, or null";

/// What a call's dispatch chose for one remote server's credential (Zaru
/// ADR-0055 D2, D9, D15): the one binding a call carries, after the call
/// picked it from its server's chosen set (AEGIS ADR-132 Update (13) S11b,
/// S11e).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextChoice {
    /// The dispatch named nothing for the server: the per-agent grant path
    /// (AEGIS ADR-132 S6) stands.
    NotGiven,
    /// The dispatch named `null`: the execution has no credential for the
    /// server, whatever is granted.
    None,
    /// The dispatch named this binding: it is the credential, no grant
    /// needed, when it is the acting person's own active binding for the
    /// server.
    Binding(crate::domain::credential::CredentialBindingId),
}

/// What an execution's dispatch chose for one server (AEGIS ADR-132 Update
/// (13) S11b): nothing, none, or a non-empty set of the person's bindings in
/// the order the person chose them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerChoice {
    /// The dispatch named nothing for the server: the grant path stands.
    NotGiven,
    /// The dispatch named `null`, or a value that cannot be read: no
    /// credential for the server, whatever is granted.
    None,
    /// The bindings chosen, distinct, at least one, in the person's order.
    Bindings(Vec<crate::domain::credential::CredentialBindingId>),
}

impl ServerChoice {
    /// The one binding a call carries when the set holds exactly one, or
    /// the choice of nothing or none as it stands; `None` for a set of two
    /// or more, where the call must say which (S11e).
    pub fn single(&self) -> Option<ContextChoice> {
        match self {
            ServerChoice::NotGiven => Some(ContextChoice::NotGiven),
            ServerChoice::None => Some(ContextChoice::None),
            ServerChoice::Bindings(ids) if ids.len() == 1 => Some(ContextChoice::Binding(ids[0])),
            ServerChoice::Bindings(_) => None,
        }
    }
}

/// One server's value in `contexts` read as S11a admits it: `null` is none,
/// a binding id is a set of one, a non-empty list of distinct binding ids is
/// that set; anything else is `None` (unreadable).
fn read_server_value(value: &serde_json::Value) -> Option<ServerChoice> {
    let binding = |value: &serde_json::Value| match value {
        serde_json::Value::String(id) => uuid::Uuid::parse_str(id)
            .ok()
            .map(crate::domain::credential::CredentialBindingId),
        _ => None,
    };
    match value {
        serde_json::Value::Null => Some(ServerChoice::None),
        serde_json::Value::String(_) => binding(value).map(|id| ServerChoice::Bindings(vec![id])),
        serde_json::Value::Array(items) if !items.is_empty() => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                let id = binding(item)?;
                if ids.contains(&id) {
                    return None;
                }
                ids.push(id);
            }
            Some(ServerChoice::Bindings(ids))
        }
        _ => None,
    }
}

/// Whether `value` is a `contexts` S11a admits: an object whose every value
/// is `null`, a binding id, or a non-empty list of distinct binding ids.
/// Anything else answers [`CONTEXTS_SHAPE`].
pub fn check_contexts_shape(value: &serde_json::Value) -> Result<(), &'static str> {
    match value {
        serde_json::Value::Object(map) if map.values().all(|v| read_server_value(v).is_some()) => {
            Ok(())
        }
        _ => Err(CONTEXTS_SHAPE),
    }
}

/// An execution's choices, server by server, read from its input's
/// reserved key [`CONTEXTS_INPUT_KEY`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionContexts {
    choices: std::collections::BTreeMap<String, ServerChoice>,
}

impl ExecutionContexts {
    /// Read the choices from a `contexts` value (S11b): `null` is none, a
    /// binding id a set of one, a list of binding ids that set; a value
    /// that cannot be read makes its server `None`, never `NotGiven`, so an
    /// unreadable choice never opens the grant path. A value that is not an
    /// object chooses nothing (the starts refuse it, S11a).
    pub fn from_value(value: Option<&serde_json::Value>) -> Self {
        let mut choices = std::collections::BTreeMap::new();
        if let Some(serde_json::Value::Object(map)) = value {
            for (server, choice) in map {
                choices.insert(
                    server.clone(),
                    read_server_value(choice).unwrap_or(ServerChoice::None),
                );
            }
        }
        Self { choices }
    }

    /// The choice for `server`.
    pub fn server(&self, server: &str) -> ServerChoice {
        self.choices
            .get(server)
            .cloned()
            .unwrap_or(ServerChoice::NotGiven)
    }

    /// Every server named, with its choice.
    pub fn servers(&self) -> impl Iterator<Item = (&str, &ServerChoice)> {
        self.choices
            .iter()
            .map(|(server, choice)| (server.as_str(), choice))
    }

    /// Whether the dispatch chose at least one binding for `server` (a
    /// declared context is filled only then, Zaru ADR-0055 D16, AEGIS
    /// ADR-132 Update (13) S11).
    pub fn is_filled(&self, server: &str) -> bool {
        matches!(self.server(server), ServerChoice::Bindings(_))
    }
}

impl ExecutionInput {
    /// The dispatch's credential choices (Zaru ADR-0055 D14).
    pub fn contexts(&self) -> ExecutionContexts {
        ExecutionContexts::from_value(self.input.get(CONTEXTS_INPUT_KEY))
    }
}

/// Structured reference to a file attached at dispatch time (ADR-113).
///
/// Mirrors the `aegis_runtime.AttachmentRef` proto message and is carried on
/// `ExecutionInput.attachments` so attached files are passed through the
/// execution pipeline without being mediated by the LLM. The agent reads the
/// file via the `aegis.attachment.read` tool using `(volume_id, path)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentRef {
    /// Tenant-scoped volume containing the file.
    pub volume_id: crate::domain::shared_kernel::VolumeId,
    /// POSIX path within the volume.
    pub path: String,
    /// Original filename (for display / agent reasoning).
    pub name: String,
    /// Content-sniffed MIME type (e.g. `text/plain`, `application/pdf`).
    pub mime_type: String,
    /// File size in bytes.
    pub size: u64,
    /// Optional SHA-256 hex digest of file contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// The reason an execution carries when the orchestrator process that ran
/// it ended while it was running ([`Execution::fail_cut_by_restart`]). It is
/// the execution's error, its in-flight iteration's error and the reason of
/// its `ExecutionFailed` event, so every reader of the execution sees it.
pub const ORCHESTRATOR_RESTART_FAILURE_REASON: &str =
    "The orchestrator restarted while this execution was running; no process supervises it any more, so it was ended as failed";

/// How long past an execution's own bound the container reaper waits before
/// it ends an execution still running (ADR-040, Update of 2026-10-04). The
/// supervisor records its own ending only after it has terminated the
/// container, which takes at most four engine calls of 120 s each (480 s);
/// the rest covers the time between the record's `started_at` and the start
/// of the supervisor's clock.
pub const REAPER_MARGIN_SECONDS: u64 = 600;

/// The prefix of the reason an execution carries when the container reaper
/// ended it ([`Execution::fail_outlived_bound`]).
pub const REAPER_OUTLIVED_FAILURE_PREFIX: &str =
    "The orchestrator's container reaper ended this execution";

/// The reason an execution carries when the container reaper ended it: it was
/// still running `elapsed_seconds` after it started, past its `bound_seconds`
/// and [`REAPER_MARGIN_SECONDS`]. It is the execution's error, its in-flight
/// iteration's error and the reason of its `ExecutionFailed` event.
pub fn reaper_outlived_failure_reason(elapsed_seconds: i64, bound_seconds: u64) -> String {
    format!(
        "{REAPER_OUTLIVED_FAILURE_PREFIX}: it was still running {elapsed_seconds} s after it started, \
         past its execution bound of {bound_seconds} s and the reaper's margin of \
         {REAPER_MARGIN_SECONDS} s, and no supervisor had ended it"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Iteration {
    pub number: u8,
    pub status: IterationStatus,
    pub action: String,
    pub output: Option<String>,
    pub validation_results: Option<ValidationResults>,
    pub error: Option<IterationError>,
    pub code_changes: Option<CodeDiff>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub llm_interactions: Vec<LlmInteraction>,
    #[serde(default)]
    pub trajectory: Option<Vec<TrajectoryStep>>,
    /// Tool names that were blocked by policy during this iteration.
    #[serde(default)]
    pub policy_violations: Vec<String>,
    /// The declared outputs this iteration left in the execution's volume,
    /// as the supervisor read them before the iteration counted as completed
    /// (AEGIS ADR-005, Update of 2026-10-06, O3). Stored in the `iterations`
    /// column; an iteration stored before it carries none. Stored with each
    /// file's volume and path in it (AEGIS ADR-005 I8, P1). Also the files its
    /// answer named that no output declared, found in the volume, marked
    /// undeclared (O8a).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "stored_produced_files"
    )]
    pub produced_files: Vec<ProducedFile>,
}

/// The stored form of one produced file: its answer form and where the
/// supervisor read it (AEGIS ADR-005 I8, P1). Only an iteration's stored
/// `produced_files` is written this way; every answer serialises
/// [`ProducedFile`] itself, `{path, size_bytes, content_type}`.
#[derive(Serialize)]
struct StoredProducedFile<'a> {
    path: &'a str,
    size_bytes: u64,
    content_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_id: Option<crate::domain::shared_kernel::VolumeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_in_volume: Option<&'a str>,
    #[serde(skip_serializing_if = "is_declared")]
    declared: bool,
}

fn stored_produced_files<S: serde::Serializer>(
    files: &[ProducedFile],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(files.iter().map(|file| StoredProducedFile {
        path: &file.path,
        size_bytes: file.size_bytes,
        content_type: &file.content_type,
        volume_id: file.volume_id,
        path_in_volume: file.path_in_volume.as_deref(),
        declared: file.declared,
    }))
}

/// A file a completed execution produced, read from its volume rather than
/// taken from the model's text (AEGIS ADR-005, Update of 2026-10-06, O3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducedFile {
    /// The path the output was declared at, inside the container.
    pub path: String,
    /// Its size in bytes when it was read.
    pub size_bytes: u64,
    /// Its content type, as the execution file route answers it.
    pub content_type: String,
    /// The volume the supervisor read it in (AEGIS ADR-005 I8, P1). Kept in
    /// the stored form only: every answer is `{path, size_bytes,
    /// content_type}`. `None` for a file recorded before I8.
    #[serde(default, skip_serializing)]
    pub volume_id: Option<crate::domain::shared_kernel::VolumeId>,
    /// Its path inside that volume, rooted at `/` (I8, P1). Stored form only.
    #[serde(default, skip_serializing)]
    pub path_in_volume: Option<String>,
    /// False for a file the execution's final output named under /workspace
    /// that no output declared: found in its volume when the iteration ended,
    /// never required. Written only when false.
    #[serde(default = "declared_by_default", skip_serializing_if = "is_declared")]
    pub declared: bool,
}

/// A produced file recorded without the `declared` key was declared: every
/// record before undeclared files were read.
fn declared_by_default() -> bool {
    true
}

fn is_declared(declared: &bool) -> bool {
    *declared
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub tool_name: String,
    pub arguments_json: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmInteraction {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: DateTime<Utc>,
}

use crate::domain::validation::ValidationResults;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IterationStatus {
    Running,
    Success,
    Failed,
    Refining,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationError {
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDiff {
    pub file_path: String,
    pub diff: String,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("Max iterations reached")]
    MaxIterationsReached,
    #[error("Execution is not running")]
    NotRunning,
    #[error("Iteration {0} not found")]
    IterationNotFound(u8),
    #[error("Maximum recursive execution depth exceeded: {0}")]
    MaxDepthExceeded(String),
    #[error("Agent manifest is missing spec.task")]
    MissingPromptTemplate,
    #[error("Failed to render prompt template: {0}")]
    PromptRenderFailed(String),
    #[error("Failed to extract user input from execution input: {0}")]
    InvalidExecutionInput(String),
    /// AEGIS ADR-005 O6: an agent that O4 or O5 refuses today is refused at
    /// start, before any container, with O4's or O5's sentence.
    #[error("Execution refused: {0}")]
    Refused(String),
    #[error(
        "Cross-tenant spawn forbidden: parent tenant '{parent_tenant}' cannot spawn child in tenant '{child_tenant}'"
    )]
    CrossTenantSpawnForbidden {
        parent_tenant: String,
        child_tenant: String,
    },
    #[error(
        "Cross-tenant access forbidden: caller tenant '{caller_tenant}' cannot execute agent owned by tenant '{requested_agent_tenant}'"
    )]
    CrossTenantAccessForbidden {
        requested_agent_tenant: String,
        caller_tenant: String,
    },
}

impl Execution {
    pub fn new(
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        security_context_name: String,
    ) -> Self {
        Self::new_with_id(
            ExecutionId::new(),
            agent_id,
            input,
            max_iterations,
            security_context_name,
        )
    }

    /// Create an execution with a pre-assigned ID (used for cluster forwarding).
    /// The execution_id is imported from the originating node to preserve tracing correlation.
    pub fn new_with_id(
        id: ExecutionId,
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        security_context_name: String,
    ) -> Self {
        Self {
            id,
            agent_id,
            tenant_id: TenantId::default(),
            status: ExecutionStatus::Pending,
            iterations: Vec::new(),
            max_iterations,
            input,
            started_at: Utc::now(),
            ended_at: None,
            error: None,
            container_uid: 1000,
            container_gid: 1000,
            hierarchy: ExecutionHierarchy::root(id),
            security_context_name,
            initiating_user_sub: None,
            timeout_seconds: None,
        }
    }

    /// Create a child execution (for nested agent calls like judges)
    pub fn new_child(
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        parent: &Execution,
    ) -> Result<Self, ExecutionError> {
        let child_id = ExecutionId::new();
        let hierarchy = ExecutionHierarchy::child(&parent.hierarchy, child_id)
            .map_err(ExecutionError::MaxDepthExceeded)?;

        Ok(Self {
            id: child_id,
            agent_id,
            tenant_id: parent.tenant_id.clone(),
            status: ExecutionStatus::Pending,
            iterations: Vec::new(),
            max_iterations,
            input,
            started_at: Utc::now(),
            ended_at: None,
            error: None,
            container_uid: 1000,
            container_gid: 1000,
            hierarchy,
            security_context_name: parent.security_context_name.clone(),
            initiating_user_sub: parent.initiating_user_sub.clone(),
            timeout_seconds: None,
        })
    }

    /// Check if this execution can spawn a child agent (for recursive calls)
    pub fn can_spawn_child(&self) -> bool {
        self.hierarchy.can_spawn_child()
    }

    /// Get execution depth (0 = root, 1 = child, 2 = grandchild, etc.)
    pub fn depth(&self) -> u8 {
        self.hierarchy.depth
    }

    /// Get parent execution ID if this is a child execution
    pub fn parent_id(&self) -> Option<ExecutionId> {
        self.hierarchy.parent_id()
    }

    pub fn start(&mut self) {
        self.status = ExecutionStatus::Running;
    }

    pub fn add_llm_interaction(
        &mut self,
        iteration_number: u8,
        interaction: LlmInteraction,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.llm_interactions.push(interaction);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    pub fn iterations(&self) -> &[Iteration] {
        &self.iterations
    }

    pub fn start_iteration(&mut self, action: String) -> Result<&mut Iteration, ExecutionError> {
        if self.iterations.len() as u8 >= self.max_iterations {
            return Err(ExecutionError::MaxIterationsReached);
        }

        let iteration = Iteration {
            number: (self.iterations.len() + 1) as u8,
            status: IterationStatus::Running,
            action,
            output: None,
            validation_results: None,
            error: None,
            code_changes: None,
            started_at: Utc::now(),
            ended_at: None,
            llm_interactions: Vec::new(),
            trajectory: None,
            policy_violations: Vec::new(),
            produced_files: Vec::new(),
        };

        self.iterations.push(iteration);
        Ok(self.iterations.last_mut().unwrap())
    }

    pub fn complete_iteration(&mut self, output: String) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.status = IterationStatus::Success;
            iter.output = Some(output);
            iter.ended_at = Some(Utc::now());
        }
    }

    pub fn store_validation_results(
        &mut self,
        iteration_number: u8,
        results: ValidationResults,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.validation_results = Some(results);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    /// Append a policy-blocked tool name to the current iteration.
    pub fn add_policy_violation(&mut self, tool_name: String) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.policy_violations.push(tool_name);
        }
    }

    /// Record on iteration `iteration_number` the refinement the next try is
    /// given (its `code_changes`): the sentence the model reads before that
    /// try, which `RefinementApplied` and the execution's narrative carry.
    pub fn store_refinement(
        &mut self,
        iteration_number: u8,
        refinement: CodeDiff,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.code_changes = Some(refinement);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    pub fn store_iteration_trajectory(
        &mut self,
        iteration_number: u8,
        trajectory: Vec<TrajectoryStep>,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.trajectory = Some(trajectory);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    /// Record what the current iteration answered without ending it: an
    /// iteration failed by its declared outputs keeps the model's text
    /// (AEGIS ADR-005, Update of 2026-10-06, O2).
    pub fn record_iteration_output(&mut self, output: String) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.output = Some(output);
        }
    }

    /// Record the declared outputs an iteration left in the volume (O3).
    pub fn store_produced_files(
        &mut self,
        iteration_number: u8,
        produced_files: Vec<ProducedFile>,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.produced_files = produced_files;
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    /// The files this execution produced: its last iteration's declared
    /// outputs once the execution has completed, and none before (AEGIS
    /// ADR-005, Update of 2026-10-06, O3). This record, not the model's text,
    /// is what "made a file" means downstream.
    pub fn produced_files(&self) -> &[ProducedFile] {
        if self.status != ExecutionStatus::Completed {
            return &[];
        }
        self.iterations
            .last()
            .map(|iteration| iteration.produced_files.as_slice())
            .unwrap_or(&[])
    }

    pub fn fail_iteration(&mut self, error: IterationError) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.status = IterationStatus::Failed;
            iter.error = Some(error);
            iter.ended_at = Some(Utc::now());
        }
    }

    pub fn complete(&mut self) {
        self.status = ExecutionStatus::Completed;
        self.ended_at = Some(Utc::now());
    }

    pub fn fail(&mut self, reason: String) {
        self.status = ExecutionStatus::Failed;
        self.error = Some(reason);
        self.ended_at = Some(Utc::now());
    }

    /// End an execution that the orchestrator process supervising it no
    /// longer exists to finish: the process ended (a deploy, a crash, a
    /// restart) while the execution was pending or running, and nothing in
    /// the new process runs it.
    ///
    /// The iteration in flight, if any, fails with
    /// [`ORCHESTRATOR_RESTART_FAILURE_REASON`], and the execution fails with
    /// the same reason. Returns the number of the iteration it failed, or
    /// `None` when there was none in flight. An execution that has already
    /// ended is left exactly as it is and `None` is returned.
    pub fn fail_cut_by_restart(&mut self) -> Option<u8> {
        if self.is_completed() {
            return None;
        }
        self.fail_with_iteration_in_flight(ORCHESTRATOR_RESTART_FAILURE_REASON.to_string())
    }

    /// Whether the container reaper may end this execution at `now`: it is
    /// still running, its bound is recorded, and `now` is at least its bound
    /// plus [`REAPER_MARGIN_SECONDS`] after it started. An execution inside
    /// that time, one already ended, or one whose bound is not recorded is
    /// never outlived.
    pub fn outlived_bound(&self, now: DateTime<Utc>) -> bool {
        let Some(bound) = self.timeout_seconds else {
            return false;
        };
        if self.status != ExecutionStatus::Running {
            return false;
        }
        let allowed = bound.saturating_add(REAPER_MARGIN_SECONDS);
        let allowed = chrono::Duration::seconds(i64::try_from(allowed).unwrap_or(i64::MAX));
        match self.started_at.checked_add_signed(allowed) {
            Some(deadline) => now >= deadline,
            None => false,
        }
    }

    /// End, as failed, an execution the container reaper found still running
    /// past its bound and [`REAPER_MARGIN_SECONDS`] ([`Self::outlived_bound`]),
    /// the way [`Self::fail_cut_by_restart`] ends one: the iteration in
    /// flight, if any, fails with [`reaper_outlived_failure_reason`], and the
    /// execution fails with the same reason. Returns `Some(failed iteration
    /// number or None)` when it ended the execution, and `None` (leaving it
    /// exactly as it is) when the execution has not outlived its bound at
    /// `now`.
    pub fn fail_outlived_bound(&mut self, now: DateTime<Utc>) -> Option<Option<u8>> {
        if !self.outlived_bound(now) {
            return None;
        }
        let bound = self.timeout_seconds.unwrap_or_default();
        let elapsed = (now - self.started_at).num_seconds();
        Some(self.fail_with_iteration_in_flight(reaper_outlived_failure_reason(elapsed, bound)))
    }

    /// Fail the iteration in flight (running or refining), if any, and the
    /// execution, with one reason; returns the number of the iteration failed.
    fn fail_with_iteration_in_flight(&mut self, reason: String) -> Option<u8> {
        let failed_iteration = match self.iterations.last() {
            Some(iteration)
                if matches!(
                    iteration.status,
                    IterationStatus::Running | IterationStatus::Refining
                ) =>
            {
                let number = iteration.number;
                self.fail_iteration(IterationError {
                    message: reason.clone(),
                    details: None,
                });
                Some(number)
            }
            _ => None,
        };
        self.fail(reason);
        failed_iteration
    }

    /// Check if execution is completed (success, failure, or cancellation)
    pub fn is_completed(&self) -> bool {
        matches!(
            self.status,
            ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
        )
    }

    /// Get the current (most recent) iteration
    pub fn current_iteration(&self) -> Option<&Iteration> {
        self.iterations.last()
    }

    /// Get the total number of attempts (iterations)
    pub fn total_attempts(&self) -> u8 {
        self.iterations.len() as u8
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInfo {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    #[serde(default)]
    pub tenant_id: TenantId,
    pub status: ExecutionStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl From<Execution> for ExecutionInfo {
    fn from(exec: Execution) -> Self {
        Self {
            id: exec.id,
            agent_id: exec.agent_id,
            tenant_id: exec.tenant_id,
            status: exec.status,
            started_at: exec.started_at,
            ended_at: exec.ended_at,
            error: exec.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;

    /// AEGIS ADR-132 Update (13) S11b: a list is a set in the person's
    /// order, a bare id a set of one, `null` none, a server not named
    /// nothing; every value that cannot be read is none, never nothing.
    #[test]
    fn contexts_read_a_list_as_a_set_and_an_unreadable_value_as_none() {
        use crate::domain::credential::CredentialBindingId;
        let a = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
        let b = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
        let id = |s: &str| CredentialBindingId(uuid::Uuid::parse_str(s).unwrap());
        let contexts = ExecutionContexts::from_value(Some(&serde_json::json!({
            "list": [b, a],
            "bare": a,
            "none": null,
            "empty": [],
            "repeated": [a, a],
            "bad-id": "not-a-uuid",
            "bad-item": [a, 7],
            "number": 7
        })));
        let mut complaints = Vec::new();
        for (server, expected) in [
            ("list", ServerChoice::Bindings(vec![id(b), id(a)])),
            ("bare", ServerChoice::Bindings(vec![id(a)])),
            ("none", ServerChoice::None),
            ("empty", ServerChoice::None),
            ("repeated", ServerChoice::None),
            ("bad-id", ServerChoice::None),
            ("bad-item", ServerChoice::None),
            ("number", ServerChoice::None),
            ("absent", ServerChoice::NotGiven),
        ] {
            let read = contexts.server(server);
            if read != expected {
                complaints.push(format!("{server}: read {read:?}, expected {expected:?}"));
            }
        }
        for (server, filled) in [
            ("list", true),
            ("bare", true),
            ("none", false),
            ("empty", false),
        ] {
            if contexts.is_filled(server) != filled {
                complaints.push(format!("{server}: is_filled is not {filled}"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    fn make_input(intent: &str) -> ExecutionInput {
        ExecutionInput {
            intent: Some(intent.to_string()),
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        }
    }

    // ── ExecutionId ───────────────────────────────────────────────────────────

    #[test]
    fn test_execution_id_new_unique() {
        let a = ExecutionId::new();
        let b = ExecutionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_execution_id_display() {
        let id = ExecutionId::new();
        let s = format!("{id}");
        assert_eq!(s, id.0.to_string());
    }

    #[test]
    fn test_execution_id_default() {
        let a = ExecutionId::default();
        let b = ExecutionId::default();
        assert_ne!(a, b);
    }

    // ── Execution state machine ───────────────────────────────────────────────

    #[test]
    fn test_new_execution_is_pending() {
        let exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        assert_eq!(exec.status, ExecutionStatus::Pending);
        assert_eq!(exec.depth(), 0);
        assert!(exec.parent_id().is_none());
        assert!(exec.can_spawn_child());
        assert_eq!(exec.container_uid, 1000);
        assert_eq!(exec.container_gid, 1000);
    }

    #[test]
    fn test_execution_start_changes_status() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        assert_eq!(exec.status, ExecutionStatus::Running);
    }

    #[test]
    fn test_execution_complete() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.complete();
        assert_eq!(exec.status, ExecutionStatus::Completed);
        assert!(exec.ended_at.is_some());
    }

    #[test]
    fn test_execution_fail() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.fail("something broke".to_string());
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(exec.error.as_deref(), Some("something broke"));
        assert!(exec.ended_at.is_some());
    }

    // ── Iteration management ──────────────────────────────────────────────────

    #[test]
    fn test_start_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        let iter = exec.start_iteration("generate".to_string()).unwrap();
        assert_eq!(iter.number, 1);
        assert_eq!(iter.status, IterationStatus::Running);
        assert_eq!(iter.action, "generate");
    }

    #[test]
    fn test_complete_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec.complete_iteration("result".to_string());
        let iter = &exec.iterations()[0];
        assert_eq!(iter.status, IterationStatus::Success);
        assert_eq!(iter.output.as_deref(), Some("result"));
    }

    #[test]
    fn test_fail_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec.fail_iteration(IterationError {
            message: "compile error".to_string(),
            details: Some("syntax".to_string()),
        });
        let iter = &exec.iterations()[0];
        assert_eq!(iter.status, IterationStatus::Failed);
        assert!(iter.error.is_some());
    }

    #[test]
    fn test_max_iterations_enforced() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            2,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("iter1".to_string()).unwrap();
        exec.complete_iteration("out1".to_string());
        exec.start_iteration("iter2".to_string()).unwrap();
        exec.complete_iteration("out2".to_string());
        let err = exec.start_iteration("iter3".to_string()).unwrap_err();
        assert!(matches!(err, ExecutionError::MaxIterationsReached));
    }

    #[test]
    fn test_store_validation_results() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        let results = ValidationResults {
            system: None,
            output: None,
            semantic: None,
            gradient: None,
            consensus: None,
        };
        exec.store_validation_results(1, results).unwrap();
        assert!(exec.iterations()[0].validation_results.is_some());
    }

    #[test]
    fn test_store_validation_results_wrong_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        let results = ValidationResults {
            system: None,
            output: None,
            semantic: None,
            gradient: None,
            consensus: None,
        };
        let err = exec.store_validation_results(99, results).unwrap_err();
        assert!(matches!(err, ExecutionError::IterationNotFound(99)));
    }

    #[test]
    fn test_add_llm_interaction() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        let interaction = LlmInteraction {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            prompt: "write hello world".to_string(),
            response: "print('hello')".to_string(),
            timestamp: chrono::Utc::now(),
        };
        exec.add_llm_interaction(1, interaction).unwrap();
        assert_eq!(exec.iterations()[0].llm_interactions.len(), 1);
    }

    #[test]
    fn test_add_llm_interaction_wrong_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        let interaction = LlmInteraction {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            prompt: "prompt".to_string(),
            response: "response".to_string(),
            timestamp: chrono::Utc::now(),
        };
        let err = exec.add_llm_interaction(99, interaction).unwrap_err();
        assert!(matches!(err, ExecutionError::IterationNotFound(99)));
    }

    // ── ExecutionHierarchy ────────────────────────────────────────────────────

    #[test]
    fn test_hierarchy_root() {
        let id = ExecutionId::new();
        let h = ExecutionHierarchy::root(id);
        assert_eq!(h.depth, 0);
        assert!(h.parent_id().is_none());
        assert!(h.can_spawn_child());
        assert_eq!(h.root_id(), id);
    }

    #[test]
    fn test_hierarchy_child() {
        let root_id = ExecutionId::new();
        let root_h = ExecutionHierarchy::root(root_id);

        let child_id = ExecutionId::new();
        let child_h = ExecutionHierarchy::child(&root_h, child_id).unwrap();

        assert_eq!(child_h.depth, 1);
        assert_eq!(child_h.parent_id(), Some(root_id));
        assert_eq!(child_h.root_id(), root_id);
    }

    #[test]
    fn test_hierarchy_depth_limit() {
        let root_id = ExecutionId::new();
        let mut h = ExecutionHierarchy::root(root_id);

        // Go up to MAX_RECURSIVE_DEPTH
        for _ in 0..MAX_RECURSIVE_DEPTH {
            let child_id = ExecutionId::new();
            h = ExecutionHierarchy::child(&h, child_id).unwrap();
        }
        assert_eq!(h.depth, MAX_RECURSIVE_DEPTH);
        assert!(!h.can_spawn_child());

        // One more should fail
        let err = ExecutionHierarchy::child(&h, ExecutionId::new());
        assert!(err.is_err());
    }

    // ── ExecutionInfo ─────────────────────────────────────────────────────────

    #[test]
    fn test_execution_info_from_execution() {
        let agent_id = AgentId::new();
        let mut exec = Execution::new(
            agent_id,
            make_input("task"),
            3,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.complete();

        let info = ExecutionInfo::from(exec.clone());
        assert_eq!(info.id, exec.id);
        assert_eq!(info.agent_id, agent_id);
        assert_eq!(info.status, ExecutionStatus::Completed);
        assert!(info.ended_at.is_some());
        assert!(info.error.is_none());
    }

    // ── The container reaper's ending (ADR-040, Update of 2026-10-04) ───────

    /// A running execution with a 60 s bound and an iteration in flight,
    /// started `ago` seconds before `now`.
    fn running_with_bound(now: DateTime<Utc>, ago: i64, bound: Option<u64>) -> Execution {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            3,
            "aegis-system-operator".to_string(),
        );
        exec.started_at = now - chrono::Duration::seconds(ago);
        exec.timeout_seconds = bound;
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec
    }

    #[test]
    fn an_execution_past_its_bound_and_the_margin_is_outlived_and_ended_by_the_reaper() {
        let now = Utc::now();
        let mut exec = running_with_bound(now, 60 + 600, Some(60));
        assert!(exec.outlived_bound(now));

        let ended = exec.fail_outlived_bound(now);

        assert_eq!(ended, Some(Some(1)), "ended, failing iteration 1");
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert!(exec.ended_at.is_some());
        let error = exec
            .error
            .clone()
            .expect("the execution carries the reason");
        assert!(error.starts_with(REAPER_OUTLIVED_FAILURE_PREFIX), "{error}");
        assert!(error.contains("execution bound of 60 s"), "{error}");
        assert!(error.contains("margin of 600 s"), "{error}");
        let iteration = exec.iterations().last().unwrap();
        assert_eq!(iteration.status, IterationStatus::Failed);
        assert_eq!(iteration.error.as_ref().unwrap().message, error);
    }

    #[test]
    fn an_execution_inside_its_bound_and_the_margin_is_never_ended_by_the_reaper() {
        let now = Utc::now();
        let mut exec = running_with_bound(now, 60 + 600 - 1, Some(60));
        assert!(!exec.outlived_bound(now));
        assert_eq!(exec.fail_outlived_bound(now), None);
        assert_eq!(exec.status, ExecutionStatus::Running);
        assert!(exec.error.is_none());
        assert_eq!(
            exec.iterations().last().unwrap().status,
            IterationStatus::Running
        );
    }

    #[test]
    fn an_execution_without_a_recorded_bound_is_never_ended_by_the_reaper() {
        let now = Utc::now();
        let mut exec = running_with_bound(now, 100_000, None);
        assert!(!exec.outlived_bound(now));
        assert_eq!(exec.fail_outlived_bound(now), None);
        assert_eq!(exec.status, ExecutionStatus::Running);
    }

    #[test]
    fn an_execution_already_ended_is_left_as_it_is_by_the_reaper() {
        let now = Utc::now();
        let mut exec = running_with_bound(now, 100_000, Some(60));
        exec.complete();
        let before = exec.ended_at;
        assert_eq!(exec.fail_outlived_bound(now), None);
        assert_eq!(exec.status, ExecutionStatus::Completed);
        assert_eq!(exec.ended_at, before);
        assert!(exec.error.is_none());
    }

    // ── Produced files (AEGIS ADR-005, Update of 2026-10-06, O3) ──────────────

    /// An iteration stored before `produced_files` existed reads with none,
    /// and an execution answers its last iteration's produced files once it
    /// has completed, and none before. Every clause is reported.
    #[test]
    fn produced_files_read_from_old_rows_as_empty_and_answer_only_once_completed() {
        let mut complaints: Vec<String> = Vec::new();
        let old_row = serde_json::json!({
            "number": 1,
            "status": "Success",
            "action": "act",
            "output": "/workspace/x.pdf",
            "validation_results": null,
            "error": null,
            "code_changes": null,
            "started_at": "2026-10-05T17:11:23Z",
            "ended_at": "2026-10-05T17:12:24Z"
        });
        match serde_json::from_value::<Iteration>(old_row) {
            Ok(iteration) if iteration.produced_files.is_empty() => {}
            other => complaints.push(format!(
                "an iteration stored without produced_files did not read as empty: {other:?}"
            )),
        }

        let mut exec = Execution::new(
            AgentId::new(),
            make_input("make a pdf"),
            3,
            "ctx".to_string(),
        );
        exec.start();
        exec.start_iteration("act".to_string()).unwrap();
        exec.complete_iteration("/workspace/x.pdf".to_string());
        let produced = vec![ProducedFile {
            path: "/workspace/x.pdf".to_string(),
            size_bytes: 2048,
            content_type: "application/pdf".to_string(),
            volume_id: Some(crate::domain::shared_kernel::VolumeId::new()),
            path_in_volume: Some("/x.pdf".to_string()),
            declared: true,
        }];
        if let Err(e) = exec.store_produced_files(1, produced.clone()) {
            complaints.push(format!("store_produced_files refused iteration 1: {e}"));
        }
        if !exec.produced_files().is_empty() {
            complaints.push("a running execution answered produced files".to_string());
        }
        exec.complete();
        if exec.produced_files() != produced.as_slice() {
            complaints.push(format!(
                "a completed execution answered {:?}, not its last iteration's files",
                exec.produced_files()
            ));
        }
        let round_trip: Execution =
            serde_json::from_value(serde_json::to_value(&exec).unwrap()).unwrap();
        if round_trip.produced_files() != produced.as_slice() {
            complaints.push("produced_files did not survive the stored form".to_string());
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// AEGIS ADR-005 I8 (P1): the stored form of an iteration keeps where
    /// each produced file was read (five fields); every answer built from
    /// the record keeps `{path, size_bytes, content_type}`; and a record
    /// stored before I8, without the two fields, still reads whole. Every
    /// clause is reported.
    #[test]
    fn a_produced_file_keeps_its_location_only_in_the_stored_form() {
        let mut complaints: Vec<String> = Vec::new();
        let volume_id = crate::domain::shared_kernel::VolumeId::new();
        let file = ProducedFile {
            path: "/workspace/x.pdf".to_string(),
            size_bytes: 21,
            content_type: "application/pdf".to_string(),
            volume_id: Some(volume_id),
            path_in_volume: Some("/x.pdf".to_string()),
            declared: true,
        };

        let answer = serde_json::to_value(std::slice::from_ref(&file)).unwrap();
        let mut answer_keys: Vec<String> = answer[0]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        answer_keys.sort();
        println!("answer form: {answer}");
        if answer_keys != ["content_type", "path", "size_bytes"] {
            complaints.push(format!(
                "the answer form is not the three keys: {answer_keys:?}"
            ));
        }

        let mut exec = Execution::new(
            AgentId::new(),
            make_input("make a pdf"),
            3,
            "ctx".to_string(),
        );
        exec.start();
        exec.start_iteration("act".to_string()).unwrap();
        exec.complete_iteration("/workspace/x.pdf".to_string());
        exec.store_produced_files(1, vec![file.clone()]).unwrap();
        exec.complete();
        let stored = serde_json::to_value(exec.iterations()).unwrap();
        let stored_file = &stored[0]["produced_files"][0];
        println!("stored form: {stored_file}");
        let mut stored_keys: Vec<String> = stored_file
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        stored_keys.sort();
        if stored_keys
            != [
                "content_type",
                "path",
                "path_in_volume",
                "size_bytes",
                "volume_id",
            ]
        {
            complaints.push(format!(
                "the stored form is not the five fields: {stored_keys:?}"
            ));
        }
        let read_back: Vec<Iteration> = serde_json::from_value(stored).unwrap();
        if read_back[0].produced_files != vec![file.clone()] {
            complaints.push(format!(
                "the stored form did not read back whole: {:?}",
                read_back[0].produced_files
            ));
        }

        let before_i8 = serde_json::json!(
            {"path": "/workspace/x.pdf", "size_bytes": 21, "content_type": "application/pdf"}
        );
        match serde_json::from_value::<ProducedFile>(before_i8) {
            Ok(old) if old.volume_id.is_none() && old.path_in_volume.is_none() => {
                println!("a record without the new fields reads as {old:?}");
            }
            other => complaints.push(format!(
                "a record serialised without the new fields did not read: {other:?}"
            )),
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T10: an undeclared produced file is written `declared: false` in the
    /// answer and the stored form, and reads back so; a declared one writes
    /// no `declared` key; a record stored without the key reads as declared.
    /// Every clause is reported.
    #[test]
    fn an_undeclared_file_is_written_declared_false_and_a_declared_one_omits_it() {
        let mut complaints: Vec<String> = Vec::new();
        let undeclared = ProducedFile {
            path: "/workspace/itinerary.md".to_string(),
            size_bytes: 19,
            content_type: "text/markdown".to_string(),
            volume_id: Some(crate::domain::shared_kernel::VolumeId::new()),
            path_in_volume: Some("/itinerary.md".to_string()),
            declared: false,
        };
        let declared = ProducedFile {
            declared: true,
            ..undeclared.clone()
        };

        let answer = serde_json::to_value([&undeclared, &declared]).unwrap();
        println!("answer form: {answer}");
        if answer[0].get("declared") != Some(&serde_json::Value::Bool(false)) {
            complaints.push(format!(
                "declared: false was not written in the answer form: {}",
                answer[0]
            ));
        }
        if answer[1].get("declared").is_some() {
            complaints.push(format!(
                "a declared file wrote a declared key: {}",
                answer[1]
            ));
        }

        let mut exec = Execution::new(
            AgentId::new(),
            make_input("plan the vans"),
            3,
            "ctx".to_string(),
        );
        exec.start();
        exec.start_iteration("act".to_string()).unwrap();
        exec.complete_iteration("/workspace/itinerary.md".to_string());
        exec.store_produced_files(1, vec![undeclared.clone(), declared.clone()])
            .unwrap();
        exec.complete();
        let stored = serde_json::to_value(exec.iterations()).unwrap();
        println!("stored form: {}", stored[0]["produced_files"]);
        if stored[0]["produced_files"][0].get("declared") != Some(&serde_json::Value::Bool(false)) {
            complaints.push(format!(
                "declared: false was not written in the stored form: {}",
                stored[0]["produced_files"][0]
            ));
        }
        if stored[0]["produced_files"][1].get("declared").is_some() {
            complaints.push(format!(
                "a declared file wrote a declared key in the stored form: {}",
                stored[0]["produced_files"][1]
            ));
        }
        let read_back: Vec<Iteration> = serde_json::from_value(stored).unwrap();
        if read_back[0].produced_files != vec![undeclared.clone(), declared.clone()] {
            complaints.push(format!(
                "the stored form did not read back with its declared marks: {:?}",
                read_back[0].produced_files
            ));
        }

        let old_row = serde_json::json!(
            {"path": "/workspace/x.pdf", "size_bytes": 21, "content_type": "application/pdf"}
        );
        match serde_json::from_value::<ProducedFile>(old_row) {
            Ok(old) if old.declared => {}
            other => complaints.push(format!(
                "a record stored without the declared key did not read as declared: {other:?}"
            )),
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
