// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # LLM Provider Registry
//!
//! `ProviderRegistry` resolves model aliases (e.g. `"default"`, `"fast"`,
//! `"smart"`) to real `LLMProvider` adapters at runtime based on node config.
//! Includes retry-with-exponential-backoff and one-level fallback: an alias's
//! own `fallback_alias` after the provider's time limit, else the node-wide
//! `fallback_provider`.

use crate::domain::llm::{
    ChatMessage, ChatResponse, GenerationOptions, GenerationResponse, LLMError, LLMProvider,
    ToolSchema, IMAGES_REFUSED,
};
use crate::domain::node_config::{
    resolve_env_value, LLMProviderConfig, LLMSelectionStrategy, NodeConfigManifest,
};
use crate::domain::secrets::SensitiveString;
use crate::domain::vision::{ImageGenerationModels, VisionProvider};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::anthropic::AnthropicAdapter;
use super::gemini::GeminiAdapter;
use super::ollama::OllamaAdapter;
use super::openai::OpenAIAdapter;
use super::workers_ai_image::WorkersAiImageAdapter;

/// Indicates whether an API key is platform-managed or user-provided (BYOK).
///
/// Used by the inner loop to decide whether to enforce LLM rate limits:
/// BYOK users consume their own provider quota, so platform rate limits
/// for `LlmCall` and `LlmToken` are skipped (ADR-072).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// Key resolved from an environment variable or node config (platform-provided).
    Platform,
    /// Key provided directly by the user (Bring Your Own Key).
    User,
}

/// Maximum exponent for exponential backoff, preventing `u64` overflow in delay calculations.
/// For example, with `retry_delay_ms = 1000` (1 second), 2^16 × 1 s ≈ 18 hours; the
/// actual upper bound on backoff duration still depends on the configured base delay.
const MAX_BACKOFF_EXPONENT: u32 = 16;

/// Maximum backoff duration in milliseconds. Delays are clamped to this value regardless
/// of the configured base delay, ensuring retries always terminate in bounded time.
/// Matches the value produced by `retry_delay_ms = 1000` and `MAX_BACKOFF_EXPONENT = 16`.
const MAX_BACKOFF_MS: u64 = 65_536_000; // 2^16 * 1000 ms

/// The least wait between attempts while a provider's rate limit or
/// unavailability is retried up to a call's deadline, so a configured
/// `retry_delay_ms` of 0 never sends attempts back to back for the whole
/// bound.
const UPSTREAM_RETRY_FLOOR_MS: u64 = 100;

/// The longest backoff between attempts while a provider's rate limit or
/// unavailability is retried inside a try: the doubling stops here, so a
/// burst's end is met within this, and the bound's last wait is shortened to
/// leave one attempt (AEGIS ADR-141, Update of 2026-10-15
/// `upstream-rate-limit-spends-no-try`, amendment
/// `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`). A longer
/// `Retry-After` from the provider is still honoured.
const UPSTREAM_BACKOFF_CAP_MS: u64 = 16_000;

/// The alias another alias falls back to, resolved at construction from the
/// alias map: the named alias, its model and its adapter.
struct AliasFallback {
    alias: String,
    model: String,
    adapter: Arc<dyn LLMProvider>,
    /// The named alias's window for a request, in bytes: its entry's
    /// `context_window - max_output_tokens` (8,192 where it sets none), the
    /// measure of ADR-131 U16a. A larger request is not sent to it (ADR-130,
    /// Update of 2026-10-05, D2b).
    window_bytes: u64,
}

/// Registry for managing LLM providers and resolving model aliases.
///
/// Each entry in `alias_map` is an `Arc<dyn LLMProvider>` that was constructed at
/// startup with the **exact model name** that won the selection-strategy evaluation.
/// There is no runtime model-override: the adapter *is* the model.
///
/// `providers` holds one health-check adapter per provider name (using `models.first()`).
pub struct ProviderRegistry {
    /// alias → pre-configured adapter for that exact model.
    alias_map: HashMap<String, (String, Arc<dyn LLMProvider>)>,
    /// provider_name → adapter used for health checks (built from models.first()).
    providers: HashMap<String, Arc<dyn LLMProvider>>,
    /// The node's image generation aliases, in the order the configuration
    /// lists them, each bound to an image provider for its exact model and to
    /// no `LLMProvider` (AEGIS ADR-143 I1, ADR-046 D1 and Alternative 2).
    image_aliases: Vec<(String, Arc<dyn VisionProvider>)>,
    /// Fallback adapter resolved at construction time; used when primary exhausts retries,
    /// for an alias that is not in `alias_fallbacks`.
    fallback_provider: Option<(String, Arc<dyn LLMProvider>)>,
    /// alias → the alias its model entry names as `fallback_alias`, tried once
    /// after the provider's time limit on the alias's own model, with the
    /// named alias's options. An alias here never reaches `fallback_provider`.
    alias_fallbacks: HashMap<String, AliasFallback>,
    /// alias → raw `api_key` value from the provider config (before env resolution).
    /// Used to determine [`ApiKeySource`] for BYOK exemption (ADR-072).
    raw_api_keys: HashMap<String, Option<SensitiveString>>,
    /// alias → per-model `max_output_tokens` override from config.
    /// When set, overrides `GenerationOptions::max_tokens` for calls on that alias.
    alias_max_output_tokens: HashMap<String, u32>,
    /// alias → per-model `temperature` override from config.
    /// When set, overrides `GenerationOptions::temperature` for calls on that alias.
    alias_temperatures: HashMap<String, f32>,
    /// Aliases whose winning model entry carries the capability `vision`:
    /// the aliases an image is shown to (AEGIS ADR-046 D2).
    vision_aliases: std::collections::HashSet<String>,
    max_retries: u32,
    retry_delay_ms: u64,
    /// Wall-clock budget for the full retry+fallback loop in `generate_chat` /
    /// `generate`. On elapsed: return `LLMError::Network("upstream timeout
    /// after Ns")`. Sourced from `LLMSelection::llm_overall_timeout_secs`.
    llm_overall_timeout_secs: u64,
    /// Bound on one attempt (primary or fallback) in `generate_chat` /
    /// `generate`. On elapsed: the attempt fails with `LLMError::Network(
    /// "attempt timeout after Ns: ...")`, retryable like an HTTP 408. `None`:
    /// no bound. Sourced from `LLMSelection::llm_attempt_timeout_secs`.
    llm_attempt_timeout_secs: Option<u64>,
}

/// Returns true for `LLMError` variants that are deterministic upstream
/// rejections — retrying or swapping to a fallback provider with the same
/// credentials/region won't change the answer.
///
/// `InvalidInput` is what every adapter returns for a provider's HTTP 400:
/// the request as sent is refused (a prompt over the model's context window,
/// among others), so the call fails at once with the provider's message,
/// where each such 400 was sent `max_retries` times (AEGIS ADR-124, the
/// measured Update of 2026-10-01). A 429, a 5xx and a 408 stay retryable,
/// except Workers AI's 408 with code 3046, its time limit, which
/// [`provider_timeout_for_alias`] sends to the fallback at once, or returns.
///
/// NOTE: `ServiceUnavailable` (HTTP 503) is intentionally NOT in this list.
/// 503 indicates transient upstream load (e.g. Gemini "high demand"); it must
/// run through the exponential-backoff retry loop and, on exhaustion, fall
/// back to the secondary provider — same as other retryable errors. This
/// aligns with the SEAL classifier's `UpstreamUnavailable = Recoverable`
/// pattern in `application::inner_loop_service`.
/// The provider's time-limit error with the alias put in, when `e` is one
/// (`openai::is_provider_timeout`). The identical request meets the same limit
/// again, so it is not sent to the same model again: in AEGIS execution
/// 18d8e0ca each retry of a 3046 answer took another 120 s and the iteration
/// ended at the 300 s budget as "upstream timeout after 300s", saying nothing
/// of why. A configured fallback, a different model, is still tried once, as
/// after the last retry; the error is returned where there is none.
fn provider_timeout_for_alias(e: &LLMError, alias: &str) -> Option<LLMError> {
    match e {
        LLMError::Provider(msg) if super::openai::is_provider_timeout(e) => {
            let detail = &msg[super::openai::PROVIDER_TIMEOUT_PREFIX.len()..];
            Some(LLMError::Provider(format!(
                "{}alias '{alias}': {detail}",
                super::openai::PROVIDER_TIMEOUT_PREFIX
            )))
        }
        _ => None,
    }
}

/// The primary's time-limit error, with the failure of the fallback tried
/// after it, so the error names both models.
fn with_fallback_failure(timeout: LLMError, fallback_model: &str, fallback: &LLMError) -> LLMError {
    with_fallback_note(
        timeout,
        &format!("the fallback model '{fallback_model}' was tried once and failed: {fallback}"),
    )
}

/// The primary's error with a sentence about its fallback appended, keeping
/// the primary's class where it is a network or provider error.
fn with_fallback_note(primary: LLMError, note: &str) -> LLMError {
    match primary {
        LLMError::Network(msg) => LLMError::Network(format!("{msg}; {note}")),
        LLMError::Provider(msg) => LLMError::Provider(format!("{msg}; {note}")),
        other => LLMError::Provider(format!("{other}; {note}")),
    }
}

/// The capability an alias's model entry declares when its model reads
/// images (`ModelConfig.capabilities`).
const VISION: &str = "vision";

/// An adapter's refusal of an image part with the alias put in, when `e` is
/// one (`llm::images_refused`); the adapter knows its provider, only the
/// registry the alias.
fn images_refused_for_alias(e: &LLMError, alias: &str) -> Option<LLMError> {
    match e {
        LLMError::InvalidInput(msg) if msg.starts_with(IMAGES_REFUSED) => {
            Some(LLMError::InvalidInput(format!("alias '{alias}' {msg}")))
        }
        _ => None,
    }
}

/// The bound on the fallback's one try after a silent primary: the time
/// ADR-130 measured the fallback model to need, gpt-oss-20b (`coder`)
/// answering the request `smart` lost in 34.64 to 77.45 s; 78 s is the
/// longest, rounded up (AEGIS ADR-130, Update of 2026-10-05, D2a). Lowered to
/// `llm_attempt_timeout_secs` where that is shorter.
const FALLBACK_RESERVE: std::time::Duration = std::time::Duration::from_secs(78);

/// The UTF-8 byte length of a chat request's messages and tool definitions
/// as JSON, the measure ADR-131 U16a sets against an alias's window.
fn chat_request_bytes(messages: &[ChatMessage], tools: &[ToolSchema]) -> u64 {
    let bytes = |v: Result<Vec<u8>, serde_json::Error>| v.map(|b| b.len() as u64).unwrap_or(0);
    bytes(serde_json::to_vec(messages)) + bytes(serde_json::to_vec(tools))
}

/// How the primary model of a reported call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryFailure {
    /// No answer within the agent's `llm_timeout_seconds`.
    Silent { secs: u64 },
    /// Ended by the provider at its time limit (Workers AI 408, code 3046).
    TimeLimit,
    /// Any other error, which never reaches the fallback alias (ADR-130 D2).
    Error,
    /// The provider's rate limit or unavailability outlasted the call's
    /// retries and ended the try, which is not counted (AEGIS ADR-141,
    /// Update of 2026-10-15 `upstream-rate-limit-spends-no-try`, clause 2,
    /// and its amendment `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`);
    /// the report's `elapsed_ms` is the time spent retrying.
    EndedTheTryUpstream,
}

/// A span of time as a person reads it: "5 min 12 s", or "45 s" under a
/// minute.
fn minutes_and_seconds(ms: u64) -> String {
    let secs = ms / 1000;
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// What became of the fallback alias on a failed call.
#[derive(Debug)]
pub enum FallbackOutcome {
    /// Not sent: the request is larger than the fallback's window
    /// (ADR-130, Update of 2026-10-05, D2b).
    Skipped {
        request_bytes: u64,
        limit_bytes: u64,
    },
    /// Not sent: the try had less time left than half the fallback's bound.
    NoTime { left_secs: u64, needed_secs: u64 },
    /// No answer within the fallback's bound.
    Silent { secs: u64 },
    /// Sent once and failed with this error.
    Failed(LLMError),
}

/// The fallback alias of a failed call and its outcome.
#[derive(Debug)]
pub struct FallbackReport {
    pub alias: String,
    pub model: String,
    pub outcome: FallbackOutcome,
}

/// What the registry did on a failed call it was asked to report on.
#[derive(Debug)]
pub struct CallReport {
    pub alias: String,
    pub model: String,
    /// Requests sent, to the primary and to the fallback together.
    pub attempts: u32,
    pub elapsed_ms: u64,
    pub primary: PrimaryFailure,
    pub fallback: Option<FallbackReport>,
}

/// A failed model call bounded by the agent's `llm_timeout_seconds`: the
/// error, and where the alias names a fallback, the registry's report of it
/// (ADR-130, Update of 2026-10-05, D10). With no report the call took
/// today's path and its attempts are not known here.
#[derive(Debug)]
pub struct ModelCallFailure {
    pub error: LLMError,
    pub report: Option<Box<CallReport>>,
}

impl std::fmt::Display for ModelCallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for ModelCallFailure {}

/// The failure of a fallback in plain words, without the provider's detail.
fn plain_failure(e: &LLMError) -> &'static str {
    match e {
        LLMError::Network(_) => "could not be reached or gave no answer",
        LLMError::Authentication(_) => "refused the platform's credentials",
        LLMError::RateLimit(_) => "was over its rate limit",
        LLMError::ModelNotFound(_) => "is not available",
        LLMError::Provider(_) => "returned an error",
        LLMError::InvalidInput(_) => "refused the request as sent",
        LLMError::ServiceUnavailable(_) => "was unavailable",
    }
}

/// The provider's time limit, as a failed model call met it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderTimeLimit {
    /// The seconds the provider took before it ended the generation, where
    /// its error states them.
    pub seconds: Option<f64>,
}

impl ModelCallFailure {
    /// `Some` when the provider ended the model's generation at its time
    /// limit (Workers AI's 408 with code 3046), whatever became of a fallback
    /// tried after it. A call the agent's own `llm_timeout_seconds` ended is
    /// not the provider's limit and answers `None`, as every other failure.
    pub fn provider_time_limit(&self) -> Option<ProviderTimeLimit> {
        super::openai::provider_time_limit_seconds(&self.error)
            .map(|seconds| ProviderTimeLimit { seconds })
    }

    pub(crate) fn unreported(error: LLMError) -> Self {
        Self {
            error,
            report: None,
        }
    }

