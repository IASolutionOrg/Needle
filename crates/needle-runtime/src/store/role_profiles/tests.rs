use super::*;
use needle_core::{
    CacheLookup, CodexHost, CodexRole, CommandPolicy, FallbackPolicy, FilesystemPolicy,
    NeedCacheEntry, NeedCacheIdentity, NeedKey, NeedResult, NetworkPolicy, RepairPolicy,
    RoleProfileBudget, RoleProfileDefinitionInput, RoleProfileId, RoleProfileProvenance,
    ServiceTier, TestPolicy, ToolPolicy, WorkerOutcome,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_store() -> (PathBuf, RuntimeStore) {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = std::env::temp_dir().join(format!("needle-role-profile-{nanos}.sqlite3"));
    (path.clone(), RuntimeStore::new(path))
}

fn definition(id: &str) -> RoleProfileDefinition {
    definition_with_execution(id, "gpt-5", 120, RepairPolicy::None)
}

fn definition_with_execution(
    id: &str,
    model: &str,
    timeout_seconds: u64,
    repair_policy: RepairPolicy,
) -> RoleProfileDefinition {
    RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: RoleProfileId::new(id).unwrap(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: model.to_owned(),
        reasoning: needle_core::ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest: Digest::blake3(b"prompt"),
        output_contract_digest: Digest::blake3(b"output"),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: vec![],
    })
    .unwrap()
}

fn definition_with_model(id: &str, model: &str) -> RoleProfileDefinition {
    let base = definition(id);
    RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: base.profile_id,
        role: base.role,
        host: base.host,
        model: model.to_owned(),
        reasoning: base.reasoning,
        service_tier: base.service_tier,
        timeout_seconds: base.timeout_seconds,
        budget: base.budget,
        prompt_profile_digest: base.prompt_profile_digest,
        output_contract_digest: base.output_contract_digest,
        tool_policy: base.tool_policy,
        command_policy: base.command_policy,
        filesystem_policy: base.filesystem_policy,
        network_policy: base.network_policy,
        test_policy: base.test_policy,
        repair_policy: base.repair_policy,
        fallback_policy: base.fallback_policy,
        concurrency: base.concurrency,
        route_assignments: base.route_assignments,
    })
    .unwrap()
}

