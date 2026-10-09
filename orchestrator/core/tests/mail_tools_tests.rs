// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The outbound mail tools in the tool service (AEGIS ADR-125 D4, its Update
//! of 2026-10-07 (3) clauses 11, 12 and 14; ADR-126):
//!
//! - the catalogue lists the six mail tools, each with a contract declaring
//!   `mailbox`; `mail.send` and `mail.reply` are gated by the catalogue and
//!   declare the approval contract, `mail.draft` is not gated;
//! - a gated call answers `approval_pending` with the declared summary and
//!   reaches no mail server, while `mail.draft` runs;
//! - the mailbox is admitted before the gate: an ungranted mailbox is
//!   refused with its sentence and no approval row is written, and a
//!   mailbox named by its context name is stored by its binding id;
//! - a send approved once runs over XOAUTH2, and its token appears in no
//!   result, event or log line;
//! - `mail.delete` (its Update of 2026-10-08 (4) clause 20) is gated with
//!   the contract `mailbox`, `thread_id`, `subject`, `from`; its admission
//!   reads the thread's subject and senders into the call over the model's
//!   values and nothing moves before approval; a call its mailbox, thread
//!   or Trash would refuse is refused before the gate with no row; a
//!   delete approved once runs over XOAUTH2 with its token nowhere;
//! - `mail.archive` (its Update of 2026-10-08 (5) clauses 27 to 29) moves a
//!   thread's `INBOX` messages to the Archive folder (`\Archive`, else the
//!   name `Archive`, else `\All`) by `UID MOVE`, else by `UID COPY`, `UID
//!   STORE +FLAGS (\Deleted)` and `UID EXPUNGE` of those uids, the rest of
//!   `INBOX` untouched and no plain `EXPUNGE` sent; it refuses with nothing
//!   changed where the mailbox has no Archive folder or the server can do
//!   neither; it is gated by the catalogue with the contract `mailbox`,
//!   `thread_id`, `subject`, `from`, a capability entry at `false` does not
//!   clear the mark, and a call its admission refuses writes no row.
//!
//! Sessions in detail are tested in `mail_session_tests.rs`.