    /// This failure as the end of a try the provider's rate limit or
    /// unavailability ended, after `retried` of the call's retries: its
    /// sentence names the alias, the time and that the try is not counted
    /// ([`PrimaryFailure::EndedTheTryUpstream`]).
    pub fn ending_the_try_upstream(self, alias: &str, retried: std::time::Duration) -> Self {
        let (model, attempts) = match self.report {
            Some(report) => (report.model, report.attempts),
            None => (alias.to_string(), 0),
        };
        Self {
            error: self.error,
            report: Some(Box::new(CallReport {
                alias: alias.to_string(),
                model,
                attempts,
                elapsed_ms: retried.as_millis() as u64,
                primary: PrimaryFailure::EndedTheTryUpstream,
                fallback: None,
            })),
        }
    }

    /// Requests the registry sent; 0 when it did not report.
    pub fn attempts(&self) -> u32 {
        self.report.as_ref().map_or(0, |r| r.attempts)
    }

    /// Milliseconds the call took; 0 when the registry did not report.
    pub fn elapsed_ms(&self) -> u64 {
        self.report.as_ref().map_or(0, |r| r.elapsed_ms)
    }

    /// True only when a request was sent to the fallback alias.
    pub fn fallback_attempted(&self) -> bool {
        matches!(
            self.report.as_ref().and_then(|r| r.fallback.as_ref()),
            Some(FallbackReport {
                outcome: FallbackOutcome::Silent { .. } | FallbackOutcome::Failed(_),
                ..
            })
        )
    }

    /// The one sentence a person reads when the fallback was in play: which
    /// model failed and how, and what became of the fallback. Otherwise the
    /// error as it reads today.
    pub fn client_sentence(&self) -> String {
        let Some(report) = &self.report else {
            return self.error.to_string();
        };
        if matches!(report.primary, PrimaryFailure::EndedTheTryUpstream) {
            let state = match self.error {
                LLMError::RateLimit(_) => "was over its rate limit",
                _ => "was unavailable",
            };
            return format!(
                "The model on alias '{}' {state} for {} of retries; this try ended there and is not counted.",
                report.alias,
                minutes_and_seconds(report.elapsed_ms)
            );
        }
        let Some(fallback) = &report.fallback else {
            return self.error.to_string();
        };
        let primary = match report.primary {
            PrimaryFailure::Silent { secs } => format!("gave no answer within {secs} s"),
            PrimaryFailure::TimeLimit => "was ended by the provider at its time limit".to_string(),
            PrimaryFailure::Error | PrimaryFailure::EndedTheTryUpstream => "failed".to_string(),
        };
        let outcome = match &fallback.outcome {
            FallbackOutcome::Skipped {
                request_bytes,
                limit_bytes,
            } => format!(
                "was not tried: the request is {request_bytes} bytes and its window holds {limit_bytes}"
            ),
            FallbackOutcome::NoTime {
                left_secs,
                needed_secs,
            } => format!(
                "was not tried: the try had {left_secs} s left and the fallback needs at least {needed_secs} s"
            ),
            FallbackOutcome::Silent { secs } => {
                format!("was tried and gave no answer within {secs} s")
            }
            FallbackOutcome::Failed(e) => format!("was tried and {}", plain_failure(e)),
        };
        format!(
            "The model on alias '{}' ({}) {primary}; its fallback '{}' ({}) {outcome}.",
            report.alias, report.model, fallback.alias, fallback.model
        )
    }
}

/// The reason a request of `request_bytes` is not sent to `fallback`, when
/// its window cannot hold it (ADR-130, Update of 2026-10-05, D2b). The
/// request is never cut to fit.
fn fallback_window_refusal(
    alias: &str,
    fallback: &AliasFallback,
    request_bytes: u64,
) -> Option<String> {
    if request_bytes <= fallback.window_bytes {
        return None;
    }
    warn!(
        "Fallback alias skipped, request larger than its window: alias='{}', fallback_alias='{}', request_bytes={}, window_bytes={}",
        alias, fallback.alias, request_bytes, fallback.window_bytes
    );
    Some(format!(
        "the fallback model '{}' was not sent the request: it is {request_bytes} bytes and the window of alias '{}' holds {} (context_window - max_output_tokens)",
        fallback.model, fallback.alias, fallback.window_bytes
    ))
}

/// The error of a call the agent's `llm_timeout_seconds` ended.
fn llm_timeout_error(alias: &str, llm_timeout: std::time::Duration) -> LLMError {
    LLMError::Network(format!(
        "model call on alias '{alias}' gave no answer within the agent's \
         llm_timeout_seconds ({} s)",
        llm_timeout.as_secs()
    ))
}

/// How the primary's attempts of a chat call ended.
enum PrimaryEnd {
    Answer(ChatResponse),
    /// A non-retryable error: returned at once, no fallback.
    NonRetryable(LLMError),
    /// The provider's time limit, with the alias put in, and when the
    /// attempt window it ended in opened.
    TimeLimit(LLMError, tokio::time::Instant),
    /// The last failed retry of another retryable error.
    Exhausted(LLMError),
    /// A bounded call's attempt window ran out with no answer, ended by
    /// the shortest of its bounds.
    Elapsed {
        end: WindowEnd,
    },
}

/// Which bound ended a bounded call's attempt window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowEnd {
    /// The agent's `llm_timeout_seconds`.
    Attempt,
    /// The node's `llm_overall_timeout_secs`.
    Overall,
    /// The try's time left less the seconds kept for the answer's way back
    /// (AEGIS ADR-141, Update of 2026-10-16
    /// `tests-load-before-code-2026-10-16`, clause 3).
    Try,
}

/// What bounds a chat call made inside a try (AEGIS ADR-141, Update of
/// 2026-10-15 `upstream-rate-limit-spends-no-try`, clause 1, and its
/// amendment `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`).
#[derive(Debug, Clone, Copy)]
struct CallBound {
    /// The agent's `llm_timeout_seconds`: the bound on one attempt window,
    /// opened at the call's start and again after each wait on the
    /// provider's rate limit or unavailability; the other retries share the
    /// window they start in.
    attempt: std::time::Duration,
    /// The node's `llm_overall_timeout_secs` where it applies, the window's
    /// other bound; the same as `attempt` where it does not.
    overall: std::time::Duration,
    /// The last moment an attempt starts after the provider's rate limit or
    /// unavailability: the try's time left less the command margin, or the
    /// call's own bound for a call outside a try.
    retry_until: tokio::time::Instant,
    /// The moment no window of a call made in a try runs past: the try's
    /// deadline less `transport`; `None` outside a try.
    try_ends: Option<tokio::time::Instant>,
    /// The seconds of a try kept after its last model call for the answer's
    /// way back.
    transport: std::time::Duration,
}

impl CallBound {
    /// The error a run-out attempt window ends the call with.
    fn elapsed_error(&self, alias: &str, end: WindowEnd) -> LLMError {
        match end {
            WindowEnd::Overall => LLMError::Network(format!(
                "upstream timeout after {}s",
                self.overall.as_secs()
            )),
            WindowEnd::Attempt => llm_timeout_error(alias, self.attempt),
            WindowEnd::Try => try_time_up_error(alias, self.transport),
        }
    }
}

/// The error of a call the try's time ended: the try's last `transport`
/// seconds are kept for the answer's way back.
fn try_time_up_error(alias: &str, transport: std::time::Duration) -> LLMError {
    LLMError::Network(format!(
        "model call on alias '{alias}' gave no answer before the last {} s of its try, \
         which are kept for the answer's way back",
        transport.as_secs()
    ))
}

/// One attempt window of a bounded call: when it opened, when it runs out
/// and which bound ends it.
#[derive(Debug, Clone, Copy)]
struct AttemptWindow {
    opened: tokio::time::Instant,
    ends: tokio::time::Instant,
    end: WindowEnd,
}

impl AttemptWindow {
    fn open(bound: &CallBound) -> Self {
        let opened = tokio::time::Instant::now();
        let (ends, end) = if bound.overall < bound.attempt {
            (opened + bound.overall, WindowEnd::Overall)
        } else {
            (opened + bound.attempt, WindowEnd::Attempt)
        };
        match bound.try_ends {
            Some(try_ends) if try_ends < ends => Self {
                opened,
                ends: try_ends,
                end: WindowEnd::Try,
            },
            _ => Self { opened, ends, end },
        }
    }
}

/// The wait before the next attempt on a provider's rate limit or
/// unavailability, or `None` when no attempt can start before `retry_until`:
/// the backoff doubling from `retry_delay_ms`, capped at
/// [`UPSTREAM_BACKOFF_CAP_MS`] and never under [`UPSTREAM_RETRY_FLOOR_MS`],
/// or the provider's `Retry-After` where longer; the last wait is shortened
/// to leave one attempt before `retry_until`. A `Retry-After` reaching past
/// `retry_until` ends the call: the provider has said no attempt before it
/// is answered.
fn upstream_wait(
    retry_delay_ms: u64,
    attempt: u32,
    error: &LLMError,
    now: tokio::time::Instant,
    retry_until: tokio::time::Instant,
) -> Option<std::time::Duration> {
    let floor = std::time::Duration::from_millis(UPSTREAM_RETRY_FLOOR_MS);
    let doubling = retry_delay_ms
        .saturating_mul(2_u64.saturating_pow(attempt.min(MAX_BACKOFF_EXPONENT)))
        .min(UPSTREAM_BACKOFF_CAP_MS);
    let wait = std::time::Duration::from_millis(doubling).max(floor);
    let wait = match error {
        LLMError::RateLimit(Some(after)) => {
            if now + *after >= retry_until {
                return None;
            }
            wait.max(*after)
        }
        _ => wait,
    };
    let left = retry_until.saturating_duration_since(now);
    if left <= floor {
        return None;
    }
    Some(wait.min(left - floor))
}

fn is_non_retryable(e: &LLMError) -> bool {
    matches!(
        e,
        LLMError::Authentication(_) | LLMError::InvalidInput(_) | LLMError::ModelNotFound(_)
    )
}

/// The provider's refusals of the moment, a rate limit (HTTP 429) or an
/// unavailable model (HTTP 503): on a call with a deadline they are retried
/// for as long as the next attempt starts before it, not `max_retries` times
/// (AEGIS ADR-141, Update of 2026-10-15 `upstream-rate-limit-spends-no-try`,
/// clause 1).
fn is_upstream_refusal(e: &LLMError) -> bool {
    matches!(e, LLMError::RateLimit(_) | LLMError::ServiceUnavailable(_))
}

/// A generation with no text and no tool call is a provider fault, not an
/// answer: an empty string handed back as the agent's final output becomes the
/// iteration's output and goes to the judge as if the agent had written it
/// (execution 9c5b304e, iteration 1, 2026-10-01). Returns the fault for such a
/// response, retryable like any other `LLMError::Provider`.
fn empty_generation_fault(
    response: &ChatResponse,
    alias: &str,
    model: &str,
    attempt: u32,
    attempts: u32,
) -> Option<LLMError> {
    match response {
        ChatResponse::FinalText(r) if r.text.trim().is_empty() => Some(LLMError::Provider(
            format!(
                "empty generation: model '{model}' on alias '{alias}' returned no text and no tool call (attempt {attempt} of {attempts})"
            ),
        )),
        _ => None,
    }
}

impl ProviderRegistry {
    /// Create provider registry from node configuration.
    ///
    /// Applies the configured `LLMSelectionStrategy` to resolve each alias to its winner,
    /// then instantiates one `Arc<dyn LLMProvider>` per winning (provider, model) pair.
    /// Each adapter is pre-configured with the exact model name — no runtime override needed.
    ///
    /// | Strategy           | Wins when …                                                     |
    /// |--------------------|-----------------------------------------------------------------|
    /// | `PreferLocal`      | New provider `is_local()` and existing entry is not local       |
    /// | `PreferCloud`      | New provider is not local and existing entry is local           |
    /// | `CostOptimized`    | New model's `cost_per_1k_tokens` < existing model's cost        |
    /// | `LatencyOptimized` | Same as `PreferLocal` — local inference has lower latency       |
    pub fn from_config(config: &NodeConfigManifest) -> anyhow::Result<Self> {
        info!("Initializing LLM provider registry");

        let strategy = &config.spec.llm_selection.strategy;

        // ── Phase 1: health-check adapters (one per provider, models.first()) ────────────
        let mut providers: HashMap<String, Arc<dyn LLMProvider>> = HashMap::new();

        // ── Phase 1 + strategy selection pass ─────────────────────────────────────────
        // alias -> (provider_name, model_name, is_local, cost_per_1k_tokens)
        let mut alias_candidates: HashMap<String, (String, String, bool, f64)> = HashMap::new();

        for provider_config in &config.spec.llm_providers {
            if !provider_config.enabled {
                info!("Provider '{}' disabled, skipping", provider_config.name);
                continue;
            }

            let new_is_local = provider_config.is_local();

            // Health-check adapter uses the first model in the provider list.
            if let Some(first_model) = provider_config.models.first() {
                match Self::create_adapter(provider_config, &first_model.model) {
                    Ok(adapter) => {
                        providers.insert(provider_config.name.clone(), adapter);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize provider '{}': {}",
                            provider_config.name, e
                        );
                        continue;
                    }
                }
            }

            // Evaluate each alias under the selection strategy.
            for model_config in &provider_config.models {
                let alias = &model_config.alias;

                let should_insert = match alias_candidates.get(alias) {
                    None => true,
                    Some((_, _, existing_is_local, existing_cost)) => match strategy {
                        LLMSelectionStrategy::PreferLocal
                        | LLMSelectionStrategy::LatencyOptimized => {
                            new_is_local && !existing_is_local
                        }
                        LLMSelectionStrategy::PreferCloud => !new_is_local && *existing_is_local,
                        LLMSelectionStrategy::CostOptimized => {
                            model_config.cost_per_1k_tokens < *existing_cost
                        }
                    },
                };

                if should_insert {
                    info!(
                        "Mapping alias '{}' -> {} ({}) [strategy={:?}]",
                        alias, model_config.model, provider_config.name, strategy
                    );
                    alias_candidates.insert(
                        alias.clone(),
                        (
                            provider_config.name.clone(),
                            model_config.model.clone(),
                            new_is_local,
                            model_config.cost_per_1k_tokens,
                        ),
                    );
                } else {
                    info!(
                        "Alias '{}' already mapped to a preferred provider, skipping {} ({}) [strategy={:?}]",
                        alias, model_config.model, provider_config.name, strategy
                    );
                }
            }
        }

