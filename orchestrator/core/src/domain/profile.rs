// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Profiles (AEGIS ADR-140 D1, D2, D4, D5, D11)
//!
//! A profile is a person's own named set of their credential bindings, any
//! number of any kinds, with an allow-list of tool patterns that can only
//! narrow what the deployment's security context admits, and optional
//! defaults (a repository, a Nuclear Notes workspace, instructions).
//!
//! Names (D11): "profile" is this object. "Security context" keeps meaning
//! the deployment's named policy (`zaru-free`, ...) and "contexts" the
//! per-server map of chosen bindings a run carries; a profile is neither.
//! The command line's login profiles (`cli/src/auth/profile.rs`) are another
//! thing and never reach a run.
//!
//! D5: a run, a conversation or a schedule that carries raw bindings (the
//! `contexts` map) is read as an implicit profile of those bindings with an
//! empty allow-list: every tool the security context admits. Nothing here is
//! built for it; [`ToolAllowList::admits`] states it for the empty list.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::credential::CredentialBindingId;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;

/// The longest profile name, in characters after trimming (D1).
pub const NAME_MAX_CHARS: usize = 64;
/// The longest `instructions`, in characters (D1).
pub const INSTRUCTIONS_MAX_CHARS: usize = 4_000;
/// The longest `notes_workspace` id or slug, in characters.
pub const NOTES_WORKSPACE_MAX_CHARS: usize = 128;

/// The refusal of a name out of range.
pub const NAME_REFUSAL: &str = "'name' must be 1 to 64 characters.";
/// The refusal of a name the owner already gave another profile.
pub const NAME_TAKEN_REFUSAL: &str =
    "You already have a profile with this name; choose another name.";
/// The refusal of `instructions` that are too long.
pub const INSTRUCTIONS_REFUSAL: &str = "'instructions' must be at most 4,000 characters.";
/// The refusal of a `notes_workspace` out of range.
pub const NOTES_WORKSPACE_REFUSAL: &str =
    "'notes_workspace' must be a Nuclear Notes workspace id or slug of 1 to 128 characters.";
/// The refusal of a `repository` that is not one of the owner's.
pub const REPOSITORY_REFUSAL: &str = "'repository' must be the id of one of your repositories.";
/// The refusal of a `bindings` that is not a list of ids.
pub const BINDINGS_SHAPE_REFUSAL: &str = "'bindings' must be a list of your connection ids.";
/// The refusal of a binding listed twice.
pub const BINDINGS_DUPLICATE_REFUSAL: &str = "A connection appears more than once in 'bindings'.";
/// The refusal of a `tools` that is not a list of strings.
pub const TOOLS_SHAPE_REFUSAL: &str = "'tools' must be a list of tool names or families.";
/// The refusal of anyone but a person (an operator, a service account).
pub const PERSON_REFUSAL: &str =
    "Profiles belong to the person who made them; only that person can read or change them.";
/// The answer of a node with no profile store.
pub const UNAVAILABLE_REFUSAL: &str = "Profiles are not available on this node.";

/// D2's sentence: a pattern that admits nothing the owner's plan allows for
/// the profile's connections.
pub fn narrow_refusal(pattern: &str) -> String {
    format!("A profile can only narrow the tools your plan allows; {pattern} is not one of them.")
}

/// The refusal of a binding that is not the owner's own active one (D1).
pub fn binding_refusal(binding: &str) -> String {
    let shown: String = binding
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    format!("'{shown}' is not an active connection of yours.")
}

/// The refusal of a tool pattern of the wrong shape.
pub fn pattern_refusal(pattern: &str) -> String {
    let shown: String = pattern
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    format!("'{shown}' must be a tool name, or a family of tools written as <family>.*")
}