#[test]
fn migration_and_revision_lifecycle_are_atomic_and_immutable() {
    let (path, store) = temporary_store();
    store.initialize().unwrap();
    let connection = Connection::open(&path).unwrap();
    let versions: Vec<u32> = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, (1..=17).collect::<Vec<_>>());
    for name in
        ["role_profiles", "role_profile_revisions", "role_profile_state", "role_profile_audit"]
    {
        let count: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing table {name}");
    }
    drop(connection);
    let first = store.create_role_profile(definition("explorer.default")).unwrap();
    let id = first.profile_id.clone();
    assert_eq!(
        store
            .read_role_profile_revision_by_digest(&id, first.definition.definition_digest)
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        store
            .read_role_profile_revision_by_digest_global(first.definition.definition_digest)
            .unwrap()
            .revision,
        1
    );
    let state = store.role_profile_state(&id).unwrap();
    let mut second_definition = definition("explorer.default");
    second_definition.model = "gpt-5-mini".to_owned();
    second_definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        model: second_definition.model,
        profile_id: second_definition.profile_id,
        role: second_definition.role,
        host: second_definition.host,
        reasoning: second_definition.reasoning,
        service_tier: second_definition.service_tier,
        timeout_seconds: second_definition.timeout_seconds,
        budget: second_definition.budget,
        prompt_profile_digest: second_definition.prompt_profile_digest,
        output_contract_digest: second_definition.output_contract_digest,
        tool_policy: second_definition.tool_policy,
        command_policy: second_definition.command_policy,
        filesystem_policy: second_definition.filesystem_policy,
        network_policy: second_definition.network_policy,
        test_policy: second_definition.test_policy,
        repair_policy: second_definition.repair_policy,
        fallback_policy: second_definition.fallback_policy,
        concurrency: second_definition.concurrency,
        route_assignments: second_definition.route_assignments,
    })
    .unwrap();
    let revised = store.revise_role_profile(&id, state.state_digest, second_definition).unwrap();
    assert_eq!(revised.revision, 2);
    assert_eq!(store.list_role_profile_revisions(&id).unwrap().len(), 2);
    let connection = Connection::open(&path).unwrap();
    assert!(connection
        .execute(
            "DELETE FROM role_profile_revisions WHERE profile_id='explorer.default' AND revision=1",
            [],
        )
        .is_err());
    assert!(
        connection
            .execute("UPDATE role_profile_audit SET created_unix_ms=0 WHERE audit_id=1", [])
            .is_err()
    );
    drop(connection);
    let stale = store.revise_role_profile(&id, state.state_digest, definition("explorer.default"));
    assert!(matches!(stale, Err(StoreError::RoleProfileConflict(_))));
    let state = store.role_profile_state(&id).unwrap();
    let active = store.activate_role_profile(&id, 1, state.state_digest).unwrap();
    assert_eq!(active.state, RoleProfileState::Active);
    let state = store.role_profile_state(&id).unwrap();
    let switched = store.activate_role_profile(&id, 2, state.state_digest).unwrap();
    assert_eq!(switched.state, RoleProfileState::Active);
    assert_eq!(store.read_role_profile_revision(&id, 1).unwrap().state, RoleProfileState::Inactive);
    let active_state = store.role_profile_state(&id).unwrap();
    assert_eq!(
        store.activate_role_profile(&id, 1, active_state.state_digest).unwrap().state,
        RoleProfileState::Active
    );
    let deactivated = store
        .deactivate_role_profile(&id, store.role_profile_state(&id).unwrap().state_digest)
        .unwrap();
    assert_eq!(deactivated.state, RoleProfileState::Inactive);
    assert_eq!(store.read_role_profile_revision(&id, 2).unwrap().state, RoleProfileState::Draft);
    assert!(store.read_active_role_profile(&id).unwrap().is_none());
    let audit = store.read_role_profile_audit(&id, 100).unwrap();
    assert_eq!(audit.len(), 6);
    assert_eq!(audit[0].operation, RoleProfileAuditOperation::Deactivate);
    assert_eq!(audit[1].operation, RoleProfileAuditOperation::Activate);
    assert_eq!(audit[2].operation, RoleProfileAuditOperation::Activate);
    assert_eq!(audit[3].operation, RoleProfileAuditOperation::Activate);
    assert_eq!(audit[4].operation, RoleProfileAuditOperation::Revise);
    assert_eq!(audit[5].operation, RoleProfileAuditOperation::Create);
    let create_audit = &audit[5];
    assert_eq!(create_audit.definition_digest, first.definition.definition_digest);
    assert_eq!(create_audit.prior_state, None);
    assert_eq!(create_audit.resulting_state, RoleProfileState::Draft);
    assert_eq!(create_audit.prior_active_revision, None);
    assert_eq!(create_audit.prior_active_digest, None);
    assert_eq!(create_audit.resulting_active_revision, None);
    assert_eq!(create_audit.resulting_active_digest, None);
    assert!(create_audit.created_unix_ms > 0);
    let activate_audit = &audit[3];
    assert_eq!(activate_audit.definition_digest, first.definition.definition_digest);
    assert_eq!(activate_audit.prior_state, Some(RoleProfileState::Inactive));
    assert_eq!(activate_audit.resulting_state, RoleProfileState::Active);
    assert!(activate_audit.prior_state_digest.is_some());
    assert_eq!(activate_audit.prior_active_revision, None);
    assert_eq!(activate_audit.prior_active_digest, None);
    assert_eq!(activate_audit.resulting_active_revision, Some(1));
    assert_eq!(activate_audit.resulting_active_digest, Some(first.definition.definition_digest));
    assert!(activate_audit.created_unix_ms > 0);
    let deactivate_audit = &audit[0];
    assert_eq!(deactivate_audit.definition_digest, first.definition.definition_digest);
    assert_eq!(deactivate_audit.prior_state, Some(RoleProfileState::Active));
    assert_eq!(deactivate_audit.resulting_state, RoleProfileState::Inactive);
    assert!(deactivate_audit.prior_state_digest.is_some());
    assert_eq!(deactivate_audit.prior_active_revision, Some(1));
    assert_eq!(deactivate_audit.prior_active_digest, Some(first.definition.definition_digest));
    assert_eq!(deactivate_audit.resulting_active_revision, None);
    assert_eq!(deactivate_audit.resulting_active_digest, None);
    assert!(deactivate_audit.created_unix_ms > 0);
    assert!(matches!(
        store.read_role_profile_audit(&id, 101),
        Err(StoreError::RoleProfileValidation(_))
    ));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn session_binding_is_idempotent_conflict_checked_and_historical() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("explorer.session")).unwrap();
    let profile_id = first.profile_id.clone();
    let state = store.role_profile_state(&profile_id).unwrap();
    store.activate_role_profile(&profile_id, 1, state.state_digest).unwrap();

    let prompt = Digest::blake3(b"session-prompt");
    store
        .record_session_start_profiled("session-a", prompt, Some("main"), None, &profile_id)
        .unwrap();
    store
        .record_session_start_profiled("session-a", prompt, Some("main"), None, &profile_id)
        .unwrap();
    let frozen = store.worker_config_for_session("session-a", "codex-a").unwrap();
    assert_eq!(frozen.model, "gpt-5");
    assert_eq!(frozen.timeout_seconds, 120);
    assert_eq!(
        frozen.evidence_failure_policy,
        needle_core::EvidenceFailurePolicy::DiscardInvalidFact
    );

    let state = store.role_profile_state(&profile_id).unwrap();
    let second = store
        .revise_role_profile(
            &profile_id,
            state.state_digest,
            definition_with_execution("explorer.session", "gpt-5-mini", 240, RepairPolicy::Once),
        )
        .unwrap();
    let state = store.role_profile_state(&profile_id).unwrap();
    store.activate_role_profile(&profile_id, second.revision, state.state_digest).unwrap();

    assert!(matches!(
        store.record_session_start_profiled("session-a", prompt, Some("main"), None, &profile_id,),
        Err(StoreError::RoleProfileConflict(_))
    ));
    let still_frozen = store.worker_config_for_session("session-a", "codex-a").unwrap();
    assert_eq!(still_frozen.model, "gpt-5");
    assert_eq!(still_frozen.timeout_seconds, 120);

    store
        .record_session_start_profiled("session-b", prompt, Some("main"), None, &profile_id)
        .unwrap();
    let current = store.worker_config_for_session("session-b", "codex-a").unwrap();
    assert_eq!(current.model, "gpt-5-mini");
    assert_eq!(current.timeout_seconds, 240);
    assert_eq!(current.evidence_failure_policy, needle_core::EvidenceFailurePolicy::RepairOnce);

    let connection = Connection::open(&path).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE sessions
             SET role_profile_revision=?2, role_profile_definition_digest=?3
             WHERE session_id=?1",
                rusqlite::params![
                    "session-a",
                    second.revision,
                    second.definition.definition_digest.to_string(),
                ],
            )
            .is_err()
    );
    drop(connection);

    store.record_legacy_session_start("legacy", prompt, None, None).unwrap();
    assert!(matches!(
        store.record_session_start_profiled("legacy", prompt, None, None, &profile_id),
        Err(StoreError::RoleProfileConflict(_))
    ));
    assert!(store.session("legacy").unwrap().unwrap().role_profile_provenance.is_none());
    let _ = std::fs::remove_file(path);
}

