// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedules (AEGIS ADR-139)
//!
//! A schedule is a person's own object that starts an agent or a workflow as
//! that person, once or on a recurrence (N1). It is never a field of an
//! agent or a workflow: any number of schedules may name one target, and a
//! manifest may only recommend a timing ([`DefaultSchedule`], N12).
//!
//! This module holds the aggregate, the timing's parsing and its refusals
//! (N2), who may own one (N3) and how many (N4), the record of each fire
//! (N6, N7), and the repository port the application layer stores them
//! through.

use std::collections::BTreeSet;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::execution::{
    check_contexts_shape, contexts_chosen, read_profile_value, AttachmentRef, ExecutionId,
    CONTEXTS_INPUT_KEY, CONVERSATION_INPUT_KEY, PROFILE_INPUT_KEY, PROFILE_WITH_CONTEXTS,
};
use crate::domain::git_repo::{parse_run_repositories, REPOSITORIES_INPUT_KEY, REPOSITORIES_SHAPE};
use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;

// ── Bounds (N2, N4, N5, N7: numbers nobody has set) ─────────────────────────

/// The shortest gap between two runs of a recurrence, in minutes (N2).
pub const MIN_GAP_MINUTES: i64 = 5;
/// The largest `jitter_seconds` (N2).
pub const JITTER_CAP_SECONDS: u32 = 3_600;
/// How far ahead `at` may be, in days (N2).
pub const AT_MAX_DAYS: i64 = 366;
/// How soon `at` may be, in seconds (N2).
pub const AT_MIN_SECONDS: i64 = 60;
/// Schedules that are not deleted, per person (N4).
pub const MAX_SCHEDULES_PER_OWNER: usize = 25;
/// The Temporal Schedule's catch-up window, in seconds (N5).
pub const CATCHUP_WINDOW_SECONDS: u64 = 600;
/// Consecutive refused fires after which a schedule pauses itself (N7).
pub const REFUSALS_BEFORE_PAUSE: usize = 3;
/// The longest name (N1).
pub const NAME_MAX_CHARS: usize = 80;
/// The prefix of a Temporal Schedule's id (N5).
pub const TEMPORAL_SCHEDULE_PREFIX: &str = "aegis-schedule-";
/// The worker's workflow a Temporal Schedule starts (N5, N6).
pub const FIRE_WORKFLOW_TYPE: &str = "aegis_schedule_fire";
/// The Temporal workflow service account allowed to fire a schedule (N6).
pub const FIRE_CLIENT_ID: &str = "aegis-temporal-worker";
/// The security context a scheduled run's states use, as the starting
/// tools use it (N8).
pub const SCHEDULED_RUN_SECURITY_CONTEXT: &str = "aegis-system-agent-runtime";

// ── Refusal sentences (N2, N3, N4, N5, N7, N12) ─────────────────────────────

pub const AT_REFUSAL: &str =
    "'at' must be a time at least one minute from now and at most a year ahead.";
pub const CRON_REFUSAL: &str =
    "'cron' must be five fields: minute, hour, day of month, month and day of week.";
pub const TIMEZONE_REFUSAL: &str = "'timezone' must be a time zone name such as Europe/Berlin.";
pub const OWNER_REFUSAL: &str =
    "A schedule runs as the person who made it; this needs your own account.";
pub const UNAVAILABLE_REFUSAL: &str = "Schedules are unavailable right now; nothing was saved.";
pub const SPEC_SCHEDULE_REFUSAL: &str = "'spec.schedule' is not read; make a schedule for this agent or workflow, and use 'spec.default_schedule' to suggest one.";
pub const TIMING_REFUSAL: &str = "Give exactly one of 'at' or 'recurrence'.";
pub const NAME_REFUSAL: &str = "'name' must be 1 to 80 characters.";
pub const TARGET_KIND_REFUSAL: &str = "'target_kind' must be agent or workflow.";
pub const TARGET_REFUSAL: &str = "'target' must name an agent or a workflow.";
pub const ATTACHMENTS_REFUSAL: &str = "'attachments' must be a list of attachment references.";

/// `A schedule runs at most once every <n> minutes.` (N2)
pub fn gap_refusal() -> String {
    format!("A schedule runs at most once every {MIN_GAP_MINUTES} minutes.")
}

/// `'jitter_seconds' must be between 0 and <cap>.` (N2)
pub fn jitter_refusal() -> String {
    format!("'jitter_seconds' must be between 0 and {JITTER_CAP_SECONDS}.")
}

/// "You have 25 schedules; delete one to make another." (N4)
pub fn count_refusal() -> String {
    format!("You have {MAX_SCHEDULES_PER_OWNER} schedules; delete one to make another.")
}

