// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool Router
//!
//! Infrastructure layer catalogue of the orchestrator's builtin tools and of
//! what the approval gate and the inner-loop judge know of every tool.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Lists the builtin tools with their schemas, and answers
//!   whether a tool skips the judge or waits at the approval gate. The
//!   orchestrator runs no MCP server process of its own: a tool it does not
//!   serve comes through the SEAL gateway (AEGIS ADR-132 G1, G4).
//!
//! # Related ADRs
//!
//! - ADR-033: Orchestrator-Mediated MCP Tool Routing
//! - ADR-035: Signed Envelope Attestation Layer (SEAL)
//! - ADR-038: Agent Iteration and Tool Gateway
//!
//! # Tool naming conventions
//!
//! Builtin tool names are defined canonically in `BUILTIN_TOOL_DEFINITIONS`.
//! For filesystem tools, the canonical create-directory name is `fs.create_dir`.

use crate::domain::node_config::{BuiltinDispatcherConfig, ToolCapabilityConfig};
use crate::domain::tool_approval::ApprovalContract;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// =============================================================================
// ToolRouter — Routes tool requests to the correct MCP server
// =============================================================================

/// The catalogue of builtin tools, and what the approval gate and the
/// inner-loop judge know of each tool by name.
pub struct ToolRouter {
    builtin_dispatchers: Vec<BuiltinDispatcherConfig>,
    /// `spec.tool_capabilities`: which tools the orchestrator does not serve
    /// itself carry `requires_approval` (AEGIS ADR-126 D1) and what each
    /// declares to the gate (its Update of 2026-10-04, clause 1), by pattern.
    tool_capabilities: Vec<ToolCapabilityConfig>,
}

/// Canonical structured definition of one builtin tool dispatcher.
///
/// Every tool name, description, and per-tool behavior flag (`skip_judge`,
/// `edge_executor`, `fleet_capable`) lives in a single row of
/// `BUILTIN_TOOL_DEFINITIONS`. Consumers derive their behavior from this
/// registry — there is no secondary list to keep in sync.
///
/// `skip_judge`: read-only / low-risk tools (and manifest deployment tools
/// whose YAML payload would be misread by the tool-call-policy-judge) bypass
/// the inner-loop semantic judge. Manifest deployment tools (aegis.agent.*
/// create/update/delete, aegis.workflow.* create/update) are included
/// because they accept a `manifest_yaml` payload that may textually contain
/// tool names such as `cmd.run` or `fs.*` as part of `spec.tools`. The judge
/// would hallucinate violations from that text; skipping it is correct
/// because these are registry-write operations with their own structural
/// validation via aegis.schema.validate before they reach this point.
///
/// `edge_executor`: ADR-117 system-tier tools whose canonical execution
/// target is the edge daemon (or fan-out across many daemons) rather than
/// the local builtin / MCP / SEAL chain. `list_tools` advertises
/// `executor = "edge"` for these.
///
/// `fleet_capable`: ADR-117 tools eligible for fleet (multi-target) fan
/// out via `aegis.edge.fleet.invoke`. Currently only that tool itself; the
/// list / cancel siblings are operator-tier coordination tools that do not
/// fan out further.
///
/// `requires_approval`: AEGIS ADR-126 D1 outbound tools: an agent's call
/// waits for its user's answer at the approval gate. A node configuration's
/// capability entry may gate further tools; it cannot clear this mark.
/// `mail.send` and `mail.reply` carry it (AEGIS ADR-125's Update of
/// 2026-10-07 (3) clause 12), as do `mail.delete` (its Update of
/// 2026-10-08 (4) clause 20), `mail.archive` (its Update of 2026-10-08
/// (5) clause 29) and `mail.forward` (its clause 35).
struct BuiltinToolDefinition {
    name: &'static str,
    description: &'static str,
    skip_judge: bool,
    edge_executor: bool,
    fleet_capable: bool,
    requires_approval: bool,
}

impl BuiltinToolDefinition {
    const fn new(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            skip_judge: false,
            edge_executor: false,
            fleet_capable: false,
            requires_approval: false,
        }
    }

    const fn skip_judge(mut self) -> Self {
        self.skip_judge = true;
        self
    }

    const fn edge_executor(mut self) -> Self {
        self.edge_executor = true;
        self
    }

    const fn fleet_capable(mut self) -> Self {
        self.fleet_capable = true;
        self
    }

    /// Mark an outbound tool for the approval gate (AEGIS ADR-126 D1).
    const fn requires_approval(mut self) -> Self {
        self.requires_approval = true;
        self
    }

    /// Look up a definition by name, if present in the registry.
    fn lookup(name: &str) -> Option<&'static BuiltinToolDefinition> {
        BUILTIN_TOOL_DEFINITIONS.iter().find(|d| d.name == name)
    }
}

/// The builtin tool a workflow's landing step is dispatched as (AEGIS
/// ADR-141 F8): gated by the person's approval as `aegis.git.push` is, and
/// listed to no agent.
pub const LAND_TOOL: &str = "aegis.git.land";