#[path = "support/mail_standins.rs"]
mod mail_standins;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::credential_service::{
    ContextBinding, ToolCallActor, ToolMailbox, ToolMailboxSource,
};
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
use aegis_orchestrator_core::application::tool_invocation_service::{
    ToolInvocationResult, ToolInvocationService,
};
use aegis_orchestrator_core::application::tools::builtin_mail::{
    MailActing, MailTools, NOT_GRANTED,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, MailSecurity, MailboxSettings,
};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, ServerChoice,
};
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::mcp::ToolInputContract;
use aegis_orchestrator_core::domain::node_config::ToolCapabilityConfig;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::seal_session::{CallerAnswer, SealSessionError};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::security_context::SecurityContext;
use aegis_orchestrator_core::domain::security_context::SecurityContextRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ApprovalContract, ToolApprovalDecision, ToolApprovalRepository, ToolApprovalStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::mail::MailAuth;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::Stream;
use mail_standins::{
    imap_mailbox_standin_with_capabilities, imap_mailbox_standin_with_folders,
    imap_xoauth2_standin_with_capabilities, imap_xoauth2_standin_with_folders,
    smtp_submission_standin, xoauth2_string, MailboxStandIn, PlainConnector, SmtpSubmission,
    StandInFolder, StoredMessage, SubmitAuth,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

const CONTEXT: &str = "mail-send-test-context";
const USER: &str = "mail-send-user";
const ADDRESS: &str = "owner@example.test";
const PASSWORD: &str = "Mk11-mailbox-password";
const TOKEN: &str = "ya29.Mk11-xoauth2-access-token";
const ANN: &str = "ann@example.test";

// ---------------------------------------------------------------------------
// The catalogue's entries and contracts (clause 12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_eight_mail_tools_list_with_contracts_declaring_mailbox_and_four_are_gated() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let mut wrong = Vec::new();
    for (name, required, gated) in [
        ("mail.list", vec!["mailbox"], false),
        ("mail.read", vec!["mailbox", "thread_id"], false),
        ("mail.label", vec!["mailbox", "thread_id"], false),
        ("mail.draft", vec!["mailbox", "body"], false),
        ("mail.send", vec!["mailbox", "to", "subject", "body"], true),
        (
            "mail.reply",
            vec!["mailbox", "thread_id", "to", "subject", "body"],
            true,
        ),
        ("mail.delete", vec!["mailbox", "thread_id"], true),
        ("mail.archive", vec!["mailbox", "thread_id"], true),
    ] {
        let Some(tool) = tools.iter().find(|t| t.name == name) else {
            wrong.push(format!("{name} is not listed"));
            continue;
        };
        if tool.input_schema["properties"]["mailbox"]["type"] != "string" {
            wrong.push(format!("{name}'s schema does not declare mailbox"));
        }
        let schema_required: Vec<&str> = tool.input_schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if schema_required != required {
            wrong.push(format!("{name}'s schema requires {schema_required:?}"));
        }
        if ToolInputContract::required_fields(name) != required.as_slice() {
            wrong.push(format!(
                "{name}'s input contract requires {:?}",
                ToolInputContract::required_fields(name)
            ));
        }
        if router.requires_approval(name) != gated {
            wrong.push(format!(
                "{name} is gated: {}",
                router.requires_approval(name)
            ));
        }
        let summary: &[&str] = if matches!(name, "mail.delete" | "mail.archive") {
            &["mailbox", "thread_id", "subject", "from"]
        } else {
            &["mailbox", "to", "cc", "subject", "body"]
        };
        let expected = if gated {
            ApprovalContract {
                binding_argument: Some("mailbox".to_string()),
                approval_summary: Some(summary.iter().map(|s| s.to_string()).collect()),
            }
        } else {
            ApprovalContract::default()
        };
        if router.approval_contract(name) != expected {
            wrong.push(format!(
                "{name}'s approval contract is {:?}",
                router.approval_contract(name)
            ));
        }
    }
    for name in [
        "mail.draft",
        "mail.send",
        "mail.reply",
        "mail.delete",
        "mail.archive",
    ] {
        if router.is_skip_judge(name).await {
            wrong.push(format!("{name} skips the judge"));
        }
        if let Some(tool) = tools.iter().find(|t| t.name == name) {
            if tool.input_schema["properties"].get("bcc").is_some() {
                wrong.push(format!("{name} offers bcc"));
            }
        }
    }
    // `mail.archive` (its Update of 2026-10-08 (5) clauses 27 and 29): it
    // says what it does and that it waits; it offers neither `folder` nor
    // the summary's `subject` and `from`, which the admission writes; and a
    // capability entry at `false` does not clear the catalogue's mark.
    match tools.iter().find(|t| t.name == "mail.archive") {
        Some(tool) => {
            if !tool.description.contains("to its Archive folder")
                || !tool
                    .description
                    .contains("Waits for the person's approval before anything is moved.")
            {
                wrong.push(format!("mail.archive's description: {}", tool.description));
            }
            for offered in ["folder", "subject", "from"] {
                if tool.input_schema["properties"].get(offered).is_some() {
                    wrong.push(format!("mail.archive offers {offered}"));
                }
            }
        }
        None => wrong.push("mail.archive is not listed".to_string()),
    }
    let entries: Vec<ToolCapabilityConfig> =
        serde_yaml::from_str("- tool_pattern: mail.archive\n  requires_approval: false\n").unwrap();
    let configured =
        ToolRouter::new(ToolRouter::builtin_dispatchers()).with_tool_capabilities(&entries);
    if !configured.requires_approval("mail.archive") {
        wrong.push("a capability entry at false cleared mail.archive's mark".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// `mail.attachment` (the Update of 2026-10-08 (5) clause 31) is listed
/// with `mailbox`, `uid` and `part` required and `folder` offered, is not
/// gated, declares no approval contract, and skips the judge as `mail.read`
/// does; its description says what it does, and `mail.read`'s says each
/// attachment comes with the part number it takes (clause 30).
#[tokio::test]
async fn mail_attachment_is_listed_ungated_skipping_the_judge_and_mail_read_names_its_part() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let mut wrong = Vec::new();
    let name = "mail.attachment";
    let required = ["mailbox", "uid", "part"];
    match tools.iter().find(|t| t.name == name) {
        Some(tool) => {
            let schema_required: Vec<&str> = tool.input_schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if schema_required != required {
                wrong.push(format!("{name}'s schema requires {schema_required:?}"));
            }
            if tool.input_schema["properties"]["folder"]["enum"]
                != json!(["inbox", "sent", "drafts", "trash", "archive", "all"])
            {
                wrong.push(format!("{name} does not offer the six folders"));
            }
            if tool.input_schema["properties"]["uid"]["type"] != "integer"
                || tool.input_schema["properties"]["part"]["type"] != "string"
            {
                wrong.push(format!("{name}'s uid and part: {}", tool.input_schema));
            }
            if !tool.description.starts_with(
                "Saves one attachment of a message in a connected mailbox to your files",
            ) {
                wrong.push(format!("{name}'s description: {}", tool.description));
            }
        }
        None => wrong.push(format!("{name} is not listed")),
    }
    if ToolInputContract::required_fields(name) != required.as_slice() {
        wrong.push(format!(
            "{name}'s input contract requires {:?}",
            ToolInputContract::required_fields(name)
        ));
    }
    if router.requires_approval(name) {
        wrong.push(format!("{name} is gated"));
    }
    if router.approval_contract(name) != ApprovalContract::default() {
        wrong.push(format!(
            "{name}'s approval contract is {:?}",
            router.approval_contract(name)
        ));
    }
    if !router.is_skip_judge(name).await {
        wrong.push(format!("{name} does not skip the judge"));
    }
    match tools.iter().find(|t| t.name == "mail.read") {
        Some(tool)
            if tool
                .description
                .contains("part number, which mail.attachment saves") => {}
        Some(tool) => wrong.push(format!("mail.read's description: {}", tool.description)),
        None => wrong.push("mail.read is not listed".to_string()),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// The tool service with mail tools and the approval gate
// ---------------------------------------------------------------------------

/// One of the person's mailboxes as the source answers it.
#[derive(Clone)]
struct Owned {
    id: CredentialBindingId,
    name: String,
    granted: bool,
    imap_port: u16,
    smtp_port: u16,
    auth: MailAuth,
}

/// The person's mailboxes, each answered to its owner only, with its
/// context name.
struct Mailboxes(Vec<Owned>);

#[async_trait]
impl ToolMailboxSource for Mailboxes {
    async fn tool_mailbox(
        &self,
        actor: &ToolCallActor<'_>,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        if actor.user_id != USER {
            return Ok(None);
        }
        Ok(self
            .0
            .iter()
            .find(|m| &m.id == binding_id)
            .map(|m| ToolMailbox {
                binding_id: m.id,
                settings: MailboxSettings {
                    address: ADDRESS.to_string(),
                    display_name: None,
                    imap_host: "127.0.0.1".to_string(),
                    imap_port: m.imap_port,
                    imap_security: MailSecurity::Tls,
                    smtp_host: "127.0.0.1".to_string(),
                    smtp_port: m.smtp_port,
                    smtp_security: MailSecurity::Tls,
                    username: ADDRESS.to_string(),
                },
                auth: m.auth.clone(),
                granted: m.granted,
            }))
    }

    async fn mailbox_contexts(
        &self,
        _tenant_id: &TenantId,
        user_id: &str,
    ) -> anyhow::Result<Vec<ContextBinding>> {
        if user_id != USER {
            return Ok(Vec::new());
        }
        Ok(self
            .0
            .iter()
            .map(|m| ContextBinding {
                id: m.id,
                name: m.name.clone(),
                reach: None,
            })
            .collect())
    }
}

fn inbox() -> Vec<StoredMessage> {
    vec![StoredMessage::new(
        1,
        &["\\Seen"],
        "05-Oct-2026 09:00:00 +0000",
        &[
            "Message-ID: <a1@x>",
            "From: Ann <ann@example.test>",
            "To: owner@example.test",
            "Subject: Invoice",
        ],
        "Please find the invoice.",
    )]
}

fn folders() -> Vec<StandInFolder> {
    vec![
        StandInFolder::new("Sent", "\\Sent"),
        StandInFolder::new("Drafts", "\\Drafts"),
    ]
}

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: mail-send-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["mail.list", "mail.read", "mail.label", "mail.draft", "mail.send", "mail.reply", "mail.delete"]
"#,
    )
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn security_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "mail send test".to_string(),
        capabilities: vec![
            aegis_orchestrator_core::domain::security_context::Capability {
                tool_pattern: "mail.*".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            },
        ],
        deny_list: vec![],
        metadata: aegis_orchestrator_core::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

struct Harness {
    service: Arc<ToolInvocationService>,
    approvals: Arc<ToolApprovalService>,
    repo: Arc<InMemoryToolApprovalRepository>,
    event_bus: Arc<EventBus>,
    agent_id: AgentId,
    runs: Vec<ExecutionId>,
}

/// A tool service with `mailboxes`, the approval gate and one execution per
/// entry of `runs` (its `contexts` input), each initiated by [`USER`].
async fn harness(mailboxes: Vec<Owned>, runs: &[Option<Value>]) -> Harness {
    let agent = agent();
    let agent_id = agent.id;
    let tenant = TenantId::default();
    let executions: Vec<Execution> = runs
        .iter()
        .map(|contexts| {
            let mut e = Execution::new_with_id(
                ExecutionId::new(),
                agent_id,
                ExecutionInput {
                    intent: None,
                    input: match contexts {
                        Some(contexts) => json!({ "contexts": contexts }),
                        None => json!({}),
                    },
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                5,
                CONTEXT.to_string(),
            );
            e.tenant_id = tenant.clone();
            e.initiating_user_sub = Some(USER.to_string());
            e
        })
        .collect();
    let ids = executions.iter().map(|e| e.id).collect();
    let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context())
        .await
        .unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-mail-send-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(1024));
    let repo = Arc::new(InMemoryToolApprovalRepository::new());
    let approvals = Arc::new(ToolApprovalService::new(repo.clone(), event_bus.clone()));
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        Arc::new(Executions(
            executions.into_iter().map(|e| (e.id, e)).collect(),
        )),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        event_bus.clone(),
        None,
    )
    .with_tool_approvals(approvals.clone())
    .with_mail_tools_over(Arc::new(Mailboxes(mailboxes)), Arc::new(PlainConnector));
    Harness {
        service: Arc::new(service),
        approvals,
        repo,
        event_bus,
        agent_id,
        runs: ids,
    }
}

