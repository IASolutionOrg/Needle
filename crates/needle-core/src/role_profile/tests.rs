use super::*;

fn input() -> RoleProfileDefinitionInput {
    RoleProfileDefinitionInput {
        profile_id: RoleProfileId::new("explorer.default").unwrap(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "gpt-5".to_owned(),
        reasoning: ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 120,
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
        repair_policy: RepairPolicy::None,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: vec![
            NeedKey::new("tests.relevant").unwrap(),
            NeedKey::new("locate.implementation").unwrap(),
            NeedKey::new("tests.relevant").unwrap(),
        ],
    }
}

#[test]
fn canonicalizes_routes_and_has_stable_digest() {
    let definition = RoleProfileDefinition::new(input()).unwrap();
    assert_eq!(
        definition.route_assignments,
        vec![
            NeedKey::new("locate.implementation").unwrap(),
            NeedKey::new("tests.relevant").unwrap()
        ]
    );
    assert!(definition.is_canonical());
    assert_eq!(
        definition.definition_digest.to_string(),
        "b3:e53953c09f87b0a76a0b453a360f878749cf2d0702ee0c6f3bd3dfc879b2191a"
    );
    assert_eq!(
        definition.canonical_json().unwrap(),
        "{\"profile_id\":\"explorer.default\",\"role\":\"explorer\",\"host\":\"codex\",\"model\":\"gpt-5\",\"reasoning\":\"medium\",\"service_tier\":\"default\",\"timeout_seconds\":120,\"budget\":{\"max_turns\":2,\"max_output_tokens\":1200,\"max_cost_microusd\":1000},\"prompt_profile_digest\":\"b3:66fabd4d998134319768c6498bd0b7ddc47a979b9d9c0b611855b128f307a694\",\"output_contract_digest\":\"b3:96e754fa8b2ab9d1f3f8f2d2bb4ec79a18016b212cf44f8fd2f7441e6e97a48b\",\"tool_policy\":\"read_only\",\"command_policy\":\"read_only\",\"filesystem_policy\":\"read_only_checkout\",\"network_policy\":\"denied\",\"test_policy\":\"disabled\",\"repair_policy\":\"none\",\"fallback_policy\":\"native\",\"concurrency\":1,\"route_assignments\":[\"locate.implementation\",\"tests.relevant\"]}"
    );
}

#[test]
fn policy_and_model_changes_change_digest_but_metadata_does_not() {
    let definition = RoleProfileDefinition::new(input()).unwrap();
    let mut changed = input();
    changed.command_policy = CommandPolicy::Denied;
    let changed = RoleProfileDefinition::new(changed).unwrap();
    assert_ne!(definition.definition_digest, changed.definition_digest);
    let revision = RoleProfileRevision {
        profile_id: definition.profile_id.clone(),
        revision: 1,
        definition: definition.clone(),
        state: RoleProfileState::Draft,
        created_unix_ms: 10,
        activated_unix_ms: None,
    };
    let mut metadata = revision.clone();
    metadata.revision = 2;
    metadata.created_unix_ms = 20;
    assert_eq!(metadata.definition.definition_digest, revision.definition.definition_digest);
}

#[test]
fn forged_digest_and_unknown_or_credential_fields_fail() {
    let mut definition = RoleProfileDefinition::new(input()).unwrap();
    definition.definition_digest = Digest::blake3(b"forged");
    assert!(matches!(
        definition.validate(),
        Err(RoleProfileValidationError::DigestMismatch { .. })
    ));
    assert!(serde_json::from_str::<RoleProfileDefinitionInput>(r#"{"profile_id":"x","role":"explorer","host":"codex","model":"gpt-5","reasoning":"medium","service_tier":"default","timeout_seconds":1,"budget":{"max_turns":1,"max_output_tokens":1,"max_cost_microusd":1},"prompt_profile_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000","output_contract_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000","tool_policy":"read_only","command_policy":"read_only","filesystem_policy":"read_only_checkout","network_policy":"denied","test_policy":"disabled","repair_policy":"none","fallback_policy":"native","concurrency":1,"route_assignments":[],"path":"x"}"#).is_err());
    let credential_values = [
        "ghp_secret",
        "gho_secret",
        "ghu_secret",
        "ghs_secret",
        "ghr_secret",
        "github_pat_secret",
        "glpat-secret",
        "sk-secret",
        "sk_secret",
        "rk_secret",
        "xoxa-secret",
        "xoxb-secret",
        "xoxp-secret",
        "xoxr-secret",
        "xoxs-secret",
        "AKIAIOSFODNN7EXAMPLE",
        "ASIAIOSFODNN7EXAMPLE",
    ];
    for value in credential_values {
        assert!(RoleProfileId::new(value).is_err(), "id {value}");
        let mut invalid = input();
        invalid.model = value.to_owned();
        assert!(RoleProfileDefinition::new(invalid).is_err(), "model {value}");
    }
    let google_key = format!("AIza{}", "A".repeat(35));
    let mut invalid = input();
    invalid.model = google_key.clone();
    assert!(RoleProfileDefinition::new(invalid).is_err(), "model {google_key}");
    for value in ["ghp_secret", "glpat-secret", "sk-secret", "rk_secret", "xoxb-secret"] {
        let mut credential_route = input();
        credential_route.route_assignments = vec![NeedKey::new(value).unwrap()];
        assert!(RoleProfileDefinition::new(credential_route).is_err(), "route {value}");
    }
    assert!(credential_prefix("Bearer token"));
    assert!(credential_prefix("basic token"));
    assert!(credential_prefix(&google_key));
    assert!(!credential_prefix("aizawa"));
}

#[test]
fn compatibility_projection_preserves_existing_worker_digest_semantics() {
    let mut value = input();
    value.service_tier = ServiceTier::Priority;
    let definition = RoleProfileDefinition::new(value).unwrap();
    let worker = definition.to_worker_profile().unwrap();
    assert_eq!(worker.platform, "codex");
    assert_eq!(worker.service_tier.as_deref(), Some("priority"));
    assert_eq!(
        worker.definition_digest,
        WorkerProfile::new("codex", "gpt-5", "medium", Some("priority".to_owned()))
            .definition_digest
    );
    let default = RoleProfileDefinition::new(input()).unwrap().to_worker_profile().unwrap();
    assert_eq!(default.service_tier, None);
}

#[test]
fn reordered_input_json_has_the_same_canonical_identity() {
    let original = input();
    let prompt = original.prompt_profile_digest.to_string();
    let output = original.output_contract_digest.to_string();
    let reordered = format!(
        "{{\"route_assignments\":[\"tests.relevant\",\"locate.implementation\",\"tests.relevant\"],\"concurrency\":1,\"fallback_policy\":\"native\",\"repair_policy\":\"none\",\"test_policy\":\"disabled\",\"network_policy\":\"denied\",\"filesystem_policy\":\"read_only_checkout\",\"command_policy\":\"read_only\",\"tool_policy\":\"read_only\",\"output_contract_digest\":\"{output}\",\"prompt_profile_digest\":\"{prompt}\",\"budget\":{{\"max_cost_microusd\":1000,\"max_output_tokens\":1200,\"max_turns\":2}},\"timeout_seconds\":120,\"service_tier\":\"default\",\"reasoning\":\"medium\",\"model\":\"gpt-5\",\"host\":\"codex\",\"role\":\"explorer\",\"profile_id\":\"explorer.default\"}}"
    );
    let parsed: RoleProfileDefinitionInput = serde_json::from_str(&reordered).unwrap();
    let one = RoleProfileDefinition::new(original).unwrap();
    let two = RoleProfileDefinition::new(parsed).unwrap();
    assert_eq!(one.definition_digest, two.definition_digest);
    assert_eq!(one.canonical_json().unwrap(), two.canonical_json().unwrap());
}

#[test]
fn validation_bounds_and_unsafe_combinations_fail_closed() {
    let mut invalid_model = input();
    invalid_model.model = "model/name".to_owned();
    assert!(RoleProfileDefinition::new(invalid_model).is_err());
    for field in ["reasoning", "service_tier", "host"] {
        let value = match field {
            "reasoning" => {
                serde_json::to_string(&input()).unwrap().replace("\"medium\"", "\"unsupported\"")
            }
            "service_tier" => {
                serde_json::to_string(&input()).unwrap().replace("\"default\"", "\"unsupported\"")
            }
            _ => serde_json::to_string(&input()).unwrap().replace("\"codex\"", "\"other\""),
        };
        assert!(serde_json::from_str::<RoleProfileDefinitionInput>(&value).is_err(), "{field}");
    }
    let mut timeout = input();
    timeout.timeout_seconds = 0;
    assert!(RoleProfileDefinition::new(timeout).is_err());
    let mut turns = input();
    turns.budget.max_turns = ROLE_PROFILE_MAX_TURNS + 1;
    assert!(RoleProfileDefinition::new(turns).is_err());
    let mut tokens = input();
    tokens.budget.max_output_tokens = HARD_RESULT_TOKENS as u32 + 1;
    assert!(RoleProfileDefinition::new(tokens).is_err());
    let mut cost = input();
    cost.budget.max_cost_microusd = ROLE_PROFILE_MAX_COST_MICROUSD + 1;
    assert!(RoleProfileDefinition::new(cost).is_err());
    let mut concurrency = input();
    concurrency.concurrency = 2;
    assert!(RoleProfileDefinition::new(concurrency).is_err());
    let mut routes = input();
    routes.route_assignments = (0..=HARD_MAX_NEEDS_PER_TASK)
        .map(|index| NeedKey::new(format!("route-{index}")).unwrap())
        .collect();
    assert!(RoleProfileDefinition::new(routes).is_err());
    let mut duplicate_routes = input();
    duplicate_routes.route_assignments =
        (0..=HARD_MAX_NEEDS_PER_TASK).map(|_| NeedKey::new("tests.relevant").unwrap()).collect();
    assert!(matches!(
        RoleProfileDefinition::new(duplicate_routes),
        Err(RoleProfileValidationError::Routes)
    ));
    let mut isolated = input();
    isolated.tool_policy = ToolPolicy::IsolatedWrite;
    assert!(RoleProfileDefinition::new(isolated).is_err());
    let mut disposable = input();
    disposable.filesystem_policy = FilesystemPolicy::DisposableCheckout;
    assert!(RoleProfileDefinition::new(disposable).is_err());
    let mut certified = input();
    certified.command_policy = CommandPolicy::CertifiedTests;
    assert!(RoleProfileDefinition::new(certified).is_err());
    let mut certified_test = input();
    certified_test.test_policy = TestPolicy::Certified;
    assert!(RoleProfileDefinition::new(certified_test).is_err());

    let mut implementer = input();
    implementer.role = CodexRole::Implementer;
    implementer.tool_policy = ToolPolicy::IsolatedWrite;
    implementer.filesystem_policy = FilesystemPolicy::DisposableCheckout;
    assert!(RoleProfileDefinition::new(implementer).is_ok());
    let mut test_runner = input();
    test_runner.role = CodexRole::TestRunner;
    test_runner.command_policy = CommandPolicy::CertifiedTests;
    test_runner.test_policy = TestPolicy::Certified;
    assert!(RoleProfileDefinition::new(test_runner).is_ok());
}

#[test]
fn every_representable_semantic_change_changes_the_digest() {
    let base = RoleProfileDefinition::new(input()).unwrap();
    let mut changes = Vec::new();
    let mut value = input();
    value.model = "gpt-5-mini".to_owned();
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.reasoning = ReasoningLevel::High;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.service_tier = ServiceTier::Priority;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.timeout_seconds += 1;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.budget.max_turns += 1;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.budget.max_output_tokens += 1;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.budget.max_cost_microusd += 1;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.prompt_profile_digest = Digest::blake3(b"other-prompt");
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.output_contract_digest = Digest::blake3(b"other-output");
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.command_policy = CommandPolicy::Denied;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.repair_policy = RepairPolicy::Once;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.fallback_policy = FallbackPolicy::Disabled;
    changes.push(RoleProfileDefinition::new(value).unwrap());
    let mut value = input();
    value.route_assignments = vec![NeedKey::new("trace.state-flow").unwrap()];
    changes.push(RoleProfileDefinition::new(value).unwrap());
    for changed in changes {
        assert_ne!(base.definition_digest, changed.definition_digest);
    }

    let mut writable = input();
    writable.role = CodexRole::Implementer;
    writable.tool_policy = ToolPolicy::IsolatedWrite;
    writable.filesystem_policy = FilesystemPolicy::DisposableCheckout;
    let writable = RoleProfileDefinition::new(writable).unwrap();
    assert_ne!(base.definition_digest, writable.definition_digest);
    let mut certified = input();
    certified.role = CodexRole::TestRunner;
    certified.command_policy = CommandPolicy::CertifiedTests;
    certified.test_policy = TestPolicy::Certified;
    let certified = RoleProfileDefinition::new(certified).unwrap();
    assert_ne!(base.definition_digest, certified.definition_digest);
}