        if providers.is_empty() {
            warn!("No LLM providers configured - semantic validation will not be available");
        }

        // ── Phase 2: build one per-model adapter per winning alias ─────────────────────
        let mut alias_map: HashMap<String, (String, Arc<dyn LLMProvider>)> = HashMap::new();
        let mut raw_api_keys: HashMap<String, Option<SensitiveString>> = HashMap::new();
        let mut alias_max_output_tokens: HashMap<String, u32> = HashMap::new();
        let mut alias_temperatures: HashMap<String, f32> = HashMap::new();
        let mut vision_aliases = std::collections::HashSet::new();
        // alias → its winning entry's `fallback_alias`, for aliases whose adapter built.
        let mut fallback_alias_names: Vec<(String, String)> = Vec::new();
        // alias → its winning entry's window for a request in bytes (ADR-131 U16a).
        let mut alias_window_bytes: HashMap<String, u64> = HashMap::new();
        let mut image_aliases: Vec<(String, Arc<dyn VisionProvider>)> = Vec::new();

        for provider_config in &config.spec.llm_providers {
            if !provider_config.enabled {
                continue;
            }
            for model_config in &provider_config.models {
                let alias = &model_config.alias;
                if let Some((winner_provider, winner_model, _, _)) = alias_candidates.get(alias) {
                    if winner_provider == &provider_config.name
                        && winner_model == &model_config.model
                        && model_config.generates_images()
                    {
                        // An image generation alias is bound to an image
                        // provider and never to a language model.
                        match Self::create_image_adapter(provider_config, winner_model) {
                            Ok(adapter) => {
                                info!("Alias '{}' generates images ({})", alias, winner_model);
                                image_aliases.push((alias.clone(), adapter));
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to create image adapter for alias '{}' ({}): {}",
                                    alias, winner_model, e
                                );
                            }
                        }
                    } else if winner_provider == &provider_config.name
                        && winner_model == &model_config.model
                    {
                        match Self::create_adapter(provider_config, winner_model) {
                            Ok(adapter) => {
                                alias_map.insert(alias.clone(), (winner_model.clone(), adapter));
                                alias_window_bytes.insert(
                                    alias.clone(),
                                    u64::from(model_config.context_window).saturating_sub(
                                        u64::from(model_config.max_output_tokens.unwrap_or(8192)),
                                    ),
                                );
                                raw_api_keys.insert(alias.clone(), provider_config.api_key.clone());
                                if model_config.capabilities.iter().any(|c| c == VISION) {
                                    vision_aliases.insert(alias.clone());
                                }
                                if let Some(max_tokens) = model_config.max_output_tokens {
                                    info!(
                                        "Alias '{}' max_output_tokens override: {}",
                                        alias, max_tokens
                                    );
                                    alias_max_output_tokens.insert(alias.clone(), max_tokens);
                                }
                                if let Some(temp) = model_config.temperature {
                                    info!("Alias '{}' temperature override: {}", alias, temp);
                                    alias_temperatures.insert(alias.clone(), temp);
                                }
                                if let Some(fallback) = &model_config.fallback_alias {
                                    fallback_alias_names.push((alias.clone(), fallback.clone()));
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to create adapter for alias '{}' ({}): {}",
                                    alias, winner_model, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Resolve each alias's own fallback to the named alias's entry in the
        // alias map. A mapped alias whose named alias has no adapter refuses
        // the registry: the node never runs with a fallback configured and
        // silently absent.
        let mut alias_fallbacks: HashMap<String, AliasFallback> = HashMap::new();
        for (alias, fallback) in fallback_alias_names {
            let Some((model, adapter)) = alias_map.get(&fallback) else {
                anyhow::bail!(
                    "alias '{alias}' names fallback_alias '{fallback}', which has no adapter: the alias it names was not mapped or its adapter failed to build"
                );
            };
            info!(
                "Alias '{}' falls back to alias '{}' ({}) after the provider's time limit",
                alias, fallback, model
            );
            alias_fallbacks.insert(
                alias,
                AliasFallback {
                    window_bytes: alias_window_bytes.get(&fallback).copied().unwrap_or(0),
                    alias: fallback,
                    model: model.clone(),
                    adapter: adapter.clone(),
                },
            );
        }

        // Resolve fallback to a concrete adapter at construction time.
        // We look up the provider name and assume the first model was used for health checks.
        let fallback_provider = config
            .spec
            .llm_selection
            .fallback_provider
            .as_deref()
            .and_then(|name| {
                let adapter = providers.get(name)?.clone();
                let provider_config = config.spec.llm_providers.iter().find(|p| p.name == name)?;
                let first_model = provider_config.models.first()?.model.clone();
                Some((first_model, adapter))
            });

        Ok(Self {
            alias_map,
            providers,
            image_aliases,
            fallback_provider,
            alias_fallbacks,
            raw_api_keys,
            alias_max_output_tokens,
            alias_temperatures,
            vision_aliases,
            max_retries: config.spec.llm_selection.max_retries,
            retry_delay_ms: config.spec.llm_selection.retry_delay_ms,
            llm_overall_timeout_secs: config.spec.llm_selection.llm_overall_timeout_secs,
            llm_attempt_timeout_secs: config.spec.llm_selection.llm_attempt_timeout_secs,
        })
    }

    /// Create an adapter for the given provider config initialized with a specific model name.
    ///
    /// Called twice per (provider, model) pair:
    /// 1. With `models.first()` to build the health-check adapter stored in `providers`.
    /// 2. With the strategy-selected model to build the per-alias adapter stored in `alias_map`.
    fn create_adapter(
        config: &LLMProviderConfig,
        model: &str,
    ) -> anyhow::Result<Arc<dyn LLMProvider>> {
        let api_key = Self::resolve_api_key(&config.api_key)?;

        let endpoint = if config.endpoint.expose().is_empty() {
            match config.provider_type.as_str() {
                "openai" | "openai-compatible" => "https://api.openai.com/v1",
                "anthropic" => "https://api.anthropic.com/v1",
                "gemini" => "https://generativelanguage.googleapis.com/v1beta",
                "ollama" => "http://localhost:11434",
                _ => "",
            }
            .to_string()
        } else {
            config.endpoint.expose().to_string()
        };

        let provider: Arc<dyn LLMProvider> = match config.provider_type.as_str() {
            "openai" => Arc::new(OpenAIAdapter::new(
                endpoint,
                api_key,
                model.to_string(),
                Self::provider_headers(config)?,
            )),
            "ollama" => Arc::new(OllamaAdapter::new(endpoint, model.to_string())),
            "anthropic" => Arc::new(AnthropicAdapter::new(endpoint, api_key, model.to_string())),
            "gemini" => Arc::new(GeminiAdapter::new(endpoint, api_key, model.to_string())),
            // OpenAI-compatible APIs (LM Studio, vLLM, etc.)
            "openai-compatible" => Arc::new(OpenAIAdapter::new(
                endpoint,
                api_key,
                model.to_string(),
                Self::provider_headers(config)?,
            )),
            _ => anyhow::bail!("Unsupported provider type: {}", config.provider_type),
        };

        Ok(provider)
    }

    /// Create the image provider for an image generation alias of `config`:
    /// an `openai-compatible` provider whose endpoint is a Workers AI `/ai/v1`
    /// base is bound to the Workers AI adapter (`/ai/run`, ADR-143 Update
    /// I1a); every other `openai` and `openai-compatible` provider to the
    /// OpenAI adapter, against the provider's `/images/generations`. Any
    /// other provider type has none.
    fn create_image_adapter(
        config: &LLMProviderConfig,
        model: &str,
    ) -> anyhow::Result<Arc<dyn VisionProvider>> {
        match config.provider_type.as_str() {
            "openai" | "openai-compatible" => {
                let endpoint = if config.endpoint.expose().is_empty() {
                    "https://api.openai.com/v1".to_string()
                } else {
                    config.endpoint.expose().to_string()
                };
                let api_key = Self::resolve_api_key(&config.api_key)?;
                let headers = Self::provider_headers(config)?;
                if config.provider_type == "openai-compatible"
                    && WorkersAiImageAdapter::serves(&endpoint)
                {
                    return Ok(Arc::new(WorkersAiImageAdapter::new(
                        endpoint,
                        api_key,
                        model.to_string(),
                        headers,
                    )));
                }
                Ok(Arc::new(OpenAIAdapter::new(
                    endpoint,
                    api_key,
                    model.to_string(),
                    headers,
                )))
            }
            other => anyhow::bail!("provider type '{other}' has no image generation adapter"),
        }
    }

    /// The provider's configured `headers` as a request header map (AEGIS
    /// ADR-124 D3). A name or value that is not a valid HTTP header refuses
    /// the adapter, naming the provider and the header, never its value.
    fn provider_headers(config: &LLMProviderConfig) -> anyhow::Result<HeaderMap> {
        let mut map = HeaderMap::new();
        for (name, value) in config.headers.iter().flatten() {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                anyhow::anyhow!(
                    "provider '{}': '{}' is not a valid header name: {}",
                    config.name,
                    name,
                    e
                )
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|_| {
                anyhow::anyhow!(
                    "provider '{}': the value of header '{}' is not a valid header value",
                    config.name,
                    name
                )
            })?;
            map.insert(header_name, header_value);
        }
        Ok(map)
    }

    /// Resolve API key from config (supports "env:VAR_NAME" syntax)
    /// Delegates to the centralized `resolve_env_value()` utility.
    fn resolve_api_key(key: &Option<SensitiveString>) -> anyhow::Result<String> {
        match key {
            Some(k) => resolve_env_value(k.expose()),
            None => Ok(String::new()), // For local providers without auth
        }
    }

    /// Apply per-alias `max_output_tokens` override to a copy of the given options.
    /// If the alias has a configured override, `max_tokens` is replaced; otherwise
    /// the original options are returned unchanged.
    fn apply_alias_options(&self, alias: &str, options: &GenerationOptions) -> GenerationOptions {
        let mut opts = options.clone();
        if let Some(&max_tokens) = self.alias_max_output_tokens.get(alias) {
            opts.max_tokens = Some(max_tokens);
        }
        if let Some(&temp) = self.alias_temperatures.get(alias) {
            opts.temperature = Some(temp);
        }
        opts
    }

    /// Run one attempt of a model call, bounded by `llm_attempt_timeout_secs`
    /// when it is set (AEGIS ADR-124, the measured Update of 2026-10-01: a
    /// stalled attempt ended only at the provider's own 408 after about 235 s,
    /// leaving too little of the judgment's budget for the retry). An elapsed
    /// attempt is abandoned and returned as `LLMError::Network`, which the
    /// retry loop treats as it treats a 408.
    async fn bounded_attempt<T>(
        &self,
        alias: &str,
        model: &str,
        attempt: u32,
        attempts: u32,
        call: impl std::future::Future<Output = Result<T, LLMError>>,
    ) -> Result<T, LLMError> {
        let Some(secs) = self.llm_attempt_timeout_secs else {
            return call.await;
        };
        match tokio::time::timeout(tokio::time::Duration::from_secs(secs), call).await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    "LLM attempt timeout: alias='{}', model='{}', attempt={}/{}, bound={}s",
                    alias, model, attempt, attempts, secs
                );
                Err(LLMError::Network(format!(
                    "attempt timeout after {secs}s: model '{model}' on alias '{alias}' gave no answer (attempt {attempt} of {attempts})"
                )))
            }
        }
    }

    /// The request sent once to the alias's own fallback alias after the
    /// provider's time limit on its model, with the named alias's options.
    /// Its answer is returned; its failure is returned with the time limit,
    /// naming both models. A request larger than the fallback's window is not
    /// sent (ADR-130, Update of 2026-10-05, D2b).
    async fn chat_on_fallback_alias(
        &self,
        alias: &str,
        fallback: &AliasFallback,
        timeout: LLMError,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        if let Some(reason) =
            fallback_window_refusal(alias, fallback, chat_request_bytes(messages, tools))
        {
            return Err(with_fallback_note(timeout, &reason));
        }
        self.send_chat_to_fallback_alias(alias, fallback, messages, tools, options)
            .await
            .map_err(|fe| with_fallback_failure(timeout, &fallback.model, &fe))
    }

    /// One request to the fallback alias with its own options, bounded by
    /// `llm_attempt_timeout_secs`; an empty generation is a fault.
    async fn send_chat_to_fallback_alias(
        &self,
        alias: &str,
        fallback: &AliasFallback,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        info!(
            "Trying fallback alias: alias='{}', fallback_alias='{}', model='{}'",
            alias, fallback.alias, fallback.model
        );
        let fallback_options = self.apply_alias_options(&fallback.alias, options);
        self.bounded_attempt(
            &fallback.alias,
            &fallback.model,
            1,
            1,
            fallback
                .adapter
                .generate_chat(messages, tools, &fallback_options),
        )
        .await
        .and_then(|r| {
            match empty_generation_fault(&r, &fallback.alias, &fallback.model, 1, 1) {
                Some(fault) => Err(fault),
                None => Ok(r),
            }
        })
    }

    /// The single-prompt form of [`Self::chat_on_fallback_alias`].
    async fn generate_on_fallback_alias(
        &self,
        alias: &str,
        fallback: &AliasFallback,
        timeout: LLMError,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        if let Some(reason) = fallback_window_refusal(alias, fallback, prompt.len() as u64) {
            return Err(with_fallback_note(timeout, &reason));
        }
        info!(
            "Trying fallback alias: alias='{}', fallback_alias='{}', model='{}'",
            alias, fallback.alias, fallback.model
        );
        let fallback_options = self.apply_alias_options(&fallback.alias, options);
        self.bounded_attempt(
            &fallback.alias,
            &fallback.model,
            1,
            1,
            fallback.adapter.generate(prompt, &fallback_options),
        )
        .await
        .map_err(|fe| with_fallback_failure(timeout, &fallback.model, &fe))
    }

    /// The primary model's attempts of a chat call: retries with backoff,
    /// each attempt bounded by `llm_attempt_timeout_secs`, ended at once by
    /// a non-retryable error or the provider's time limit. `sent` counts the
    /// requests sent.
    ///
    /// With a `bound` (a call inside a try), every attempt runs inside an
    /// attempt window of the agent's `llm_timeout_seconds` (and the node's
    /// overall budget where shorter); a window that runs out ends the call
    /// as the call's bound always did. A provider's rate limit or
    /// unavailability ([`is_upstream_refusal`]) is retried past
    /// `max_retries` for as long as an attempt can start before the bound's
    /// `retry_until` ([`upstream_wait`]), each such attempt in a window of
    /// its own. Every other retryable error is sent `max_retries` times
    /// inside the window it started in.
    #[allow(clippy::too_many_arguments)]
    async fn primary_attempts(
        &self,
        alias: &str,
        model_name: &str,
        provider: &Arc<dyn LLMProvider>,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        effective_options: &GenerationOptions,
        sent: &std::sync::atomic::AtomicU32,
        bound: Option<CallBound>,
    ) -> PrimaryEnd {
        let call_started = tokio::time::Instant::now();
        let mut window = bound.as_ref().map(AttemptWindow::open);
        let mut last_error: Option<LLMError> = None;
        let mut attempt: u32 = 0;
        while attempt < self.max_retries
            || (bound.is_some() && last_error.as_ref().is_some_and(is_upstream_refusal))
        {
            sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let attempt_call = self.bounded_attempt(
                alias,
                model_name,
                attempt + 1,
                self.max_retries,
                provider.generate_chat(messages, tools, effective_options),
            );
            let outcome = match &window {
                Some(w) => match tokio::time::timeout_at(w.ends, attempt_call).await {
                    Ok(outcome) => outcome,
                    Err(_) => return PrimaryEnd::Elapsed { end: w.end },
                },
                None => attempt_call.await,
            }
            .and_then(|response| {
                match empty_generation_fault(
                    &response,
                    alias,
                    model_name,
                    attempt + 1,
                    self.max_retries,
                ) {
                    Some(fault) => Err(fault),
                    None => Ok(response),
                }
            });
            match outcome {
                Ok(response) => {
                    info!(
                        "generate_chat successful: alias='{}', model='{}', attempt={}",
                        alias,
                        model_name,
                        attempt + 1
                    );
                    return PrimaryEnd::Answer(response);
                }
                Err(e) => {
                    warn!(
                        "generate_chat failed: alias='{}', attempt={}/{}: {:?}",
                        alias,
                        attempt + 1,
                        self.max_retries,
                        e
                    );

                    if let Some(timeout) = provider_timeout_for_alias(&e, alias) {
                        warn!(
                            "LLM provider time limit; not sent again to this model: alias='{}', model='{}', attempt={}/{}",
                            alias,
                            model_name,
                            attempt + 1,
                            self.max_retries
                        );
                        return PrimaryEnd::TimeLimit(
                            timeout,
                            window.map_or(call_started, |w| w.opened),
                        );
                    }

                    if let Some(refusal) = images_refused_for_alias(&e, alias) {
                        return PrimaryEnd::NonRetryable(refusal);
                    }

                    // Short-circuit on deterministic upstream rejections — retrying with
                    // the same credentials (or with the fallback that shares them) is futile.
                    if is_non_retryable(&e) {
                        info!(
                            "LLM call non-retryable; short-circuiting: alias='{}', attempts={}, error={:?}",
                            alias,
                            attempt + 1,
                            e
                        );
                        return PrimaryEnd::NonRetryable(e);
                    }

                    if let Some(bound) = bound.filter(|_| is_upstream_refusal(&e)) {
                        let Some(wait) = upstream_wait(
                            self.retry_delay_ms,
                            attempt,
                            &e,
                            tokio::time::Instant::now(),
                            bound.retry_until,
                        ) else {
                            info!(
                                "LLM upstream refusal outlasts the call's bound: alias='{}', model='{}', attempts={}, error={:?}",
                                alias,
                                model_name,
                                attempt + 1,
                                e
                            );
                            return PrimaryEnd::Exhausted(e);
                        };
                        last_error = Some(e);
                        debug!(
                            "LLM upstream retry wait: alias='{}', duration_ms={}, attempt={}",
                            alias,
                            wait.as_millis(),
                            attempt + 1
                        );
                        tokio::time::sleep(wait).await;
                        window = Some(AttemptWindow::open(&bound));
                    } else {
                        if attempt + 1 >= self.max_retries {
                            return PrimaryEnd::Exhausted(e);
                        }
                        let backoff_ms = {
                            let capped_attempt = attempt.min(MAX_BACKOFF_EXPONENT);
                            self.retry_delay_ms
                                .saturating_mul(2_u64.saturating_pow(capped_attempt))
                                .min(MAX_BACKOFF_MS)
                        };
                        // A provider's `Retry-After` is waited when it is
                        // longer than the backoff.
                        let backoff = match &e {
                            LLMError::RateLimit(Some(after)) => {
                                tokio::time::Duration::from_millis(backoff_ms).max(*after)
                            }
                            _ => tokio::time::Duration::from_millis(backoff_ms),
                        };
                        last_error = Some(e);
                        debug!(
                            "LLM retry backoff: alias='{}', duration_ms={}, attempt={}",
                            alias,
                            backoff.as_millis(),
                            attempt + 1
                        );
                        match &window {
                            Some(w) => {
                                if tokio::time::timeout_at(w.ends, tokio::time::sleep(backoff))
                                    .await
                                    .is_err()
                                {
                                    return PrimaryEnd::Elapsed { end: w.end };
                                }
                            }
                            None => tokio::time::sleep(backoff).await,
                        }
                    }
                }
            }
            attempt += 1;
        }
        PrimaryEnd::Exhausted(
            last_error.unwrap_or_else(|| LLMError::Provider("Unknown error".into())),
        )
    }

    /// Generate a chat response for the given model alias.
    ///
    /// Resolves the alias directly to a pre-configured `Arc<dyn LLMProvider>` adapter;
    /// no model name override is needed at call time.
    /// Includes retry-with-exponential-backoff and one-level fallback.
    pub async fn generate_chat(
        &self,
        alias: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        self.generate_chat_until(alias, messages, tools, options, None)
            .await
    }

    /// [`Self::generate_chat`], with the bound of a call made inside a try
    /// ([`Self::primary_attempts`]): its attempt windows also carry the
    /// node's overall budget, and the call runs as long as its windows and
    /// its rate-limit retries do. `None` sends every retryable error
    /// `max_retries` times inside the overall budget.
    async fn generate_chat_until(
        &self,
        alias: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
        bound: Option<CallBound>,
    ) -> Result<ChatResponse, LLMError> {
        let (model_name, provider) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{alias}' not found")))?;

        info!("LLM inference: alias='{}', model='{}'", alias, model_name);

        let effective_options = self.apply_alias_options(alias, options);

        let overall_budget = tokio::time::Duration::from_secs(self.llm_overall_timeout_secs.max(1));
        let bound = bound.map(|b| CallBound {
            overall: overall_budget,
            ..b
        });

        let inner = async {
            let sent = std::sync::atomic::AtomicU32::new(0);
            let end = self
                .primary_attempts(
                    alias,
                    model_name,
                    provider,
                    messages,
                    tools,
                    &effective_options,
                    &sent,
                    bound,
                )
                .await;
            // A time limit ends the primary's attempts: the fallback, a
            // different model, is tried as after the last retry. An alias
            // naming its own fallback alias goes there after the time limit
            // only, and never to the node-wide fallback.
            let (time_limit, last_error) = match end {
                PrimaryEnd::Answer(r) => return Ok(r),
                PrimaryEnd::NonRetryable(e) => return Err(e),
                PrimaryEnd::TimeLimit(t, _) => (Some(t), None),
                PrimaryEnd::Exhausted(e) if sent.load(std::sync::atomic::Ordering::Relaxed) > 0 => {
                    (None, Some(e))
                }
                PrimaryEnd::Exhausted(e) => return Err(e),
                PrimaryEnd::Elapsed { end } => {
                    return Err(bound
                        .map(|b| b.elapsed_error(alias, end))
                        .unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
                }
            };
            if let Some(own) = self.alias_fallbacks.get(alias) {
                return match time_limit {
                    Some(timeout) => {
                        self.chat_on_fallback_alias(alias, own, timeout, messages, tools, options)
                            .await
                    }
                    None => {
                        Err(last_error
                            .unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
                    }
                };
            }
            if let Some((fallback_model, fallback)) = &self.fallback_provider {
                info!("Trying fallback provider (model='{}')", fallback_model);
                let attempt_call = self.bounded_attempt(
                    alias,
                    fallback_model,
                    1,
                    1,
                    fallback.generate_chat(messages, tools, options),
                );
                let outcome = match &bound {
                    Some(b) => {
                        let w = AttemptWindow::open(b);
                        match tokio::time::timeout_at(w.ends, attempt_call).await {
                            Ok(outcome) => outcome,
                            Err(_) => Err(b.elapsed_error(alias, w.end)),
                        }
                    }
                    None => attempt_call.await,
                }
                .and_then(|r| {
                    match empty_generation_fault(&r, alias, fallback_model, 1, 1) {
                        Some(fault) => Err(fault),
                        None => Ok(r),
                    }
                });
                return match outcome {
                    Ok(r) => Ok(r),
                    Err(fe) => Err(match time_limit {
                        Some(timeout) => with_fallback_failure(timeout, fallback_model, &fe),
                        None => fe,
                    }),
                };
            }
            Err(time_limit
                .or(last_error)
                .unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
        };

        // A bounded call is ended by its attempt windows and its retries'
        // bound, never by one budget for the whole of it.
        if bound.is_some() {
            return inner.await;
        }
        match tokio::time::timeout(overall_budget, inner).await {
            Ok(result) => result,
            Err(_) => {
                let secs = overall_budget.as_secs();
                warn!(
                    "generate_chat overall timeout: alias='{}', model='{}', budget={}s",
                    alias, model_name, secs
                );
                Err(LLMError::Network(format!("upstream timeout after {secs}s")))
            }
        }
    }

    /// A chat call bounded by the agent's `llm_timeout_seconds`. A call not
    /// answered by then is ended with an error naming the field and the
    /// seconds, typed as a network error so the gateway answers 502 as for
    /// the registry's own budget.
    ///
    /// A provider's rate limit or unavailability is retried until the try's
    /// time left less `command_margin` (the call's own `llm_timeout_seconds`
    /// outside a try), each attempt after a wait bounded by
    /// `llm_timeout_seconds` afresh (AEGIS ADR-141, Update of 2026-10-15
    /// `upstream-rate-limit-spends-no-try`, amendment
    /// `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`).
    ///
    /// On an alias that names a `fallback_alias` (AEGIS ADR-130, Update of
    /// 2026-10-05): the primary keeps the whole timeout, its time limit (3046)
    /// path unchanged. A primary silent for all of it goes once to the
    /// fallback alias, bounded by `FALLBACK_RESERVE` or by what is left of
    /// `try_left` (the time to the try's deadline at the call's start) when
    /// that is shorter but at least half the bound (D2a), and where its window
    /// holds the request (D2b); a failure carries the registry's report (D10).
    ///
    /// Inside a try, no window runs past the try's time left less `transport`,
    /// the seconds kept for the answer's way back: a call is bounded by the
    /// lesser of `llm_timeout` and that, and a call with no more than
    /// `transport` of the try left is not sent (AEGIS ADR-141, Update of
    /// 2026-10-16 `tests-load-before-code-2026-10-16`, clause 3).
    #[allow(clippy::too_many_arguments)]
    pub async fn generate_chat_within(
        &self,
        alias: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
        llm_timeout: std::time::Duration,
        try_left: Option<std::time::Duration>,
        command_margin: std::time::Duration,
        transport: std::time::Duration,
    ) -> Result<ChatResponse, ModelCallFailure> {
        if let Some(left) = try_left.filter(|left| *left <= transport) {
            warn!(
                "LLM call not sent, the try's time is up: alias='{}', left={}s, kept={}s",
                alias,
                left.as_secs(),
                transport.as_secs()
            );
            return Err(ModelCallFailure::unreported(LLMError::Network(format!(
                "model call on alias '{alias}' was not sent: its try had {} s left, and the \
                 last {} s of a try are kept for the answer's way back",
                left.as_secs(),
                transport.as_secs()
            ))));
        }
        let now = tokio::time::Instant::now();
        // Until when a provider's rate limit or unavailability is retried:
        // the try's time left less the margin kept after it for a command's
        // result, or the call's own bound outside a try.
        let bound = CallBound {
            attempt: llm_timeout,
            overall: llm_timeout,
            retry_until: now
                + try_left.map_or(llm_timeout, |left| left.saturating_sub(command_margin)),
            try_ends: try_left.map(|left| now + left.saturating_sub(transport)),
            transport,
        };
        let (Some(own), Some((model_name, provider))) =
            (self.alias_fallbacks.get(alias), self.alias_map.get(alias))
        else {
            return self
                .generate_chat_until(alias, messages, tools, options, Some(bound))
                .await
                .map_err(ModelCallFailure::unreported);
        };

        info!("LLM inference: alias='{}', model='{}'", alias, model_name);
        let started = tokio::time::Instant::now();
        let effective_options = self.apply_alias_options(alias, options);
        let sent = std::sync::atomic::AtomicU32::new(0);
        let sent_now = || sent.load(std::sync::atomic::Ordering::Relaxed);
        let report = |primary: PrimaryFailure, fallback: Option<FallbackReport>| {
            let fallback_sent = matches!(
                fallback,
                Some(FallbackReport {
                    outcome: FallbackOutcome::Silent { .. } | FallbackOutcome::Failed(_),
                    ..
                })
            );
            Some(Box::new(CallReport {
                alias: alias.to_string(),
                model: model_name.clone(),
                attempts: sent_now() + u32::from(fallback_sent),
                elapsed_ms: started.elapsed().as_millis() as u64,
                primary,
                fallback,
            }))
        };
        let fallback_report = |outcome: FallbackOutcome| FallbackReport {
            alias: own.alias.clone(),
            model: own.model.clone(),
            outcome,
        };

        // The primary keeps the agent's whole timeout for each attempt
        // window; its 3046 path goes to the fallback alias inside the window
        // it ended in, as before.
        let end = self
            .primary_attempts(
                alias,
                model_name,
                provider,
                messages,
                tools,
                &effective_options,
                &sent,
                Some(bound),
            )
            .await;
        let primary_error = match end {
            PrimaryEnd::Answer(r) => return Ok(r),
            PrimaryEnd::NonRetryable(e) | PrimaryEnd::Exhausted(e) => {
                return Err(ModelCallFailure {
                    error: e,
                    report: report(PrimaryFailure::Error, None),
                });
            }
            PrimaryEnd::TimeLimit(timeout, window_opened) => {
                let left = llm_timeout.saturating_sub(window_opened.elapsed());
                let left = match bound.try_ends {
                    Some(try_ends) => {
                        left.min(try_ends.saturating_duration_since(tokio::time::Instant::now()))
                    }
                    None => left,
                };
                return match tokio::time::timeout(
                    left,
                    self.chat_on_fallback_alias(alias, own, timeout, messages, tools, options),
                )
                .await
                {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(ModelCallFailure::unreported(e)),
                    Err(_) => Err(ModelCallFailure::unreported(llm_timeout_error(
                        alias,
                        llm_timeout,
                    ))),
                };
            }
            // The try's time is up: no fallback can answer inside it.
            PrimaryEnd::Elapsed {
                end: WindowEnd::Try,
            } => {
                return Err(ModelCallFailure {
                    error: try_time_up_error(alias, transport),
                    report: report(
                        PrimaryFailure::Silent {
                            secs: started.elapsed().as_secs(),
                        },
                        None,
                    ),
                });
            }
            PrimaryEnd::Elapsed { .. } => llm_timeout_error(alias, llm_timeout),
        };
        let primary = PrimaryFailure::Silent {
            secs: llm_timeout.as_secs(),
        };
        warn!(
            "LLM silent primary: alias='{}', model='{}', no answer within the agent's {}s",
            alias,
            model_name,
            llm_timeout.as_secs()
        );

        let bound = self
            .llm_attempt_timeout_secs
            .map(std::time::Duration::from_secs)
            .map_or(FALLBACK_RESERVE, |b| b.min(FALLBACK_RESERVE));
        // The fallback's bound, or the time left to the try's deadline where
        // that is shorter and at least half the bound; otherwise not tried.
        let mut bound = bound;
        if let Some(left) = try_left.map(|l| {
            l.saturating_sub(started.elapsed())
                .saturating_sub(transport)
        }) {
            if left < bound / 2 {
                let left_secs = left.as_secs_f64().round() as u64;
                let needed_secs = (bound / 2).as_secs();
                warn!(
                    "Fallback alias not tried, too little of the try left: alias='{}', fallback_alias='{}', left={}s, needed={}s",
                    alias, own.alias, left_secs, needed_secs
                );
                return Err(ModelCallFailure {
                    error: with_fallback_note(
                        primary_error,
                        &format!(
                            "the fallback model '{}' was not tried: the try had {left_secs} s left and it needs at least {needed_secs} s",
                            own.model
                        ),
                    ),
                    report: report(
                        primary,
                        Some(fallback_report(FallbackOutcome::NoTime {
                            left_secs,
                            needed_secs,
                        })),
                    ),
                });
            }
            bound = bound.min(left);
        }

        let request_bytes = chat_request_bytes(messages, tools);
        if let Some(reason) = fallback_window_refusal(alias, own, request_bytes) {
            return Err(ModelCallFailure {
                error: with_fallback_note(primary_error, &reason),
                report: report(
                    primary,
                    Some(fallback_report(FallbackOutcome::Skipped {
                        request_bytes,
                        limit_bytes: own.window_bytes,
                    })),
                ),
            });
        }

        info!(
            "Trying fallback alias: alias='{}', fallback_alias='{}', model='{}'",
            alias, own.alias, own.model
        );
        let fallback_options = self.apply_alias_options(&own.alias, options);
        let (outcome, note) = match tokio::time::timeout(
            bound,
            own.adapter
                .generate_chat(messages, tools, &fallback_options),
        )
        .await
        .map(|res| {
            res.and_then(
                |r| match empty_generation_fault(&r, &own.alias, &own.model, 1, 1) {
                    Some(fault) => Err(fault),
                    None => Ok(r),
                },
            )
        }) {
            Ok(Ok(r)) => {
                info!(
                    "Fallback alias answered: alias='{}', fallback_alias='{}', model='{}'",
                    alias, own.alias, own.model
                );
                return Ok(r);
            }
            Ok(Err(fe)) => {
                let note = format!(
                    "the fallback model '{}' was tried once and failed: {fe}",
                    own.model
                );
                (FallbackOutcome::Failed(fe), note)
            }
            Err(_) => {
                let secs = bound.as_secs();
                warn!(
                    "LLM silent fallback: alias='{}', fallback_alias='{}', model='{}', no answer within {}s",
                    alias, own.alias, own.model, secs
                );
                let note = format!(
                    "the fallback model '{}' was tried once and gave no answer within {secs} s",
                    own.model
                );
                (FallbackOutcome::Silent { secs }, note)
            }
        };
        Err(ModelCallFailure {
            error: with_fallback_note(primary_error, &note),
            report: report(primary, Some(fallback_report(outcome))),
        })
    }

    /// Generate text for the given model alias.
    ///
    /// Resolves the alias directly to a pre-configured `Arc<dyn LLMProvider>` adapter;
    /// no model name override is needed at call time.
    /// Includes retry-with-exponential-backoff and one-level fallback.
    pub async fn generate(
        &self,
        alias: &str,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        let (model_name, provider) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{alias}' not found")))?;

        info!(
            "LLM text generation: alias='{}', model='{}'",
            alias, model_name
        );

        let effective_options = self.apply_alias_options(alias, options);
        let mut last_error = None;

        for attempt in 0..self.max_retries {
            let outcome = self
                .bounded_attempt(
                    alias,
                    model_name,
                    attempt + 1,
                    self.max_retries,
                    provider.generate(prompt, &effective_options),
                )
                .await;
            match outcome {
                Ok(response) => {
                    info!(
                        "Generation successful on attempt {} (model='{}')",
                        attempt + 1,
                        model_name
                    );
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        "Generation failed (attempt {}/{}): {:?}",
                        attempt + 1,
                        self.max_retries,
                        e
                    );
                    let time_limit = provider_timeout_for_alias(&e, alias);
                    if time_limit.is_some() {
                        warn!(
                            "LLM provider time limit; not sent again to this model: alias='{}', model='{}', attempt={}/{}",
                            alias,
                            model_name,
                            attempt + 1,
                            self.max_retries
                        );
                    }
                    last_error = Some(e);

                    // A time limit ends the primary's attempts: the fallback, a
                    // different model, is tried as after the last retry. An
                    // alias naming its own fallback alias goes there after the
                    // time limit only, and never to the node-wide fallback.
                    if time_limit.is_some() || attempt == self.max_retries - 1 {
                        if let Some(own) = self.alias_fallbacks.get(alias) {
                            if let Some(timeout) = time_limit {
                                return self
                                    .generate_on_fallback_alias(
                                        alias, own, timeout, prompt, options,
                                    )
                                    .await;
                            }
                        } else if let Some((fallback_model, fallback)) = &self.fallback_provider {
                            info!("Trying fallback provider (model='{}')", fallback_model);
                            let outcome = self
                                .bounded_attempt(
                                    alias,
                                    fallback_model,
                                    1,
                                    1,
                                    fallback.generate(prompt, options),
                                )
                                .await;
                            return match (outcome, time_limit) {
                                (Err(fe), Some(timeout)) => {
                                    Err(with_fallback_failure(timeout, fallback_model, &fe))
                                }
                                (outcome, _) => outcome,
                            };
                        }
                        if let Some(timeout) = time_limit {
                            return Err(timeout);
                        }
                    }

                    // Exponential backoff with capped exponent to avoid overflow
                    let exp = std::cmp::min(attempt, MAX_BACKOFF_EXPONENT);
                    let backoff_factor = 2_u64.saturating_pow(exp);
                    let delay_ms = self
                        .retry_delay_ms
                        .saturating_mul(backoff_factor)
                        .min(MAX_BACKOFF_MS);

                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
    }

    /// Check health of all providers
    pub async fn health_check_all(&self) -> HashMap<String, Result<(), LLMError>> {
        let mut results = HashMap::new();

        for (name, provider) in &self.providers {
            info!("Health checking provider: {}", name);
            results.insert(name.clone(), provider.health_check().await);
        }

        results
    }

    /// Get list of available model aliases
    pub fn available_aliases(&self) -> Vec<String> {
        self.alias_map.keys().cloned().collect()
    }

    /// Check if a model alias exists
    pub fn has_alias(&self, alias: &str) -> bool {
        self.alias_map.contains_key(alias)
    }

    /// The node's image generation aliases, in the order its configuration
    /// lists them: the first is the default of an image generation call that
    /// names none (AEGIS ADR-143 I2).
    pub fn image_generation_aliases(&self) -> Vec<String> {
        self.image_aliases
            .iter()
            .map(|(alias, _)| alias.clone())
            .collect()
    }

    /// The image provider an image generation alias is bound to; `None` for
    /// any other alias.
    pub fn image_provider(&self, alias: &str) -> Option<Arc<dyn VisionProvider>> {
        self.image_aliases
            .iter()
            .find(|(name, _)| name == alias)
            .map(|(_, adapter)| adapter.clone())
    }

    /// The model a call on `alias` is sent to, read from the alias map this
    /// registry routes every call through (AEGIS ADR-124 D3): the winner of
    /// the selection strategy whose adapter was built. `None` for an alias
    /// the registry does not hold.
    pub fn model_for_alias(&self, alias: &str) -> Option<&str> {
        self.alias_map.get(alias).map(|(model, _)| model.as_str())
    }

    /// Whether the alias's model reads images: its model entry declares the
    /// capability `vision`. An alias the table does not hold reads none.
    pub fn alias_reads_images(&self, alias: &str) -> bool {
        self.vision_aliases.contains(alias)
    }

    /// Determine whether the API key for a given model alias is platform-managed
    /// or user-provided (BYOK).
    ///
    /// Heuristic:
    /// - `None` (no key configured, e.g. local Ollama) → `Platform`
    /// - Starts with `"env:"` → `Platform` (resolved from an environment variable)
    /// - Any other literal value → `User` (directly configured BYOK)
    ///
    /// BYOK users are exempt from `LlmCall` and `LlmToken` rate limits (ADR-072)
    /// because they consume their own provider quota.
    pub fn key_source_for_alias(&self, alias: &str) -> ApiKeySource {
        match self.raw_api_keys.get(alias) {
            Some(Some(key)) if !key.expose().starts_with("env:") => ApiKeySource::User,
            _ => ApiKeySource::Platform,
        }
    }
}

// Implement domain LLMProvider trait for infrastructure ProviderRegistry
// This allows the infrastructure to be used through domain interfaces
impl ImageGenerationModels for ProviderRegistry {
    fn image_generation_aliases(&self) -> Vec<String> {
        ProviderRegistry::image_generation_aliases(self)
    }

    fn image_provider(&self, alias: &str) -> Option<Arc<dyn VisionProvider>> {
        ProviderRegistry::image_provider(self, alias)
    }
}

#[async_trait]
impl LLMProvider for ProviderRegistry {
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        self.generate("default", prompt, options).await
    }

    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        self.generate_chat("default", messages, tools, options)
            .await
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        let (_, provider) = self
            .alias_map
            .get("default")
            .ok_or_else(|| LLMError::Provider("Default model alias not configured".into()))?;

        provider.health_check().await
    }
}

impl ProviderRegistry {
    /// Test-only constructor that wires concrete primary + optional fallback
    /// adapters without going through `from_config`. Used to inject mock
    /// providers in retry-policy regression tests.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        primary: Arc<dyn LLMProvider>,
        fallback: Option<Arc<dyn LLMProvider>>,
        max_retries: u32,
        retry_delay_ms: u64,
        llm_overall_timeout_secs: u64,
    ) -> Self {
        let mut alias_map = HashMap::new();
        alias_map.insert(
            "default".to_string(),
            ("test-model".to_string(), primary.clone()),
        );
        let mut providers = HashMap::new();
        providers.insert("primary".to_string(), primary);
        let fallback_provider = fallback.map(|f| ("test-fallback-model".to_string(), f));

        Self {
            alias_map,
            providers,
            image_aliases: Vec::new(),
            fallback_provider,
            alias_fallbacks: HashMap::new(),
            raw_api_keys: HashMap::new(),
            alias_max_output_tokens: HashMap::new(),
            alias_temperatures: HashMap::new(),
            vision_aliases: std::collections::HashSet::new(),
            max_retries,
            retry_delay_ms,
            llm_overall_timeout_secs,
            llm_attempt_timeout_secs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::{ChatResponse, FinishReason, GenerationResponse, TokenUsage};
    use crate::domain::node_config::{
        LLMSelection, ManifestMetadata, ModelConfig, NodeConfigManifest, NodeConfigSpec,
        NodeIdentity, NodeType,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Programmable mock provider. Returns one of the queued errors per call,
    /// or a success response when the queue is exhausted (configurable).
    /// Records the number of `generate_chat` calls received.
    struct MockProvider {
        /// Queue of responses to return, in order. When empty, returns the
        /// `default_response` if set, else `LLMError::Provider("queue empty")`.
        queue: Mutex<Vec<Result<ChatResponse, LLMError>>>,
        default_response: Mutex<Option<Result<ChatResponse, LLMError>>>,
        calls: AtomicUsize,
        /// When true, the mock panics if called — used to assert "this provider
        /// must NOT be invoked" in fallback short-circuit tests.
        panic_on_call: bool,
        /// When true, every call blocks forever (used for overall-timeout test).
        block_forever: bool,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                queue: Mutex::new(Vec::new()),
                default_response: Mutex::new(None),
                calls: AtomicUsize::new(0),
                panic_on_call: false,
                block_forever: false,
            }
        }

        fn with_responses(responses: Vec<Result<ChatResponse, LLMError>>) -> Arc<Self> {
            let m = Self::new();
            *m.queue.lock().unwrap() = responses;
            Arc::new(m)
        }

        fn panicking() -> Arc<Self> {
            let mut m = Self::new();
            m.panic_on_call = true;
            Arc::new(m)
        }

        fn blocking() -> Arc<Self> {
            let mut m = Self::new();
            m.block_forever = true;
            Arc::new(m)
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LLMProvider for MockProvider {
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> Result<GenerationResponse, LLMError> {
            unimplemented!("test mock: generate not used")
        }

        async fn generate_chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _options: &GenerationOptions,
        ) -> Result<ChatResponse, LLMError> {
            if self.panic_on_call {
                panic!("MockProvider configured to fail test if invoked");
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block_forever {
                futures::future::pending::<()>().await;
                unreachable!();
            }
            let mut q = self.queue.lock().unwrap();
            if !q.is_empty() {
                return q.remove(0);
            }
            drop(q);
            if let Some(r) = self.default_response.lock().unwrap().as_ref() {
                return match r {
                    Ok(c) => Ok(c.clone()),
                    Err(e) => Err(clone_llm_error(e)),
                };
            }
            Err(LLMError::Provider("queue empty".into()))
        }

        async fn health_check(&self) -> Result<(), LLMError> {
            Ok(())
        }
    }

    fn clone_llm_error(e: &LLMError) -> LLMError {
        match e {
            LLMError::Network(s) => LLMError::Network(s.clone()),
            LLMError::Authentication(s) => LLMError::Authentication(s.clone()),
            LLMError::RateLimit(d) => LLMError::RateLimit(*d),
            LLMError::ModelNotFound(s) => LLMError::ModelNotFound(s.clone()),
            LLMError::Provider(s) => LLMError::Provider(s.clone()),
            LLMError::InvalidInput(s) => LLMError::InvalidInput(s.clone()),
            LLMError::ServiceUnavailable(s) => LLMError::ServiceUnavailable(s.clone()),
        }
    }

    fn ok_response() -> ChatResponse {
        ChatResponse::FinalText(GenerationResponse {
            text: "ok".to_string(),
            usage: TokenUsage::default(),
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn make_registry(
        primary: Arc<dyn LLMProvider>,
        fallback: Option<Arc<dyn LLMProvider>>,
        timeout_secs: u64,
    ) -> ProviderRegistry {
        ProviderRegistry::new_for_test(primary, fallback, 3, 1, timeout_secs)
    }

    #[tokio::test]
    async fn auth_error_short_circuits() {
        let primary = MockProvider::with_responses(vec![Err(LLMError::Authentication(
            "403 forbidden".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback.clone() as Arc<dyn LLMProvider>),
            30,
        );

        let start = std::time::Instant::now();
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        let elapsed = start.elapsed();

        assert!(matches!(res, Err(LLMError::Authentication(_))));
        assert_eq!(
            primary.call_count(),
            1,
            "primary must be called exactly once"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "auth error must short-circuit fast (got {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn model_not_found_short_circuits() {
        let primary = MockProvider::with_responses(vec![Err(LLMError::ModelNotFound(
            "no-such-model".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::ModelNotFound(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn invalid_input_short_circuits() {
        let primary =
            MockProvider::with_responses(vec![Err(LLMError::InvalidInput("bad arg".into()))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::InvalidInput(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn provider_error_retries_then_succeeds() {
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::Provider("transient".into())),
            Err(LLMError::Provider("transient".into())),
            Ok(ok_response()),
        ]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 30);

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(res.is_ok(), "expected success after retries: {res:?}");
        assert_eq!(primary.call_count(), 3);
    }

    #[tokio::test]
    async fn network_error_overall_timeout_fires() {
        let primary = MockProvider::blocking();
        let registry = make_registry(primary as Arc<dyn LLMProvider>, None, 1);

        let start = std::time::Instant::now();
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        let elapsed = start.elapsed();

        assert!(
            matches!(&res, Err(LLMError::Network(msg)) if msg.contains("upstream timeout")),
            "expected upstream timeout, got {res:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "must return within budget (got {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn fallback_short_circuits_on_auth() {
        // Primary returns Authentication on every attempt; fallback would
        // succeed if invoked, but must NOT be invoked.
        let primary = MockProvider::with_responses(vec![Err(LLMError::Authentication(
            "403 forbidden".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::Authentication(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn service_unavailable_retries_with_backoff() {
        // Regression: HTTP 503 ServiceUnavailable was wrongly classified as
        // non-retryable, short-circuiting after attempt 1/3. It must run
        // through the exponential-backoff retry loop and recover when the
        // upstream becomes available again.
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::ServiceUnavailable("503 high demand".into())),
            Err(LLMError::ServiceUnavailable("503 high demand".into())),
            Ok(ok_response()),
        ]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 30);

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(
            res.is_ok(),
            "ServiceUnavailable must retry with backoff and succeed on 3rd attempt: {res:?}"
        );
        assert_eq!(
            primary.call_count(),
            3,
            "primary must be retried max_retries times, not short-circuited after 1"
        );
    }

    #[tokio::test]
    async fn service_unavailable_falls_back_after_exhausting_primary_retries() {
        // Regression: after the primary exhausts its retries on
        // ServiceUnavailable, the registry must invoke the fallback provider
        // (same path as other transient errors), not return the 503 directly.
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::ServiceUnavailable("503 high demand".into())),
            Err(LLMError::ServiceUnavailable("503 high demand".into())),
            Err(LLMError::ServiceUnavailable("503 high demand".into())),
        ]);
        let fallback = MockProvider::with_responses(vec![Ok(ok_response())]);
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback.clone() as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(
            res.is_ok(),
            "fallback must serve the request after primary exhausts 503 retries: {res:?}"
        );
        assert_eq!(
            primary.call_count(),
            3,
            "primary must be retried max_retries times before falling back"
        );
        assert_eq!(
            fallback.call_count(),
            1,
            "fallback must be invoked exactly once after primary exhausts retries"
        );
    }

    #[tokio::test]
    async fn service_unavailable_returns_err_when_both_providers_exhausted() {
        // Regression: when primary AND fallback both exhaust their retries
        // on ServiceUnavailable, the final error returned must be
        // ServiceUnavailable (not silently masked or transformed).
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::ServiceUnavailable("503 primary".into())),
            Err(LLMError::ServiceUnavailable("503 primary".into())),
            Err(LLMError::ServiceUnavailable("503 primary".into())),
        ]);
        let fallback = MockProvider::with_responses(vec![Err(LLMError::ServiceUnavailable(
            "503 fallback".into(),
        ))]);
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback.clone() as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(
            matches!(res, Err(LLMError::ServiceUnavailable(_))),
            "expected ServiceUnavailable when both providers exhausted, got {res:?}"
        );
        assert_eq!(primary.call_count(), 3);
        assert_eq!(fallback.call_count(), 1);
    }

    #[tokio::test]
    async fn llm_error_class_maps_correctly() {
        use crate::domain::events::LlmErrorClass;
        assert_eq!(
            LlmErrorClass::from(&LLMError::Authentication("x".into())),
            LlmErrorClass::Authentication
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::RateLimit(None)),
            LlmErrorClass::RateLimit
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::ModelNotFound("m".into())),
            LlmErrorClass::ModelNotFound
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::InvalidInput("x".into())),
            LlmErrorClass::InvalidInput
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::ServiceUnavailable("x".into())),
            LlmErrorClass::ServiceUnavailable
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::Network("x".into())),
            LlmErrorClass::Network
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::Provider("x".into())),
            LlmErrorClass::Provider
        );
    }

    #[test]
    fn test_registry_creation() {
        let config = NodeConfigManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "NodeConfig".to_string(),
            metadata: ManifestMetadata {
                name: "test-node".to_string(),
                version: Some("1.0.0".to_string()),
                labels: None,
            },
            spec: NodeConfigSpec {
                node: NodeIdentity {
                    id: "test".to_string(),
                    node_type: NodeType::Edge,
                    region: None,
                    tags: vec![],
                    resources: None,
                },
                llm_providers: vec![LLMProviderConfig {
                    name: "test-ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    endpoint: "http://localhost:11434".into(),
                    api_key: None,
                    headers: None,
                    enabled: true,
                    models: vec![ModelConfig {
                        alias: "default".to_string(),
                        model: "llama3.2".to_string(),
                        capabilities: vec!["chat".to_string()],
                        context_window: 8192,
                        cost_per_1k_tokens: 0.0,
                        max_output_tokens: None,
                        temperature: None,
                        fallback_alias: None,
                    }],
                }],
                llm_selection: LLMSelection::default(),
                runtime: crate::domain::node_config::RuntimeConfig::default(),
                network: None,
                observability: None,
                storage: None, // Optional storage configuration (ADR-032)
                tool_capabilities: None,
                seal: None,
                security_contexts: None,
                database: None,
                temporal: None,
                cortex: None,
                secrets: None,
                builtin_dispatchers: None,
                cluster: None,
                registry_credentials: vec![],
                iam: None,
                grpc_auth: None,
                seal_gateway: None,
                image_tag: None,
                deploy_builtins: false,
                force_deploy_builtins: None,
                max_execution_list_limit: None,
                billing: None,
                zaru: None,
                oauth_providers: Vec::new(),
                goals: crate::domain::node_config::GoalsConfig::default(),
            },
        };

        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert!(registry.has_alias("default"));
        assert_eq!(registry.available_aliases().len(), 1);
    }

    // ── Empty generation (regression: execution 9c5b304e, iteration 1) ─────
    //
    // A generation with no text and no tool call came back as a successful
    // final answer, and the empty string went to the judge as the iteration's
    // output. It is a provider fault: retried inside `max_retries`, and an
    // error that says so once the retries are spent.

    fn empty_response() -> ChatResponse {
        ChatResponse::FinalText(GenerationResponse {
            text: "  \n".to_string(),
            usage: TokenUsage::default(),
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            finish_reason: FinishReason::Stop,
        })
    }

    #[tokio::test]
    async fn empty_generation_is_retried() {
        let primary = MockProvider::with_responses(vec![Ok(empty_response()), Ok(ok_response())]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 30);
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert_eq!(
            primary.call_count(),
            2,
            "an empty generation must be retried, not returned as the answer"
        );
        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
            other => panic!("expected the retried answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_generation_after_retries_is_an_error_that_says_so() {
        let primary = MockProvider::with_responses(vec![
            Ok(empty_response()),
            Ok(empty_response()),
            Ok(empty_response()),
        ]);
        let fallback = MockProvider::with_responses(vec![Ok(empty_response())]);
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback.clone() as Arc<dyn LLMProvider>),
            30,
        );
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert_eq!(primary.call_count(), 3, "every retry is spent");
        assert_eq!(fallback.call_count(), 1, "the fallback is tried once");
        match res {
            Err(LLMError::Provider(msg)) => assert!(
                msg.contains("empty generation") && msg.contains("no text and no tool call"),
                "the error must say the generation was empty, got: {msg}"
            ),
            other => panic!("an empty generation must end as a provider error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_calls_without_text_are_not_an_empty_generation() {
        let calls = ChatResponse::ToolCalls(vec![crate::domain::llm::ChatToolCall {
            id: "1".to_string(),
            name: "fs.read".to_string(),
            arguments: serde_json::json!({}),
        }]);
        let primary = MockProvider::with_responses(vec![Ok(calls)]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 30);
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert_eq!(primary.call_count(), 1);
        assert!(matches!(res, Ok(ChatResponse::ToolCalls(_))));
    }

    /// Workers AI's answer when it ends a generation at its time limit, as the
    /// core pod logged it for execution 18d8e0ca on 2026-10-03.
    const WORKERS_AI_408_BODY: &str = r#"{"errors":[{"message":"AiError: AiError: Request timeout (df1159f0-0e44-40d5-b6ba-66bfec32483f)","code":3046}],"success":false,"result":{},"messages":[]}"#;

    /// A registry whose only alias, `default`, is the real OpenAI-compatible
    /// adapter pointed at `endpoint`.
    fn openai_registry(endpoint: String, max_retries: u32) -> ProviderRegistry {
        let adapter = crate::infrastructure::llm::openai::OpenAIAdapter::new(
            endpoint,
            "test-key".to_string(),
            "test-model".to_string(),
            reqwest::header::HeaderMap::new(),
        );
        ProviderRegistry::new_for_test(Arc::new(adapter), None, max_retries, 1, 60)
    }

    /// AEGIS execution 18d8e0ca (2026-10-03): `smart` on nemotron-3-120b-a12b
    /// answered HTTP 408 with code 3046 at about 120 s, the registry sent the
    /// identical request again into the same limit, and every iteration ended
    /// at the 300 s budget as "upstream timeout after 300s". The 3046 answer
    /// is sent once and comes back at once as a provider timeout naming the
    /// alias, the model and the seconds.
    #[tokio::test]
    async fn workers_ai_408_code_3046_is_not_sent_again_and_is_a_provider_timeout() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_header("content-type", "application/json")
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let registry = openai_registry(server.url(), 3);

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;

        mock.assert_async().await;
        match res {
            Err(LLMError::Provider(msg)) => {
                assert!(
                    msg.starts_with("provider timeout: "),
                    "the error must say it is a provider timeout, got: {msg}"
                );
                assert!(
                    msg.contains("alias 'default'"),
                    "the error must name the alias, got: {msg}"
                );
                assert!(
                    msg.contains("model 'test-model'"),
                    "the error must name the model, got: {msg}"
                );
                assert!(
                    msg.contains(" s ") && msg.contains("code 3046"),
                    "the error must give the seconds and the code, got: {msg}"
                );
            }
            other => panic!("a 3046 answer must end as a provider timeout, got {other:?}"),
        }
    }

    /// The single-prompt path takes the same rule.
    #[tokio::test]
    async fn workers_ai_408_code_3046_is_not_sent_again_on_generate() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let registry = openai_registry(server.url(), 3);

        let res = registry
            .generate("default", "hello", &GenerationOptions::default())
            .await;

        mock.assert_async().await;
        assert!(
            matches!(&res, Err(LLMError::Provider(msg)) if msg.starts_with("provider timeout: alias 'default'")),
            "got {res:?}"
        );
    }

    /// A primary and a fallback, each the real OpenAI-compatible adapter
    /// pointed at its own endpoint, under the alias `default`.
    fn openai_registry_with_fallback(primary: String, fallback: String) -> ProviderRegistry {
        let adapter = |endpoint: String, model: &str| {
            Arc::new(crate::infrastructure::llm::openai::OpenAIAdapter::new(
                endpoint,
                "test-key".to_string(),
                model.to_string(),
                reqwest::header::HeaderMap::new(),
            )) as Arc<dyn LLMProvider>
        };
        ProviderRegistry::new_for_test(
            adapter(primary, "test-model"),
            Some(adapter(fallback, "test-fallback-model")),
            3,
            1,
            60,
        )
    }

    const FALLBACK_ANSWER_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"fallback answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;

    /// A fallback is another model, not the identical request: a primary 3046
    /// skips the primary's remaining retries and goes to the fallback, whose
    /// answer is returned.
    #[tokio::test]
    async fn workers_ai_408_code_3046_goes_to_the_fallback_once() {
        let mut primary = mockito::Server::new_async().await;
        let mut fallback = mockito::Server::new_async().await;
        let primary_mock = primary
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let fallback_mock = fallback
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let registry = openai_registry_with_fallback(primary.url(), fallback.url());

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;

        primary_mock.assert_async().await;
        fallback_mock.assert_async().await;
        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "fallback answer"),
            other => panic!("the fallback's answer must be returned, got {other:?}"),
        }
    }

    /// The single-prompt path goes to the fallback the same way.
    #[tokio::test]
    async fn workers_ai_408_code_3046_goes_to_the_fallback_once_on_generate() {
        let mut primary = mockito::Server::new_async().await;
        let mut fallback = mockito::Server::new_async().await;
        let primary_mock = primary
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let fallback_mock = fallback
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let registry = openai_registry_with_fallback(primary.url(), fallback.url());

        let res = registry
            .generate("default", "hello", &GenerationOptions::default())
            .await;

        primary_mock.assert_async().await;
        fallback_mock.assert_async().await;
        assert!(
            matches!(&res, Ok(r) if r.text == "fallback answer"),
            "got {res:?}"
        );
    }

    /// A fallback that also meets its limit is not retried, and the error
    /// names both models.
    #[tokio::test]
    async fn workers_ai_408_code_3046_on_both_names_both_models() {
        let mut primary = mockito::Server::new_async().await;
        let mut fallback = mockito::Server::new_async().await;
        let primary_mock = primary
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let fallback_mock = fallback
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let registry = openai_registry_with_fallback(primary.url(), fallback.url());

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;

        primary_mock.assert_async().await;
        fallback_mock.assert_async().await;
        match res {
            Err(LLMError::Provider(msg)) => {
                assert!(
                    msg.starts_with("provider timeout: alias 'default'"),
                    "got: {msg}"
                );
                assert!(msg.contains("model 'test-model'"), "got: {msg}");
                assert!(msg.contains("model 'test-fallback-model'"), "got: {msg}");
            }
            other => panic!("both 3046 answers must end as a provider timeout, got {other:?}"),
        }
    }

    /// Every other 408 keeps its retries: only the 3046 body is the limit.
    #[tokio::test]
    async fn a_408_without_code_3046_is_still_retried() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(r#"{"error":"request timeout"}"#)
            .expect(3)
            .create_async()
            .await;
        let registry = openai_registry(server.url(), 3);

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;

        mock.assert_async().await;
        assert!(
            matches!(&res, Err(LLMError::Provider(msg)) if msg.starts_with("HTTP 408")),
            "got {res:?}"
        );
    }

    // ── Per-alias fallback (`fallback_alias`) ──────────────────────────────
    //
    // `smart` on nemotron-3-120b-a12b is ended by Workers AI at about 120 s
    // with a 408 and code 3046 (execution 18d8e0ca; the planning arc
    // `smart-fallback-update`, m6 at 120.60 s), and the identical request
    // meets the same limit again. A model entry may name another alias as its
    // fallback: after the provider's time limit, and only then, the request
    // goes once to the named alias's model with the named alias's own options.
    // Every other failure behaves as it does today for that alias.
    //
    // Each registry here is built by `from_config` from the configuration an
    // operator writes: `smart` (model `m-smart`) on one stand-in, `coder`
    // (model `m-coder`, max_output_tokens 4321, temperature 0.5) on another.

    /// The configuration under test: the providers block and the
    /// `llm_selection` block as written in `aegis-config.yaml`.
    fn alias_fallback_manifest(providers_yaml: &str, selection_yaml: &str) -> NodeConfigManifest {
        let providers: Vec<LLMProviderConfig> =
            serde_yaml::from_str(providers_yaml).expect("the providers block parses");
        let selection: LLMSelection =
            serde_yaml::from_str(selection_yaml).expect("the llm_selection block parses");
        let mut manifest = NodeConfigManifest::default();
        manifest.spec.llm_providers = providers;
        manifest.spec.llm_selection = selection;
        manifest
    }

    /// `smart` on `smart_url`, `coder` on `coder_url`, and an optional third
    /// provider `node-fallback` on `node_url` (model `m-node`, alias `spare`).
    /// `smart_fallback` is the `fallback_alias` line of `smart`'s entry, or
    /// empty for none.
    fn alias_fallback_providers(
        smart_url: &str,
        coder_url: &str,
        node_url: Option<&str>,
        smart_fallback: &str,
    ) -> String {
        let mut yaml = format!(
            r#"- name: p-smart
  type: openai-compatible
  endpoint: "{smart_url}"
  api_key: "test-key"
  models:
    - alias: "smart"
      model: "m-smart"
      capabilities: ["chat"]
      context_window: 8192
      max_output_tokens: 16384
      temperature: 0.25
      {smart_fallback}
- name: p-coder
  type: openai-compatible
  endpoint: "{coder_url}"
  api_key: "test-key"
  models:
    - alias: "coder"
      model: "m-coder"
      capabilities: ["chat"]
      context_window: 8192
      max_output_tokens: 4321
      temperature: 0.5
"#
        );
        if let Some(node_url) = node_url {
            yaml.push_str(&format!(
                r#"- name: node-fallback
  type: openai-compatible
  endpoint: "{node_url}"
  api_key: "test-key"
  models:
    - alias: "spare"
      model: "m-node"
      capabilities: ["chat"]
      context_window: 8192
"#
            ));
        }
        yaml
    }

    const ALIAS_FALLBACK_SELECTION: &str =
        "max_retries: 3\nretry_delay_ms: 1\nllm_overall_timeout_secs: 60\n";

    fn alias_fallback_registry(manifest: &NodeConfigManifest) -> ProviderRegistry {
        manifest
            .validate()
            .expect("the configuration under test is valid");
        ProviderRegistry::from_config(manifest).expect("registry builds")
    }

    /// A request at `model` carrying exactly these `max_tokens` and
    /// `temperature` (both exact in binary, so the JSON compares equal).
    fn body_of(model: &str, max_tokens: u32, temperature: f64) -> mockito::Matcher {
        mockito::Matcher::PartialJson(serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "temperature": temperature,
        }))
    }

    /// After a 3046 on `smart`, exactly one request goes to `coder`'s model
    /// with `coder`'s max_output_tokens and temperature (not the caller's 8192
    /// and 0.4, not `smart`'s), and its answer is returned.
    #[tokio::test]
    async fn fallback_alias_after_3046_sends_one_request_at_the_named_alias_with_its_options() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .match_body(body_of("m-coder", 4321, 0.5))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                None,
                r#"fallback_alias: "coder""#,
            ),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "fallback answer"),
            other => panic!("the fallback alias's answer must be returned, got {other:?}"),
        }
    }

    /// The single-prompt path takes the same rule.
    #[tokio::test]
    async fn fallback_alias_after_3046_sends_one_request_at_the_named_alias_on_generate() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .match_body(body_of("m-coder", 4321, 0.5))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                None,
                r#"fallback_alias: "coder""#,
            ),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate("smart", "hello", &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        assert!(
            matches!(&res, Ok(r) if r.text == "fallback answer"),
            "got {res:?}"
        );
    }

    /// The fallback alias is used only after the provider's time limit: the
    /// last failed retry of another retryable error sends nothing to it, and
    /// the error is today's. A node-wide `fallback_provider` is not reached
    /// either, since the alias names its own fallback.
    #[tokio::test]
    async fn fallback_alias_is_not_used_after_the_last_failed_retry_of_another_error() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let mut node = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(503)
            .with_body(r#"{"error":"busy"}"#)
            .expect(3)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let node_mock = node
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                Some(&node.url()),
                r#"fallback_alias: "coder""#,
            ),
            &format!("{ALIAS_FALLBACK_SELECTION}fallback_provider: node-fallback\n"),
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        node_mock.assert_async().await;
        assert!(
            matches!(&res, Err(LLMError::ServiceUnavailable(_))),
            "the primary's own error must be returned, got {res:?}"
        );
    }

    /// An alias that names a fallback alias never reaches the node-wide
    /// `fallback_provider`, after its time limit either.
    #[tokio::test]
    async fn fallback_alias_after_3046_takes_precedence_over_fallback_provider() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let mut node = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let node_mock = node
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                Some(&node.url()),
                r#"fallback_alias: "coder""#,
            ),
            &format!("{ALIAS_FALLBACK_SELECTION}fallback_provider: node-fallback\n"),
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        node_mock.assert_async().await;
        assert!(res.is_ok(), "got {res:?}");
    }