/// Canonical registry of all builtin tool dispatchers. Single source of
/// truth — the daemon startup and every other consumer derives its data
/// from this slice.
const BUILTIN_TOOL_DEFINITIONS: &[BuiltinToolDefinition] = &[
    BuiltinToolDefinition::new("cmd.run", "Executes a shell command inside the agent's ephemeral container environment. Use this to build, run, or analyze code locally."),
    BuiltinToolDefinition::new("fs.read", "Read the contents of a file at the given POSIX path from the mounted Workspace volume.").skip_judge(),
    BuiltinToolDefinition::new("fs.write", "Write content to a file at the given POSIX path in the Workspace volume. Automatically creates missing parent directories."),
    BuiltinToolDefinition::new("fs.list", "List the contents of a directory in the Workspace volume.").skip_judge(),
    BuiltinToolDefinition::new("fs.create_dir", "Creates a new directory along with any necessary parent directories."),
    BuiltinToolDefinition::new("fs.delete", "Deletes a file or directory."),
    BuiltinToolDefinition::new("fs.edit", "Performs an exact string replacement in a file."),
    BuiltinToolDefinition::new("fs.multi_edit", "Performs multiple sequential string replacements in a file."),
    BuiltinToolDefinition::new("fs.grep", "Recursively searches for a regex pattern within files in a given directory.").skip_judge(),
    BuiltinToolDefinition::new("fs.glob", "Recursively matches files against a glob pattern.").skip_judge(),
    BuiltinToolDefinition::new("web.search", "Performs an internet search query.").skip_judge(),
    BuiltinToolDefinition::new("web.fetch", "Fetches content from a URL, optionally converting HTML to Markdown. Returns at most 50,000 characters of the page per call: a longer page comes back cut, with truncated, total_chars, next_offset and a notice, and the offset argument reads on.").skip_judge(),
    BuiltinToolDefinition::new("mail.draft", "Saves a plain-text message as a draft in a connected mailbox's Drafts folder, optionally as a reply in a thread. Sends nothing."),
    BuiltinToolDefinition::new("mail.send", "Sends a plain-text message from a connected mailbox to the addresses in to and cc, with up to 10 of the person's own files attached (20 MiB together), then saves a copy in its Sent folder. Waits for the person's approval, which shows each attached file's name and size, before anything is sent; a file changed after the approval is not sent.").requires_approval(),
    BuiltinToolDefinition::new("mail.reply", "Replies in a thread of a connected mailbox: sends a plain-text message to the addresses in to and cc, threaded to the thread's newest message, with up to 10 of the person's own files attached (20 MiB together), then saves a copy in its Sent folder. Waits for the person's approval, which shows each attached file's name and size, before anything is sent; a file changed after the approval is not sent.").requires_approval(),
    BuiltinToolDefinition::new("mail.list", "Lists threads in one folder of a connected mailbox that match a query, newest first: the inbox by default, or by folder its Sent, Drafts, Trash, Archive or all its mail. Answers each thread's id, subject, participants, latest date, message count, unread count, flag and labels, and the folder read. Marks nothing as read.").skip_judge(),
    BuiltinToolDefinition::new("mail.read", "Reads one thread in one folder of a connected mailbox: the inbox by default, or by folder its Sent, Drafts, Trash, Archive or all its mail. Answers every message of the thread in that folder, oldest first, with its uid, headers, flags, labels, plain-text body and attachments (each by name, type, size and part number, which mail.attachment saves), and the folder read; a thread's id is the same in every folder. Marks nothing as read.").skip_judge(),
    BuiltinToolDefinition::new("mail.attachment", "Saves one attachment of a message in a connected mailbox to your files: the message by its uid in one folder (the inbox by default) and the attachment by its part number, both as mail.read answered them. Answers the saved file's volume, path, name, type, size and SHA-256, and the text of a short text attachment. Marks nothing as read and changes nothing in the mailbox.").skip_judge(),
    BuiltinToolDefinition::new("mail.label", "Adds or removes labels on every message of a thread in a connected mailbox's inbox, flags or unflags it, and marks it read or unread."),
    BuiltinToolDefinition::new("mail.delete", "Moves every message of a thread in a connected mailbox's inbox to its Trash folder; deletes nothing permanently. Waits for the person's approval before anything is moved.").requires_approval(),
    BuiltinToolDefinition::new("mail.archive", "Archives a thread of a connected mailbox: moves every message of the thread in its inbox to its Archive folder (on a server that keeps all mail in one folder, to that folder), so the thread leaves the inbox and stays in the mailbox; deletes nothing. Waits for the person's approval before anything is moved.").requires_approval(),
    BuiltinToolDefinition::new("mail.forward", "Forwards a thread of a connected mailbox, or one message of it, from one folder (the inbox by default): sends the messages as attachments, each whole as it arrived, after an optional plain-text note, to the addresses in to and cc, with up to 10 of the person's own files attached (20 MiB together), then saves a copy in its Sent folder. At most 20 messages, 20 MiB together. Waits for the person's approval, which shows the recipients, the subject, the note and each forwarded message's sender, subject and date, before anything is sent.").requires_approval(),
    BuiltinToolDefinition::new("calendar.calendars", "Lists the calendars of a connected calendar account: each calendar's id, name, description, colour where given, and whether the account may write to it. Changes nothing.").skip_judge(),
    BuiltinToolDefinition::new("calendar.list", "Lists the events of one calendar of a connected calendar account in a window of at most 92 days (by default now and the seven days on), by start: repeating events as their occurrences, each with its id, title, times, location, organiser, attendees and their answers, and status. Changes nothing.").skip_judge(),
    BuiltinToolDefinition::new("calendar.read", "Reads one event of a calendar of a connected calendar account: everything calendar.list answers, its description, its start and end as written with their time zone, and its etag. Changes nothing.").skip_judge(),
    BuiltinToolDefinition::new("calendar.create", "Creates an event on one calendar of a connected calendar account: its title, start and end (RFC 3339 times with an offset, written in UTC, or dates YYYY-MM-DD for an all-day event), and optionally a description, a location and up to 50 attendees, with the account as organiser. Attendees may be sent an invitation or an update by the calendar's server. Waits for the person's approval before anything is changed.").requires_approval(),
    BuiltinToolDefinition::new("calendar.update", "Changes an event of a calendar of a connected calendar account that the account organises: only the title, start, end, description, location or attendees given; attendees given replace the list. A repeating event cannot be changed. Attendees may be sent an invitation or an update by the calendar's server. Waits for the person's approval before anything is changed.").requires_approval(),
    BuiltinToolDefinition::new("calendar.delete", "Deletes an event of a calendar of a connected calendar account that the account organises. A repeating event cannot be deleted. Waits for the person's approval before anything is changed.").requires_approval(),
    BuiltinToolDefinition::new("calendar.respond", "Answers an invitation to an event of a calendar of a connected calendar account: accepted, declined or tentative, as the account's attendee answer. Waits for the person's approval before anything is changed.").requires_approval(),
    BuiltinToolDefinition::new("aegis.git.list", "Lists the caller's git repository bindings (redacted): each binding's id, repository URL, ref, state and label. A workflow run on a repository names a binding by its id in repositories.").skip_judge(),
    BuiltinToolDefinition::new("aegis.git.status", "Shows one of your run's repositories: its work branch, whether the tree is clean or changed, and the commit HEAD is on.").skip_judge(),
    BuiltinToolDefinition::new("aegis.git.diff", "Shows the changes in one of your run's repositories as a unified diff: unstaged by default, or what is staged.").skip_judge(),
    BuiltinToolDefinition::new("aegis.git.commit", "Stages every change in one of your run's repositories and commits it on the run's work branch."),
    BuiltinToolDefinition::new("aegis.git.push", "Pushes the run's work branch of one of your run's repositories to its origin, never with force. Only that branch is pushed."),
    BuiltinToolDefinition::new("aegis.git.land", "Lands a workflow run's work branch on its repository's branch as a fast-forward: pushes the work branch to its origin, then the branch, never with force. Answered only for a workflow's own landing step, after the person's approval.").requires_approval(),
    BuiltinToolDefinition::new("aegis.schema.get", "Returns the canonical JSON Schema for a manifest kind (agent or workflow).").skip_judge(),
    BuiltinToolDefinition::new("aegis.schema.validate", "Validates a manifest YAML string against its canonical JSON Schema.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.create", "Parses, validates, and deploys an Agent manifest to the registry.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.list", "Lists currently deployed agents and metadata.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.update", "Updates an existing Agent manifest in the registry.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.export", "Exports an Agent manifest by name.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.delete", "Removes a deployed agent from the registry by UUID.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.generate", "Generates an Agent manifest from a natural-language intent."),
    BuiltinToolDefinition::new("aegis.agent.logs", "Retrieve agent-level activity log snapshot.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.search", "Semantic search over deployed agents by natural-language query.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.create", "Performs strict deterministic and semantic workflow validation, then registers on pass.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.list", "Lists currently registered workflows and metadata.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.validate", "Validate a workflow manifest against the schema.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.update", "Updates an existing Workflow manifest in the registry.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.export", "Exports a Workflow manifest by name.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.delete", "Removes a registered workflow from the registry by workflow name (not UUID)."),
    BuiltinToolDefinition::new("aegis.workflow.run", "Executes a registered workflow by name with optional input parameters."),
    BuiltinToolDefinition::new("aegis.workflow.generate", "Generates a Workflow manifest from a natural-language objective."),
    BuiltinToolDefinition::new("aegis.workflow.logs", "Returns paginated workflow execution events.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.wait", "Polls a workflow execution until it reaches a terminal state and returns the result.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.cancel", "Cancel a running workflow execution.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.signal", "Send human input response to a paused workflow execution.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.remove", "Remove a workflow execution record.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.promote", "Promote a workflow from user scope to tenant scope."),
    BuiltinToolDefinition::new("aegis.workflow.demote", "Demote a workflow from tenant scope to user scope."),
    BuiltinToolDefinition::new("aegis.workflow.executions.list", "Lists workflow executions, optionally filtered.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.executions.get", "Returns details of a specific workflow execution.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.status", "Returns current status of a workflow execution.").skip_judge(),
    BuiltinToolDefinition::new("aegis.workflow.search", "Semantic search over registered workflows.").skip_judge(),
    BuiltinToolDefinition::new("aegis.task.execute", "Starts a new agent execution (task) by agent UUID or name."),
    BuiltinToolDefinition::new("aegis.task.status", "Returns the current status and output of an execution by UUID.").skip_judge(),
    BuiltinToolDefinition::new("aegis.task.list", "Lists recent executions, optionally filtered by agent.").skip_judge(),
    BuiltinToolDefinition::new("aegis.task.cancel", "Cancels an active agent execution by UUID."),
    BuiltinToolDefinition::new("aegis.task.remove", "Removes a completed or failed execution record by UUID."),
    BuiltinToolDefinition::new("aegis.task.logs", "Returns paginated execution events for a task by UUID.").skip_judge(),
    BuiltinToolDefinition::new("aegis.task.wait", "Polls an execution until it reaches a terminal state and returns the result.").skip_judge(),
    BuiltinToolDefinition::new("aegis.agent.wait", "Alias for aegis.task.wait. Blocks until an agent execution completes.").skip_judge(),
    BuiltinToolDefinition::new("aegis.schedule.create", "Creates a schedule that starts an agent or a workflow as you, once at a time (at) or on a recurrence (cron, timezone, jitter_seconds). Anything a run would send waits for your approval."),
    BuiltinToolDefinition::new("aegis.schedule.list", "Lists your schedules with their state, next run and last run.").skip_judge(),
    BuiltinToolDefinition::new("aegis.schedule.get", "Returns one of your schedules by schedule_id, with its state, next run and last run.").skip_judge(),
    BuiltinToolDefinition::new("aegis.schedule.update", "Updates a schedule by schedule_id: only the fields given, and its time (at or recurrence) replaced when one is given."),
    BuiltinToolDefinition::new("aegis.schedule.pause", "Pauses a schedule by schedule_id: it starts nothing until it is resumed."),
    BuiltinToolDefinition::new("aegis.schedule.resume", "Resumes a paused schedule by schedule_id."),
    BuiltinToolDefinition::new("aegis.schedule.run_now", "Starts one run of a schedule now by schedule_id, whether it is active or paused, as its timed runs start; refused while its last run is still running."),
    BuiltinToolDefinition::new("aegis.schedule.delete", "Deletes a schedule by schedule_id. The runs it started are kept."),
    BuiltinToolDefinition::new("aegis.schedule.runs", "Lists a schedule's runs by schedule_id, newest first: each time it fired, its outcome and the execution it started.").skip_judge(),
    BuiltinToolDefinition::new("aegis.execute.intent", "Starts the intent-to-execution pipeline: discovers or generates an agent, writes code, executes in a container, and returns the formatted result."),
    BuiltinToolDefinition::new("aegis.execute.status", "Returns the current status of an intent-to-execution pipeline run.").skip_judge(),
    BuiltinToolDefinition::new("aegis.execute.wait", "Alias for aegis.workflow.wait. Blocks until pipeline execution completes.").skip_judge(),
    BuiltinToolDefinition::new("aegis.tools.list", "List all MCP tools available to your security context with pagination and optional source/category filtering.").skip_judge(),
    BuiltinToolDefinition::new("aegis.tools.search", "Search for MCP tools by keyword, name pattern, source, category, or tags. Returns tools matching your query within your security context.").skip_judge(),
    BuiltinToolDefinition::new("aegis.system.info", "Returns system version, status, and capabilities.").skip_judge(),
    BuiltinToolDefinition::new("aegis.system.config", "Returns the current node configuration.").skip_judge(),
    BuiltinToolDefinition::new("aegis.runtime.list", "List all supported standard runtime environments (language/version pairs). Call this before creating an agent manifest to ensure the declared runtime is valid.").skip_judge(),
    BuiltinToolDefinition::new("aegis.execution.file", "Read a file from a completed execution's workspace volume. Use this to retrieve output files after an agent or task execution finishes.").skip_judge(),
    BuiltinToolDefinition::new("aegis.attachment.read", "Read the contents of a file attached to a chat message. Returns the file content (UTF-8 text or base64-encoded bytes for binary), MIME type, size, and SHA-256 digest. Tenant-scoped and read-only.").skip_judge(),
    BuiltinToolDefinition::new("aegis.edge.fleet.list", "Resolve an edge fleet target (selector / group / @node / all) and return the matched node ids without dispatching. Operator-tier.").skip_judge().edge_executor(),
    BuiltinToolDefinition::new("aegis.edge.fleet.invoke", "Dispatch a tool to a fleet of edge daemons (selector / group / @node / all). Returns the fleet_command_id; per-node progress streams via /v1/edge/fleet/invoke. Operator-tier, fleet-capable.").skip_judge().edge_executor().fleet_capable(),
    BuiltinToolDefinition::new("aegis.edge.fleet.cancel", "Cancel an in-flight fleet operation by fleet_command_id. Operator-tier.").skip_judge().edge_executor(),
    BuiltinToolDefinition::new("aegis.approval.status", "Returns the status of a tool call that waited for its user's approval (approval_pending, approved_once, approved_always, denied, expired, auto_allowed, auto_denied) and, once it ran, its result. Only the call's own user can read it.").skip_judge(),
    BuiltinToolDefinition::new("aegis.goal.create", "Hold the user's request as a goal that the executions started for it are bound to. Called by the turn, never by a model.").skip_judge(),
    BuiltinToolDefinition::new("aegis.goal.evaluate", "Judge the goal after an execution turn with the built-in judge agent goal-judge, and answer whether a round is granted. Blocks at most 45 s, answering judging until the verdict is in. Called by the turn, never by a model.").skip_judge(),
    BuiltinToolDefinition::new("aegis.goal.status", "The goal, its bound executions with their states, and every verdict. Read-only.").skip_judge(),
    BuiltinToolDefinition::new("aegis.goal.cancel", "Stop the person's goal: no further round, every execution still running for it cancelled, and the goal closed cancelled with the reason given. Call it when the person asks to stop.").skip_judge(),
    BuiltinToolDefinition::new("aegis.document.render", "Render a document from Markdown or plain text into a file the person downloads: pdf, docx, html or md. Answers the file's path, size_bytes and format, and the execution_id that holds it; give that id to aegis.execution.file to read the file.").skip_judge(),
];