#[test]
fn bounded_revision_listing_reads_only_the_latest_ordered_window() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("explorer.default")).unwrap();
    let id = first.profile_id.clone();
    let mut state = store.role_profile_state(&id).unwrap();
    for model in ["gpt-5-mini", "gpt-5-pro", "gpt-4.1", "gpt-4.1-mini"] {
        let revision = store
            .revise_role_profile(
                &id,
                state.state_digest,
                definition_with_model("explorer.default", model),
            )
            .unwrap();
        assert_eq!(revision.revision, state.latest_revision + 1);
        state = store.role_profile_state(&id).unwrap();
    }
    let (revisions, total) = store.list_role_profile_revisions_bounded(&id, 2).unwrap();
    assert_eq!(total, 5);
    assert_eq!(revisions.iter().map(|value| value.revision).collect::<Vec<_>>(), vec![4, 5]);
    assert_eq!(revisions[0].definition.model, "gpt-4.1");
    assert_eq!(revisions[1].definition.model, "gpt-4.1-mini");
    assert!(matches!(
        store.list_role_profile_revisions_bounded(&id, 0),
        Err(StoreError::RoleProfileValidation(_))
    ));
    assert!(matches!(
        store.list_role_profile_revisions_bounded(&id, 101),
        Err(StoreError::RoleProfileValidation(_))
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn cache_identity_separates_revisions_and_rejects_unknown_or_mismatched_provenance() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("explorer.cache")).unwrap();
    let profile_id = first.profile_id.clone();
    let state = store.role_profile_state(&profile_id).unwrap();
    let second = store
        .revise_role_profile(
            &profile_id,
            state.state_digest,
            definition_with_execution("explorer.cache", "gpt-5-mini", 180, RepairPolicy::Once),
        )
        .unwrap();
    let first_provenance = RoleProfileProvenance::from_revision(&first).unwrap();
    let second_provenance = RoleProfileProvenance::from_revision(&second).unwrap();
    let identity = |provenance: Option<RoleProfileProvenance>| NeedCacheIdentity {
        repository_id: Digest::blake3(b"repository"),
        source_snapshot_digest: Digest::blake3(b"source"),
        prompt_profile_digest: Digest::blake3(b"prompt"),
        route_definition_digest: Digest::blake3(b"route"),
        preset_definition_digest: Digest::blake3(b"preset"),
        need_key: NeedKey::new("trace.state-flow").unwrap(),
        normalized_request_digest: Digest::blake3(b"request"),
        worker_configuration_digest: Digest::blake3(b"worker"),
        output_schema_digest: Digest::blake3(b"schema"),
        role_profile_provenance: provenance,
    };
    let first_identity = identity(Some(first_provenance.clone()));
    let second_identity = identity(Some(second_provenance.clone()));
    assert_ne!(first_identity.digest(), second_identity.digest());
    assert_ne!(first_identity.logical_digest(), second_identity.logical_digest());

    let unknown = identity(Some(
        RoleProfileProvenance::new(
            RoleProfileId::new("ghost").unwrap(),
            1,
            Digest::blake3(b"ghost"),
        )
        .unwrap(),
    ));
    assert!(matches!(
        store.cache_lookup(&unknown).unwrap(),
        CacheLookup::Bypass(reason) if reason == "role-profile-provenance-invalid"
    ));
    assert!(matches!(
        store.cache_lookup(&identity(None)).unwrap(),
        CacheLookup::Bypass(reason) if reason == "role-profile-provenance-unknown"
    ));

    let result = NeedResult {
        complete: true,
        summary: "bounded".to_owned(),
        claims: Vec::new(),
        evidence: Vec::new(),
        suggested_reads: Vec::new(),
        suggested_commands: Vec::new(),
        uncertainty: Vec::new(),
    };
    let outcome = |provenance: RoleProfileProvenance| WorkerOutcome {
        result: result.clone(),
        artifact_result: None,
        semantic_artifact_result: None,
        worker_model: "gpt-5".to_owned(),
        worker_reasoning: "medium".to_owned(),
        codex_version: "test".to_owned(),
        input_tokens: Some(1),
        cached_input_tokens: Some(0),
        output_tokens: Some(1),
        duration_ms: 1,
        process_status: "success".to_owned(),
        logical_worker_spawns: 1,
        worker_turns: 1,
        repair_performed: false,
        discarded_facts: 0,
        worker_session_id: None,
        session_cleanup_success: Some(true),
        role_profile_provenance: Some(provenance),
    };
    let mismatched = NeedCacheEntry {
        identity: first_identity.clone(),
        result: result.clone(),
        worker_outcome: outcome(second_provenance),
        created_unix_ms: 1,
        hit_count: 0,
    };
    assert!(matches!(store.publish(&mismatched), Err(StoreError::ArtifactIdentity(_))));

    let matching = NeedCacheEntry {
        identity: first_identity.clone(),
        result: result.clone(),
        worker_outcome: outcome(first_provenance),
        created_unix_ms: 1,
        hit_count: 0,
    };
    store.publish(&matching).unwrap();
    assert!(matches!(store.cache_lookup(&first_identity).unwrap(), CacheLookup::Hit(_)));
    assert!(matches!(store.cache_lookup(&second_identity).unwrap(), CacheLookup::Miss));
    let _ = std::fs::remove_file(path);
}