/// `Paused after three runs could not start: <the last sentence>.` (N7)
pub fn paused_after_refusals(last: &str) -> String {
    let last = last.trim_end_matches('.');
    format!("Paused after three runs could not start: {last}.")
}

// ── Identifiers and enums ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduleId(pub Uuid);

impl ScheduleId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(text: &str) -> Option<Self> {
        Uuid::parse_str(text).ok().map(Self)
    }

    /// The id of the Temporal Schedule that backs this schedule (N5).
    pub fn temporal_schedule_id(&self) -> String {
        format!("{TEMPORAL_SCHEDULE_PREFIX}{}", self.0)
    }
}

impl Default for ScheduleId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Agent,
    Workflow,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Workflow => "workflow",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    Active,
    Paused,
    Completed,
}

impl ScheduleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

// ── The timing (N2) ──────────────────────────────────────────────────────────

/// A recurrence: a five-field cron in an IANA time zone, with a jitter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    pub cron: String,
    pub timezone: String,
    pub jitter_seconds: u32,
}

impl Recurrence {
    /// Check the recurrence against N2, in the order N2 lists the refusals:
    /// the cron, the time zone, the shortest gap, the jitter.
    pub fn validate(&self) -> Result<(), String> {
        let cron = CronSpec::parse(&self.cron).map_err(|_| CRON_REFUSAL.to_string())?;
        if !is_time_zone_name(&self.timezone) {
            return Err(TIMEZONE_REFUSAL.to_string());
        }
        if let Some(gap) = cron.shortest_gap_minutes() {
            if gap < MIN_GAP_MINUTES {
                return Err(gap_refusal());
            }
        }
        if self.jitter_seconds > JITTER_CAP_SECONDS {
            return Err(jitter_refusal());
        }
        Ok(())
    }
}

/// When a schedule runs: once at an instant, or on a recurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Timing {
    Once { at: DateTime<Utc> },
    Recurrence(Recurrence),
}

/// The timing as a request carries it, before it is checked.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecurrenceInput {
    pub cron: Option<String>,
    pub timezone: Option<String>,
    pub jitter_seconds: Option<i64>,
}

impl RecurrenceInput {
    /// The recurrence with its defaults (`UTC`, a jitter of 0), checked.
    pub fn into_recurrence(self) -> Result<Recurrence, String> {
        let cron = self.cron.ok_or_else(|| CRON_REFUSAL.to_string())?;
        let jitter = self.jitter_seconds.unwrap_or(0);
        let jitter_seconds = u32::try_from(jitter).map_err(|_| jitter_refusal())?;
        let recurrence = Recurrence {
            cron,
            timezone: self.timezone.unwrap_or_else(|| "UTC".to_string()),
            jitter_seconds,
        };
        recurrence.validate()?;
        Ok(recurrence)
    }
}

/// Parse `at` (an RFC 3339 instant) and check it lies between one minute
/// and 366 days from `now` (N2).
pub fn parse_at(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let at = DateTime::parse_from_rfc3339(text)
        .map_err(|_| AT_REFUSAL.to_string())?
        .with_timezone(&Utc);
    check_at(at, now)?;
    Ok(at)
}

pub fn check_at(at: DateTime<Utc>, now: DateTime<Utc>) -> Result<(), String> {
    if at < now + Duration::seconds(AT_MIN_SECONDS) || at > now + Duration::days(AT_MAX_DAYS) {
        return Err(AT_REFUSAL.to_string());
    }
    Ok(())
}

/// Exactly one of `at` or `recurrence` (N2).
pub fn parse_timing(
    at: Option<&str>,
    recurrence: Option<RecurrenceInput>,
    now: DateTime<Utc>,
) -> Result<Timing, String> {
    match (at, recurrence) {
        (Some(at), None) => Ok(Timing::Once {
            at: parse_at(at, now)?,
        }),
        (None, Some(recurrence)) => Ok(Timing::Recurrence(recurrence.into_recurrence()?)),
        _ => Err(TIMING_REFUSAL.to_string()),
    }
}

/// True when `name` is an IANA time zone name (from the time zone database
/// built into the binary, so the answer does not depend on the host).
pub fn is_time_zone_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && jiff::tz::db().get(name).is_ok()
}

// ── Five-field cron (N2) ─────────────────────────────────────────────────────

/// A five-field cron expression: minute, hour, day of month, month, day of
/// week. Each field is `*`, a number, a range `a-b`, any of those with a
/// step `/n`, or a comma-separated list of them. Day of week 0 and 7 are
/// Sunday. There is no seconds field and no `@` shorthand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSpec {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days_of_month: BTreeSet<u32>,
    months: BTreeSet<u32>,
    days_of_week: BTreeSet<u32>,
    day_of_month_star: bool,
    day_of_week_star: bool,
}