/// A profile's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The id a route names, or `None` for anything that is not a UUID.
    pub fn parse(raw: &str) -> Option<Self> {
        Uuid::parse_str(raw).ok().map(Self)
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One entry of a profile's allow-list (D1): an exact tool name, or every
/// tool of a family written `<family>.*`. The dot is required, so `github.*`
/// admits `github.list_issues` and never `githubx.list`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolPattern {
    /// One tool, by its full name (`mail.reply`).
    Exact(String),
    /// Every tool whose name is `<family>.<something>`.
    Family(String),
}

impl ToolPattern {
    /// Parse a pattern; `Err` carries the refusal's sentence.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let well_formed = |s: &str| {
            !s.is_empty()
                && s.split('.').all(|part| {
                    !part.is_empty()
                        && part
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                })
        };
        match raw.strip_suffix(".*") {
            Some(family) if well_formed(family) => Ok(Self::Family(family.to_string())),
            Some(_) => Err(pattern_refusal(raw)),
            None if raw.contains('.') && well_formed(raw) => Ok(Self::Exact(raw.to_string())),
            None => Err(pattern_refusal(raw)),
        }
    }

    /// The pattern as written.
    pub fn as_string(&self) -> String {
        match self {
            Self::Exact(name) => name.clone(),
            Self::Family(family) => format!("{family}.*"),
        }
    }

    /// The family the pattern governs: the name before the first dot.
    pub fn family(&self) -> &str {
        match self {
            Self::Exact(name) => name.split('.').next().unwrap_or(name),
            Self::Family(family) => family.split('.').next().unwrap_or(family),
        }
    }

    /// Whether the pattern admits `tool_name`.
    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            Self::Exact(name) => name == tool_name,
            Self::Family(family) => tool_name
                .strip_prefix(family.as_str())
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|rest| !rest.is_empty()),
        }
    }
}

/// A profile's allow-list (D2): the empty list admits every tool the
/// security context admits for the profile's connections; a non-empty list
/// admits only what one of its patterns matches. It never admits what the
/// security context does not; that check is the dispatch's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAllowList(pub Vec<ToolPattern>);

impl ToolAllowList {
    /// Whether the list lets `tool_name` through to the security context's
    /// own check.
    pub fn admits(&self, tool_name: &str) -> bool {
        self.0.is_empty() || self.0.iter().any(|p| p.matches(tool_name))
    }

    /// The patterns as written, in order.
    pub fn as_strings(&self) -> Vec<String> {
        self.0.iter().map(ToolPattern::as_string).collect()
    }
}

/// A person's profile (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub tenant_id: TenantId,
    /// The owner's `sub`.
    pub user_sub: String,
    pub name: String,
    /// The owner's bindings, in order. A binding revoked or deleted since
    /// stays here and is answered removed on read (D4).
    pub bindings: Vec<CredentialBindingId>,
    pub tools: ToolAllowList,
    /// The default repository: one of the owner's git repository bindings.
    pub repository: Option<Uuid>,
    /// The default Nuclear Notes workspace, by id or slug.
    pub notes_workspace: Option<String>,
    pub instructions: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A profile name as stored: trimmed and within D1's bounds.
pub fn check_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > NAME_MAX_CHARS {
        return Err(NAME_REFUSAL.to_string());
    }
    Ok(name.to_string())
}

/// `instructions` as stored: none when empty after trimming.
pub fn check_instructions(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) if text.chars().count() > INSTRUCTIONS_MAX_CHARS => {
            Err(INSTRUCTIONS_REFUSAL.to_string())
        }
        Some(text) => Ok(Some(text.to_string())),
    }
}

/// `notes_workspace` as stored: none when absent.
pub fn check_notes_workspace(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw.map(str::trim) {
        None => Ok(None),
        Some(text)
            if text.is_empty()
                || text.chars().count() > NOTES_WORKSPACE_MAX_CHARS
                || text.chars().any(char::is_control) =>
        {
            Err(NOTES_WORKSPACE_REFUSAL.to_string())
        }
        Some(text) => Ok(Some(text.to_string())),
    }
}