#[test]
fn revising_an_active_profile_preserves_prior_active_audit_pointer() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("explorer.default")).unwrap();
    let id = first.profile_id.clone();
    let state = store.role_profile_state(&id).unwrap();
    let active = store.activate_role_profile(&id, 1, state.state_digest).unwrap();
    let active_state = store.role_profile_state(&id).unwrap();
    let revised = store
        .revise_role_profile(
            &id,
            active_state.state_digest,
            definition_with_model("explorer.default", "gpt-5-mini"),
        )
        .unwrap();
    assert_eq!(revised.revision, 2);
    let audit = store.read_role_profile_audit(&id, 100).unwrap();
    let revise_audit =
        audit.iter().find(|record| record.operation == RoleProfileAuditOperation::Revise).unwrap();
    assert_eq!(revise_audit.prior_active_revision, Some(1));
    assert_eq!(revise_audit.prior_active_digest, Some(active.definition.definition_digest));
    let next_state = store.role_profile_state(&id).unwrap();
    assert_eq!(next_state.active_revision, Some(1));
    assert_eq!(next_state.latest_revision, 2);
    let _ = std::fs::remove_file(path);
}

#[test]
fn stale_state_does_not_mutate_and_trigger_blocks_definition_mutation() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("auditor")).unwrap();
    let id = first.profile_id.clone();
    let before_state = store.role_profile_state(&id).unwrap();
    let before_audit_count = store.read_role_profile_audit(&id, 100).unwrap().len();
    let stale = store.activate_role_profile(&id, 1, Digest::blake3(b"stale"));
    assert!(matches!(stale, Err(StoreError::RoleProfileConflict(_))));
    let after_state = store.role_profile_state(&id).unwrap();
    assert_eq!(before_state.state_generation, after_state.state_generation);
    assert_eq!(before_state.state_digest, after_state.state_digest);
    assert_eq!(before_audit_count, store.read_role_profile_audit(&id, 100).unwrap().len());
    let connection = Connection::open(&path).unwrap();
    assert!(connection
        .execute("UPDATE role_profile_revisions SET definition_json='{}' WHERE profile_id='auditor' AND revision=1", [])
        .is_err());
    drop(connection);
    assert_eq!(store.list_role_profile_revisions(&id).unwrap().len(), 1);
    let _ = std::fs::remove_file(path);
}