impl CronSpec {
    /// Refused with N2's cron sentence.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_fields(text).map_err(|()| CRON_REFUSAL.to_string())
    }

    fn parse_fields(text: &str) -> Result<Self, ()> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(());
        }
        let mut days_of_week = parse_cron_field(fields[4], 0, 7)?;
        if days_of_week.remove(&7) {
            days_of_week.insert(0);
        }
        Ok(Self {
            minutes: parse_cron_field(fields[0], 0, 59)?,
            hours: parse_cron_field(fields[1], 0, 23)?,
            days_of_month: parse_cron_field(fields[2], 1, 31)?,
            months: parse_cron_field(fields[3], 1, 12)?,
            days_of_week,
            day_of_month_star: fields[2].starts_with('*'),
            day_of_week_star: fields[4].starts_with('*'),
        })
    }

    /// Whether the expression fires on `date` (cron's rule: when both the
    /// day of month and the day of week are restricted, either matches).
    fn fires_on(&self, date: NaiveDate) -> bool {
        if !self.months.contains(&date.month()) {
            return false;
        }
        let dom = self.days_of_month.contains(&date.day());
        let dow = self
            .days_of_week
            .contains(&date.weekday().num_days_from_sunday());
        if self.day_of_month_star || self.day_of_week_star {
            dom && dow
        } else {
            dom || dow
        }
    }

    /// The shortest gap between two runs, in minutes, over eight calendar
    /// years from 2024 (every weekday, month length and leap day appears):
    /// `None` when it fires at most once in that span.
    pub fn shortest_gap_minutes(&self) -> Option<i64> {
        let times: Vec<i64> = self
            .hours
            .iter()
            .flat_map(|h| {
                self.minutes
                    .iter()
                    .map(move |m| i64::from(*h) * 60 + i64::from(*m))
            })
            .collect();
        let first = *times.first()?;
        let last = *times.last()?;
        let within_day = times.windows(2).map(|w| w[1] - w[0]).min();
        let start = NaiveDate::from_ymd_opt(2024, 1, 1)?;
        let mut shortest: Option<i64> = None;
        let mut previous_day: Option<i64> = None;
        for offset in 0..(8 * 366) {
            let date = start + Duration::days(offset);
            if !self.fires_on(date) {
                continue;
            }
            if let Some(gap) = within_day {
                shortest = Some(shortest.map_or(gap, |s| s.min(gap)));
            }
            if let Some(prev) = previous_day {
                let gap = (offset - prev) * 1_440 + first - last;
                shortest = Some(shortest.map_or(gap, |s| s.min(gap)));
            }
            if shortest.is_some_and(|s| s < MIN_GAP_MINUTES) {
                break;
            }
            previous_day = Some(offset);
        }
        shortest
    }
}

fn parse_cron_number(text: &str, min: u32, max: u32) -> Result<u32, ()> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    let value: u32 = text.parse().map_err(|_| ())?;
    if value < min || value > max {
        return Err(());
    }
    Ok(value)
}

fn parse_cron_field(text: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, ()> {
    let mut values = BTreeSet::new();
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => {
                let step = parse_cron_number(step, 1, max.max(1))?;
                (base, Some(step))
            }
            None => (item, None),
        };
        let (from, to) = if base == "*" {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            let (a, b) = (
                parse_cron_number(a, min, max)?,
                parse_cron_number(b, min, max)?,
            );
            if a > b {
                return Err(());
            }
            (a, b)
        } else {
            let a = parse_cron_number(base, min, max)?;
            (a, if step.is_some() { max } else { a })
        };
        let step = step.unwrap_or(1) as usize;
        values.extend((from..=to).step_by(step));
    }
    if values.is_empty() {
        return Err(());
    }
    Ok(values)
}

// ── What a manifest may recommend (N12) ──────────────────────────────────────

/// A timing a manifest recommends for a schedule of its agent or workflow.
/// It is offered pre-filled when a person makes a schedule; it never
/// creates, changes or pauses one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultSchedule {
    /// Five fields: minute, hour, day of month, month and day of week.
    pub cron: String,
    /// An IANA time zone name such as Europe/Berlin. Default: UTC.
    #[serde(default = "default_time_zone")]
    pub timezone: String,
    /// Up to this many seconds of random delay before each run. Default: 0.
    #[serde(default)]
    pub jitter_seconds: u32,
}

fn default_time_zone() -> String {
    "UTC".to_string()
}

impl DefaultSchedule {
    /// Checked by the same rules as a schedule's recurrence.
    pub fn validate(&self) -> Result<(), String> {
        self.recurrence().validate()
    }