impl Harness {
    async fn call(
        &self,
        run: usize,
        tool: &str,
        args: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        self.service
            .invoke_tool_internal(
                &self.agent_id,
                self.runs[run],
                TenantId::default(),
                0,
                Vec::new(),
                tool.to_string(),
                args,
            )
            .await
    }

    async fn rows(
        &self,
    ) -> Vec<aegis_orchestrator_core::domain::tool_approval::ToolApprovalRequest> {
        self.repo
            .list_requests_for_user(&TenantId::default(), USER, None)
            .await
            .unwrap()
    }
}

/// The value a call answered directly.
fn direct(result: &Result<ToolInvocationResult, SealSessionError>) -> Option<Value> {
    match result {
        Ok(ToolInvocationResult::Direct(value)) => Some(value.clone()),
        _ => None,
    }
}

/// The sentence a refusal tells its caller.
fn told(result: &Result<ToolInvocationResult, SealSessionError>) -> String {
    match result {
        Ok(ToolInvocationResult::Direct(value)) => format!("answered {value}"),
        Ok(_) => "the call was dispatched".to_string(),
        Err(SealSessionError::Answered {
            answer: CallerAnswer::CredentialBindingRequired { message },
            ..
        }) => message.clone(),
        Err(SealSessionError::InvalidArguments(message))
        | Err(SealSessionError::UpstreamUnavailable(message)) => message.clone(),
        Err(other) => format!("{other:?}"),
    }
}