impl ToolRouter {
    /// Returns the canonical list of all builtin tool dispatchers.
    /// This is the single source of truth — the daemon startup and
    /// all other consumers derive their dispatcher list from here.
    pub fn builtin_dispatchers() -> Vec<BuiltinDispatcherConfig> {
        BUILTIN_TOOL_DEFINITIONS
            .iter()
            .map(|def| BuiltinDispatcherConfig {
                name: def.name.to_string(),
                description: def.description.to_string(),
                enabled: true,
                capabilities: vec![crate::domain::node_config::CapabilityConfig {
                    name: def.name.to_string(),
                    skip_judge: def.skip_judge,
                    requires_approval: def.requires_approval,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            })
            .collect()
    }

    /// Returns `true` for any builtin workflow / execute tool currently
    /// present in the canonical registry. Derived from
    /// `BUILTIN_TOOL_DEFINITIONS`, restricted to the `aegis.workflow.` and
    /// `aegis.execute.` prefixes so the gate semantically matches the
    /// pre-consolidation behavior: only registered workflow / execute
    /// tools are considered supported here.
    fn is_supported_builtin_workflow_tool(tool_name: &str) -> bool {
        if !tool_name.starts_with("aegis.workflow.") && !tool_name.starts_with("aegis.execute.") {
            return false;
        }
        BuiltinToolDefinition::lookup(tool_name).is_some()
    }

    fn should_advertise_builtin_tool(tool_name: &str) -> bool {
        // AEGIS ADR-141 F8: a landing is a workflow's own step, listed to
        // no agent.
        if tool_name == LAND_TOOL {
            return false;
        }
        if tool_name.starts_with("aegis.workflow.") || tool_name.starts_with("aegis.execute.") {
            return Self::is_supported_builtin_workflow_tool(tool_name);
        }

        true
    }

    pub fn new(builtin_dispatchers: Vec<BuiltinDispatcherConfig>) -> Self {
        Self {
            builtin_dispatchers,
            tool_capabilities: Vec::new(),
        }
    }

    /// The entries of `spec.tool_capabilities`, which gate and describe to
    /// the approval gate the tools they match (AEGIS ADR-126 D1).
    pub fn with_tool_capabilities(mut self, entries: &[ToolCapabilityConfig]) -> Self {
        self.tool_capabilities = entries.to_vec();
        self
    }

    /// List the builtin tools with their metadata.
    pub async fn list_tools(&self) -> anyhow::Result<Vec<ToolMetadata>> {
        let mut all_tools = Vec::new();

        for dispatcher in &self.builtin_dispatchers {
            for cap in &dispatcher.capabilities {
                if !Self::should_advertise_builtin_tool(&cap.name) {
                    continue;
                }

                let registry_def = BuiltinToolDefinition::lookup(&cap.name);
                let executor = registry_def
                    .filter(|d| d.edge_executor)
                    .map(|_| "edge".to_string());
                let fleet_capable = registry_def.is_some_and(|d| d.fleet_capable);
                all_tools.push(ToolMetadata {
                    name: cap.name.clone(),
                    description: dispatcher.description.clone(),
                    input_schema: Self::schema_for_builtin(&cap.name),
                    executor,
                    fleet_capable,
                });
            }
        }

        // Reconciliation pass: ensure every advertisable builtin tool appears in the
        // output even when the builtin_dispatchers vec does not include it (e.g. the
        // daemon was constructed with a partial dispatcher list).
        let existing_names: std::collections::HashSet<String> =
            all_tools.iter().map(|t| t.name.clone()).collect();
        for def in BUILTIN_TOOL_DEFINITIONS {
            if !Self::should_advertise_builtin_tool(def.name) {
                continue;
            }
            if existing_names.contains(def.name) {
                continue;
            }
            let executor = if def.edge_executor {
                Some("edge".to_string())
            } else {
                None
            };
            all_tools.push(ToolMetadata {
                name: def.name.to_string(),
                description: def.description.to_string(),
                input_schema: Self::schema_for_builtin(def.name),
                executor,
                fleet_capable: def.fleet_capable,
            });
        }

        Ok(all_tools)
    }

    /// Returns the canonical JSON Schema for a builtin tool by name.
    /// Falls back to a bare `{"type": "object"}` for unknown tools.
    fn schema_for_builtin(tool_name: &str) -> Value {
        match tool_name {
            "cmd.run" => Self::schema_cmd_run(),
            "fs.read" => Self::schema_fs_read(),
            "fs.write" => Self::schema_fs_write(),
            "fs.list" => Self::schema_fs_list(),
            "fs.create_dir" => Self::schema_fs_create_dir(),
            "fs.delete" => Self::schema_fs_delete(),
            "fs.edit" => Self::schema_fs_edit(),
            "fs.multi_edit" => Self::schema_fs_multi_edit(),
            "fs.grep" => Self::schema_fs_grep(),
            "fs.glob" => Self::schema_fs_glob(),
            "web.search" => Self::schema_web_search(),
            "web.fetch" => Self::schema_web_fetch(),
            "mail.draft" => Self::schema_mail_outbound(OutboundShape::Draft),
            "mail.send" => Self::schema_mail_outbound(OutboundShape::Send),
            "mail.reply" => Self::schema_mail_outbound(OutboundShape::Reply),
            "mail.list" => Self::schema_mail_list(),
            "mail.read" => Self::schema_mail_read(),
            "mail.label" => Self::schema_mail_label(),
            "mail.delete" => Self::schema_mail_delete(),
            "mail.archive" => Self::schema_mail_archive(),
            "mail.attachment" => Self::schema_mail_attachment(),
            "mail.forward" => Self::schema_mail_forward(),
            "calendar.calendars" => Self::schema_calendar(CalendarShape::Calendars),
            "calendar.list" => Self::schema_calendar(CalendarShape::List),
            "calendar.read" => Self::schema_calendar(CalendarShape::Read),
            "calendar.create" => Self::schema_calendar_write(CalendarShape::Create),
            "calendar.update" => Self::schema_calendar_write(CalendarShape::Update),
            "calendar.delete" => Self::schema_calendar_write(CalendarShape::Delete),
            "calendar.respond" => Self::schema_calendar_write(CalendarShape::Respond),
            "aegis.git.list" => Self::schema_aegis_git_list(),
            "aegis.git.status" => Self::schema_aegis_git(false, false),
            "aegis.git.diff" => Self::schema_aegis_git(false, true),
            "aegis.git.commit" => Self::schema_aegis_git(true, false),
            "aegis.git.push" => Self::schema_aegis_git(false, false),
            "aegis.git.land" => Self::schema_aegis_git(false, false),
            "aegis.schema.get" => Self::schema_aegis_schema_get(),
            "aegis.schema.validate" => Self::schema_aegis_schema_validate(),
            "aegis.agent.create" => Self::schema_aegis_agent_create(),
            "aegis.agent.list" => Self::schema_aegis_agent_list(),
            "aegis.agent.update" => Self::schema_aegis_agent_update(),
            "aegis.agent.export" => Self::schema_aegis_agent_export(),
            "aegis.agent.delete" => Self::schema_aegis_agent_delete(),
            "aegis.agent.generate" => Self::schema_aegis_agent_generate(),
            "aegis.agent.logs" => Self::schema_aegis_agent_logs(),
            "aegis.workflow.list" => Self::schema_aegis_workflow_list(),
            "aegis.workflow.validate" => Self::schema_aegis_workflow_validate(),
            "aegis.workflow.update" => Self::schema_aegis_workflow_update(),
            "aegis.workflow.export" => Self::schema_aegis_workflow_export(),
            "aegis.workflow.delete" => Self::schema_aegis_workflow_delete(),
            "aegis.workflow.run" => Self::schema_aegis_workflow_run(),
            "aegis.workflow.executions.list" => Self::schema_aegis_workflow_executions_list(),
            "aegis.workflow.executions.get" => Self::schema_aegis_workflow_executions_get(),
            "aegis.workflow.status" => Self::schema_aegis_workflow_status(),
            "aegis.workflow.generate" => Self::schema_aegis_workflow_generate(),
            "aegis.workflow.wait" => Self::schema_aegis_workflow_wait(),
            "aegis.workflow.cancel" => Self::schema_aegis_workflow_cancel(),
            "aegis.workflow.signal" => Self::schema_aegis_workflow_signal(),
            "aegis.workflow.remove" => Self::schema_aegis_workflow_remove(),
            "aegis.workflow.promote" => Self::schema_aegis_workflow_promote(),
            "aegis.workflow.demote" => Self::schema_aegis_workflow_demote(),
            "aegis.execute.intent" => Self::schema_aegis_execute_intent(),
            "aegis.execute.status" => Self::schema_aegis_execute_status(),
            "aegis.execute.wait" => Self::schema_aegis_execute_wait(),
            "aegis.task.execute" => Self::schema_aegis_task_execute(),
            "aegis.task.status" => Self::schema_aegis_task_status(),
            "aegis.task.wait" => Self::schema_aegis_task_wait(),
            "aegis.agent.wait" => Self::schema_aegis_agent_wait(),
            "aegis.task.logs" => Self::schema_aegis_task_logs(),
            "aegis.task.list" => Self::schema_aegis_task_list(),
            "aegis.task.cancel" => Self::schema_aegis_task_cancel(),
            "aegis.task.remove" => Self::schema_aegis_task_remove(),
            "aegis.schedule.create" => Self::schema_aegis_schedule_write(true),
            "aegis.schedule.update" => Self::schema_aegis_schedule_write(false),
            "aegis.schedule.list" => json!({ "type": "object", "properties": {} }),
            "aegis.schedule.get"
            | "aegis.schedule.pause"
            | "aegis.schedule.resume"
            | "aegis.schedule.run_now"
            | "aegis.schedule.delete" => Self::schema_aegis_schedule_by_id(),
            "aegis.schedule.runs" => Self::schema_aegis_schedule_runs(),
            "aegis.system.info" => Self::schema_aegis_system_info(),
            "aegis.system.config" => Self::schema_aegis_system_config(),
            // ADR-117 §D edge fleet system tools.
            "aegis.edge.fleet.list" => Self::schema_aegis_edge_fleet_list(),
            "aegis.edge.fleet.invoke" => Self::schema_aegis_edge_fleet_invoke(),
            "aegis.edge.fleet.cancel" => Self::schema_aegis_edge_fleet_cancel(),
            "aegis.agent.search" => Self::schema_aegis_agent_search(),
            "aegis.workflow.search" => Self::schema_aegis_workflow_search(),
            "aegis.workflow.create" => Self::schema_aegis_workflow_create(),
            "aegis.runtime.list" => Self::schema_aegis_runtime_list(),
            "aegis.execution.file" => Self::schema_aegis_execution_file(),
            "aegis.attachment.read" => Self::schema_aegis_attachment_read(),
            "aegis.approval.status" => Self::schema_aegis_approval_status(),
            "aegis.goal.create" => Self::schema_aegis_goal_create(),
            "aegis.goal.evaluate" => Self::schema_aegis_goal_evaluate(),
            "aegis.goal.status" => Self::schema_aegis_goal_status(),
            "aegis.goal.cancel" => Self::schema_aegis_goal_cancel(),
            "aegis.document.render" => Self::schema_aegis_document_render(),
            "aegis.tools.list" => Self::schema_aegis_tools_list(),
            "aegis.tools.search" => Self::schema_aegis_tools_search(),
            _ => json!({ "type": "object" }),
        }
    }

    /// JSON schema for the `aegis.tools.list` builtin tool.
    fn schema_aegis_tools_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "offset": {
                    "type": "integer",
                    "description": "How many matching tools to skip. Default: 0."
                },
                "limit": {
                    "type": "integer",
                    "description": "The most tools answered (at most 200). Default: 50."
                },
                "source": {
                    "type": "string",
                    "enum": ["builtin", "mcp_server", "seal_gateway"],
                    "description": "Only tools from this source."
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "agent_management", "workflow_management", "task_management",
                        "schedule_management", "system_management", "schema_validation",
                        "filesystem", "execution", "web_network", "tool_discovery", "external"
                    ],
                    "description": "Only tools of this category."
                },
                "fleet_capable": {
                    "type": "boolean",
                    "description": "Only tools that can, or cannot, run across an edge fleet."
                }
            }
        })
    }

    /// JSON schema for the `aegis.tools.search` builtin tool.
    fn schema_aegis_tools_search() -> Value {
        json!({
            "type": "object",
            "properties": {
                "keyword": {
                    "type": "string",
                    "description": "Text the tool's name or description contains, matched without regard to case."
                },
                "name_pattern": {
                    "type": "string",
                    "description": "A glob over tool names, such as \"fs.*\"."
                },
                "source": {
                    "type": "string",
                    "enum": ["builtin", "mcp_server", "seal_gateway"],
                    "description": "Only tools from this source."
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "agent_management", "workflow_management", "task_management",
                        "system_management", "schema_validation", "filesystem", "execution",
                        "web_network", "tool_discovery", "external"
                    ],
                    "description": "Only tools of this category."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tags every answered tool carries."
                },
                "fleet_capable": {
                    "type": "boolean",
                    "description": "Only tools that can, or cannot, run across an edge fleet."
                }
            }
        })
    }

    /// JSON schema for the `cmd.run` builtin tool.
    fn schema_cmd_run() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to execute"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed to the command, as an array of strings, such as [\"-c\", \"env\"]."
                },
                "env_additions": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Environment variables added for this command, as an object of string values, such as {\"INTENT_INPUTS\": \"{\\\"text\\\": \\\"hello\\\"}\"}. env_additions is an object, not a JSON string."
                },
                "stdin": {
                    "type": "string",
                    "description": "Text written to the command's standard input, which is then closed. Use it to feed a script that reads standard input, such as {\"value\": 43}. Without it the command reads an immediate end of input. At most max_output_bytes bytes."
                }
            },
            "required": ["command"]
        })
    }

    /// JSON schema for the `fs.read` builtin tool.
    fn schema_fs_read() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the file to read."
                }
            },
            "required": ["path"]
        })
    }

    /// JSON schema for the `fs.write` builtin tool.
    fn schema_fs_write() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the file to write."
                },
                "content": {
                    "type": "string",
                    "description": "String content to write to the file."
                }
            },
            "required": ["path", "content"]
        })
    }

    /// JSON schema for the `fs.list` builtin tool.
    fn schema_fs_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the directory to list."
                }
            },
            "required": ["path"]
        })
    }

    /// JSON schema for the `fs.create_dir` builtin tool.
    fn schema_fs_create_dir() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the directory to create."
                }
            },
            "required": ["path"]
        })
    }

    /// JSON schema for the `fs.delete` builtin tool.
    fn schema_fs_delete() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the file or directory to delete."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Set to true to delete a directory and all its contents."
                }
            },
            "required": ["path"]
        })
    }

    /// JSON schema for the `fs.edit` builtin tool.
    fn schema_fs_edit() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the file to edit."
                },
                "target_content": {
                    "type": "string",
                    "description": "Exact string content to find and replace. Must match exactly once."
                },
                "replacement_content": {
                    "type": "string",
                    "description": "New string content to insert in place of target_content."
                }
            },
            "required": ["path", "target_content", "replacement_content"]
        })
    }

    /// JSON schema for the `fs.multi_edit` builtin tool.
    fn schema_fs_multi_edit() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative POSIX path of the file to edit."
                },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "target_content": { "type": "string" },
                            "replacement_content": { "type": "string" }
                        },
                        "required": ["target_content", "replacement_content"]
                    },
                    "description": "Array of edits to apply sequentially."
                }
            },
            "required": ["path", "edits"]
        })
    }

    /// JSON schema for the `fs.grep` builtin tool.
    fn schema_fs_grep() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for."
                },
                "path": {
                    "type": "string",
                    "description": "Directory path to start the recursive search from."
                }
            },
            "required": ["pattern", "path"]
        })
    }

    /// JSON schema for the `fs.glob` builtin tool.
    fn schema_fs_glob() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files (e.g. *.rs)."
                },
                "path": {
                    "type": "string",
                    "description": "Directory path to start the recursive search from."
                }
            },
            "required": ["pattern", "path"]
        })
    }

    /// JSON schema for the `web.search` builtin tool.
    fn schema_web_search() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query."
                }
            },
            "required": ["query"]
        })
    }

    /// JSON schema for the `web.fetch` builtin tool.
    fn schema_web_fetch() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch content from."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Character position in the page to start from (default 0). One call returns at most 50,000 characters; when the page is longer the result says truncated, total_chars and next_offset, and calling again with the same url and offset set to next_offset reads the next part."
                }
            },
            "required": ["url"]
        })
    }

    /// JSON schema for the outbound mail tools (AEGIS ADR-125's Update of
    /// 2026-10-07 (3) clause 13): a message given in full, `to` and `cc`
    /// lists of addresses (no `bcc`), a one-line `subject` and a plain-text
    /// `body`; `mail.reply` and an optional `mail.draft` name a thread.
    fn schema_mail_outbound(shape: OutboundShape) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "mailbox".to_string(),
            json!({"type": "string", "description": "The id of one of your mailbox connections."}),
        );
        if shape != OutboundShape::Send {
            let description = if shape == OutboundShape::Reply {
                "A thread id mail.list answered; the reply goes to its newest message."
            } else {
                "A thread id mail.list answered, to save the draft as a reply to its newest message."
            };
            properties.insert(
                "thread_id".to_string(),
                json!({"type": "string", "description": description}),
            );
        }
        let min_to = if shape == OutboundShape::Draft { 0 } else { 1 };
        properties.insert(
            "to".to_string(),
            json!({
                "type": "array",
                "items": {"type": "string"},
                "minItems": min_to,
                "maxItems": 50,
                "description": "The recipients' email addresses, such as ann@example.com; to and cc together hold at most 50."
            }),
        );
        properties.insert(
            "cc".to_string(),
            json!({
                "type": "array",
                "items": {"type": "string"},
                "maxItems": 50,
                "description": "Further recipients' email addresses, copied; to and cc together hold at most 50."
            }),
        );
        let subject = if shape == OutboundShape::Reply {
            "One line of at most 998 characters, usually the thread's subject with Re: before it."
        } else {
            "One line of at most 998 characters."
        };
        properties.insert(
            "subject".to_string(),
            json!({"type": "string", "maxLength": 998, "description": subject}),
        );
        properties.insert(
            "body".to_string(),
            json!({
                "type": "string",
                "maxLength": 100000,
                "description": "The message as plain text, at most 100000 characters."
            }),
        );
        if shape != OutboundShape::Draft {
            properties.insert("attachments".to_string(), Self::schema_mail_files());
        }
        let required: Vec<&str> = match shape {
            OutboundShape::Draft => vec!["mailbox", "body"],
            OutboundShape::Send => vec!["mailbox", "to", "subject", "body"],
            OutboundShape::Reply => vec!["mailbox", "thread_id", "to", "subject", "body"],
        };
        json!({"type": "object", "properties": properties, "required": required})
    }

    /// The `attachments` the tools that send take (AEGIS ADR-125's Update
    /// of 2026-10-08 (5) clause 32).
    fn schema_mail_files() -> Value {
        json!({
            "type": "array",
            "maxItems": 10,
            "items": {
                "type": "object",
                "properties": {
                    "volume_id": {"type": "string", "description": "The id of one of the person's own volumes holding the file."},
                    "path": {"type": "string", "description": "The file's path in that volume."}
                },
                "required": ["volume_id", "path"]
            },
            "description": "Files of the person's own to attach, at most 10 and 20 MiB together, each by its volume_id and path, such as an uploaded file's reference or one mail.attachment saved."
        })
    }

    /// JSON schema for the `mail.forward` builtin tool (AEGIS ADR-125's
    /// Update of 2026-10-08 (5) clause 34). `forwarded` and `message_uids`
    /// are not offered: the admission writes them before the gate.
    fn schema_mail_forward() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "folder": {
                    "type": "string",
                    "enum": ["inbox", "sent", "drafts", "trash", "archive", "all"],
                    "description": "Which folder the thread is in: inbox (the default), sent, drafts, trash, archive, or all your mail where the server keeps such a folder."
                },
                "thread_id": {
                    "type": "string",
                    "description": "A thread id mail.list answered; every message of it in that folder is forwarded, at most 20."
                },
                "message_uid": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "One message of the thread to forward alone, by its uid as mail.read answered it in that folder."
                },
                "to": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": 50,
                    "description": "The recipients' email addresses, such as ann@example.com; to and cc together hold at most 50."
                },
                "cc": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": 50,
                    "description": "Further recipients' email addresses, copied; to and cc together hold at most 50."
                },
                "subject": {
                    "type": "string",
                    "maxLength": 998,
                    "description": "One line of at most 998 characters; without it the subject is Fwd: and the oldest forwarded message's subject."
                },
                "note": {
                    "type": "string",
                    "maxLength": 100000,
                    "description": "A note as plain text before the forwarded messages, at most 100000 characters."
                },
                "attachments": Self::schema_mail_files()
            },
            "required": ["mailbox", "thread_id", "to"]
        })
    }

    /// JSON schema for the calendar read tools (AEGIS ADR-138 K6). Every
    /// one takes `account`; a run with chosen accounts lists it as their
    /// names' `enum` instead.
    fn schema_calendar(shape: CalendarShape) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "account".to_string(),
            json!({
                "type": "string",
                "description": "The id of one of your calendar accounts."
            }),
        );
        let mut required = vec!["account"];
        if matches!(shape, CalendarShape::List | CalendarShape::Read) {
            properties.insert(
                "calendar_id".to_string(),
                json!({
                    "type": "string",
                    "description": "One of the calendar_id values calendar.calendars answers for this account, or primary for the account's own calendar; any other value is refused."
                }),
            );
            required.push("calendar_id");
        }
        if matches!(shape, CalendarShape::List) {
            properties.insert(
                "start".to_string(),
                json!({
                    "type": "string",
                    "description": "The window's start, an RFC 3339 time with an offset (default now)."
                }),
            );
            properties.insert(
                "end".to_string(),
                json!({
                    "type": "string",
                    "description": "The window's end, an RFC 3339 time with an offset after start and at most 92 days on (default seven days after start)."
                }),
            );
            properties.insert(
                "query".to_string(),
                json!({
                    "type": "string",
                    "description": "Words an event's title, location or description contains."
                }),
            );
            properties.insert(
                "limit".to_string(),
                json!({
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "The most events to answer (default 50); truncated says when there were more."
                }),
            );
        }
        if matches!(shape, CalendarShape::Read) {
            properties.insert(
                "event_id".to_string(),
                json!({
                    "type": "string",
                    "description": "An event_id calendar.list answered."
                }),
            );
            required.push("event_id");
        }
        json!({"type": "object", "properties": properties, "required": required})
    }

    /// JSON schema for the calendar writes (AEGIS ADR-138 K6, K7a). Every
    /// one takes `account` and `calendar_id`; update, delete and respond an
    /// `event_id`. The values the admission reads from the event before the
    /// gate (`current_title`, `current_start`, and for delete and respond
    /// `title`, `start`, `end`, `attendees`, `organizer`, `repeats`) are not
    /// offered.
    fn schema_calendar_write(shape: CalendarShape) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "account".to_string(),
            json!({
                "type": "string",
                "description": "The id of one of your calendar accounts."
            }),
        );
        properties.insert(
            "calendar_id".to_string(),
            json!({
                "type": "string",
                "description": "One of the calendar_id values calendar.calendars answers for this account, or primary for the account's own calendar; any other value is refused."
            }),
        );
        let mut required = vec!["account", "calendar_id"];
        if shape != CalendarShape::Create {
            properties.insert(
                "event_id".to_string(),
                json!({
                    "type": "string",
                    "description": "An event_id calendar.list answered."
                }),
            );
            required.push("event_id");
        }
        if matches!(shape, CalendarShape::Create | CalendarShape::Update) {
            properties.insert(
                "title".to_string(),
                json!({
                    "type": "string",
                    "maxLength": 1000,
                    "description": "The event's title: one line of at most 1000 characters."
                }),
            );
            properties.insert(
                "start".to_string(),
                json!({
                    "type": "string",
                    "description": "When the event starts: an RFC 3339 time with an offset, or a date YYYY-MM-DD for an all-day event."
                }),
            );
            properties.insert(
                "end".to_string(),
                json!({
                    "type": "string",
                    "description": "When the event ends, after start and of the same form: an RFC 3339 time with an offset, or the date after an all-day event's last day."
                }),
            );
            properties.insert(
                "description".to_string(),
                json!({
                    "type": "string",
                    "maxLength": 32000,
                    "description": "Plain text of at most 32000 characters."
                }),
            );
            properties.insert(
                "location".to_string(),
                json!({
                    "type": "string",
                    "description": "Where the event takes place."
                }),
            );
            properties.insert(
                "attendees".to_string(),
                json!({
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": 50,
                    "description": if shape == CalendarShape::Create {
                        "Up to 50 email addresses to invite."
                    } else {
                        "Up to 50 email addresses: the event's attendees replaced by these; an empty list removes them all."
                    }
                }),
            );
        }
        if shape == CalendarShape::Create {
            required.extend(["title", "start", "end"]);
        }
        if shape == CalendarShape::Respond {
            properties.insert(
                "response".to_string(),
                json!({
                    "type": "string",
                    "enum": ["accepted", "declined", "tentative"],
                    "description": "The account's answer to the invitation."
                }),
            );
            required.push("response");
        }
        json!({"type": "object", "properties": properties, "required": required})
    }

    /// JSON schema for the `mail.list` builtin tool.
    fn schema_mail_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "folder": {
                    "type": "string",
                    "enum": ["inbox", "sent", "drafts", "trash", "archive", "all"],
                    "description": "Which folder: inbox (the default), sent, drafts, trash, archive, or all your mail where the server keeps such a folder."
                },
                "query": {
                    "type": "string",
                    "description": "Words a message's headers or body contain."
                },
                "from": {
                    "type": "string",
                    "description": "Part of the sender's name or address."
                },
                "unread_only": {
                    "type": "boolean",
                    "description": "Only threads with an unread message matching (default false)."
                },
                "flagged_only": {
                    "type": "boolean",
                    "description": "Only flagged messages (default false)."
                },
                "since": {
                    "type": "string",
                    "description": "Only messages received on or after this date, written YYYY-MM-DD."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "The most threads to answer (default 20). The 200 newest matching messages are grouped into threads; truncated says when there were more."
                }
            },
            "required": ["mailbox"]
        })
    }

    /// JSON schema for the `mail.read` builtin tool.
    fn schema_mail_read() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "folder": {
                    "type": "string",
                    "enum": ["inbox", "sent", "drafts", "trash", "archive", "all"],
                    "description": "Which folder: inbox (the default), sent, drafts, trash, archive, or all your mail where the server keeps such a folder."
                },
                "thread_id": {
                    "type": "string",
                    "description": "A thread id mail.list answered."
                }
            },
            "required": ["mailbox", "thread_id"]
        })
    }

    /// JSON schema for the `mail.attachment` builtin tool (AEGIS ADR-125's
    /// Update of 2026-10-08 (5) clause 31).
    fn schema_mail_attachment() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "folder": {
                    "type": "string",
                    "enum": ["inbox", "sent", "drafts", "trash", "archive", "all"],
                    "description": "Which folder the message is in: inbox (the default), sent, drafts, trash, archive, or all your mail where the server keeps such a folder."
                },
                "uid": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The message's uid as mail.read answered it in that folder."
                },
                "part": {
                    "type": "string",
                    "pattern": "^[1-9][0-9]*(\\.[1-9][0-9]*)*$",
                    "description": "The attachment's part number as mail.read answered it, such as 2 or 1.2."
                }
            },
            "required": ["mailbox", "uid", "part"]
        })
    }

    /// JSON schema for the `mail.delete` builtin tool (AEGIS ADR-125's
    /// Update of 2026-10-08 (4) clause 17). `subject` and `from` are not
    /// offered: the admission writes them before the gate.
    fn schema_mail_delete() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "thread_id": {
                    "type": "string",
                    "description": "A thread id mail.list answered; every message of it in the inbox moves to Trash."
                }
            },
            "required": ["mailbox", "thread_id"]
        })
    }

    /// JSON schema for the `mail.archive` builtin tool (AEGIS ADR-125's
    /// Update of 2026-10-08 (5) clause 27). `subject` and `from` are not
    /// offered: the admission writes them before the gate.
    fn schema_mail_archive() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "thread_id": {
                    "type": "string",
                    "description": "A thread id mail.list answered; every message of it in the inbox moves to the Archive folder."
                }
            },
            "required": ["mailbox", "thread_id"]
        })
    }

    /// JSON schema for the `mail.label` builtin tool.
    fn schema_mail_label() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mailbox": {
                    "type": "string",
                    "description": "The id of one of your mailbox connections."
                },
                "thread_id": {
                    "type": "string",
                    "description": "A thread id mail.list answered."
                },
                "add": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Labels to add to every message of the thread, such as zaru/triaged: up to 64 characters, no spaces."
                },
                "remove": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Labels to remove from every message of the thread."
                },
                "flagged": {
                    "type": "boolean",
                    "description": "true flags every message of the thread, false unflags it; leave it out to keep the flag as it is."
                },
                "seen": {
                    "type": "boolean",
                    "description": "true marks every message of the thread read, false marks it unread; leave it out to keep it as it is."
                }
            },
            "required": ["mailbox", "thread_id"]
        })
    }

    /// JSON schema for the `aegis.git.*` builtin tools (AEGIS ADR-136 G7,
    /// G7a): inside a run, `repository` names one of the run's repositories
    /// by its label; outside a run, `binding_id` names one of your git
    /// repository bindings. Neither is required, since each caller has one.
    /// `aegis.git.commit` adds `message`, `aegis.git.diff` adds `staged`.
    /// JSON schema for the `aegis.git.list` builtin tool: no arguments; the
    /// caller is the person whose bindings are listed.
    fn schema_aegis_git_list() -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn schema_aegis_git(message: bool, staged: bool) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "repository".to_string(),
            json!({
                "type": "string",
                "description": "Inside a run: the label of one of the run's repositories, mounted at /workspace/<label>."
            }),
        );
        properties.insert(
            "binding_id".to_string(),
            json!({
                "type": "string",
                "description": "Outside a run: the id of one of your git repository bindings."
            }),
        );
        let mut required = Vec::new();
        if message {
            properties.insert(
                "message".to_string(),
                json!({"type": "string", "description": "The commit message."}),
            );
            required.push("message");
        }
        if staged {
            properties.insert(
                "staged".to_string(),
                json!({
                    "type": "boolean",
                    "description": "true shows what is staged; leave it out for the changes not yet staged."
                }),
            );
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": required
        })
    }

    /// JSON schema for the `aegis.schema.get` builtin tool.
    fn schema_aegis_schema_get() -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Schema key to retrieve. Supported: \"agent/manifest/v1\", \"workflow/manifest/v1\""
                }
            },
            "required": ["key"]
        })
    }

    /// JSON schema for the `aegis.schema.validate` builtin tool.
    fn schema_aegis_schema_validate() -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Manifest kind to validate against. Supported: \"agent\", \"workflow\""
                },
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full manifest YAML text to validate against the canonical schema."
                }
            },
            "required": ["kind", "manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.agent.create` builtin tool.
    fn schema_aegis_agent_create() -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full Agent manifest YAML to parse, validate, and deploy. Supports spec.type: 'user' (default) | 'system' — declares the agent's execution tier. 'user' agents run within the requesting tenant's context. 'system' agents run in the privileged platform tier with elevated access. Omit to default to 'user'."
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing deployed agent with the same name/version."
                }
            },
            "required": ["manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.agent.list` builtin tool.
    fn schema_aegis_agent_list() -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    /// JSON schema for the `aegis.agent.update` builtin tool.
    fn schema_aegis_agent_update() -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full Agent manifest YAML to update an existing agent. Supports spec.type: 'user' (default) | 'system' — declares the agent's execution tier. 'user' agents run within the requesting tenant's context. 'system' agents run in the privileged platform tier with elevated access. Omit to default to 'user'."
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing version if it already exists."
                }
            },
            "required": ["manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.agent.export` builtin tool.
    fn schema_aegis_agent_export() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the agent to export."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.agent.delete` builtin tool.
    fn schema_aegis_agent_delete() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "UUID of the agent to remove."
                }
            },
            "required": ["agent_id"]
        })
    }

    /// JSON schema for the `aegis.agent.generate` builtin tool.
    fn schema_aegis_agent_generate() -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Natural-language intent for the agent to create."
                },
                "attachments": {
                    "type": "array",
                    "description": "Files attached to this dispatch. Each entry references a file in a tenant-scoped volume; the agent reads each via aegis.attachment.read({volume_id, path}).",
                    "items": {
                        "type": "object",
                        "required": ["volume_id", "path", "name", "mime_type", "size"],
                        "properties": {
                            "volume_id": { "type": "string" },
                            "path":      { "type": "string" },
                            "name":      { "type": "string" },
                            "mime_type": { "type": "string" },
                            "size":      { "type": "integer" },
                            "sha256":    { "type": "string" }
                        }
                    }
                }
            },
            "required": ["input"]
        })
    }

    /// JSON schema for the `aegis.agent.logs` builtin tool.
    fn schema_aegis_agent_logs() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "UUID of the agent whose activity log should be retrieved."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of events to return.",
                    "default": 50
                },
                "offset": {
                    "type": "integer",
                    "description": "Zero-based starting offset into the activity log.",
                    "default": 0
                }
            },
            "required": ["agent_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.list` builtin tool.
    fn schema_aegis_workflow_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "visible"],
                    "description": "Optional scope filter. 'global' lists only global workflows. 'visible' lists user+tenant+global. Omit to list all for tenant."
                },
                "user_id": {
                    "type": "string",
                    "description": "Optional user ID for 'visible' scope filter."
                }
            }
        })
    }

    /// JSON schema for the `aegis.workflow.validate` builtin tool.
    fn schema_aegis_workflow_validate() -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full Workflow manifest YAML to parse and deterministically validate."
                }
            },
            "required": ["manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.workflow.update` builtin tool.
    fn schema_aegis_workflow_update() -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full Workflow manifest YAML to update an existing workflow."
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing version if it already exists."
                }
            },
            "required": ["manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.workflow.export` builtin tool.
    fn schema_aegis_workflow_export() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to export."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.workflow.delete` builtin tool.
    fn schema_aegis_workflow_delete() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to delete."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.workflow.run` builtin tool.
    fn schema_aegis_workflow_run() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to execute."
                },
                "intent": {
                    "type": "string",
                    "description": "Natural-language description of the goal for this workflow run. Injected into the workflow input so task activities and agents can access it."
                },
                "input": {
                    "type": "object",
                    "description": "Workflow input parameters."
                },
                "blackboard": {
                    "type": "object",
                    "description": "Optional blackboard overrides merged into the workflow execution before startup."
                },
                "version": {
                    "type": "string",
                    "description": "Optional semantic version of the workflow to execute. When omitted, the latest deployed version is used."
                },
                "repositories": {
                    "type": "array",
                    "description": "Optional: your git repositories for the run, each by the id of one of your git repository bindings (aegis.git.list lists them), with an optional work branch.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "binding_id": {
                                "type": "string",
                                "description": "The id of one of your git repository bindings."
                            },
                            "branch": {
                                "type": "string",
                                "description": "Optional work branch for the run."
                            }
                        },
                        "required": ["binding_id"],
                        "additionalProperties": false
                    }
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.workflow.executions.list` builtin tool.
    fn schema_aegis_workflow_executions_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return.",
                    "default": 20
                },
                "offset": {
                    "type": "integer",
                    "description": "Pagination offset.",
                    "default": 0
                },
                "workflow_id": {
                    "type": "string",
                    "description": "Optional workflow UUID or workflow name filter."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            }
        })
    }

    /// JSON schema for the `aegis.workflow.executions.get` builtin tool.
    fn schema_aegis_workflow_executions_get() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to inspect."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.status` builtin tool.
    fn schema_aegis_workflow_status() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to inspect."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.generate` builtin tool.
    fn schema_aegis_workflow_generate() -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Natural-language workflow objective."
                }
            },
            "required": ["input"]
        })
    }

    /// JSON schema for the `aegis.workflow.wait` builtin tool.
    fn schema_aegis_workflow_wait() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to wait for."
                },
                "poll_interval_seconds": {
                    "type": "integer",
                    "description": "Seconds between polls (default: 5)."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum wait time in seconds (default: 300)."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.cancel` builtin tool.
    fn schema_aegis_workflow_cancel() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to cancel."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.signal` builtin tool.
    fn schema_aegis_workflow_signal() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to signal."
                },
                "response": {
                    "type": "string",
                    "description": "Human input response text to send to the paused workflow."
                },
                "feedback": {
                    "type": "string",
                    "description": "The person's feedback with the response, read by the workflow as {{human.feedback}}."
                }
            },
            "required": ["execution_id", "response"]
        })
    }

    /// JSON schema for the `aegis.workflow.remove` builtin tool.
    fn schema_aegis_workflow_remove() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow execution to remove."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.workflow.promote` builtin tool.
    fn schema_aegis_workflow_promote() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Workflow name or ID to promote."
                },
                "target_scope": {
                    "type": "string",
                    "enum": ["tenant", "global"],
                    "description": "Target scope (default: global)."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.workflow.demote` builtin tool.
    fn schema_aegis_workflow_demote() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Workflow name or ID to demote."
                },
                "target_scope": {
                    "type": "string",
                    "enum": ["tenant", "user"],
                    "description": "Target scope (default: tenant)."
                },
                "user_id": {
                    "type": "string",
                    "description": "Owner user ID when demoting to user scope."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["name"]
        })
    }

    /// JSON schema for the `aegis.execute.intent` builtin tool.
    fn schema_aegis_execute_intent() -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": {
                    "type": "string",
                    "description": "Natural-language description of what to execute (e.g. 'resize images in /workspace to 800x600')."
                },
                "inputs": {
                    "type": "object",
                    "description": "Optional structured inputs passed to the pipeline."
                },
                "volume_id": {
                    "type": "string",
                    "description": "Optional persistent volume ID to use as workspace. When omitted, an ephemeral volume is created."
                },
                "language": {
                    "type": "string",
                    "enum": ["python", "javascript", "bash"],
                    "description": "Execution language (default: python)."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Optional execution timeout in seconds."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                },
                "attachments": {
                    "type": "array",
                    "description": "Files attached to this dispatch. Each entry references a file in a tenant-scoped volume; the agent reads each via aegis.attachment.read({volume_id, path}).",
                    "items": {
                        "type": "object",
                        "required": ["volume_id", "path", "name", "mime_type", "size"],
                        "properties": {
                            "volume_id": { "type": "string" },
                            "path":      { "type": "string" },
                            "name":      { "type": "string" },
                            "mime_type": { "type": "string" },
                            "size":      { "type": "integer" },
                            "sha256":    { "type": "string" }
                        }
                    }
                }
            },
            "required": ["intent"]
        })
    }

    /// JSON schema for the `aegis.execute.status` builtin tool.
    fn schema_aegis_execute_status() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pipeline_execution_id": {
                    "type": "string",
                    "description": "UUID of the pipeline execution to check."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Optional tenant identifier. Defaults to the local tenant."
                }
            },
            "required": ["pipeline_execution_id"]
        })
    }

    /// JSON schema for the `aegis.execute.wait` builtin tool.
    fn schema_aegis_execute_wait() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the workflow/pipeline execution to wait for."
                },
                "poll_interval_seconds": {
                    "type": "integer",
                    "description": "Seconds between polls (default: 5)."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum wait time in seconds (default: 300)."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.task.execute` builtin tool.
    fn schema_aegis_task_execute() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "UUID or Name of the agent to execute."
                },
                "intent": {
                    "type": "string",
                    "description": "Free-form natural-language steering for the agent. Use when the agent has no input_schema. When input_schema is present, intent is optional supplemental context."
                },
                "input": {
                    "type": "object",
                    "description": "Structured data for the agent. When the agent declares input_schema, pass exactly the properties defined there — do not wrap them in additional keys. When the agent has no input_schema, omit this field and use 'intent' instead."
                },
                "version": {
                    "type": "string",
                    "description": "Optional semantic version of the agent to execute. When omitted, the latest deployed version is used."
                },
                "attachments": {
                    "type": "array",
                    "description": "Files attached to this dispatch. Each entry references a file in a tenant-scoped volume; the agent reads each via aegis.attachment.read({volume_id, path}).",
                    "items": {
                        "type": "object",
                        "required": ["volume_id", "path", "name", "mime_type", "size"],
                        "properties": {
                            "volume_id": { "type": "string" },
                            "path":      { "type": "string" },
                            "name":      { "type": "string" },
                            "mime_type": { "type": "string" },
                            "size":      { "type": "integer" },
                            "sha256":    { "type": "string" }
                        }
                    }
                }
            },
            "required": ["agent_id"]
        })
    }

    /// JSON schema for `aegis.schedule.create` (`create`) and
    /// `aegis.schedule.update` (AEGIS ADR-139 N11). `contexts` and
    /// `repositories` are reserved dispatch keys the client writes from the
    /// person's choices, never offered here.
    fn schema_aegis_schedule_write(create: bool) -> Value {
        let mut properties = json!({
            "name": {
                "type": "string",
                "description": "Your label for the schedule, 1 to 80 characters."
            },
            "target_kind": {
                "type": "string",
                "enum": ["agent", "workflow"],
                "description": "Whether the schedule starts an agent or a workflow."
            },
            "target": {
                "type": "string",
                "description": "UUID or name of the agent, or name of the workflow, each run starts."
            },
            "version": {
                "type": "string",
                "description": "Optional version of the agent or workflow, by name only. When omitted, each run uses the latest."
            },
            "intent": {
                "type": "string",
                "description": "Free-form natural-language steering each run is given, as aegis.task.execute and aegis.workflow.run take it."
            },
            "input": {
                "type": "object",
                "description": "Structured input each run is given. When the agent or workflow declares input_schema, pass exactly the properties defined there."
            },
            "attachments": {
                "type": "array",
                "description": "Files each run is given. Each entry references a file in a tenant-scoped volume.",
                "items": {
                    "type": "object",
                    "required": ["volume_id", "path", "name", "mime_type", "size"],
                    "properties": {
                        "volume_id": { "type": "string" },
                        "path":      { "type": "string" },
                        "name":      { "type": "string" },
                        "mime_type": { "type": "string" },
                        "size":      { "type": "integer" },
                        "sha256":    { "type": "string" }
                    }
                }
            },
            "at": {
                "type": "string",
                "description": "For one run: an RFC 3339 time at least one minute from now and at most a year ahead. Give either at or recurrence."
            },
            "recurrence": {
                "type": "object",
                "description": "For a run again and again. Give either at or recurrence.",
                "properties": {
                    "cron": {
                        "type": "string",
                        "description": "Five fields: minute, hour, day of month, month and day of week, such as \"0 15 * * 1-5\"."
                    },
                    "timezone": {
                        "type": "string",
                        "description": "A time zone name such as Europe/Berlin. Default: UTC."
                    },
                    "jitter_seconds": {
                        "type": "integer",
                        "description": "Up to this many seconds of random delay before each run. Default: 0."
                    }
                },
                "required": ["cron"]
            }
        });
        let required = if create {
            json!(["name", "target_kind", "target"])
        } else {
            properties["schedule_id"] = json!({
                "type": "string",
                "description": "The id of the schedule to update."
            });
            json!(["schedule_id"])
        };
        json!({
            "type": "object",
            "properties": properties,
            "required": required
        })
    }

    /// JSON schema for the `aegis.schedule.*` tools that name one schedule.
    fn schema_aegis_schedule_by_id() -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule_id": {
                    "type": "string",
                    "description": "The id of the schedule."
                }
            },
            "required": ["schedule_id"]
        })
    }

    /// JSON schema for `aegis.schedule.runs`.
    fn schema_aegis_schedule_runs() -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule_id": {
                    "type": "string",
                    "description": "The id of the schedule."
                },
                "limit": {
                    "type": "integer",
                    "description": "The most runs answered, 1 to 100. Default: 20."
                }
            },
            "required": ["schedule_id"]
        })
    }

    /// JSON schema for the `aegis.approval.status` builtin tool (ADR-126 D4).
    fn schema_aegis_approval_status() -> Value {
        json!({
            "type": "object",
            "properties": {
                "approval_id": {
                    "type": "string",
                    "description": "The approval_id an approval_pending result returned."
                }
            },
            "required": ["approval_id"]
        })
    }

    /// JSON schema for the `aegis.goal.create` builtin tool (ADR-131 D2, U8).
    fn schema_aegis_goal_create() -> Value {
        json!({
            "type": "object",
            "properties": {
                "statement": {
                    "type": "string",
                    "description": "The user's request, verbatim; at most 32,768 characters."
                },
                "client_ref": {
                    "type": "string",
                    "description": "The caller's opaque reference, for Zaru Web the conversation id. An open goal under the same reference is closed superseded."
                },
                "channel": {
                    "type": "string",
                    "enum": ["web", "api"]
                }
            },
            "required": ["statement", "client_ref", "channel"]
        })
    }

    /// JSON schema for the `aegis.goal.evaluate` builtin tool (ADR-131 D2,
    /// U2, U8).
    fn schema_aegis_goal_evaluate() -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string" },
                "companion_answer": {
                    "type": "string",
                    "description": "The text the companion showed the user in the turn just ended; read to 8,192 characters."
                },
                "round": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The round asked about: absent on a goal's first evaluation, then the round the last answer carried."
                }
            },
            "required": ["goal_id", "companion_answer"]
        })
    }

    /// JSON schema for the `aegis.goal.status` builtin tool (ADR-131 D2, U8).
    fn schema_aegis_goal_status() -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string" }
            },
            "required": ["goal_id"]
        })
    }

    /// JSON schema for the `aegis.goal.cancel` builtin tool (ADR-131 U33).
    fn schema_aegis_goal_cancel() -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string" },
                "reason": {
                    "type": "string",
                    "description": "Why the person stopped the goal, in their words; kept with the goal, to 1,000 characters."
                }
            },
            "required": ["goal_id"]
        })
    }

    /// JSON schema for the `aegis.document.render` builtin tool (ADR-135 D6):
    /// the document's text and its format; a title and a file name optional.
    fn schema_aegis_document_render() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The document's text, in Markdown or plain text."
                },
                "format": {
                    "type": "string",
                    "enum": ["pdf", "docx", "html", "md"],
                    "description": "The file format to render: pdf, docx, html or md."
                },
                "title": {
                    "type": "string",
                    "description": "The document's title, shown at its head; it names the file when no filename is given."
                },
                "filename": {
                    "type": "string",
                    "description": "The file's name, without a directory; the format's extension is added."
                }
            },
            "required": ["content", "format"]
        })
    }

    /// JSON schema for the `aegis.task.status` builtin tool.
    fn schema_aegis_task_status() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution to check."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.task.wait` builtin tool.
    fn schema_aegis_task_wait() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution to wait for."
                },
                "poll_interval_seconds": {
                    "type": "integer",
                    "description": "Seconds between status polls (default 10).",
                    "minimum": 1
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum seconds to wait before returning a timeout (default 300).",
                    "minimum": 1
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.agent.wait` builtin tool.
    fn schema_aegis_agent_wait() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution to wait for."
                },
                "poll_interval_seconds": {
                    "type": "integer",
                    "description": "Seconds between status polls (default 10).",
                    "minimum": 1
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum seconds to wait before returning a timeout (default 300).",
                    "minimum": 1
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.task.logs` builtin tool.
    fn schema_aegis_task_logs() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution whose event log should be retrieved."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of events to return.",
                    "default": 50
                },
                "offset": {
                    "type": "integer",
                    "description": "Zero-based starting offset into the persisted event log.",
                    "default": 0
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.task.list` builtin tool.
    fn schema_aegis_task_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Optional UUID to filter by agent."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results.",
                    "default": 20
                }
            }
        })
    }

    /// JSON schema for the `aegis.task.cancel` builtin tool.
    fn schema_aegis_task_cancel() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution to cancel."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.task.remove` builtin tool.
    fn schema_aegis_task_remove() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "UUID of the execution to remove."
                }
            },
            "required": ["execution_id"]
        })
    }

    /// JSON schema for the `aegis.system.info` builtin tool.
    fn schema_aegis_system_info() -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    /// JSON schema for the `aegis.system.config` builtin tool.
    fn schema_aegis_system_config() -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    /// JSON schema for the `aegis.edge.fleet.list` builtin tool.
    fn schema_aegis_edge_fleet_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "description": "EdgeTarget: { Node: '...' } | { Group: '...' } | { Selector: { os, arch, tools, labels, tags } } | 'All'."
                }
            },
            "required": ["target"]
        })
    }

    /// JSON schema for the `aegis.edge.fleet.invoke` builtin tool.
    fn schema_aegis_edge_fleet_invoke() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": { "description": "EdgeTarget — see aegis.edge.fleet.list." },
                "tool_name": { "type": "string", "description": "Name of the tool to dispatch on every resolved edge daemon." },
                "args": { "type": "object", "description": "JSON object of tool arguments." }
            },
            "required": ["target", "tool_name"]
        })
    }

    /// JSON schema for the `aegis.edge.fleet.cancel` builtin tool.
    fn schema_aegis_edge_fleet_cancel() -> Value {
        json!({
            "type": "object",
            "properties": {
                "fleet_command_id": { "type": "string", "description": "UUID returned by aegis.edge.fleet.invoke." }
            },
            "required": ["fleet_command_id"]
        })
    }

    /// JSON schema for the `aegis.agent.search` builtin tool.
    fn schema_aegis_agent_search() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of the agent you are looking for."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Tenant ID to search within. Defaults to current tenant."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results (1-100, tier-dependent cap). Default: 10."
                },
                "min_score": {
                    "type": "number",
                    "description": "Minimum relevance score threshold (0.0-1.0). Default: 0.3."
                },
                "labels": {
                    "type": "object",
                    "description": "Label key-value pairs to filter by. All must match.",
                    "additionalProperties": { "type": "string" }
                },
                "status": {
                    "type": "string",
                    "description": "Filter by agent status.",
                    "enum": ["active", "paused", "failed"]
                },
                "include_platform_templates": {
                    "type": "boolean",
                    "description": "Include platform-provided template agents. Default: true."
                }
            },
            "required": ["query"]
        })
    }

    /// JSON schema for the `aegis.workflow.search` builtin tool.
    fn schema_aegis_workflow_search() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of the workflow you are looking for."
                },
                "tenant_id": {
                    "type": "string",
                    "description": "Tenant ID to search within. Defaults to current tenant."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results (1-100, tier-dependent cap). Default: 10."
                },
                "min_score": {
                    "type": "number",
                    "description": "Minimum relevance score threshold (0.0-1.0). Default: 0.3."
                },
                "labels": {
                    "type": "object",
                    "description": "Label key-value pairs to filter by. All must match.",
                    "additionalProperties": { "type": "string" }
                },
                "include_platform_templates": {
                    "type": "boolean",
                    "description": "Include platform-provided template workflows. Default: true."
                }
            },
            "required": ["query"]
        })
    }

    /// JSON schema for the `aegis.workflow.create` builtin tool.
    fn schema_aegis_workflow_create() -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest_yaml": {
                    "type": "string",
                    "description": "Full Workflow manifest YAML to parse, validate, semantically judge, and register."
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing version if it already exists."
                },
                "task_context": {
                    "type": "string",
                    "description": "Optional task context to guide semantic judges."
                },
                "judge_agents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Judge agent names to use for semantic validation."
                },
                "min_score": {
                    "type": "number",
                    "description": "Minimum consensus score required for deployment."
                },
                "min_confidence": {
                    "type": "number",
                    "description": "Minimum consensus confidence required for deployment."
                }
            },
            "required": ["manifest_yaml"]
        })
    }

    /// JSON schema for the `aegis.runtime.list` builtin tool.
    fn schema_aegis_runtime_list() -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "description": "Optional: filter by language name (e.g. \"python\", \"go\")"
                }
            }
        })
    }

    /// JSON schema for the `aegis.execution.file` builtin tool.
    fn schema_aegis_execution_file() -> Value {
        json!({
            "type": "object",
            "properties": {
                "execution_id": {
                    "type": "string",
                    "description": "The execution ID."
                },
                "path": {
                    "type": "string",
                    "description": "File path (e.g. 'output.md' or '/workspace/output.md')."
                }
            },
            "required": ["execution_id", "path"]
        })
    }

    /// JSON schema for the `aegis.attachment.read` builtin tool.
    fn schema_aegis_attachment_read() -> Value {
        json!({
            "type": "object",
            "properties": {
                "volume_id": {
                    "type": "string",
                    "description": "UUID of the tenant-scoped volume containing the attachment."
                },
                "path": {
                    "type": "string",
                    "description": "POSIX path of the attachment within the volume."
                }
            },
            "required": ["volume_id", "path"]
        })
    }

    /// Returns `true` if the operator has flagged `tool_name` to bypass the inner-loop
    /// semantic judge in a builtin dispatcher's capability entry.
    ///
    /// Called by `ToolInvocationService::invoke_tool_internal` before running the
    /// `spec.execution.tool_validation` pipeline (see ADR-049 and NODE_CONFIGURATION_SPEC_V1.md).
    pub async fn is_skip_judge(&self, tool_name: &str) -> bool {
        for dispatcher in &self.builtin_dispatchers {
            for cap in &dispatcher.capabilities {
                if cap.name == tool_name && cap.skip_judge {
                    return true;
                }
            }
        }

        false
    }

    /// Whether a call of `tool_name` waits for its user at the approval gate
    /// (AEGIS ADR-126 D1): the tool catalogue's entry is marked, or a
    /// capability entry of the node configuration (a builtin dispatcher's, or
    /// a `spec.tool_capabilities` entry whose pattern matches) carries
    /// `requires_approval: true`. Any mark gates; none can clear another.
    pub fn requires_approval(&self, tool_name: &str) -> bool {
        if BuiltinToolDefinition::lookup(tool_name).is_some_and(|d| d.requires_approval) {
            return true;
        }
        if self
            .builtin_dispatchers
            .iter()
            .flat_map(|d| d.capabilities.iter())
            .any(|cap| cap.name == tool_name && cap.requires_approval)
        {
            return true;
        }
        self.tool_capabilities
            .iter()
            .any(|entry| entry.requires_approval && entry.matches(tool_name))
    }

    /// What `tool_name` declares to the approval gate (AEGIS ADR-126, Update
    /// of 2026-10-04, clause 1): its input contract's declaration where it
    /// makes one, otherwise its builtin dispatcher's capability entry,
    /// otherwise the first `spec.tool_capabilities` entry, in the order the
    /// configuration lists them, whose pattern matches; otherwise nothing,
    /// and the gate's fallback applies.
    pub fn approval_contract(&self, tool_name: &str) -> ApprovalContract {
        let declared = crate::domain::mcp::ToolInputContract::approval_contract(tool_name);
        if !declared.is_empty() {
            return declared;
        }
        if let Some(cap) = self
            .builtin_dispatchers
            .iter()
            .flat_map(|d| d.capabilities.iter())
            .find(|cap| cap.name == tool_name)
        {
            return cap.approval_contract();
        }
        self.tool_capabilities
            .iter()
            .find(|entry| entry.matches(tool_name))
            .map(|entry| entry.approval_contract())
            .unwrap_or_default()
    }
}