#[test]
fn inconsistent_state_pointers_fail_closed_without_historical_fallback() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("auditor")).unwrap();
    let id = first.profile_id.clone();
    let state = store.role_profile_state(&id).unwrap();
    let mut revised = definition("auditor");
    revised.model = "gpt-5-mini".to_owned();
    revised = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        model: revised.model,
        profile_id: revised.profile_id,
        role: revised.role,
        host: revised.host,
        reasoning: revised.reasoning,
        service_tier: revised.service_tier,
        timeout_seconds: revised.timeout_seconds,
        budget: revised.budget,
        prompt_profile_digest: revised.prompt_profile_digest,
        output_contract_digest: revised.output_contract_digest,
        tool_policy: revised.tool_policy,
        command_policy: revised.command_policy,
        filesystem_policy: revised.filesystem_policy,
        network_policy: revised.network_policy,
        test_policy: revised.test_policy,
        repair_policy: revised.repair_policy,
        fallback_policy: revised.fallback_policy,
        concurrency: revised.concurrency,
        route_assignments: revised.route_assignments,
    })
    .unwrap();
    store.revise_role_profile(&id, state.state_digest, revised).unwrap();
    let valid_state = store.role_profile_state(&id).unwrap();

    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute(
            "UPDATE role_profile_state SET latest_revision=1, active_revision=2 WHERE profile_id='auditor'",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(store.role_profile_state(&id), Err(StoreError::RoleProfileCorruption(_))));
    assert!(matches!(
        store.read_active_role_profile(&id),
        Err(StoreError::RoleProfileCorruption(_))
    ));
    assert!(matches!(
        store.activate_role_profile(&id, 2, valid_state.state_digest),
        Err(StoreError::RoleProfileCorruption(_))
    ));

    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute(
            "UPDATE role_profile_state SET latest_revision=2, active_revision=NULL, state_generation=0 WHERE profile_id='auditor'",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(store.role_profile_state(&id), Err(StoreError::RoleProfileCorruption(_))));
    let _ = std::fs::remove_file(path);
}

