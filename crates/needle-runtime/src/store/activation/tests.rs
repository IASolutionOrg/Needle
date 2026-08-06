use super::*;
use needle_core::{
    CodexHost, CodexRole, CommandPolicy, Digest, FallbackPolicy, FilesystemPolicy, NetworkPolicy,
    ReasoningLevel, RepairPolicy, RoleProfileBudget, RoleProfileDefinition,
    RoleProfileDefinitionInput, ServiceTier, TestPolicy, ToolPolicy,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture() -> (std::path::PathBuf, RuntimeStore, std::path::PathBuf, RoleProfileId) {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("needle-activation-{suffix}"));
    let repository = root.join("repository");
    std::fs::create_dir_all(&repository).unwrap();
    let store = RuntimeStore::new(root.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile_id = RoleProfileId::new("explorer.default").unwrap();
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: profile_id.clone(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "gpt-5-mini".to_owned(),
        reasoning: ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 120,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest: Digest::blake3(b"activation-prompt"),
        output_contract_digest: Digest::blake3(b"activation-output"),
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
fn repository_activation_overrides_global_and_is_idempotent() {
    let (root, store, repository, profile_id) = fixture();
    let global = store.set_global_activation(true, Some(&profile_id)).unwrap();
    let inherited = store.activation_status(&repository).unwrap();
    assert!(inherited.enabled);
    assert_eq!(inherited.effective_scope, Some(ActivationScope::Global));
    assert_eq!(inherited.role_profile_id.as_ref(), Some(&profile_id));

    let local_disabled = store.set_repository_activation(&repository, false, None).unwrap();
    let disabled = store.activation_status(&repository).unwrap();
    assert!(!disabled.enabled);
    assert!(matches!(disabled.effective_scope, Some(ActivationScope::Repository { .. })));
    assert_eq!(local_disabled.generation, 0);

    let enabled = store.set_repository_activation(&repository, true, Some(&profile_id)).unwrap();
    let repeated = store.set_repository_activation(&repository, true, Some(&profile_id)).unwrap();
    assert_eq!(enabled, repeated);
    assert_eq!(enabled.generation, 1);
    assert_ne!(enabled.state_digest, global.state_digest);
    assert!(store.activation_status(&repository).unwrap().enabled);
    let profile_state = store.role_profile_state(&profile_id).unwrap();
    assert!(store.deactivate_role_profile(&profile_id, profile_state.state_digest).is_err());

    let connection = store.connection().unwrap();
    let audit_count: u64 = connection
        .query_row("SELECT COUNT(*) FROM product_activation_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(audit_count, 3);
    drop(connection);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn enable_requires_an_active_profile_and_compare_and_set_rejects_stale_state() {
    let (root, store, repository, profile_id) = fixture();
    let missing = RoleProfileId::new("explorer.missing").unwrap();
    assert!(store.set_global_activation(true, Some(&missing)).is_err());

    let initial = store.set_repository_activation(&repository, false, None).unwrap();
    let scope = ActivationScope::repository(&repository).unwrap();
    assert!(matches!(
        store.compare_and_set_activation(scope.clone(), true, Some(&profile_id), None),
        Err(StoreError::ActivationConflict(_))
    ));
    let updated = store
        .compare_and_set_activation(
            scope.clone(),
            true,
            Some(&profile_id),
            Some(initial.state_digest),
        )
        .unwrap();
    assert!(updated.enabled);
    assert!(matches!(
        store.compare_and_set_activation(scope, false, None, Some(initial.state_digest),),
        Err(StoreError::ActivationConflict(_))
    ));
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