    pub fn recurrence(&self) -> Recurrence {
        Recurrence {
            cron: self.cron.clone(),
            timezone: self.timezone.clone(),
            jitter_seconds: self.jitter_seconds,
        }
    }
}

/// The refusal of a manifest that still carries `spec.schedule` (N12),
/// read from the manifest's YAML or JSON before it is deserialized.
pub fn refuse_spec_schedule(manifest: &serde_yaml::Value) -> Result<(), String> {
    let has = manifest
        .get("spec")
        .and_then(|spec| spec.get("schedule"))
        .is_some();
    if has {
        return Err(SPEC_SCHEDULE_REFUSAL.to_string());
    }
    Ok(())
}

// ── The owner (N3) ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    ConsumerUser,
    TenantUser,
}

impl OwnerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConsumerUser => "consumer_user",
            Self::TenantUser => "tenant_user",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "consumer_user" => Some(Self::ConsumerUser),
            "tenant_user" => Some(Self::TenantUser),
            _ => None,
        }
    }
}

/// The owner's identity projection, as an API key keeps it: enough to act
/// as the person without a live token (N1, N7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleOwner {
    pub sub: String,
    pub realm: String,
    pub kind: OwnerKind,
    /// The consumer's tier claim (`free`, `pro`, …); `None` for a tenant user.
    pub zaru_tier: Option<String>,
}

impl ScheduleOwner {
    /// A person's projection; a service account or an operator acting for
    /// no person is refused (N3).
    pub fn from_identity(identity: &UserIdentity) -> Result<Self, String> {
        match &identity.identity_kind {
            IdentityKind::ConsumerUser { zaru_tier, .. } => Ok(Self {
                sub: identity.sub.clone(),
                realm: identity.realm_slug.clone(),
                kind: OwnerKind::ConsumerUser,
                zaru_tier: Some(zaru_tier.to_claim_str().to_string()),
            }),
            IdentityKind::TenantUser { .. } => Ok(Self {
                sub: identity.sub.clone(),
                realm: identity.realm_slug.clone(),
                kind: OwnerKind::TenantUser,
                zaru_tier: None,
            }),
            IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
                Err(OWNER_REFUSAL.to_string())
            }
        }
    }

    /// The owner's identity, rebuilt from the projection for a run started
    /// in `tenant` (N7, N8).
    pub fn to_identity(&self, tenant: &TenantId) -> Result<UserIdentity, String> {
        let identity_kind = match self.kind {
            OwnerKind::ConsumerUser => IdentityKind::ConsumerUser {
                zaru_tier: self
                    .zaru_tier
                    .as_deref()
                    .and_then(ZaruTier::from_claim)
                    .unwrap_or(ZaruTier::Free),
                tenant_id: tenant.clone(),
            },
            OwnerKind::TenantUser => IdentityKind::TenantUser {
                tenant_slug: self
                    .realm
                    .strip_prefix("tenant-")
                    .map(str::to_string)
                    .ok_or_else(|| format!("the owner's realm '{}' is not a tenant", self.realm))?,
            },
        };
        Ok(UserIdentity {
            sub: self.sub.clone(),
            realm_slug: self.realm.clone(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind,
        })
    }
}

// ── The aggregate (N1) ───────────────────────────────────────────────────────

/// A person's schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: ScheduleId,
    pub tenant_id: TenantId,
    pub owner: ScheduleOwner,
    pub name: String,
    pub target_kind: TargetKind,
    /// The name or UUID the starting tool takes.
    pub target: String,
    /// `None`: the latest at each run.
    pub target_version: Option<String>,
    pub intent: Option<String>,
    pub input: Value,
    pub attachments: Vec<AttachmentRef>,
    pub repositories: Option<Value>,
    pub contexts: Option<Value>,
    /// The person's profile its runs start on (AEGIS ADR-140 D8), read as
    /// the owner's at each fire; never beside a non-empty `contexts` (D10).
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    pub timing: Timing,
    pub state: ScheduleState,
    /// Why the schedule paused itself, when it did (N7).
    pub paused_reason: Option<String>,
    pub temporal_schedule_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// What a person sends to make a schedule.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScheduleDraft {
    pub name: Option<String>,
    pub target_kind: Option<String>,
    pub target: Option<String>,
    pub version: Option<String>,
    pub intent: Option<String>,
    pub input: Option<Value>,
    pub attachments: Option<Value>,
    pub repositories: Option<Value>,
    pub contexts: Option<Value>,
    /// One profile id (AEGIS ADR-140 D8), or none.
    pub profile: Option<Value>,
    pub at: Option<String>,
    pub recurrence: Option<RecurrenceInput>,
}