async fn password_mailbox(name: &str, granted: bool) -> (MailboxStandIn, SmtpSubmission, Owned) {
    let mailbox = imap_mailbox_standin_with_folders(ADDRESS, PASSWORD, inbox(), folders()).await;
    let smtp = smtp_submission_standin(
        SubmitAuth::Plain {
            user: ADDRESS.to_string(),
            password: PASSWORD.to_string(),
        },
        None,
    )
    .await;
    let owned = Owned {
        id: CredentialBindingId::new(),
        name: name.to_string(),
        granted,
        imap_port: mailbox.port(),
        smtp_port: smtp.port(),
        auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
    };
    (mailbox, smtp, owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_and_reply_answer_approval_pending_with_the_declared_summary_and_draft_does_not() {
    let (mailbox, smtp, owned) = password_mailbox("Inbox", false).await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[Some(json!({ "imap": [id.clone()] }))]).await;
    let mut wrong = Vec::new();
    for (tool, args, summary) in [
        (
            "mail.send",
            json!({"mailbox": id, "to": [ANN], "subject": "Hello", "body": "Hi."}),
            format!("mail.send\nmailbox: {id}\nto: {ANN}\ncc: \nsubject: Hello\nbody: Hi."),
        ),
        (
            "mail.reply",
            json!({"mailbox": id, "thread_id": "<a1@x>", "to": [ANN], "cc": ["b@example.test"], "subject": "Re: Invoice", "body": "Got it."}),
            format!("mail.reply\nmailbox: {id}\nto: {ANN}\ncc: b@example.test\nsubject: Re: Invoice\nbody: Got it."),
        ),
    ] {
        let result = h.call(0, tool, args).await;
        match direct(&result) {
            Some(value) if value["status"] == "approval_pending" => {
                if value["summary"] != summary.as_str() {
                    wrong.push(format!("{tool}'s summary is {:?}", value["summary"]));
                }
            }
            _ => wrong.push(format!("{tool} did not wait for approval: {}", told(&result))),
        }
    }
    let draft = h
        .call(
            0,
            "mail.draft",
            json!({"mailbox": id, "to": [ANN], "subject": "Later", "body": "A draft."}),
        )
        .await;
    match direct(&draft) {
        Some(value) if value.get("message_id").is_some() => {}
        _ => wrong.push(format!("mail.draft did not run: {}", told(&draft))),
    }
    if mailbox.folder_messages("Drafts").len() != 1 {
        wrong.push("the draft was not saved".to_string());
    }
    let rows = h.rows().await;
    if rows.len() != 2
        || rows
            .iter()
            .any(|r| r.status != ToolApprovalStatus::Pending || r.tool_name == "mail.draft")
    {
        wrong.push(format!(
            "the approval rows are {:?}",
            rows.iter()
                .map(|r| (r.tool_name.clone(), r.status))
                .collect::<Vec<_>>()
        ));
    }
    if smtp.standin.connections() != 0 {
        wrong.push("a pending call reached the SMTP server".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ungranted_mailbox_is_refused_before_the_gate_and_no_row_is_written() {
    let (_mailbox, smtp, owned) = password_mailbox("Inbox", false).await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[None]).await;
    let mut events = h.event_bus.subscribe();
    let result = h
        .call(
            0,
            "mail.send",
            json!({"mailbox": id, "to": [ANN], "subject": "Hello", "body": "Hi."}),
        )
        .await;
    let mut wrong = Vec::new();
    if told(&result) != NOT_GRANTED {
        wrong.push(format!(
            "an ungranted mailbox was not refused before the gate: {}",
            told(&result)
        ));
    }
    if !h.rows().await.is_empty() {
        wrong.push(format!(
            "approval rows were written: {}",
            h.rows().await.len()
        ));
    }
    while let Ok(event) = events.try_recv() {
        if format!("{event:?}").contains("ApprovalRequested") {
            wrong.push("an approval was requested".to_string());
        }
    }
    if smtp.standin.connections() != 0 {
        wrong.push("the refused call reached the SMTP server".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// `mail.attachment` (the Update of 2026-10-08 (5) clause 31) reaches the
/// mail tools through the tool service and is not gated: no approval is
/// asked and no row is written. A node whose mail tools hold no file
/// services refuses it before any mail server is reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_attachment_reaches_the_mail_tools_ungated_and_needs_their_file_services() {
    let (mailbox, _smtp, owned) = password_mailbox("Inbox", true).await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[None]).await;
    let mut events = h.event_bus.subscribe();
    let result = h
        .call(
            0,
            "mail.attachment",
            json!({"mailbox": id, "uid": 1, "part": "2"}),
        )
        .await;
    let mut wrong = Vec::new();
    if !told(&result).contains("the mail tools are not configured on this node") {
        wrong.push(format!(
            "mail.attachment did not reach the mail tools: {}",
            told(&result)
        ));
    }
    if !h.rows().await.is_empty() {
        wrong.push("an approval row was written".to_string());
    }
    while let Ok(event) = events.try_recv() {
        if format!("{event:?}").contains("ApprovalRequested") {
            wrong.push("an approval was requested".to_string());
        }
    }
    if mailbox.connections() != 0 {
        wrong.push("a mail server was reached with no file services".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A gated or ungated inbox tool naming a folder other than the inbox is
/// refused before any connection and, for a gated tool, before the gate,
/// with no row written (AEGIS ADR-125's Update of 2026-10-08 (5) clause
/// 25); named `inbox`, the gated call waits for approval as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbox_tool_naming_another_folder_is_refused_before_the_gate_and_no_row_is_written() {
    const INBOX_ONLY: &str =
        "This tool works on threads in the inbox only; leave out 'folder' or set it to inbox.";
    let (mailbox, smtp, owned) = password_mailbox("Inbox", false).await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[Some(json!({ "imap": [id.clone()] }))]).await;
    let mut events = h.event_bus.subscribe();
    let mut wrong = Vec::new();
    for (tool, args) in [
        (
            "mail.reply",
            json!({"mailbox": id, "thread_id": "<a1@x>", "to": [ANN], "subject": "Re: Invoice", "body": "Got it."}),
        ),
        ("mail.delete", json!({"mailbox": id, "thread_id": "<a1@x>"})),
        (
            "mail.label",
            json!({"mailbox": id, "thread_id": "<a1@x>", "flagged": true}),
        ),
        ("mail.draft", json!({"mailbox": id, "body": "A draft."})),
    ] {
        for folder in ["sent", "drafts", "trash", "archive", "all"] {
            let mut args = args.clone();
            args["folder"] = json!(folder);
            let result = h.call(0, tool, args).await;
            if told(&result) != INBOX_ONLY {
                wrong.push(format!("{tool} in {folder}: {}", told(&result)));
            }
        }
    }
    if !h.rows().await.is_empty() {
        wrong.push(format!(
            "approval rows were written: {:?}",
            h.rows()
                .await
                .iter()
                .map(|r| r.tool_name.clone())
                .collect::<Vec<_>>()
        ));
    }
    while let Ok(event) = events.try_recv() {
        if format!("{event:?}").contains("ApprovalRequested") {
            wrong.push("an approval was requested".to_string());
        }
    }
    if mailbox.connections() != 0 || smtp.standin.connections() != 0 {
        wrong.push(format!(
            "the refused calls reached the mail servers: IMAP {}, SMTP {}",
            mailbox.connections(),
            smtp.standin.connections()
        ));
    }
    // Named `inbox`, the reply is admitted and waits for approval.
    let result = h
        .call(
            0,
            "mail.reply",
            json!({"mailbox": id, "thread_id": "<a1@x>", "to": [ANN], "subject": "Re: Invoice", "body": "Got it.", "folder": "inbox"}),
        )
        .await;
    match direct(&result) {
        Some(value) if value["status"] == "approval_pending" => {}
        _ => wrong.push(format!(
            "mail.reply with folder inbox did not wait: {}",
            told(&result)
        )),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mailbox_named_by_its_context_name_is_stored_with_its_binding_id() {
    let (_m1, _s1, inbox_box) = password_mailbox("Inbox", false).await;
    let (_m2, _s2, sales) = password_mailbox("Sales", false).await;
    let (inbox_id, sales_id) = (inbox_box.id.0.to_string(), sales.id.0.to_string());
    let h = harness(
        vec![inbox_box, sales],
        &[Some(json!({ "imap": [inbox_id, sales_id.clone()] }))],
    )
    .await;
    let result = h
        .call(
            0,
            "mail.send",
            json!({"mailbox": "Sales", "to": [ANN], "subject": "Hello", "body": "Hi."}),
        )
        .await;
    let mut wrong = Vec::new();
    match direct(&result) {
        Some(value) if value["status"] == "approval_pending" => {}
        _ => wrong.push(format!(
            "the call did not wait for approval: {}",
            told(&result)
        )),
    }
    match h.rows().await.as_slice() {
        [row] => {
            if row.arguments["mailbox"] != sales_id.as_str()
                || row.binding_id.as_deref() != Some(sales_id.as_str())
            {
                wrong.push(format!(
                    "the stored call names {} and binding {:?}, not {sales_id}",
                    row.arguments["mailbox"], row.binding_id
                ));
            }
        }
        rows => wrong.push(format!("{} approval rows, not 1", rows.len())),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// An approved send, and where its token may not go (clause 14)
// ---------------------------------------------------------------------------

/// Every event and span field written while it is the default subscriber.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Captured {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = String::new();
        span.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push_str(&line);
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::new();
        values.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push_str(&line);
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        line.push('\n');
        self.0.lock().unwrap().push_str(&line);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn an_approved_send_runs_over_xoauth2_and_its_token_reaches_no_result_event_or_log() {
    let captured = Captured::default();
    let _logs = tracing::subscriber::set_default(captured.clone());
    let mailbox = imap_xoauth2_standin_with_folders(ADDRESS, TOKEN, inbox(), folders()).await;
    let smtp = smtp_submission_standin(
        SubmitAuth::XOAuth2 {
            user: ADDRESS.to_string(),
            token: TOKEN.to_string(),
        },
        None,
    )
    .await;
    let owned = Owned {
        id: CredentialBindingId::new(),
        name: ADDRESS.to_string(),
        granted: true,
        imap_port: mailbox.port(),
        smtp_port: smtp.port(),
        auth: MailAuth::XOAuth2(SensitiveString::new(TOKEN)),
    };
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[None]).await;
    let mut events = h.event_bus.subscribe();
    let pending = h
        .call(
            0,
            "mail.send",
            json!({"mailbox": id, "to": [ANN], "subject": "Hello", "body": "Hi."}),
        )
        .await;
    let mut wrong = Vec::new();
    let approval_id = direct(&pending)
        .and_then(|v| v["approval_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("mail.send did not wait for approval: {}", told(&pending)));
    let decided = h
        .approvals
        .decide(
            aegis_orchestrator_core::domain::tool_approval::ToolApprovalId::from_string(
                &approval_id,
            )
            .unwrap(),
            &TenantId::default(),
            USER,
            ToolApprovalDecision::Once,
            h.service.as_ref(),
        )
        .await
        .unwrap();
    if decided.status != ToolApprovalStatus::ApprovedOnce {
        wrong.push(format!("the request reads {:?}", decided.status));
    }
    let result = decided.result.clone().unwrap_or(Value::Null);
    if result["message_id"].as_str().is_none() {
        wrong.push(format!(
            "the approved send did not answer a message_id: {result} {:?}",
            decided.error
        ));
    }
    if smtp.mechanisms() != vec!["XOAUTH2".to_string()] || smtp.submitted().len() != 1 {
        wrong.push(format!(
            "the approved send did not submit over XOAUTH2: {:?}, {} submitted",
            smtp.mechanisms(),
            smtp.submitted().len()
        ));
    }
    let mut seen: Vec<DomainEvent> = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    let logs = captured.0.lock().unwrap().clone();
    if !logs.contains("Running a stored tool call") {
        wrong.push("the log capture saw nothing of the run".to_string());
    }
    if seen.is_empty() {
        wrong.push("no event was published".to_string());
    }
    let base64_response = STANDARD.encode(xoauth2_string(ADDRESS, TOKEN));
    for (place, text) in [
        ("the pending answer", format!("{pending:?}")),
        ("the request", format!("{decided:?}")),
        ("an event", format!("{seen:?}")),
        ("a log line", logs.clone()),
    ] {
        if text.contains(TOKEN) || text.contains(&base64_response) {
            wrong.push(format!("the token reached {place}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// mail.delete at the gate (its Update of 2026-10-08 (4) clause 20)
// ---------------------------------------------------------------------------

/// The refusal for a mailbox with no Trash folder, as the clause gives it.
const NO_TRASH_SENTENCE: &str = "This mailbox has no Trash folder; nothing was deleted.";

/// Thread `<a1@x>` from Ann and then Bea (uids 1 and 2), and another
/// message (uid 3).
fn delete_inbox() -> Vec<StoredMessage> {
    vec![
        StoredMessage::new(
            1,
            &["\\Seen"],
            "05-Oct-2026 09:00:00 +0000",
            &[
                "Message-ID: <a1@x>",
                "From: Ann <ann@example.test>",
                "Subject: Invoice",
            ],
            "Please find the invoice.",
        ),
        StoredMessage::new(
            2,
            &[],
            "05-Oct-2026 10:00:00 +0000",
            &[
                "Message-ID: <a2@x>",
                "References: <a1@x>",
                "From: Bea <bea@example.test>",
                "Subject: Re: Invoice",
            ],
            "Paid.",
        ),
        StoredMessage::new(
            3,
            &[],
            "05-Oct-2026 11:00:00 +0000",
            &[
                "Message-ID: <c1@x>",
                "From: Cid <cid@example.test>",
                "Subject: Lunch",
            ],
            "Lunch?",
        ),
    ]
}

fn trash() -> Vec<StandInFolder> {
    vec![StandInFolder::new("Trash", "\\Trash")]
}

async fn delete_mailbox(folders: Vec<StandInFolder>, granted: bool) -> (MailboxStandIn, Owned) {
    let mailbox = imap_mailbox_standin_with_capabilities(
        ADDRESS,
        PASSWORD,
        delete_inbox(),
        folders,
        "IMAP4rev1 MOVE",
    )
    .await;
    let owned = Owned {
        id: CredentialBindingId::new(),
        name: "Inbox".to_string(),
        granted,
        imap_port: mailbox.port(),
        smtp_port: 465,
        auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
    };
    (mailbox, owned)
}

fn moved_anything(mailbox: &MailboxStandIn) -> Vec<String> {
    mailbox
        .commands()
        .into_iter()
        .filter(|c| {
            let upper = c.to_ascii_uppercase();
            upper.contains(" UID MOVE ")
                || upper.contains(" UID COPY ")
                || upper.contains(" UID STORE ")
                || upper.contains("EXPUNGE")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_waits_with_the_threads_subject_and_senders_and_moves_nothing_before_approval() {
    let (mailbox, owned) = delete_mailbox(trash(), false).await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[Some(json!({ "imap": [id.clone()] }))]).await;
    let result = h
        .call(
            0,
            "mail.delete",
            json!({"mailbox": id, "thread_id": "<a1@x>", "subject": "Harmless", "from": "nobody"}),
        )
        .await;
    let mut wrong = Vec::new();
    let summary = format!(
        "mail.delete\nmailbox: {id}\nthread_id: <a1@x>\nsubject: Invoice\nfrom: Ann <ann@example.test>, Bea <bea@example.test>"
    );
    match direct(&result) {
        Some(value) if value["status"] == "approval_pending" => {
            if value["summary"] != summary.as_str() {
                wrong.push(format!("the summary is {:?}", value["summary"]));
            }
        }
        _ => wrong.push(format!(
            "mail.delete did not wait for approval: {}",
            told(&result)
        )),
    }
    match h.rows().await.as_slice() {
        [row] => {
            if row.arguments["subject"] != "Invoice"
                || row.arguments["from"]
                    != json!(["Ann <ann@example.test>", "Bea <bea@example.test>"])
            {
                wrong.push(format!(
                    "the stored call keeps the model's values: {}",
                    row.arguments
                ));
            }
        }
        rows => wrong.push(format!("{} approval rows, not 1", rows.len())),
    }
    let moved = moved_anything(&mailbox);
    if !moved.is_empty() || mailbox.folder_messages("INBOX").len() != 3 {
        wrong.push(format!("a pending delete changed the mailbox: {moved:?}"));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_its_mailbox_thread_or_trash_would_refuse_is_refused_before_the_gate_with_no_row()
{
    let mut wrong = Vec::new();
    for (case, folders, granted, chosen, thread, expected) in [
        (
            "an ungranted mailbox",
            trash(),
            false,
            false,
            "<a1@x>",
            NOT_GRANTED.to_string(),
        ),
        (
            "a thread not in the inbox",
            trash(),
            false,
            true,
            "<nope@x>",
            "There is no thread '<nope@x>' in this mailbox's inbox.".to_string(),
        ),
        (
            "a mailbox with no Trash folder",
            vec![StandInFolder::new("Sent", "\\Sent")],
            false,
            true,
            "<a1@x>",
            NO_TRASH_SENTENCE.to_string(),
        ),
    ] {
        let (mailbox, owned) = delete_mailbox(folders, granted).await;
        let id = owned.id.0.to_string();
        let contexts = chosen.then(|| json!({ "imap": [id.clone()] }));
        let h = harness(vec![owned], &[contexts]).await;
        let mut events = h.event_bus.subscribe();
        let result = h
            .call(
                0,
                "mail.delete",
                json!({"mailbox": id, "thread_id": thread}),
            )
            .await;
        let said = match &result {
            Err(SealSessionError::Answered {
                answer: CallerAnswer::NotFound(message),
                ..
            }) => message.clone(),
            other => told(other),
        };
        if said != expected {
            wrong.push(format!("{case} was not refused before the gate: {said}"));
        }
        if !h.rows().await.is_empty() {
            wrong.push(format!("{case}: approval rows were written"));
        }
        while let Ok(event) = events.try_recv() {
            if format!("{event:?}").contains("ApprovalRequested") {
                wrong.push(format!("{case}: an approval was requested"));
            }
        }
        if !moved_anything(&mailbox).is_empty() {
            wrong.push(format!("{case}: the mailbox changed"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn an_approved_delete_runs_over_xoauth2_and_its_token_reaches_no_result_event_or_log() {
    let captured = Captured::default();
    let _logs = tracing::subscriber::set_default(captured.clone());
    let mailbox = imap_xoauth2_standin_with_capabilities(
        ADDRESS,
        TOKEN,
        delete_inbox(),
        trash(),
        "IMAP4rev1 MOVE",
    )
    .await;
    let owned = Owned {
        id: CredentialBindingId::new(),
        name: ADDRESS.to_string(),
        granted: true,
        imap_port: mailbox.port(),
        smtp_port: 465,
        auth: MailAuth::XOAuth2(SensitiveString::new(TOKEN)),
    };
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[None]).await;
    let mut events = h.event_bus.subscribe();
    let pending = h
        .call(
            0,
            "mail.delete",
            json!({"mailbox": id, "thread_id": "<a1@x>"}),
        )
        .await;
    let mut wrong = Vec::new();
    let approval_id = direct(&pending)
        .and_then(|v| v["approval_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("mail.delete did not wait for approval: {}", told(&pending)));
    let decided = h
        .approvals
        .decide(
            aegis_orchestrator_core::domain::tool_approval::ToolApprovalId::from_string(
                &approval_id,
            )
            .unwrap(),
            &TenantId::default(),
            USER,
            ToolApprovalDecision::Once,
            h.service.as_ref(),
        )
        .await
        .unwrap();
    if decided.status != ToolApprovalStatus::ApprovedOnce {
        wrong.push(format!("the request reads {:?}", decided.status));
    }
    let result = decided.result.clone().unwrap_or(Value::Null);
    if result["moved"] != 2 || result["trash_folder"] != "Trash" {
        wrong.push(format!(
            "the approved delete did not move the thread: {result} {:?}",
            decided.error
        ));
    }
    let inbox: Vec<u32> = mailbox
        .folder_messages("INBOX")
        .iter()
        .map(|m| m.uid)
        .collect();
    if inbox != vec![3] || mailbox.folder_messages("Trash").len() != 2 {
        wrong.push(format!("INBOX holds {inbox:?} after the delete"));
    }
    let mut seen: Vec<DomainEvent> = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    let logs = captured.0.lock().unwrap().clone();
    if !logs.contains("Running a stored tool call") {
        wrong.push("the log capture saw nothing of the run".to_string());
    }
    if seen.is_empty() {
        wrong.push("no event was published".to_string());
    }
    let base64_response = STANDARD.encode(xoauth2_string(ADDRESS, TOKEN));
    for (place, text) in [
        ("the pending answer", format!("{pending:?}")),
        ("the request", format!("{decided:?}")),
        ("an event", format!("{seen:?}")),
        ("a log line", logs.clone()),
    ] {
        if text.contains(TOKEN) || text.contains(&base64_response) {
            wrong.push(format!("the token reached {place}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Test doubles the dispatch reads
// ---------------------------------------------------------------------------

struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, id: ExecutionId) -> Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Resolves every agent to one agent with no `tool_validation`, so the
/// inner-loop judge does not run.
struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct NoOpPublisher;

#[async_trait]
impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(
        &self,
        _event: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

// ---------------------------------------------------------------------------
// mail.archive (its Update of 2026-10-08 (5), clauses 27 to 29)
// ---------------------------------------------------------------------------

/// The refusal for a mailbox with no Archive folder, as clause 28 gives it.
const NO_ARCHIVE_SENTENCE: &str = "This mailbox has no Archive folder; nothing was archived.";
/// The refusal for a server with neither `MOVE` nor `UIDPLUS`, as clause 27
/// gives it.
const NO_SAFE_ARCHIVE_SENTENCE: &str =
    "This mailbox's server can neither move messages nor expunge only chosen ones; nothing was archived.";

/// A mailbox whose `CAPABILITY` answers `capabilities`, its `INBOX` the
/// delete tests' (thread `<a1@x>` at uids 1 and 2, another message at 3),
/// with `folders` beside it.
async fn archive_mailbox(
    folders: Vec<StandInFolder>,
    capabilities: &str,
    inbox: Vec<StoredMessage>,
    granted: bool,
) -> (MailboxStandIn, Owned) {
    let mailbox =
        imap_mailbox_standin_with_capabilities(ADDRESS, PASSWORD, inbox, folders, capabilities)
            .await;
    let owned = Owned {
        id: CredentialBindingId::new(),
        name: "Inbox".to_string(),
        granted,
        imap_port: mailbox.port(),
        smtp_port: 465,
        auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
    };
    (mailbox, owned)
}

/// A conversation's call by the owner: no execution record, nothing chosen.
fn owner_in_conversation() -> MailActing {
    MailActing {
        tenant_id: TenantId::default(),
        user_id: Some(USER.to_string()),
        agent_id: AgentId::new(),
        workflow_id: None,
        choice: ServerChoice::NotGiven,
        has_execution_record: false,
        tier: None,
    }
}

/// `mail.archive` run by the mail tools themselves, past the gate.
async fn archive_directly(owned: &Owned, args: Value) -> Result<Value, SealSessionError> {
    let tools = MailTools::with_connector(
        Arc::new(Mailboxes(vec![owned.clone()])),
        Arc::new(PlainConnector),
    );
    let mut args = args;
    args["mailbox"] = json!(owned.id.0.to_string());
    tools
        .invoke("mail.archive", &args, &owner_in_conversation())
        .await
}

/// A recorded command without its tag.
fn untagged(command: &str) -> String {
    command
        .split_once(' ')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default()
}

/// The commands that change a mailbox: a move, copy, store or expunge, plain
/// or by UID, without their tags.
fn changes(mailbox: &MailboxStandIn) -> Vec<String> {
    mailbox
        .commands()
        .iter()
        .map(|c| untagged(c))
        .filter(|c| {
            let upper = c.to_ascii_uppercase();
            upper.starts_with("UID MOVE ")
                || upper.starts_with("UID COPY ")
                || upper.starts_with("UID STORE ")
                || upper.starts_with("UID EXPUNGE")
                || upper.starts_with("EXPUNGE")
        })
        .collect()
}

fn uids_in(mailbox: &MailboxStandIn, folder: &str) -> Vec<u32> {
    mailbox
        .folder_messages(folder)
        .iter()
        .map(|m| m.uid)
        .collect()
}

/// The sentence a direct call's refusal tells its caller.
fn refused_with(result: &Result<Value, SealSessionError>) -> String {
    match result {
        Ok(value) => format!("answered {value}"),
        Err(SealSessionError::Answered {
            answer: CallerAnswer::NotFound(message),
            ..
        }) => message.clone(),
        Err(SealSessionError::InvalidArguments(message)) => message.clone(),
        Err(other) => format!("{other:?}"),
    }
}

/// Clause 27 and 28: by each move path, into the folder marked `\Archive`
/// (over a folder named Archive and the `\All` folder), into the folder
/// named `Archive` at the top level or under `INBOX` (over `\All`), and into
/// the `\All` folder where there is neither, the thread's `INBOX` messages
/// move and the rest of `INBOX` stays; the copy path stores `\Deleted` and
/// expunges by UID exactly the thread's uids, so a message another client
/// marked deleted survives; no plain `EXPUNGE` is sent and the Archive
/// folder is never opened. The result carries clause 27's fields with
/// `folder_kind` `inbox` (clause 23).
#[tokio::test]
async fn mail_archive_moves_exactly_the_threads_inbox_messages_into_the_archive_folder_by_either_path(
) {
    let mut wrong = Vec::new();
    let all_mail = || StandInFolder::new("[Gmail]/All Mail", "\\All");
    for (case, folders, expected) in [
        (
            "the folder marked \\Archive",
            vec![
                StandInFolder::new("Archive", ""),
                StandInFolder::new("Kept", "\\Archive"),
                all_mail(),
            ],
            "Kept",
        ),
        (
            "the folder named Archive",
            vec![StandInFolder::new("Archive", ""), all_mail()],
            "Archive",
        ),
        (
            "the folder named Archive under INBOX",
            vec![all_mail(), StandInFolder::new("INBOX/Archive", "")],
            "INBOX/Archive",
        ),
        (
            "the folder marked \\All",
            vec![StandInFolder::new("[Gmail]/Trash", "\\Trash"), all_mail()],
            "[Gmail]/All Mail",
        ),
    ] {
        for capabilities in ["IMAP4rev1 MOVE", "IMAP4rev1 UIDPLUS", "IMAP4rev2"] {
            let mut inbox = delete_inbox();
            // Another client marked message 3 deleted: a plain EXPUNGE
            // would remove it, UID EXPUNGE of the thread's uids does not.
            inbox[2].flags.push("\\Deleted".to_string());
            let (mailbox, owned) =
                archive_mailbox(folders.clone(), capabilities, inbox, false).await;
            let at = format!("{case} with {capabilities}");
            match archive_directly(&owned, json!({"thread_id": "<a1@x>"})).await {
                Ok(result) => {
                    let fields = json!({
                        "mailbox": owned.id.0.to_string(),
                        "folder": "INBOX",
                        "folder_kind": "inbox",
                        "thread_id": "<a1@x>",
                        "archive_folder": expected,
                        "moved": 2,
                        "message_uids": [1, 2],
                    });
                    if result != fields {
                        wrong.push(format!("{at}: the result is {result}"));
                    }
                }
                Err(e) => wrong.push(format!("{at}: mail.archive failed: {e:?}")),
            }
            let quoted = format!("\"{expected}\"");
            let wanted = if capabilities == "IMAP4rev1 UIDPLUS" {
                vec![
                    format!("UID COPY 1,2 {quoted}"),
                    "UID STORE 1,2 +FLAGS (\\Deleted)".to_string(),
                    "UID EXPUNGE 1,2".to_string(),
                ]
            } else {
                vec![format!("UID MOVE 1,2 {quoted}")]
            };
            let changed = changes(&mailbox);
            if changed != wanted {
                wrong.push(format!(
                    "{at}: the changing commands were {changed:?}, not {wanted:?}"
                ));
            }
            if uids_in(&mailbox, "INBOX") != vec![3] {
                wrong.push(format!(
                    "{at}: INBOX holds {:?}, not the other message 3",
                    uids_in(&mailbox, "INBOX")
                ));
            }
            let archived = mailbox.folder_messages(expected);
            if archived.len() != 2 || !archived.iter().all(|m| m.raw.contains("Invoice")) {
                wrong.push(format!(
                    "{at}: {expected} holds {} messages, not the thread's two",
                    archived.len()
                ));
            }
            for folder in &folders {
                if folder.name != expected && !mailbox.folder_messages(&folder.name).is_empty() {
                    wrong.push(format!("{at}: {} received messages", folder.name));
                }
            }
            let opened: Vec<String> = mailbox
                .commands()
                .iter()
                .map(|c| untagged(c))
                .filter(|c| {
                    let upper = c.to_ascii_uppercase();
                    (upper.starts_with("SELECT ") || upper.starts_with("EXAMINE "))
                        && c.contains(&quoted)
                })
                .collect();
            if !opened.is_empty() {
                wrong.push(format!("{at}: the Archive folder was opened: {opened:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Clause 27 and 28: a mailbox with no Archive folder (none marked
/// `\Archive` or `\All`, none named Archive), and a server that can neither
/// move nor expunge chosen messages, are refused with their sentences and
/// nothing changes.
#[tokio::test]
async fn mail_archive_refuses_with_no_archive_folder_or_no_safe_move_and_changes_nothing() {
    let mut wrong = Vec::new();
    for (case, folders, capabilities, expected) in [
        (
            "a mailbox with no Archive folder",
            vec![
                StandInFolder::new("Sent", "\\Sent"),
                StandInFolder::new("Trash", "\\Trash"),
                StandInFolder::new("Archived", ""),
                StandInFolder::new("Work/Archive", ""),
            ],
            "IMAP4rev1 MOVE UIDPLUS",
            NO_ARCHIVE_SENTENCE,
        ),
        (
            "a server with neither MOVE nor UIDPLUS",
            vec![StandInFolder::new("Archive", "\\Archive")],
            "IMAP4rev1",
            NO_SAFE_ARCHIVE_SENTENCE,
        ),
    ] {
        let (mailbox, owned) = archive_mailbox(folders, capabilities, delete_inbox(), false).await;
        let result = archive_directly(&owned, json!({"thread_id": "<a1@x>"})).await;
        if refused_with(&result) != expected {
            wrong.push(format!("{case}: {}", refused_with(&result)));
        }
        let changed = changes(&mailbox);
        if !changed.is_empty() {
            wrong.push(format!("{case}: the refused call sent {changed:?}"));
        }
        if uids_in(&mailbox, "INBOX") != vec![1, 2, 3] {
            wrong.push(format!("{case}: INBOX changed"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Clause 29: an archive waits for approval with the thread's subject and
/// senders read before the gate over the model's values (clause 20's
/// admission), and nothing moves before the person answers; approved once,
/// the run moves the thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_waits_with_the_threads_subject_and_senders_and_moves_only_once_approved() {
    let (mailbox, owned) = archive_mailbox(
        vec![StandInFolder::new("Archive", "\\Archive")],
        "IMAP4rev1 MOVE",
        delete_inbox(),
        false,
    )
    .await;
    let id = owned.id.0.to_string();
    let h = harness(vec![owned], &[Some(json!({ "imap": [id.clone()] }))]).await;
    let result = h
        .call(
            0,
            "mail.archive",
            json!({"mailbox": id, "thread_id": "<a1@x>", "subject": "Harmless", "from": "nobody"}),
        )
        .await;
    let mut wrong = Vec::new();
    let summary = format!(
        "mail.archive\nmailbox: {id}\nthread_id: <a1@x>\nsubject: Invoice\nfrom: Ann <ann@example.test>, Bea <bea@example.test>"
    );
    let approval_id = match direct(&result) {
        Some(value) if value["status"] == "approval_pending" => {
            if value["summary"] != summary.as_str() {
                wrong.push(format!("the summary is {:?}", value["summary"]));
            }
            value["approval_id"].as_str().map(str::to_string)
        }
        _ => {
            wrong.push(format!(
                "mail.archive did not wait for approval: {}",
                told(&result)
            ));
            None
        }
    };
    match h.rows().await.as_slice() {
        [row] => {
            if row.arguments["subject"] != "Invoice"
                || row.arguments["from"]
                    != json!(["Ann <ann@example.test>", "Bea <bea@example.test>"])
            {
                wrong.push(format!(
                    "the stored call keeps the model's values: {}",
                    row.arguments
                ));
            }
        }
        rows => wrong.push(format!("{} approval rows, not 1", rows.len())),
    }
    let moved = changes(&mailbox);
    if !moved.is_empty() || uids_in(&mailbox, "INBOX") != vec![1, 2, 3] {
        wrong.push(format!("a pending archive changed the mailbox: {moved:?}"));
    }
    if let Some(approval_id) = approval_id {
        let decided = h
            .approvals
            .decide(
                aegis_orchestrator_core::domain::tool_approval::ToolApprovalId::from_string(
                    &approval_id,
                )
                .unwrap(),
                &TenantId::default(),
                USER,
                ToolApprovalDecision::Once,
                h.service.as_ref(),
            )
            .await
            .unwrap();
        let result = decided.result.clone().unwrap_or(Value::Null);
        if decided.status != ToolApprovalStatus::ApprovedOnce
            || result["moved"] != 2
            || result["archive_folder"] != "Archive"
        {
            wrong.push(format!(
                "the approved archive did not move the thread: {:?} {result} {:?}",
                decided.status, decided.error
            ));
        }
        if uids_in(&mailbox, "INBOX") != vec![3] || uids_in(&mailbox, "Archive").len() != 2 {
            wrong.push(format!(
                "INBOX holds {:?} after the archive",
                uids_in(&mailbox, "INBOX")
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Clauses 25, 27, 28 and 29: a call its mailbox, its `folder`, its thread,
/// its Archive folder or its server would refuse is refused before the gate
/// with its sentence; no approval row is written, no approval is requested
/// and nothing changes; a `folder` other than the inbox is refused before
/// any connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_its_admission_would_refuse_is_refused_before_the_gate_with_no_row() {
    const INBOX_ONLY: &str =
        "This tool works on threads in the inbox only; leave out 'folder' or set it to inbox.";
    let archive = || vec![StandInFolder::new("Archive", "\\Archive")];
    let mut wrong = Vec::new();
    for (case, folders, capabilities, granted, chosen, args, expected) in [
        (
            "an ungranted mailbox",
            archive(),
            "IMAP4rev1 MOVE",
            false,
            false,
            json!({"thread_id": "<a1@x>"}),
            NOT_GRANTED,
        ),
        (
            "a folder other than the inbox",
            archive(),
            "IMAP4rev1 MOVE",
            false,
            true,
            json!({"thread_id": "<a1@x>", "folder": "sent"}),
            INBOX_ONLY,
        ),
        (
            "a thread not in the inbox",
            archive(),
            "IMAP4rev1 MOVE",
            false,
            true,
            json!({"thread_id": "<nope@x>"}),
            "There is no thread '<nope@x>' in this mailbox's inbox.",
        ),
        (
            "a mailbox with no Archive folder",
            vec![StandInFolder::new("Trash", "\\Trash")],
            "IMAP4rev1 MOVE",
            false,
            true,
            json!({"thread_id": "<a1@x>"}),
            NO_ARCHIVE_SENTENCE,
        ),
        (
            "a server that cannot move safely",
            archive(),
            "IMAP4rev1",
            false,
            true,
            json!({"thread_id": "<a1@x>"}),
            NO_SAFE_ARCHIVE_SENTENCE,
        ),
    ] {
        let (mailbox, owned) =
            archive_mailbox(folders, capabilities, delete_inbox(), granted).await;
        let id = owned.id.0.to_string();
        let contexts = chosen.then(|| json!({ "imap": [id.clone()] }));
        let h = harness(vec![owned], &[contexts]).await;
        let mut events = h.event_bus.subscribe();
        let mut args = args;
        args["mailbox"] = json!(id);
        let result = h.call(0, "mail.archive", args).await;
        let said = match &result {
            Err(SealSessionError::Answered {
                answer: CallerAnswer::NotFound(message),
                ..
            }) => message.clone(),
            other => told(other),
        };
        if said != expected {
            wrong.push(format!("{case} was not refused before the gate: {said}"));
        }
        if !h.rows().await.is_empty() {
            wrong.push(format!("{case}: approval rows were written"));
        }
        while let Ok(event) = events.try_recv() {
            if format!("{event:?}").contains("ApprovalRequested") {
                wrong.push(format!("{case}: an approval was requested"));
            }
        }
        if !changes(&mailbox).is_empty() || uids_in(&mailbox, "INBOX") != vec![1, 2, 3] {
            wrong.push(format!("{case}: the mailbox changed"));
        }
        if expected == INBOX_ONLY && mailbox.connections() != 0 {
            wrong.push(format!("{case}: the mail server was reached"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
