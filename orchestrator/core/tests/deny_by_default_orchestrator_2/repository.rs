// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Zaru ADR-0071 D1 (d): a repository by URL only through a named GitHub binding
//!
//! A run's `repositories` entry naming a binding id is an explicit naming
//! and stands. The URL form makes or reuses a binding only through the
//! GitHub binding the run named (its profile's, or chosen one by one);
//! with none named it is refused with the run sentence, whatever GitHub
//! credentials the person holds. A child module of `repository_by_url_tests`,
//! whose fixture it uses.

use super::*;
use aegis_orchestrator_core::domain::execution::ServerChoice;

const RUN_SENTENCE: &str =
    "This run has no profile that names your GitHub. Choose a profile that names it, then start the run again.";

#[tokio::test]
async fn a_url_is_refused_with_the_run_sentence_when_the_run_names_no_github_binding() {
    let fx = fixture().await;
    // The person holds one active GitHub credential; the run named none.
    fx.credential("github", CredentialStatus::Active).await;

    for choice in [ServerChoice::NotGiven, ServerChoice::None] {
        let sentence = refusal(fx.resolve(json!([{ "url": URL }]), choice.clone()).await);
        assert_eq!(sentence, RUN_SENTENCE, "for {choice:?}");
    }
    assert!(fx.bindings.all().is_empty(), "a binding was made");
    assert_eq!(fx.runner.steps(), 0, "a git step ran");
}

#[tokio::test]
async fn a_url_is_bound_with_the_github_binding_the_run_named_among_several() {
    let fx = fixture().await;
    fx.credential("github", CredentialStatus::Active).await;
    let named = fx.credential("github", CredentialStatus::Active).await;

    let entries = fx
        .resolve(json!([{ "url": URL }]), ServerChoice::Bindings(vec![named]))
        .await
        .unwrap_or_else(|e| panic!("the URL with a named GitHub binding was refused: {e:?}"));

    let made = fx.bindings.all();
    assert_eq!(made.len(), 1, "not exactly one binding was made: {made:?}");
    assert_eq!(entries[0].binding_id, made[0].id);
    assert_eq!(
        made[0].credential_binding_id,
        Some(named),
        "the binding was not made with the GitHub binding the run named"
    );
    assert_eq!(made[0].status, GitRepoStatus::Ready);
}

#[tokio::test]
async fn a_binding_of_that_url_linked_with_another_credential_is_refused_not_reused() {
    let fx = fixture().await;
    let other = fx.credential("github", CredentialStatus::Active).await;
    let named = fx.credential("github", CredentialStatus::Active).await;
    let existing = fx
        .existing_binding("https://git.example.test/org/repo", "main", "repo")
        .await;
    let mut binding = fx.bindings.find_by_id(&existing).await.unwrap().unwrap();
    binding.credential_binding_id = Some(other);
    fx.bindings.save(&binding).await.unwrap();

    let sentence = refusal(
        fx.resolve(json!([{ "url": URL }]), ServerChoice::Bindings(vec![named]))
            .await,
    );

    assert_eq!(
        sentence,
        "your binding of this repository is linked with another GitHub credential than the one this run names; name its binding_id, or choose a profile that names that credential"
    );
    assert_eq!(fx.bindings.all().len(), 1, "a binding was made");
    assert_eq!(fx.runner.steps(), 0, "a git step ran");
}

#[tokio::test]
async fn a_github_binding_the_run_named_that_is_not_an_active_one_of_yours_is_refused() {
    let fx = fixture().await;
    fx.credential("github", CredentialStatus::Active).await;
    let revoked = fx.credential("github", CredentialStatus::Revoked).await;
    let gitlab = fx.credential("gitlab", CredentialStatus::Active).await;

    for named in [revoked, gitlab, CredentialBindingId::new()] {
        let sentence = refusal(
            fx.resolve(json!([{ "url": URL }]), ServerChoice::Bindings(vec![named]))
                .await,
        );
        assert_eq!(sentence, "no active GitHub credential of yours");
    }
    assert!(fx.bindings.all().is_empty(), "a binding was made");
}

/// The workflow start hands the URL's resolution the run's GitHub choice,
/// read from its `contexts` (a profile's bindings are written there first).
#[tokio::test]
async fn a_workflow_start_resolves_a_url_through_the_github_binding_it_named() {
    let named = CredentialBindingId::new();
    let (use_case, _engine, repositories) = start_with(None).await;
    use_case
        .start_execution(request(json!({
            "task": "x",
            "contexts": { "github": named.0.to_string() },
            "repositories": [{ "url": URL }]
        })))
        .await
        .unwrap_or_else(|e| panic!("the start was refused: {e:#}"));
    let (use_case, _engine, without) = start_with(None).await;
    use_case
        .start_execution(request(json!({
            "task": "x",
            "repositories": [{ "url": URL }]
        })))
        .await
        .unwrap_or_else(|e| panic!("the start was refused: {e:#}"));

    assert_eq!(
        *repositories.github.lock().unwrap(),
        vec![ServerChoice::Bindings(vec![named])]
    );
    assert_eq!(
        *without.github.lock().unwrap(),
        vec![ServerChoice::NotGiven]
    );
}