/// A field that was sent, `null` included: `Some(Value::Null)` for `null`,
/// `None` (by `default`) when it was not sent.
fn sent<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// What a person sends to change a schedule: any settable field, and the
/// timing (`at` or `recurrence`, replacing the one it had).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SchedulePatch {
    pub name: Option<String>,
    pub target_kind: Option<String>,
    pub target: Option<String>,
    pub version: Option<String>,
    pub intent: Option<String>,
    pub input: Option<Value>,
    pub attachments: Option<Value>,
    pub repositories: Option<Value>,
    pub contexts: Option<Value>,
    /// One profile id, or `null` to carry none (AEGIS ADR-140 D8); not sent,
    /// unchanged.
    #[serde(default, deserialize_with = "sent")]
    pub profile: Option<Value>,
    pub at: Option<String>,
    pub recurrence: Option<RecurrenceInput>,
}

fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > NAME_MAX_CHARS {
        return Err(NAME_REFUSAL.to_string());
    }
    Ok(name.to_string())
}

fn check_target(target: &str) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err(TARGET_REFUSAL.to_string());
    }
    Ok(target.to_string())
}

fn check_attachments(raw: Option<Value>) -> Result<Vec<AttachmentRef>, String> {
    match raw {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value).map_err(|_| ATTACHMENTS_REFUSAL.to_string()),
    }
}

fn check_contexts(raw: Option<Value>) -> Result<Option<Value>, String> {
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            check_contexts_shape(&value).map_err(str::to_string)?;
            Ok(Some(value))
        }
    }
}

/// A schedule's `profile` (AEGIS ADR-140 D8): `null` or absent is none, a
/// string holding a UUID is that profile; anything else is refused.
fn check_profile(raw: Option<Value>) -> Result<Option<Uuid>, String> {
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read_profile_value(&value).map(Some).map_err(str::to_string),
    }
}

/// AEGIS ADR-140 D10: a schedule carries one profile or raw bindings,
/// never a profile with bindings beside it.
fn check_one_choice(profile_id: Option<Uuid>, contexts: Option<&Value>) -> Result<(), String> {
    if profile_id.is_some() && contexts_chosen(contexts) {
        return Err(PROFILE_WITH_CONTEXTS.to_string());
    }
    Ok(())
}

fn check_repositories(raw: Option<Value>) -> Result<Option<Value>, String> {
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let entries = parse_run_repositories(&value).map_err(str::to_string)?;
            if entries.iter().any(|entry| entry.author.is_some()) {
                return Err(REPOSITORIES_SHAPE.to_string());
            }
            Ok(Some(value))
        }
    }
}

