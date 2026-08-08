use super::*;
use needle_core::{
    CodexHost, CodexRole, CommandPolicy, FallbackPolicy, FilesystemPolicy, NetworkPolicy,
    ReasoningLevel, RepairPolicy, RoleProfileBudget, RoleProfileDefinition,
    RoleProfileDefinitionInput, RoleProfileId, ServiceTier, TestPolicy, ToolPolicy,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture() -> (PathBuf, RuntimeStore, PathBuf, RoleProfileId) {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let root = std::env::temp_dir().join(format!("needle-activation-http-{nonce}"));
    let repository = root.join("repository");
    fs::create_dir_all(&repository).unwrap();
    let store = RuntimeStore::new(root.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile_id = RoleProfileId::new("explorer.default").unwrap();
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: profile_id.clone(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "gpt-5.6-terra".to_owned(),
        reasoning: ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 120,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1_000_000,
        },
        prompt_profile_digest: Digest::blake3(b"activation-http-prompt"),
        output_contract_digest: Digest::blake3(b"activation-http-output"),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: RepairPolicy::None,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: Vec::new(),
    })
    .unwrap();
    let revision = store.create_role_profile(definition).unwrap();
    let state = store.role_profile_state(&profile_id).unwrap();
    store.activate_role_profile(&profile_id, revision.revision, state.state_digest).unwrap();
    (root, store, repository, profile_id)
}

#[test]
fn stale_activation_never_runs_desktop_reconciliation() {
    let (root, store, repository, profile_id) = fixture();
    let initial = store.set_repository_activation(&repository, false, None).unwrap();
    let reconciled = AtomicBool::new(false);

    let result = commit_activation_and_reconcile(
        &store,
        ActivationScope::repository(&repository).unwrap(),
        true,
        Some(&profile_id),
        None,
        |_| {
            reconciled.store(true, Ordering::SeqCst);
            Ok(())
        },
    );

    assert!(matches!(
        result,
        Err(ActivationMutationError::Store(StoreError::ActivationConflict(_)))
    ));
    assert!(!reconciled.load(Ordering::SeqCst));
    assert_eq!(store.activation_status(&repository).unwrap().repository, Some(initial));
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn integration_failure_preserves_committed_activation_as_desired_state() {
    let (root, store, repository, profile_id) = fixture();
    let initial = store.set_repository_activation(&repository, false, None).unwrap();

    let result = commit_activation_and_reconcile(
        &store,
        ActivationScope::repository(&repository).unwrap(),
        true,
        Some(&profile_id),
        Some(initial.state_digest),
        |_| Err("simulated Desktop skill failure".to_owned()),
    );

    assert!(matches!(
        result,
        Err(ActivationMutationError::Integration(error))
            if error == "simulated Desktop skill failure"
    ));
    let status = store.activation_status(&repository).unwrap();
    assert!(status.enabled);
    assert_eq!(status.role_profile_id.as_ref(), Some(&profile_id));
    assert_eq!(status.repository.unwrap().generation, initial.generation + 1);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_failure_preserves_committed_deactivation_as_desired_state() {
    let (root, store, repository, profile_id) = fixture();
    let initial = store.set_repository_activation(&repository, true, Some(&profile_id)).unwrap();

    let result = commit_activation_and_reconcile(
        &store,
        ActivationScope::repository(&repository).unwrap(),
        false,
        None,
        Some(initial.state_digest),
        |_| Err("simulated Desktop skill cleanup failure".to_owned()),
    );

    assert!(matches!(result, Err(ActivationMutationError::Integration(_))));
    let status = store.activation_status(&repository).unwrap();
    assert!(!status.enabled);
    assert_eq!(status.repository.unwrap().generation, initial.generation + 1);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