#[test]
fn current_schema_upgrades_a_valid_v14_database_without_attributing_legacy_rows() {
    let (path, store) = temporary_store();
    let connection = Connection::open(&path).unwrap();
    let migrations = [
        (1, super::super::MIGRATION_V1),
        (2, super::super::MIGRATION_V2),
        (3, super::super::MIGRATION_V3),
        (4, super::super::MIGRATION_V4),
        (5, super::super::MIGRATION_V5),
        (6, super::super::MIGRATION_V6),
        (7, super::super::MIGRATION_V7),
        (8, super::super::MIGRATION_V8),
        (9, super::super::MIGRATION_V9),
        (10, super::super::MIGRATION_V10),
        (11, super::super::MIGRATION_V11),
        (12, super::super::MIGRATION_V12),
        (13, super::super::MIGRATION_V13),
        (14, super::super::MIGRATION_V14),
    ];
    for (version, migration) in migrations {
        connection.execute_batch(migration).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, checksum, applied_unix_ms) VALUES(?1, ?2, 0)",
                rusqlite::params![version, Digest::blake3(migration).to_string()],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO sessions(
                session_id, prompt_profile_digest, route_set_digest, updated_unix_ms
             ) VALUES('legacy-session', ?1, ?2, 0)",
            rusqlite::params![
                Digest::blake3(b"prompt").to_string(),
                Digest::blake3(b"routes").to_string()
            ],
        )
        .unwrap();
    drop(connection);
    store.initialize().unwrap();
    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 17);
    let legacy: (Option<String>, Option<u64>, Option<String>) = connection
        .query_row(
            "SELECT role_profile_id, role_profile_revision,
                    role_profile_definition_digest
             FROM sessions WHERE session_id='legacy-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(legacy, (None, None, None));
    connection
        .execute("UPDATE schema_migrations SET checksum='b3:invalid' WHERE version=15", [])
        .unwrap();
    drop(connection);
    let drifted = RuntimeStore::new(&path);
    assert!(matches!(drifted.initialize(), Err(StoreError::MigrationChecksum)));
    let _ = std::fs::remove_file(path);
}

#[test]
fn corrupt_role_digest_or_active_pointer_fails_closed_without_fallback() {
    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("auditor")).unwrap();
    let id = first.profile_id.clone();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute("UPDATE role_profiles SET role='reviewer' WHERE profile_id='auditor'", [])
        .unwrap();
    drop(connection);
    assert!(matches!(store.role_profile_state(&id), Err(StoreError::RoleProfileCorruption(_))));
    drop(store);
    let _ = std::fs::remove_file(path);

    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("auditor")).unwrap();
    let id = first.profile_id.clone();
    let connection = Connection::open(&path).unwrap();
    connection.execute("DROP TRIGGER role_profile_revisions_immutable", []).unwrap();
    connection
        .execute(
            "UPDATE role_profile_revisions SET definition_digest=?1 WHERE profile_id='auditor' AND revision=1",
            [Digest::blake3(b"corrupt").to_string()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        store.read_role_profile_revision(&id, 1),
        Err(StoreError::RoleProfileCorruption(_))
    ));
    drop(store);
    let _ = std::fs::remove_file(path);

    let (path, store) = temporary_store();
    let first = store.create_role_profile(definition("auditor")).unwrap();
    let id = first.profile_id.clone();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute("UPDATE role_profile_state SET active_revision=99 WHERE profile_id='auditor'", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        store.read_active_role_profile(&id),
        Err(StoreError::RoleProfileCorruption(_))
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn role_profile_persistence_does_not_mutate_settings_or_model_policy() {
    let (path, store) = temporary_store();
    store
        .initialize_defaults(&crate::RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 30,
            evidence_failure_policy: needle_core::EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .unwrap();
    let before_settings = store.settings().unwrap();
    let before_policy = store.model_policy().unwrap();
    store.create_role_profile(definition("explorer.default")).unwrap();
    assert_eq!(store.settings().unwrap(), before_settings);
    assert_eq!(store.model_policy().unwrap(), before_policy);
    let _ = std::fs::remove_file(path);
}

#[test]
fn bounded_state_listing_is_sorted_and_fails_closed_on_corruption() {
    let (path, store) = temporary_store();
    store.create_role_profile(definition("zeta")).unwrap();
    store.create_role_profile(definition("alpha")).unwrap();
    let listed = store.list_role_profile_states(100).unwrap();
    assert_eq!(
        listed.iter().map(|state| state.profile_id.as_str()).collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    assert!(matches!(
        store.list_role_profile_states(101),
        Err(StoreError::RoleProfileValidation(_))
    ));
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute("UPDATE role_profile_state SET latest_revision=99 WHERE profile_id='alpha'", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        store.list_role_profile_states(100),
        Err(StoreError::RoleProfileCorruption(_))
    ));
    let _ = std::fs::remove_file(path);
}