/// Whether a save was made, or refused because the owner already has a
/// profile of that name, ignoring case (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSave {
    Saved,
    NameTaken,
}

/// Where profiles are kept.
#[async_trait]
pub trait ProfileRepository: Send + Sync {
    /// Store a new profile.
    async fn insert(&self, profile: &Profile) -> Result<ProfileSave, RepositoryError>;

    /// Replace a stored profile's fields and bindings.
    async fn update(&self, profile: &Profile) -> Result<ProfileSave, RepositoryError>;

    /// A profile that is not deleted.
    async fn find(&self, id: &ProfileId) -> Result<Option<Profile>, RepositoryError>;

    /// The owner's profiles that are not deleted, by name ignoring case.
    async fn list_for_owner(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<Profile>, RepositoryError>;

    /// Mark a profile deleted; `false` when there was none to delete.
    async fn delete(&self, id: &ProfileId, at: DateTime<Utc>) -> Result<bool, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D1: a family pattern needs its dot, so `github.*` never admits
    /// `githubx.list`; an exact name admits only itself.
    #[test]
    fn a_family_pattern_needs_its_dot() {
        let github = ToolPattern::parse("github.*").expect("github.* parses");
        let wrong = [
            ("github.list_issues", true),
            ("githubx.list", false),
            ("github", false),
            ("github.", false),
        ]
        .into_iter()
        .filter(|(tool, admitted)| github.matches(tool) != *admitted)
        .collect::<Vec<_>>();
        assert!(wrong.is_empty(), "github.* answered wrongly for {wrong:?}");
        let reply = ToolPattern::parse("mail.reply").expect("mail.reply parses");
        assert!(reply.matches("mail.reply") && !reply.matches("mail.reply_all"));
        assert_eq!(github.family(), "github");
        assert_eq!(reply.family(), "mail");
    }

    /// A pattern of the wrong shape is refused with a sentence naming it.
    #[test]
    fn a_malformed_pattern_is_refused() {
        let mut accepted = Vec::new();
        for raw in [
            "*",
            "mail",
            ".*",
            "mail.*.*",
            "mail..reply",
            "mail.re ply",
            "",
        ] {
            if ToolPattern::parse(raw).is_ok() {
                accepted.push(raw);
            }
        }
        assert!(
            accepted.is_empty(),
            "malformed patterns accepted: {accepted:?}"
        );
        assert_eq!(
            ToolPattern::parse("mail").unwrap_err(),
            "'mail' must be a tool name, or a family of tools written as <family>.*"
        );
    }

    /// D5: the empty allow-list (the implicit profile of raw bindings)
    /// admits every tool; a non-empty one only what it lists.
    #[test]
    fn the_empty_allow_list_admits_everything() {
        assert!(ToolAllowList::default().admits("mail.send"));
        let list = ToolAllowList(vec![ToolPattern::parse("mail.reply").unwrap()]);
        assert!(list.admits("mail.reply"));
        assert!(
            !list.admits("mail.send"),
            "a narrowed list admitted mail.send"
        );
    }

    /// D1's bounds on the name, the instructions and the workspace.
    #[test]
    fn names_and_defaults_are_bounded() {
        assert_eq!(check_name("  Fundraising ").unwrap(), "Fundraising");
        assert_eq!(check_name("   ").unwrap_err(), NAME_REFUSAL);
        assert_eq!(check_name(&"x".repeat(65)).unwrap_err(), NAME_REFUSAL);
        assert!(check_name(&"x".repeat(64)).is_ok());
        assert_eq!(
            check_instructions(Some(&"x".repeat(4_001))).unwrap_err(),
            INSTRUCTIONS_REFUSAL
        );
        assert_eq!(check_instructions(Some("  ")).unwrap(), None);
        assert_eq!(
            check_notes_workspace(Some("")).unwrap_err(),
            NOTES_WORKSPACE_REFUSAL
        );
    }
}