/// Which outbound mail tool a schema is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundShape {
    Draft,
    Send,
    Reply,
}

/// Which calendar tool a schema is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalendarShape {
    Calendars,
    List,
    Read,
    Create,
    Update,
    Delete,
    Respond,
}

// =============================================================================
// ToolMetadata — Tool discovery metadata
// =============================================================================

/// Tool metadata exposed to LLM prompts for tool discovery and schema injection.
///
/// Fields are serialized as camelCase to match the MCP protocol specification
/// (e.g., `input_schema` → `inputSchema`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// ADR-117: when set to `"edge"` the dispatcher routes this tool through
    /// the EdgeRouter instead of the local builtin / MCP / SEAL chain. Default
    /// `None` keeps every existing tool on its current path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// ADR-117: when `true` the tool is eligible for fleet (multi-target) fan
    /// out via `aegis.edge.fleet.invoke`. Default `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fleet_capable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::node_config::{BuiltinDispatcherConfig, CapabilityConfig};

    #[tokio::test]
    async fn test_list_tools_includes_aegis_authoring_tool_schemas() {
        let builtins = vec![
            BuiltinDispatcherConfig {
                name: "aegis.agent.create".to_string(),
                description: "Create and deploy agent manifests".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.agent.create".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.create".to_string(),
                description: "Create, validate, and register workflows".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.create".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.task.logs".to_string(),
                description: "Inspect persisted task execution events".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.task.logs".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.status".to_string(),
                description: "Inspect workflow execution state".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.status".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
        ];

        let router = ToolRouter::new(builtins);
        let tools = router.list_tools().await.unwrap();

        let agent_tool = tools.iter().find(|t| t.name == "aegis.agent.create");
        assert!(agent_tool.is_some(), "expected aegis.agent.create tool");
        let agent_schema = &agent_tool.unwrap().input_schema;
        assert_eq!(
            agent_schema["required"][0].as_str(),
            Some("manifest_yaml"),
            "manifest_yaml must be required for aegis.agent.create"
        );

        let workflow_tool = tools.iter().find(|t| t.name == "aegis.workflow.create");
        assert!(
            workflow_tool.is_some(),
            "expected aegis.workflow.create tool"
        );
        let workflow_schema = &workflow_tool.unwrap().input_schema;
        assert_eq!(
            workflow_schema["required"][0].as_str(),
            Some("manifest_yaml"),
            "manifest_yaml must be required for aegis.workflow.create"
        );
        assert!(
            workflow_schema["properties"]["judge_agents"].is_object(),
            "judge_agents property should be present in workflow schema"
        );

        let task_logs_tool = tools.iter().find(|t| t.name == "aegis.task.logs");
        assert!(task_logs_tool.is_some(), "expected aegis.task.logs tool");
        let task_logs_schema = &task_logs_tool.unwrap().input_schema;
        assert_eq!(
            task_logs_schema["required"][0].as_str(),
            Some("execution_id"),
            "execution_id must be required for aegis.task.logs"
        );
        assert_eq!(
            task_logs_schema["properties"]["limit"]["default"],
            json!(50)
        );
        assert_eq!(
            task_logs_schema["properties"]["offset"]["default"],
            json!(0)
        );

        let workflow_status_tool = tools.iter().find(|t| t.name == "aegis.workflow.status");
        assert!(
            workflow_status_tool.is_some(),
            "expected aegis.workflow.status tool"
        );
        let workflow_status_schema = &workflow_status_tool.unwrap().input_schema;
        assert_eq!(
            workflow_status_schema["required"][0].as_str(),
            Some("execution_id"),
            "execution_id must be required for aegis.workflow.status"
        );
    }

    #[tokio::test]
    async fn test_list_tools_advertises_implemented_workflow_builtins() {
        let builtins = vec![
            BuiltinDispatcherConfig {
                name: "aegis.workflow.cancel".to_string(),
                description: "Cancel workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.cancel".to_string(),
                    skip_judge: false,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.signal".to_string(),
                description: "Signal workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.signal".to_string(),
                    skip_judge: false,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.remove".to_string(),
                description: "Remove workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.remove".to_string(),
                    skip_judge: false,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.status".to_string(),
                description: "Inspect workflow execution state".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.status".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
        ];

        let router = ToolRouter::new(builtins);
        let tools = router.list_tools().await.unwrap();

        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.status"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.cancel"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.signal"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.remove"));
    }

    /// Regression: the three attachment-capable MCP tools must declare
    /// `attachments` as a top-level optional array in their input schemas
    /// so clients listing tools via `tools/list` can discover the field
    /// (ADR-113). Verified via serde_json Value traversal — not substring
    /// matching — so the structural shape is enforced.
    fn assert_attachments_property_shape(schema: &Value, tool_name: &str) {
        let attachments = &schema["properties"]["attachments"];
        assert!(
            attachments.is_object(),
            "{tool_name}: expected top-level `attachments` property of type object"
        );
        assert_eq!(
            attachments["type"].as_str(),
            Some("array"),
            "{tool_name}: attachments must be an array"
        );

        let items = &attachments["items"];
        assert!(
            items.is_object(),
            "{tool_name}: attachments.items must be an object schema"
        );
        assert_eq!(
            items["type"].as_str(),
            Some("object"),
            "{tool_name}: attachments.items.type must be 'object'"
        );

        let required = items["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool_name}: attachments.items.required must be an array"));
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        for field in ["volume_id", "path", "name", "mime_type", "size"] {
            assert!(
                required_names.contains(&field),
                "{tool_name}: attachments.items.required must include `{field}`, got {required_names:?}"
            );
        }

        let item_props = &items["properties"];
        assert!(
            item_props.is_object(),
            "{tool_name}: attachments.items.properties must be an object"
        );
        for (field, expected_type) in [
            ("volume_id", "string"),
            ("path", "string"),
            ("name", "string"),
            ("mime_type", "string"),
            ("size", "integer"),
            ("sha256", "string"),
        ] {
            assert_eq!(
                item_props[field]["type"].as_str(),
                Some(expected_type),
                "{tool_name}: attachments.items.properties.{field} must be of type `{expected_type}`"
            );
        }
    }

    #[tokio::test]
    async fn test_attachment_capable_tool_schemas_declare_attachments() {
        let builtins = vec![
            BuiltinDispatcherConfig {
                name: "aegis.task.execute".to_string(),
                description: "Starts a new agent execution (task)".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.task.execute".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.agent.generate".to_string(),
                description: "Generates an Agent manifest from a natural-language intent."
                    .to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.agent.generate".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
            BuiltinDispatcherConfig {
                name: "aegis.execute.intent".to_string(),
                description: "Intent-to-execution pipeline".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.execute.intent".to_string(),
                    skip_judge: true,
                    requires_approval: false,
                    binding_argument: None,
                    approval_summary: None,
                }],
                api_key: None,
            },
        ];

        let router = ToolRouter::new(builtins);
        let tools = router.list_tools().await.unwrap();

        for tool_name in [
            "aegis.task.execute",
            "aegis.agent.generate",
            "aegis.execute.intent",
        ] {
            let tool = tools
                .iter()
                .find(|t| t.name == tool_name)
                .unwrap_or_else(|| panic!("expected `{tool_name}` to be advertised"));
            assert_attachments_property_shape(&tool.input_schema, tool_name);
        }
    }

    // -----------------------------------------------------------------------
    // Regression tests for the builtin tool registry consolidation.
    //
    // Prior to consolidation the registry was fragmented across four
    // separate constants (`BUILTIN_TOOL_DEFINITIONS` as `(name, description)`
    // tuples, plus three sibling `&[&str]` slices: `SKIP_JUDGE_TOOLS`,
    // `EDGE_EXECUTOR_TOOLS`, `FLEET_CAPABLE_TOOLS`). These tests freeze the
    // pre-consolidation membership of each sibling slice and assert that the
    // corresponding boolean flag on every consolidated `BuiltinToolDefinition`
    // remains `true`. They are deliberately written as test-as-checklist:
    // each list is hard-coded so future drift is caught here, not in
    // production.
    // -----------------------------------------------------------------------

    /// Pre-consolidation `SKIP_JUDGE_TOOLS` membership (frozen).
    const FROZEN_SKIP_JUDGE_TOOLS: &[&str] = &[
        "fs.read",
        "fs.list",
        "fs.grep",
        "fs.glob",
        "web.search",
        "web.fetch",
        "mail.list",
        "mail.read",
        "mail.attachment",
        "calendar.calendars",
        "calendar.list",
        "calendar.read",
        "aegis.schema.get",
        "aegis.schema.validate",
        "aegis.agent.create",
        "aegis.agent.update",
        "aegis.agent.delete",
        "aegis.workflow.create",
        "aegis.workflow.update",
        "aegis.workflow.list",
        "aegis.workflow.export",
        "aegis.workflow.validate",
        "aegis.workflow.status",
        "aegis.workflow.logs",
        "aegis.workflow.executions.list",
        "aegis.workflow.executions.get",
        "aegis.workflow.wait",
        "aegis.workflow.cancel",
        "aegis.workflow.signal",
        "aegis.workflow.remove",
        "aegis.workflow.search",
        "aegis.agent.list",
        "aegis.agent.export",
        "aegis.agent.logs",
        "aegis.agent.search",
        "aegis.task.status",
        "aegis.task.list",
        "aegis.task.logs",
        "aegis.task.wait",
        "aegis.agent.wait",
        "aegis.execute.status",
        "aegis.execute.wait",
        "aegis.tools.list",
        "aegis.tools.search",
        "aegis.system.info",
        "aegis.system.config",
        "aegis.runtime.list",
        "aegis.execution.file",
        "aegis.attachment.read",
        "aegis.edge.fleet.list",
        "aegis.edge.fleet.invoke",
        "aegis.edge.fleet.cancel",
        // AEGIS ADR-126 D4: a read-only status lookup, added deliberately.
        "aegis.approval.status",
        // AEGIS ADR-131 D2: the turn's goal tools, never an agent's inner
        // loop (no agent context admits them), added deliberately.
        "aegis.goal.create",
        "aegis.goal.evaluate",
        "aegis.goal.status",
        // AEGIS ADR-131 U33: the person's stop, called by the companion's
        // model or the person's page, never by an agent's inner loop (no
        // agent context admits it), added deliberately.
        "aegis.goal.cancel",
        // AEGIS ADR-135 D1f: the renderer's tool takes the judge choice
        // aegis.task.wait has, added deliberately.
        "aegis.document.render",
        // AEGIS ADR-136 G7: a run's read-only git tools, added
        // deliberately; commit and push stay judged.
        "aegis.git.status",
        // The person's git repository bindings, read only, added
        // deliberately.
        "aegis.git.list",
        "aegis.git.diff",
        // AEGIS ADR-139 N11: the schedule tools that only read, added
        // deliberately; create, update, pause, resume and delete stay judged.
        "aegis.schedule.list",
        "aegis.schedule.get",
        "aegis.schedule.runs",
    ];

    /// Pre-consolidation `EDGE_EXECUTOR_TOOLS` membership (frozen).
    const FROZEN_EDGE_EXECUTOR_TOOLS: &[&str] = &[
        "aegis.edge.fleet.list",
        "aegis.edge.fleet.invoke",
        "aegis.edge.fleet.cancel",
    ];

    /// Pre-consolidation `FLEET_CAPABLE_TOOLS` membership (frozen).
    const FROZEN_FLEET_CAPABLE_TOOLS: &[&str] = &["aegis.edge.fleet.invoke"];

    #[test]
    fn every_skip_judge_tool_in_old_list_remains_skip_judge() {
        for name in FROZEN_SKIP_JUDGE_TOOLS {
            let def = BuiltinToolDefinition::lookup(name).unwrap_or_else(|| {
                panic!(
                    "tool `{name}` from frozen SKIP_JUDGE_TOOLS list is missing from \
                     BUILTIN_TOOL_DEFINITIONS"
                )
            });
            assert!(
                def.skip_judge,
                "tool `{name}` was in SKIP_JUDGE_TOOLS before consolidation but \
                 BuiltinToolDefinition::skip_judge is now false — regression!"
            );
        }
    }

    #[test]
    fn every_edge_executor_tool_in_old_list_remains_edge_executor() {
        for name in FROZEN_EDGE_EXECUTOR_TOOLS {
            let def = BuiltinToolDefinition::lookup(name).unwrap_or_else(|| {
                panic!(
                    "tool `{name}` from frozen EDGE_EXECUTOR_TOOLS list is missing from \
                     BUILTIN_TOOL_DEFINITIONS"
                )
            });
            assert!(
                def.edge_executor,
                "tool `{name}` was in EDGE_EXECUTOR_TOOLS before consolidation but \
                 BuiltinToolDefinition::edge_executor is now false — regression!"
            );
        }
    }

    #[test]
    fn every_fleet_capable_tool_in_old_list_remains_fleet_capable() {
        for name in FROZEN_FLEET_CAPABLE_TOOLS {
            let def = BuiltinToolDefinition::lookup(name).unwrap_or_else(|| {
                panic!(
                    "tool `{name}` from frozen FLEET_CAPABLE_TOOLS list is missing from \
                     BUILTIN_TOOL_DEFINITIONS"
                )
            });
            assert!(
                def.fleet_capable,
                "tool `{name}` was in FLEET_CAPABLE_TOOLS before consolidation but \
                 BuiltinToolDefinition::fleet_capable is now false — regression!"
            );
        }
    }

    #[test]
    fn no_unexpected_skip_judge_flags_outside_frozen_set() {
        // Every tool with `skip_judge == true` in the registry must appear in
        // the frozen pre-consolidation list. Catches the inverse drift:
        // accidentally flagging a tool that was previously NOT skip-judge.
        let frozen: std::collections::HashSet<&str> =
            FROZEN_SKIP_JUDGE_TOOLS.iter().copied().collect();
        for def in BUILTIN_TOOL_DEFINITIONS {
            if def.skip_judge {
                assert!(
                    frozen.contains(def.name),
                    "tool `{}` is now flagged skip_judge but was not in the frozen \
                     pre-consolidation list — was this an intentional behavior change?",
                    def.name,
                );
            }
        }
    }

    #[test]
    fn is_supported_builtin_workflow_tool_derives_from_registry() {
        // Every aegis.workflow.* / aegis.execute.* tool present in the
        // canonical registry must be reported as supported.
        for def in BUILTIN_TOOL_DEFINITIONS {
            if def.name.starts_with("aegis.workflow.") || def.name.starts_with("aegis.execute.") {
                assert!(
                    ToolRouter::is_supported_builtin_workflow_tool(def.name),
                    "registry-listed tool `{}` should be reported as a supported \
                     builtin workflow tool",
                    def.name,
                );
            }
        }

        // Nonsense names with the right prefix must be rejected.
        assert!(!ToolRouter::is_supported_builtin_workflow_tool(
            "aegis.workflow.does_not_exist"
        ));
        assert!(!ToolRouter::is_supported_builtin_workflow_tool(
            "aegis.execute.bogus"
        ));

        // Non-prefix tools must be rejected even if they are in the registry.
        assert!(!ToolRouter::is_supported_builtin_workflow_tool("cmd.run"));
        assert!(!ToolRouter::is_supported_builtin_workflow_tool("fs.read"));

        // Wholly unknown tool names must be rejected.
        assert!(!ToolRouter::is_supported_builtin_workflow_tool(
            "totally.unrelated.tool"
        ));
    }

    #[test]
    fn schema_for_builtin_returns_specific_schema_for_known_tool() {
        // aegis.workflow.create — manifest_yaml must be required.
        let wf_create = ToolRouter::schema_for_builtin("aegis.workflow.create");
        assert_eq!(wf_create["type"], json!("object"));
        let wf_required = wf_create["required"]
            .as_array()
            .expect("aegis.workflow.create must declare a `required` array");
        assert!(
            wf_required
                .iter()
                .any(|v| v.as_str() == Some("manifest_yaml")),
            "aegis.workflow.create.required must include `manifest_yaml`, got {wf_required:?}"
        );
        assert!(
            wf_create["properties"]["manifest_yaml"].is_object(),
            "aegis.workflow.create.properties.manifest_yaml must exist"
        );

        // aegis.runtime.list — properties.language is the only declared field.
        let rt_list = ToolRouter::schema_for_builtin("aegis.runtime.list");
        assert_eq!(rt_list["type"], json!("object"));
        assert!(
            rt_list["properties"]["language"].is_object(),
            "aegis.runtime.list.properties.language must exist"
        );

        // cmd.run — `command` required.
        let cmd_run = ToolRouter::schema_for_builtin("cmd.run");
        let cmd_required = cmd_run["required"]
            .as_array()
            .expect("cmd.run must declare a `required` array");
        assert!(cmd_required.iter().any(|v| v.as_str() == Some("command")));
    }

    /// The model reads this schema as the function's `parameters`; on
    /// 2026-10-01 it declared only `command`, and the model sent
    /// `env_additions` and `args` as JSON-encoded strings.
    #[test]
    fn cmd_run_schema_declares_args_and_env_additions_shapes() {
        let cmd_run = ToolRouter::schema_for_builtin("cmd.run");
        let props = &cmd_run["properties"];
        assert_eq!(
            props["args"]["type"],
            json!("array"),
            "cmd.run schema: {cmd_run}"
        );
        assert_eq!(
            props["args"]["items"]["type"],
            json!("string"),
            "cmd.run schema: {cmd_run}"
        );
        assert_eq!(
            props["env_additions"]["type"],
            json!("object"),
            "cmd.run schema: {cmd_run}"
        );
        assert_eq!(
            props["env_additions"]["additionalProperties"]["type"],
            json!("string"),
            "cmd.run schema: {cmd_run}"
        );
        let env_desc = props["env_additions"]["description"]
            .as_str()
            .expect("env_additions must carry a description");
        assert!(
            env_desc.contains("not a JSON string"),
            "env_additions description: {env_desc}"
        );
    }

    /// AEGIS ADR-040, Update of 2026-10-06, R2: `cmd.run` declares `stdin`,
    /// an optional string, so the model can feed a script that reads it.
    #[test]
    fn cmd_run_schema_declares_optional_stdin_string() {
        let cmd_run = ToolRouter::schema_for_builtin("cmd.run");
        assert_eq!(
            cmd_run["properties"]["stdin"]["type"],
            json!("string"),
            "cmd.run schema does not declare stdin as a string: {cmd_run}"
        );
        let desc = cmd_run["properties"]["stdin"]["description"]
            .as_str()
            .expect("stdin must carry a description");
        assert!(desc.contains("standard input"), "stdin description: {desc}");
        assert!(
            !cmd_run["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|v| v.as_str() == Some("stdin")),
            "stdin must be optional: {cmd_run}"
        );
    }

    #[test]
    fn schema_for_builtin_returns_empty_object_for_unknown_tool() {
        let bogus = ToolRouter::schema_for_builtin("totally.unknown.tool");
        assert_eq!(bogus, json!({ "type": "object" }));
    }

    /// web.fetch's schema and description state the bound on what one call
    /// returns and the argument that reads on (AEGIS ADR-124, the measured
    /// Update of 2026-10-01).
    #[test]
    fn web_fetch_schema_states_the_bound_and_the_offset_argument() {
        use crate::application::tools::builtin_web::WEB_FETCH_MAX_CHARS;
        let bound = "50,000";
        assert_eq!(WEB_FETCH_MAX_CHARS, 50_000, "the stated bound is the bound");

        let schema = ToolRouter::schema_for_builtin("web.fetch");
        let offset = &schema["properties"]["offset"];
        assert_eq!(offset["type"], json!("integer"), "schema: {schema}");
        assert_eq!(offset["minimum"], json!(0), "schema: {schema}");
        let offset_doc = offset["description"].as_str().unwrap_or_default();
        assert!(
            offset_doc.contains(bound) && offset_doc.contains("next_offset"),
            "the offset argument states the bound and how to read on: {offset_doc}"
        );
        assert_eq!(schema["required"], json!(["url"]), "offset is optional");

        let description = BUILTIN_TOOL_DEFINITIONS
            .iter()
            .find(|d| d.name == "web.fetch")
            .expect("web.fetch is a builtin")
            .description;
        assert!(
            description.contains(bound) && description.contains("offset"),
            "the description states the bound and the offset argument: {description}"
        );
    }

    fn tool_capabilities(yaml: &str) -> Vec<ToolCapabilityConfig> {
        serde_yaml::from_str(yaml).expect("spec.tool_capabilities entries parse")
    }

    /// AEGIS ADR-126 D1: a capability entry of the node configuration gates
    /// its tool, a builtin dispatcher's or a `spec.tool_capabilities` entry
    /// whose pattern matches; with no entry only the catalogue's marked
    /// tools, `mail.send` and `mail.reply` (AEGIS ADR-125's Update of
    /// 2026-10-07 (3) clause 12), `mail.delete` (its Update of 2026-10-08
    /// (4) clause 20), `mail.archive` and `mail.forward` (its Update of
    /// 2026-10-08 (5) clauses 29 and 35), the four calendar writes (AEGIS ADR-138 K7) and a
    /// workflow's landing, `aegis.git.land` (AEGIS ADR-141 F8), are gated, and
    /// an entry without the flag does not gate.
    #[test]
    fn requires_approval_follows_capability_entries_of_the_node_configuration() {
        let plain = ToolRouter::new(ToolRouter::builtin_dispatchers());
        for def in BUILTIN_TOOL_DEFINITIONS {
            let marked = matches!(
                def.name,
                "mail.send"
                    | "mail.reply"
                    | "mail.delete"
                    | "mail.archive"
                    | "mail.forward"
                    | "calendar.create"
                    | "calendar.update"
                    | "calendar.delete"
                    | "calendar.respond"
                    | "aegis.git.land"
            );
            assert_eq!(
                plain.requires_approval(def.name),
                marked,
                "{} is gated: {}, with no capability entry",
                def.name,
                plain.requires_approval(def.name)
            );
        }

        let dispatchers = vec![BuiltinDispatcherConfig {
            name: "aegis.system.info".to_string(),
            description: "info".to_string(),
            enabled: true,
            capabilities: vec![CapabilityConfig {
                name: "aegis.system.info".to_string(),
                skip_judge: true,
                requires_approval: true,
                binding_argument: None,
                approval_summary: None,
            }],
            api_key: None,
        }];
        let gated = ToolRouter::new(dispatchers).with_tool_capabilities(&tool_capabilities(
            "- tool_pattern: gmail.send\n  requires_approval: true\n- tool_pattern: gmail.list\n",
        ));
        assert!(gated.requires_approval("aegis.system.info"));
        assert!(gated.requires_approval("gmail.send"));
        assert!(!gated.requires_approval("gmail.list"));
        assert!(!gated.requires_approval("fs.read"));
    }

    /// ADR-132's Update (the coordinator's settlement of C1): a tool the
    /// orchestrator does not serve, such as a SEAL gateway tool of a remote
    /// server, is gated by a `spec.tool_capabilities` pattern with no server
    /// entry; `<prefix>.*` matches the tools under `<prefix>.` and no other.
    #[test]
    fn a_gateway_tool_matched_by_a_pattern_is_gated() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers()).with_tool_capabilities(
            &tool_capabilities(
                "- tool_pattern: nuclear-notes.pages.*\n  requires_approval: true\n- tool_pattern: nuclear-notes.search.literal\n  requires_approval: true\n",
            ),
        );
        assert!(router.requires_approval("nuclear-notes.pages.create"));
        assert!(router.requires_approval("nuclear-notes.pages.apply_patch"));
        assert!(router.requires_approval("nuclear-notes.search.literal"));
        assert!(!router.requires_approval("nuclear-notes.search.global"));
        assert!(!router.requires_approval("nuclear-notes.pagesx.create"));
        assert!(!router.requires_approval("nuclear-notes.pages"));

        let everything = ToolRouter::new(ToolRouter::builtin_dispatchers()).with_tool_capabilities(
            &tool_capabilities("- tool_pattern: \"*\"\n  requires_approval: true\n"),
        );
        assert!(everything.requires_approval("any.tool"));
    }

    /// `spec.tool_capabilities` refuses a key it does not know, so a key
    /// that once lived on an MCP server entry is not silently dropped.
    #[test]
    fn a_tool_capability_entry_with_an_unknown_key_is_refused() {
        let parsed: Result<Vec<ToolCapabilityConfig>, _> =
            serde_yaml::from_str("- tool_pattern: chat.post\n  skip_judge: true\n");
        assert!(parsed.is_err());
    }

    /// ADR-126, Update of 2026-10-04, clause 1: the gate's keys come from the
    /// tool's capability entry, a builtin dispatcher's or the first matching
    /// `spec.tool_capabilities` entry; a tool whose entry declares none gets
    /// the empty contract.
    #[test]
    fn approval_contract_comes_from_the_capability_entry() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers()).with_tool_capabilities(
            &tool_capabilities(
                "- tool_pattern: chat.post\n  requires_approval: true\n  binding_argument: workspace\n  approval_summary: [channel, text]\n- tool_pattern: chat.*\n  binding_argument: other\n- tool_pattern: chat.list\n",
            ),
        );
        assert_eq!(
            router.approval_contract("chat.post"),
            ApprovalContract {
                binding_argument: Some("workspace".to_string()),
                approval_summary: Some(vec!["channel".to_string(), "text".to_string()]),
            }
        );
        assert_eq!(
            router.approval_contract("chat.edit"),
            ApprovalContract {
                binding_argument: Some("other".to_string()),
                approval_summary: None,
            }
        );
        assert!(router.approval_contract("fs.write").is_empty());
        // ADR-125's Update of 2026-10-07 (3) clause 12: the input contract's
        // declaration, whatever the capability entries say.
        assert_eq!(
            router.approval_contract("mail.send"),
            ApprovalContract {
                binding_argument: Some("mailbox".to_string()),
                approval_summary: Some(
                    ["mailbox", "to", "cc", "subject", "body", "attachment_names"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                ),
            }
        );
    }
}