    /// A fallback alias that also meets its limit is not retried, and the
    /// error names both models.
    #[tokio::test]
    async fn fallback_alias_3046_on_both_names_both_models() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                None,
                r#"fallback_alias: "coder""#,
            ),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        match res {
            Err(LLMError::Provider(msg)) => {
                assert!(
                    msg.starts_with("provider timeout: alias 'smart'"),
                    "got: {msg}"
                );
                assert!(msg.contains("model 'm-smart'"), "got: {msg}");
                assert!(msg.contains("model 'm-coder'"), "got: {msg}");
            }
            other => panic!("both 3046 answers must end as a provider timeout, got {other:?}"),
        }
    }

    /// No `fallback_alias` and no `fallback_provider`: the 3046 is returned
    /// with no further request, though another alias exists.
    #[tokio::test]
    async fn no_fallback_alias_and_no_fallback_provider_returns_the_error_with_no_further_request()
    {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart.url(), &coder.url(), None, ""),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        assert!(
            matches!(&res, Err(LLMError::Provider(msg)) if msg.starts_with("provider timeout: alias 'smart'")),
            "got {res:?}"
        );
    }

    /// No `fallback_alias` under a configured `fallback_provider`: today's
    /// behaviour, the node-wide fallback tried once with the caller's options.
    #[tokio::test]
    async fn no_fallback_alias_under_fallback_provider_behaves_as_today() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let mut node = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let node_mock = node
            .mock("POST", "/chat/completions")
            .match_body(body_of("m-node", 8192, 0.4))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart.url(), &coder.url(), Some(&node.url()), ""),
            &format!("{ALIAS_FALLBACK_SELECTION}fallback_provider: node-fallback\n"),
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat("smart", &[], &[], &GenerationOptions::default())
            .await;

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        node_mock.assert_async().await;
        assert!(
            matches!(&res, Ok(ChatResponse::FinalText(r)) if r.text == "fallback answer"),
            "got {res:?}"
        );
    }

    /// A node never runs with a fallback configured and silently absent: when
    /// the alias is mapped and the alias it names has no adapter, the registry
    /// refuses to build, naming both.
    #[test]
    fn fallback_alias_without_an_adapter_refuses_the_registry() {
        let mut manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                "http://127.0.0.1:9",
                "http://127.0.0.1:9",
                None,
                r#"fallback_alias: "coder""#,
            ),
            ALIAS_FALLBACK_SELECTION,
        );
        // The coder provider's key names an environment variable that is not
        // set, so its adapters fail to build.
        manifest.spec.llm_providers[1].api_key = Some(SensitiveString::new(
            "env:AEGIS_TEST_UNSET_FALLBACK_ALIAS_KEY_7c1e".to_string(),
        ));
        manifest
            .validate()
            .expect("the configuration is valid as written");

        let err = match ProviderRegistry::from_config(&manifest) {
            Ok(_) => panic!("a fallback alias with no adapter must refuse the registry"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("'smart'") && err.contains("'coder'"),
            "the error must name both aliases, got: {err}"
        );
    }

    // ── ADR-130, Update of 2026-10-05: a silent primary under the agent's
    // timeout, a fallback too small for the request, the person's sentence.
    // Each drives `from_config` and the real OpenAI-compatible adapters over
    // loopback, as the daemon constructs them.

    /// A model endpoint that accepts every connection and never answers.
    async fn silent_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let addr = listener.local_addr().expect("its address");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        format!("http://{addr}")
    }

    /// A model endpoint that answers every request with `body` after `delay`.
    async fn slow_server(delay: std::time::Duration, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let addr = listener.local_addr().expect("its address");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    const PRIMARY_ANSWER_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"primary answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;

    /// A primary that answers at 90 percent of the agent's timeout is not cut
    /// short, and no fallback is tried (CORRECTION C2: production's smart
    /// answered one generation in about 59 s, execution 9506f85b).
    #[tokio::test]
    async fn healthy_slow_primary_at_ninety_percent_of_the_agent_timeout_is_not_cut() {
        let smart_url =
            slow_server(std::time::Duration::from_millis(2_700), PRIMARY_ANSWER_BODY).await;
        let mut coder = mockito::Server::new_async().await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder.url(), None, r#"fallback_alias: "coder""#),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let res = registry
            .generate_chat_within(
                "smart",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(3),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await;

        coder_mock.assert_async().await;
        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "primary answer"),
            other => panic!("the primary's answer at 2.7 s of 3 s must be returned, got {other:?}"),
        }
    }

    fn user_message(content: String) -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".to_string(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }]
    }

    /// `llm_attempt_timeout_secs: 2`, which lowers the fallback's 78 s bound
    /// to 2 s so a silent fallback is seen inside a test.
    const SHORT_ATTEMPT_SELECTION: &str =
        "max_retries: 3\nretry_delay_ms: 1\nllm_overall_timeout_secs: 60\nllm_attempt_timeout_secs: 2\n";

    /// The reproduction of execution a0a230fd: `smart` is silent for the
    /// agent's whole timeout, then its fallback alias `coder`, with time left
    /// in the try, answers with `coder`'s options.
    #[tokio::test]
    async fn silent_primary_is_answered_by_the_fallback_alias_after_the_agent_timeout() {
        let smart_url = silent_server().await;
        let mut coder = mockito::Server::new_async().await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .match_body(body_of("m-coder", 4321, 0.5))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(FALLBACK_ANSWER_BODY)
            .expect(1)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder.url(), None, r#"fallback_alias: "coder""#),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let started = std::time::Instant::now();
        let res = registry
            .generate_chat_within(
                "smart",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(1),
                Some(std::time::Duration::from_secs(200)),
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await;
        let waited = started.elapsed();

        coder_mock.assert_async().await;
        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "fallback answer"),
            other => panic!("the fallback alias must answer a silent primary, got {other:?}"),
        }
        assert!(
            waited >= std::time::Duration::from_secs(1),
            "the primary keeps the agent's whole 1 s, took {waited:?}"
        );
    }

    /// Too little of the try left for the fallback's bound: it is not tried,
    /// and the sentence says so.
    #[tokio::test]
    async fn silent_primary_with_too_little_of_the_try_left_does_not_try_the_fallback() {
        let smart_url = silent_server().await;
        let mut coder = mockito::Server::new_async().await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder.url(), None, r#"fallback_alias: "coder""#),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let failure = registry
            .generate_chat_within(
                "smart",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(1),
                Some(std::time::Duration::from_secs(30)),
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("no fallback fits in the try");

        coder_mock.assert_async().await;
        assert!(!failure.fallback_attempted(), "{failure:?}");
        assert_eq!(failure.attempts(), 1, "{failure:?}");
        assert_eq!(
            failure.client_sentence(),
            "The model on alias 'smart' (m-smart) gave no answer within 1 s; its fallback 'coder' (m-coder) was not tried: the try had 29 s left and the fallback needs at least 39 s."
        );
    }

    /// Both silent: one fallback attempt, the event's fields true to it, and
    /// the person reads one plain sentence naming both models and seconds.
    #[tokio::test]
    async fn silent_primary_and_silent_fallback_end_in_one_plain_sentence() {
        let smart_url = silent_server().await;
        let coder_url = silent_server().await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder_url, None, r#"fallback_alias: "coder""#),
            SHORT_ATTEMPT_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let started = std::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "smart",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(1),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("both models are silent");
        let waited = started.elapsed();

        assert!(failure.fallback_attempted(), "{failure:?}");
        assert_eq!(failure.attempts(), 2, "one to each model: {failure:?}");
        assert!(failure.elapsed_ms() >= 2_900, "{failure:?}");
        assert!(
            waited < std::time::Duration::from_millis(3_500),
            "took {waited:?}"
        );
        assert_eq!(
            failure.client_sentence(),
            "The model on alias 'smart' (m-smart) gave no answer within 1 s; its fallback 'coder' (m-coder) was tried and gave no answer within 2 s."
        );
        assert!(matches!(failure.error, LLMError::Network(_)), "{failure:?}");
    }

    /// The fallback's window (8,192 - 4,321 = 3,871 bytes) cannot hold the
    /// request: it is not sent, and the reason is recorded and said.
    #[tokio::test]
    async fn silent_primary_fallback_too_small_for_the_request_is_skipped_with_the_reason() {
        let smart_url = silent_server().await;
        let mut coder = mockito::Server::new_async().await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder.url(), None, r#"fallback_alias: "coder""#),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);
        let messages = user_message("x".repeat(5_000));

        let failure = registry
            .generate_chat_within(
                "smart",
                &messages,
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(1),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("the primary is silent and the fallback cannot hold the request");

        coder_mock.assert_async().await;
        assert!(!failure.fallback_attempted(), "{failure:?}");
        let request_bytes = chat_request_bytes(&messages, &[]);
        assert_eq!(
            failure.client_sentence(),
            format!(
                "The model on alias 'smart' (m-smart) gave no answer within 1 s; its fallback 'coder' (m-coder) was not tried: the request is {request_bytes} bytes and its window holds 3871."
            )
        );
        assert!(
            failure
                .error
                .to_string()
                .contains("was not sent the request"),
            "{failure:?}"
        );
    }

    /// The same skip after the provider's time limit (execution a15a5bbb's
    /// 413 from a fallback whose window was smaller than the request).
    #[tokio::test]
    async fn fallback_alias_after_3046_too_small_for_the_request_is_not_sent_it() {
        let mut smart = mockito::Server::new_async().await;
        let mut coder = mockito::Server::new_async().await;
        let smart_mock = smart
            .mock("POST", "/chat/completions")
            .with_status(408)
            .with_body(WORKERS_AI_408_BODY)
            .expect(1)
            .create_async()
            .await;
        let coder_mock = coder
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(
                &smart.url(),
                &coder.url(),
                None,
                r#"fallback_alias: "coder""#,
            ),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let err = registry
            .generate_chat(
                "smart",
                &user_message("x".repeat(5_000)),
                &[],
                &GenerationOptions::default(),
            )
            .await
            .expect_err("the fallback cannot hold the request")
            .to_string();

        smart_mock.assert_async().await;
        coder_mock.assert_async().await;
        assert!(
            err.contains("provider timeout") && err.contains("was not sent the request"),
            "{err}"
        );
    }

    /// No `fallback_alias`: the call is bounded by the agent's timeout as
    /// before, with today's error and no report.
    #[tokio::test]
    async fn silent_primary_without_fallback_alias_is_ended_at_the_agent_timeout_as_before() {
        let smart_url = silent_server().await;
        let coder = mockito::Server::new_async().await;
        let manifest = alias_fallback_manifest(
            &alias_fallback_providers(&smart_url, &coder.url(), None, ""),
            ALIAS_FALLBACK_SELECTION,
        );
        let registry = alias_fallback_registry(&manifest);

        let started = std::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "smart",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(2),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("a silent primary with no fallback fails");
        let waited = started.elapsed();

        assert!(
            waited >= std::time::Duration::from_secs(2),
            "took {waited:?}"
        );
        assert!(failure.report.is_none(), "{failure:?}");
        assert_eq!(failure.attempts(), 0);
        assert!(!failure.fallback_attempted());
        assert_eq!(
            failure.client_sentence(),
            "Network error: model call on alias 'smart' gave no answer within the agent's llm_timeout_seconds (2 s)"
        );
    }

    /// A provider's rate limit is retried with the backoff for as long as the
    /// next attempt starts inside the call's `llm_timeout_seconds`, not
    /// `max_retries` times: five 429s then an answer, with `max_retries` 3,
    /// answer (proof 19, run `ebd5f9ae`: four tries lost to one burst).
    #[tokio::test]
    async fn a_rate_limit_is_retried_within_the_call_bound_not_max_retries_times() {
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::RateLimit(None)),
            Err(LLMError::RateLimit(None)),
            Err(LLMError::RateLimit(None)),
            Err(LLMError::RateLimit(None)),
            Err(LLMError::RateLimit(None)),
            Ok(ok_response()),
        ]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 60);

        let res = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(60),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await;

        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
            other => panic!("the call must ride out five rate limits inside 60 s, got {other:?}"),
        }
        assert_eq!(
            primary.call_count(),
            6,
            "five refused attempts and the answer"
        );
    }

    /// A rate limit that outlasts the try's time left ends the call before
    /// the try ends: no backoff is slept past what is left of the try, and
    /// the call fails with the provider's rate limit.
    #[tokio::test]
    async fn a_rate_limit_retry_stops_before_the_try_ends() {
        let primary = MockProvider::new();
        *primary.default_response.lock().unwrap() = Some(Err(LLMError::RateLimit(None)));
        let primary = Arc::new(primary);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 60);

        let started = std::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(60),
                Some(std::time::Duration::from_secs(2)),
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("a rate limit for the whole try fails the call");
        let waited = started.elapsed();

        assert!(
            matches!(failure.error, LLMError::RateLimit(_)),
            "the provider's rate limit is the call's error, got {failure:?}"
        );
        assert!(
            primary.call_count() > 3,
            "the rate limit is retried past max_retries while the try has time, sent {}",
            primary.call_count()
        );
        assert!(
            waited < std::time::Duration::from_secs(2),
            "no backoff is slept past the try's 2 s, took {waited:?}"
        );
    }

    /// A provider's `Retry-After` is honoured, and one longer than the try's
    /// time left ends the call at once with the rate limit, never sleeping
    /// past the try.
    #[tokio::test]
    async fn a_retry_after_past_the_try_left_ends_the_call_without_waiting() {
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::RateLimit(Some(std::time::Duration::from_secs(
                30,
            )))),
            Ok(ok_response()),
        ]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 60);

        let started = std::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(60),
                Some(std::time::Duration::from_secs(10)),
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .expect_err("a 30 s Retry-After does not fit in 10 s of the try");

        assert!(
            matches!(failure.error, LLMError::RateLimit(Some(_))),
            "{failure:?}"
        );
        assert_eq!(primary.call_count(), 1, "no attempt after the Retry-After");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the call ends at once, took {:?}",
            started.elapsed()
        );
    }

    /// A provider answering for as long as `until`, then with the answer:
    /// each call before it fails with `error`.
    struct RefusedUntil {
        until: tokio::time::Instant,
        error: fn() -> LLMError,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LLMProvider for RefusedUntil {
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> Result<GenerationResponse, LLMError> {
            unimplemented!("test provider: generate not used")
        }

        async fn generate_chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _options: &GenerationOptions,
        ) -> Result<ChatResponse, LLMError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if tokio::time::Instant::now() < self.until {
                Err((self.error)())
            } else {
                Ok(ok_response())
            }
        }

        async fn health_check(&self) -> Result<(), LLMError> {
            Ok(())
        }
    }

    const WORKERS_AI_4009_BODY: &str = r#"{"errors":[{"message":"AiError: AiError: An internal server error occured. (x)","code":4009}],"success":false,"result":{},"messages":[]}"#;

    /// Workers AI's HTTP 500 code 4009 is retried as the provider's
    /// unavailability, inside the call's bound and past `max_retries`: five
    /// such answers then text, `max_retries` 3, and the call answers
    /// (amendment `upstream-rate-limit-spends-no-try-amend-provider-5xx-2026-10-15`;
    /// Forge proof 20, run `5c024c91`: three sends in about 3.5 s, then the
    /// try was lost).
    #[tokio::test]
    async fn a_provider_internal_error_is_retried_within_the_call_bound() {
        let mut server = mockito::Server::new_async().await;
        let refused = server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body(WORKERS_AI_4009_BODY)
            .expect(5)
            .create_async()
            .await;
        let answered = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let adapter = Arc::new(super::super::openai::OpenAIAdapter::new(
            server.url(),
            "test-key".to_string(),
            "test-model".to_string(),
            reqwest::header::HeaderMap::new(),
        ));
        let registry = ProviderRegistry::new_for_test(adapter, None, 3, 1, 60);

        let res = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(60),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await;

        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
            other => panic!(
                "the call must ride out five HTTP 500 code 4009 answers inside 60 s, got {other:?}"
            ),
        }
        refused.assert_async().await;
        answered.assert_async().await;
    }

    /// A rate limit is retried for the whole of the call's bound: the backoff
    /// is capped at 16 s and its last wait shortened to leave one attempt,
    /// so a provider over its limit for 100 s of a 120 s bound is answered
    /// (amendment `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`;
    /// Forge proof 20, run `5c024c91`: each TEST call ended at about 65 s,
    /// the doubling's next wait of 64 s crossing the 120 s bound).
    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_retry_uses_the_whole_bound() {
        let started = tokio::time::Instant::now();
        let provider = Arc::new(RefusedUntil {
            until: started + std::time::Duration::from_secs(100),
            error: || LLMError::RateLimit(None),
            calls: AtomicUsize::new(0),
        });
        let registry = ProviderRegistry::new_for_test(provider.clone(), None, 3, 1000, 60);

        let res = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(120),
                None,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await;

        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
            other => panic!(
                "a 100 s rate limit inside a 120 s bound must be ridden out, got {other:?} after {:?} and {} sends",
                started.elapsed(),
                provider.calls.load(Ordering::SeqCst)
            ),
        }
    }

    /// A rate limit is waited against the try's time left less the command
    /// margin, not against `llm_timeout_seconds`, which bounds each attempt:
    /// with 600 s of the try left and a 120 s `llm_timeout_seconds`, a
    /// provider over its limit for 200 s is answered (amendment
    /// `upstream-rate-limit-spends-no-try-amend-bound-2026-10-15`).
    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_is_waited_against_the_try_not_llm_timeout() {
        let started = tokio::time::Instant::now();
        let provider = Arc::new(RefusedUntil {
            until: started + std::time::Duration::from_secs(200),
            error: || LLMError::RateLimit(None),
            calls: AtomicUsize::new(0),
        });
        let registry = ProviderRegistry::new_for_test(provider.clone(), None, 3, 1000, 60);

        let res = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(120),
                Some(std::time::Duration::from_secs(600)),
                std::time::Duration::from_secs(130),
                std::time::Duration::ZERO,
            )
            .await;

        match res {
            Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
            other => panic!(
                "a 200 s rate limit inside 600 s of the try must be ridden out, got {other:?} after {:?} and {} sends",
                started.elapsed(),
                provider.calls.load(Ordering::SeqCst)
            ),
        }
        assert!(
            started.elapsed() > std::time::Duration::from_secs(120),
            "the call outlived llm_timeout_seconds only by waiting, took {:?}",
            started.elapsed()
        );
    }

    /// A provider that never answers.
    struct NeverAnswers {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LLMProvider for NeverAnswers {
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> Result<GenerationResponse, LLMError> {
            unimplemented!("test provider: generate not used")
        }

        async fn generate_chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _options: &GenerationOptions,
        ) -> Result<ChatResponse, LLMError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(ok_response())
        }

        async fn health_check(&self) -> Result<(), LLMError> {
            Ok(())
        }
    }

    /// AEGIS ADR-141, Update of 2026-10-16 `tests-load-before-code-2026-10-16`,
    /// clause 3: a model call in a try is bounded by the lesser of
    /// `llm_timeout_seconds` and the try's time left less the 10 s kept for
    /// the answer's way back. On run `e7f2c2aa` the coder's last call started
    /// 26 s before its try's end and was still running when the supervisor
    /// ended the try. With 40 s of the try left and `llm_timeout_seconds` 300,
    /// the call ends at 30 s, saying so.
    #[tokio::test(start_paused = true)]
    async fn a_call_with_40_s_of_the_try_left_ends_at_30_s() {
        let primary = Arc::new(NeverAnswers {
            calls: AtomicUsize::new(0),
        });
        let registry = ProviderRegistry::new_for_test(primary.clone(), None, 3, 1, 600);
        let started = tokio::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(300),
                Some(std::time::Duration::from_secs(40)),
                std::time::Duration::from_secs(310),
                std::time::Duration::from_secs(10),
            )
            .await
            .expect_err("a call that never answers fails");
        assert_eq!(
            started.elapsed(),
            std::time::Duration::from_secs(30),
            "the call was ended after {:?}",
            started.elapsed()
        );
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            failure.error.to_string(),
            "Network error: model call on alias 'default' gave no answer before the last 10 s \
             of its try, which are kept for the answer's way back"
        );
    }

    /// Clause 3: a call that cannot start with more than the 10 s kept for
    /// the answer's way back is not sent.
    #[tokio::test(start_paused = true)]
    async fn a_call_with_10_s_of_the_try_left_is_not_sent() {
        let primary = Arc::new(NeverAnswers {
            calls: AtomicUsize::new(0),
        });
        let registry = ProviderRegistry::new_for_test(primary.clone(), None, 3, 1, 600);
        let started = tokio::time::Instant::now();
        let failure = registry
            .generate_chat_within(
                "default",
                &[],
                &[],
                &GenerationOptions::default(),
                std::time::Duration::from_secs(300),
                Some(std::time::Duration::from_secs(10)),
                std::time::Duration::from_secs(310),
                std::time::Duration::from_secs(10),
            )
            .await
            .expect_err("a call with no time left is not sent");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 0, "the call was sent");
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
        assert_eq!(
            failure.error.to_string(),
            "Network error: model call on alias 'default' was not sent: its try had 10 s left, \
             and the last 10 s of a try are kept for the answer's way back"
        );
    }
}