impl Schedule {
    /// A new, active schedule from a person's draft, checked before
    /// anything is stored (N1, N2, N3).
    pub fn create(
        draft: ScheduleDraft,
        owner: &UserIdentity,
        tenant_id: TenantId,
        now: DateTime<Utc>,
    ) -> Result<Self, String> {
        let owner = ScheduleOwner::from_identity(owner)?;
        let name = check_name(draft.name.as_deref().unwrap_or_default())?;
        let target_kind = draft
            .target_kind
            .as_deref()
            .and_then(TargetKind::parse)
            .ok_or_else(|| TARGET_KIND_REFUSAL.to_string())?;
        let target = check_target(draft.target.as_deref().unwrap_or_default())?;
        let attachments = check_attachments(draft.attachments)?;
        let contexts = check_contexts(draft.contexts)?;
        let profile_id = check_profile(draft.profile)?;
        check_one_choice(profile_id, contexts.as_ref())?;
        let repositories = check_repositories(draft.repositories)?;
        let timing = parse_timing(draft.at.as_deref(), draft.recurrence, now)?;
        let id = ScheduleId::new();
        Ok(Self {
            id,
            tenant_id,
            owner,
            name,
            target_kind,
            target,
            target_version: draft.version.filter(|v| !v.trim().is_empty()),
            intent: draft.intent,
            input: draft.input.unwrap_or_else(|| serde_json::json!({})),
            attachments,
            repositories,
            contexts,
            profile_id,
            timing,
            state: ScheduleState::Active,
            paused_reason: None,
            temporal_schedule_id: id.temporal_schedule_id(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        })
    }

    /// Apply a person's change, checked as a create is (N10). The owner's
    /// projection is written again from their live token. A completed
    /// schedule given a new timing is active again.
    pub fn apply(
        &mut self,
        patch: SchedulePatch,
        owner: &UserIdentity,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let owner = ScheduleOwner::from_identity(owner)?;
        let mut next = self.clone();
        next.owner = owner;
        if let Some(name) = patch.name {
            next.name = check_name(&name)?;
        }
        if let Some(kind) = patch.target_kind {
            next.target_kind =
                TargetKind::parse(&kind).ok_or_else(|| TARGET_KIND_REFUSAL.to_string())?;
        }
        if let Some(target) = patch.target {
            next.target = check_target(&target)?;
        }
        if let Some(version) = patch.version {
            next.target_version = Some(version).filter(|v| !v.trim().is_empty());
        }
        if let Some(intent) = patch.intent {
            next.intent = Some(intent).filter(|i| !i.is_empty());
        }
        if let Some(input) = patch.input {
            next.input = input;
        }
        if patch.attachments.is_some() {
            next.attachments = check_attachments(patch.attachments)?;
        }
        if patch.contexts.is_some() {
            next.contexts = check_contexts(patch.contexts)?;
        }
        if patch.profile.is_some() {
            next.profile_id = check_profile(patch.profile)?;
        }
        check_one_choice(next.profile_id, next.contexts.as_ref())?;
        if patch.repositories.is_some() {
            next.repositories = check_repositories(patch.repositories)?;
        }
        if patch.at.is_some() || patch.recurrence.is_some() {
            next.timing = parse_timing(patch.at.as_deref(), patch.recurrence, now)?;
            if next.state == ScheduleState::Completed {
                next.state = ScheduleState::Active;
            }
        }
        next.updated_at = now;
        *self = next;
        Ok(())
    }

    /// The input a run starts with: the schedule's input, its `contexts`,
    /// `profile` (AEGIS ADR-140 D8) and `repositories` in the input's
    /// reserved keys, and no conversation (N7). A non-object input is wrapped as
    /// `{"input": <value>}`, as the starting tools wrap it.
    pub fn start_input(&self) -> Value {
        let mut input = self.input.clone();
        if !input.is_object() {
            let original = std::mem::replace(&mut input, Value::Null);
            input = serde_json::json!({ "input": original });
        }
        if let Value::Object(map) = &mut input {
            map.remove(CONTEXTS_INPUT_KEY);
            map.remove(REPOSITORIES_INPUT_KEY);
            map.remove(CONVERSATION_INPUT_KEY);
            map.remove(PROFILE_INPUT_KEY);
            if let Some(contexts) = &self.contexts {
                map.insert(CONTEXTS_INPUT_KEY.to_string(), contexts.clone());
            }
            if let Some(profile) = &self.profile_id {
                map.insert(
                    PROFILE_INPUT_KEY.to_string(),
                    Value::String(profile.to_string()),
                );
            }
            if let Some(repositories) = &self.repositories {
                map.insert(REPOSITORIES_INPUT_KEY.to_string(), repositories.clone());
            }
        }
        input
    }

    pub fn is_once(&self) -> bool {
        matches!(self.timing, Timing::Once { .. })
    }
}

// ── Fires (N6, N7) ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FireOutcome {
    /// The fire is being decided.
    Starting,
    Started,
    Refused,
    SkippedPaused,
    SkippedOverlap,
}

impl FireOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Refused => "refused",
            Self::SkippedPaused => "skipped_paused",
            Self::SkippedOverlap => "skipped_overlap",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "starting" => Some(Self::Starting),
            "started" => Some(Self::Started),
            "refused" => Some(Self::Refused),
            "skipped_paused" => Some(Self::SkippedPaused),
            "skipped_overlap" => Some(Self::SkippedOverlap),
            _ => None,
        }
    }
}

/// One fire of a schedule: one row per (schedule, scheduled time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleFire {
    pub id: Uuid,
    pub schedule_id: ScheduleId,
    pub scheduled_time: DateTime<Utc>,
    pub fired_at: DateTime<Utc>,
    pub outcome: FireOutcome,
    pub execution_id: Option<ExecutionId>,
    pub detail: Option<String>,
}

/// The answer of [`ScheduleRepository::claim_fire`].
#[derive(Debug, Clone, PartialEq)]
pub enum FireClaim {
    /// This call claimed the scheduled time: it decides the fire.
    Claimed(ScheduleFire),
    /// The scheduled time was fired before: the first fire's row.
    Repeated(ScheduleFire),
}

// ── The repository port ──────────────────────────────────────────────────────

#[async_trait]
pub trait ScheduleRepository: Send + Sync {
    async fn insert(&self, schedule: &Schedule) -> Result<(), RepositoryError>;
    async fn update(&self, schedule: &Schedule) -> Result<(), RepositoryError>;
    /// Remove a row outright: only for a schedule whose Temporal Schedule
    /// could not be made, so nothing was saved (N5).
    async fn remove(&self, id: ScheduleId) -> Result<(), RepositoryError>;
    /// A schedule by id, deleted or not.
    async fn get(&self, id: ScheduleId) -> Result<Option<Schedule>, RepositoryError>;
    /// The owner's schedules that are not deleted, newest first.
    async fn list_for_owner(&self, owner_sub: &str) -> Result<Vec<Schedule>, RepositoryError>;
    /// A tenant's schedules that are not deleted, newest first.
    async fn list_for_tenant(&self, tenant: &TenantId) -> Result<Vec<Schedule>, RepositoryError>;
    /// Every schedule that is active or paused and not deleted (the boot
    /// re-creation, N5).
    async fn list_live(&self) -> Result<Vec<Schedule>, RepositoryError>;
    /// Insert a `starting` fire for (schedule, scheduled time), or answer
    /// the row a fire of that time already wrote.
    async fn claim_fire(
        &self,
        schedule_id: ScheduleId,
        scheduled_time: DateTime<Utc>,
        fired_at: DateTime<Utc>,
    ) -> Result<FireClaim, RepositoryError>;
    /// Write a decided fire's outcome.
    async fn finish_fire(&self, fire: &ScheduleFire) -> Result<(), RepositoryError>;
    /// The schedule's fires, newest scheduled time first.
    async fn fires(
        &self,
        schedule_id: ScheduleId,
        limit: usize,
    ) -> Result<Vec<ScheduleFire>, RepositoryError>;
    /// Record the schedule on the started run's execution record (N7).
    async fn bind_execution(
        &self,
        kind: TargetKind,
        execution_id: ExecutionId,
        schedule_id: ScheduleId,
    ) -> Result<(), RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(cron: &str) -> Recurrence {
        Recurrence {
            cron: cron.to_string(),
            timezone: "UTC".to_string(),
            jitter_seconds: 0,
        }
    }

    #[test]
    fn cron_fields_parse_ranges_steps_and_lists() {
        let spec = CronSpec::parse("*/15 9-17 * * 1-5").expect("parses");
        assert_eq!(
            spec.minutes.iter().copied().collect::<Vec<_>>(),
            vec![0, 15, 30, 45]
        );
        assert_eq!(spec.hours.len(), 9);
        assert!(CronSpec::parse("0 0 * * 7")
            .unwrap()
            .days_of_week
            .contains(&0));
        for bad in [
            "* * * *",
            "* * * * * *",
            "@daily",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "*/0 * * * *",
            "a * * * *",
            ", * * * *",
        ] {
            assert!(CronSpec::parse(bad).is_err(), "'{bad}' parsed");
        }
    }

    #[test]
    fn the_shortest_gap_is_measured_within_a_day_and_across_midnight() {
        assert_eq!(
            CronSpec::parse("*/30 * * * *")
                .unwrap()
                .shortest_gap_minutes(),
            Some(30)
        );
        assert_eq!(
            CronSpec::parse("0 15 * * 1-5")
                .unwrap()
                .shortest_gap_minutes(),
            Some(1_440)
        );
        assert_eq!(
            CronSpec::parse("58 23 * * *")
                .unwrap()
                .shortest_gap_minutes(),
            Some(1_440)
        );
        // 12:00 and 12:03: three minutes within the day.
        assert_eq!(
            CronSpec::parse("0,3 12 * * *")
                .unwrap()
                .shortest_gap_minutes(),
            Some(3)
        );
        // 23:58 and 00:01: three minutes across midnight.
        assert_eq!(
            CronSpec::parse("1,58 0,23 * * *")
                .unwrap()
                .shortest_gap_minutes(),
            Some(3)
        );
        assert_eq!(
            CronSpec::parse("0 0 30 2 *")
                .unwrap()
                .shortest_gap_minutes(),
            None
        );
    }

    #[test]
    fn a_recurrence_is_refused_in_n2s_words() {
        assert_eq!(rec("0 15 * * 1-5").validate(), Ok(()));
        assert_eq!(rec("0 15 * *").validate(), Err(CRON_REFUSAL.to_string()));
        let mut zone = rec("0 15 * * *");
        zone.timezone = "Mars/Olympus".into();
        assert_eq!(zone.validate(), Err(TIMEZONE_REFUSAL.to_string()));
        zone.timezone = "Europe/Berlin".into();
        assert_eq!(zone.validate(), Ok(()));
        assert_eq!(
            rec("*/2 * * * *").validate(),
            Err("A schedule runs at most once every 5 minutes.".to_string())
        );
        assert_eq!(rec("*/5 * * * *").validate(), Ok(()));
        let mut jitter = rec("0 15 * * *");
        jitter.jitter_seconds = 3_601;
        assert_eq!(
            jitter.validate(),
            Err("'jitter_seconds' must be between 0 and 3600.".to_string())
        );
        jitter.jitter_seconds = 3_600;
        assert_eq!(jitter.validate(), Ok(()));
    }

    #[test]
    fn at_lies_between_one_minute_and_366_days_ahead() {
        let now = Utc::now();
        let fmt = |t: DateTime<Utc>| t.to_rfc3339();
        assert!(parse_at(&fmt(now + Duration::seconds(61)), now).is_ok());
        assert!(parse_at(&fmt(now + Duration::days(366)), now).is_ok());
        for bad in [
            fmt(now + Duration::seconds(59)),
            fmt(now - Duration::hours(1)),
            fmt(now + Duration::days(366) + Duration::seconds(1)),
            "tomorrow".to_string(),
        ] {
            assert_eq!(parse_at(&bad, now), Err(AT_REFUSAL.to_string()), "{bad}");
        }
    }

    /// AEGIS ADR-140 D8 and D10: a schedule saved with a profile starts its
    /// runs on it (the reserved key, any `profile` of the input replaced);
    /// one saved or changed to carry a profile beside chosen contexts is
    /// refused with D10's sentence; `null` clears the profile.
    #[test]
    fn a_schedule_carries_one_profile_into_its_runs_and_never_beside_contexts() {
        let owner = crate::domain::iam::UserIdentity {
            sub: "owner".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::for_consumer_user("owner").unwrap(),
            },
        };
        let tenant = TenantId::for_consumer_user("owner").unwrap();
        let profile = "2b7e4c1a-9d3f-4e5a-8b6c-7d8e9f0a1b2c";
        let binding = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
        let draft = |profile: Option<Value>, contexts: Option<Value>| ScheduleDraft {
            name: Some("triage".into()),
            target_kind: Some("agent".into()),
            target: Some("mail-triage".into()),
            input: Some(serde_json::json!({"q": 1, "profile": "someone-elses"})),
            contexts,
            profile,
            recurrence: Some(RecurrenceInput {
                cron: Some("0 15 * * 1-5".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut complaints = Vec::new();
        let mut schedule = Schedule::create(
            draft(Some(serde_json::json!(profile)), None),
            &owner,
            tenant.clone(),
            Utc::now(),
        )
        .expect("creates");
        if schedule.start_input() != serde_json::json!({ "q": 1, "profile": profile }) {
            complaints.push(format!("the run's input was {}", schedule.start_input()));
        }
        let both = Schedule::create(
            draft(
                Some(serde_json::json!(profile)),
                Some(serde_json::json!({ "imap": binding })),
            ),
            &owner,
            tenant.clone(),
            Utc::now(),
        );
        if both.as_ref().err().map(String::as_str) != Some(PROFILE_WITH_CONTEXTS) {
            complaints.push(format!("a profile beside contexts was {both:?}"));
        }
        let changed: SchedulePatch =
            serde_json::from_value(serde_json::json!({ "contexts": { "imap": binding } })).unwrap();
        let refused = schedule.apply(changed, &owner, Utc::now());
        if refused.as_ref().err().map(String::as_str) != Some(PROFILE_WITH_CONTEXTS) {
            complaints.push(format!("contexts added beside the profile was {refused:?}"));
        }
        let switched: SchedulePatch = serde_json::from_value(
            serde_json::json!({ "profile": null, "contexts": { "imap": binding } }),
        )
        .unwrap();
        if let Err(e) = schedule.apply(switched, &owner, Utc::now()) {
            complaints.push(format!("switching to raw bindings was refused: {e}"));
        }
        if schedule.profile_id.is_some() {
            complaints.push("`profile: null` left the profile".to_string());
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    #[test]
    fn the_start_input_carries_contexts_and_repositories_and_no_conversation() {
        let owner = crate::domain::iam::UserIdentity {
            sub: "owner".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::for_consumer_user("owner").unwrap(),
            },
        };
        let binding = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
        let schedule = Schedule::create(
            ScheduleDraft {
                name: Some("triage".into()),
                target_kind: Some("agent".into()),
                target: Some("mail-triage".into()),
                input: Some(serde_json::json!({"q": 1, "conversation_id": "x", "contexts": {}})),
                contexts: Some(serde_json::json!({ "imap": binding })),
                repositories: Some(serde_json::json!([{ "binding_id": binding }])),
                recurrence: Some(RecurrenceInput {
                    cron: Some("0 15 * * 1-5".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &owner,
            TenantId::for_consumer_user("owner").unwrap(),
            Utc::now(),
        )
        .expect("creates");
        assert_eq!(
            schedule.start_input(),
            serde_json::json!({
                "q": 1,
                "contexts": { "imap": binding },
                "repositories": [{ "binding_id": binding }]
            })
        );
        let rebuilt = schedule
            .owner
            .to_identity(&schedule.tenant_id)
            .expect("rebuilds");
        assert_eq!(rebuilt.sub, "owner");
        assert!(matches!(
            rebuilt.identity_kind,
            IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                ..
            }
        ));
    }
}
