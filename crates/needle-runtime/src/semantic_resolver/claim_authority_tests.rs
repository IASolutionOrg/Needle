use super::*;
use crate::{
    ClaimAdvisoryResolutionKind, NeedShadowWrite, RuntimeSettings,
    validate_semantic_artifact_with_trace, validate_semantic_test_plan,
};
use needle_core::{
    ArtifactRequest, CacheResolution, CapabilityMode, Digest, EvidenceBrief, EvidenceFailurePolicy,
    FlowStepRole, NeedIr, ReuseUnit, SemanticFlowStep, SemanticLocation, SemanticWorkerArtifact,
    TestPlan, built_in_route_contracts, need_fragment,
};
use std::fs;

#[test]
fn promoted_runtime_flow_claim_reuse_is_authoritative_only_with_fresh_claim_dependencies() {
    let root = std::env::temp_dir().join(format!(
        "needle-runtime-flow-claim-authority-{}-{}",
        std::process::id(),
        Digest::blake3(format!("{:?}", Instant::now())).to_hex()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/flow.rs"), "pub fn answer() { let _ = 1; }\n").unwrap();
    fs::write(root.join("src/unrelated.rs"), "pub fn unrelated() {}\n").unwrap();

    let store = RuntimeStore::new(root.join("needle.sqlite3"));
    store.initialize().unwrap();
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "test".to_owned(),
            worker_reasoning: "low".to_owned(),
            worker_timeout_seconds: 1,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .unwrap();

    let ir = NeedIr::parse(
        "@@need\n\
         @route trace.state-flow\n\
         @subject symbol:\"answer\"\n\
         @require implementation-location granularity=exact-location polarity=positive selection=primary\n\
         @require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
         @world source=current features=default\n\
         \n\
         Trace the runtime flow.\n\
         @@end",
    )
    .unwrap()
    .unwrap();
    let route = built_in_route_contracts()
        .into_iter()
        .find(|route| route.route.as_str() == "trace.state-flow")
        .unwrap();
    let need =
        needle_core::compile_need(&ir, Digest::blake3(b"runtime-flow-repository"), &route).unwrap();
    let fragment = need_fragment(&need, need.required.clone(), Vec::new());
    store
        .record_need_shadow(NeedShadowWrite {
            session_id: "runtime-flow-claim-session",
            turn_id: "runtime-flow-claim-turn",
            transport_digest: Digest::blake3(b"runtime-flow-transport"),
            parser_definition_digest: Digest::blake3(b"parser"),
            prompt_profile_digest: Digest::blake3(b"profile"),
            need_ir: &ir,
            need: &need,
            fragments: std::slice::from_ref(&fragment),
        })
        .unwrap();

    let source_snapshot_digest = Digest::blake3(b"runtime-flow-source");
    let location_request = ArtifactRequest {
        contract_id: "needle.semantic.code-location".to_owned(),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest,
        route_key: route.route.clone(),
        normalized_request: "locate answer".to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    };
    let location_bytes = fs::read(root.join("src/flow.rs")).unwrap();
    let location_artifact = SemanticWorkerArtifact::CodeLocation {
        locations: vec![SemanticLocation {
            role: needle_core::LocationRole::Primary,
            path: "src/flow.rs".to_owned(),
            symbol: Some("answer".to_owned()),
            byte_start: Some(0),
            byte_end: Some(location_bytes.len().try_into().unwrap()),
        }],
        gaps: Vec::new(),
    };
    let validated_location = validate_semantic_artifact_with_trace(
        &fragment,
        &location_artifact,
        &root,
        location_request.semantic_id().digest(),
        Some(&needle_core::WorkerObservationTrace {
            observed_files: vec!["src/unrelated.rs".to_owned()],
            gaps: Vec::new(),
        }),
    )
    .unwrap();
    store
        .publish_semantic_artifact(
            &location_request,
            &need,
            &validated_location.artifact,
            &validated_location.certificate,
        )
        .unwrap();
    store
        .publish_claims_shadow(
            &validated_location.artifact,
            &validated_location.certificate,
            &validated_location.claims.claims,
            &validated_location.claims.origins,
            &validated_location.claims.relations,
            &validated_location.claims.certificates,
        )
        .unwrap();

    let request = ArtifactRequest {
        contract_id: "needle.semantic.behavior-trace".to_owned(),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest,
        route_key: route.route.clone(),
        normalized_request: "trace wording".to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    };
    let behavior = SemanticWorkerArtifact::BehaviorTrace {
        scenario: "default".to_owned(),
        steps: [
            FlowStepRole::Producer,
            FlowStepRole::Carrier,
            FlowStepRole::Transformation,
            FlowStepRole::Precedence,
            FlowStepRole::Consumer,
        ]
        .into_iter()
        .enumerate()
        .map(|(ordinal, role)| SemanticFlowStep {
            role,
            location: SemanticLocation {
                role: needle_core::LocationRole::Supporting,
                path: "src/flow.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: Some(ordinal as u64),
                byte_end: Some(ordinal as u64 + 1),
            },
            description: format!("{role:?} step"),
        })
        .collect(),
        gaps: Vec::new(),
    };
    let validated = validate_semantic_artifact_with_trace(
        &fragment,
        &behavior,
        &root,
        request.semantic_id().digest(),
        Some(&needle_core::WorkerObservationTrace {
            observed_files: vec!["src/unrelated.rs".to_owned()],
            gaps: Vec::new(),
        }),
    )
    .unwrap();
    assert_eq!(validated.artifact.contract.cache_scope, CacheScope::WorktreeSemantic);
    assert!(
        validated
            .artifact
            .dependency_manifest
            .dependencies
            .iter()
            .any(|dependency| { dependency.path == "src/unrelated.rs" })
    );
    store
        .publish_semantic_artifact(&request, &need, &validated.artifact, &validated.certificate)
        .unwrap();
    store
        .publish_claims_shadow(
            &validated.artifact,
            &validated.certificate,
            &validated.claims.claims,
            &validated.claims.origins,
            &validated.claims.relations,
            &validated.claims.certificates,
        )
        .unwrap();

    let resolver = SemanticResolver::new(store.clone());
    let costs = SemanticCostEstimates {
        fresh_microusd: Some(100),
        artifact_reuse_microusd: Some(1),
        claim_reuse_microusd: Some(1),
        claim_partial_reuse_microusd: None,
    };
    let default_shadow = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!default_shadow.authoritative);
    assert!(matches!(default_shadow.resolution, CacheResolution::Bypass { .. }));

    fs::write(root.join("src/unrelated.rs"), "pub fn unrelated_changed() {}\n").unwrap();
    let claim_class = store
        .capability_classes()
        .unwrap()
        .into_iter()
        .find(|class| {
            class.reuse_unit == ReuseUnit::Claim && class.predicate == PredicateKind::RuntimeFlow
        })
        .expect("runtime-flow claim capability");
    assert_eq!(claim_class.mode, CapabilityMode::Shadow);
    let location_claim_class = store
        .capability_classes()
        .unwrap()
        .into_iter()
        .find(|class| {
            class.reuse_unit == ReuseUnit::Claim
                && class.predicate == PredicateKind::ImplementationLocation
        })
        .expect("implementation-location claim capability");
    assert_eq!(location_claim_class.mode, CapabilityMode::Shadow);
    store
        .set_capability_mode(
            &location_claim_class.id,
            location_claim_class.definition_digest,
            CapabilityMode::Authoritative,
            Some(Digest::blake3(b"implementation-claim-offline-evidence")),
        )
        .unwrap();
    assert!(store.capability_classes().unwrap().iter().any(|class| {
        class.reuse_unit == ReuseUnit::Claim
            && class.predicate == PredicateKind::ImplementationLocation
            && class.mode == CapabilityMode::Authoritative
    }));
    let runtime_shadow = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!runtime_shadow.authoritative);
    assert!(matches!(runtime_shadow.resolution, CacheResolution::Stale { .. }));
    assert_eq!(
        runtime_shadow.claim_advisory.as_ref().map(|advisory| advisory.resolution),
        Some(ClaimAdvisoryResolutionKind::ClaimCompositeHit)
    );

    store
        .set_capability_mode(
            &claim_class.id,
            claim_class.definition_digest,
            CapabilityMode::Authoritative,
            Some(Digest::blake3(b"runtime-flow-claim-offline-evidence")),
        )
        .unwrap();
    let authoritative = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(authoritative.authoritative);
    assert!(authoritative.artifacts.is_empty());
    assert!(authoritative.claim_certificate.is_some());
    let claim_material = authoritative.claim_material.as_ref().expect("claim material");
    assert_eq!(claim_material.claims.len(), 6);
    assert!(
        claim_material
            .claims
            .iter()
            .any(|claim| claim.kind == needle_core::ClaimKind::ImplementationLocation)
    );
    assert_eq!(
        claim_material
            .claims
            .iter()
            .filter(|claim| claim.kind == needle_core::ClaimKind::RuntimeFlowStep)
            .count(),
        5
    );
    assert_eq!(
        authoritative.plan.as_ref().and_then(|plan| plan.economics.expected_net_microusd),
        Some(99)
    );
    match &authoritative.resolution {
        CacheResolution::ClaimCompositeHit {
            artifact_ids,
            claim_ids,
            claim_set_certificate_id,
            selected_plan_id,
            ..
        } => {
            assert_eq!(artifact_ids.len(), 2);
            assert_eq!(claim_ids.len(), 6);
            assert_eq!(
                Some(*claim_set_certificate_id),
                authoritative.claim_certificate.as_ref().map(|certificate| certificate.id)
            );
            assert_eq!(Some(*selected_plan_id), authoritative.plan.as_ref().map(|plan| plan.id));
        }
        other => panic!("expected claim-only ClaimHit, got {other:?}"),
    }

    let request =
        needle_core::NeedRequest::parse("@@need:trace.state-flow\nTrace the runtime flow.\n@@end")
            .unwrap()
            .unwrap();
    let projected = crate::semantic_claim_projection::project_claim_brief(
        &request,
        need.world.repository_lineage,
        source_snapshot_digest,
        &root,
        claim_material,
        EvidenceBrief {
            summary: "runtime flow".to_owned(),
            locations: Vec::new(),
            behavior: None,
            test_plan: None,
            claims: Default::default(),
        },
        &authoritative.artifacts,
    )
    .unwrap();
    let brief: EvidenceBrief = serde_json::from_value(projected.payload).unwrap();
    assert_eq!(brief.behavior.as_ref().map(|behavior| behavior.steps.len()), Some(5));
    assert_eq!(brief.locations.len(), 1);
    assert!(brief.behavior.as_ref().is_some_and(|behavior| {
        behavior.steps.iter().all(|step| step.location.path == "src/flow.rs")
    }));

    fs::write(root.join("src/flow.rs"), "pub fn changed_answer() {}\n").unwrap();
    let stale = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!stale.authoritative);
    assert!(matches!(stale.resolution, CacheResolution::Stale { .. }));
    assert!(stale.claim_advisory.is_none());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn promoted_focused_test_claim_reuse_prefers_fresh_narrow_certificate() {
    let root = std::env::temp_dir().join(format!(
        "needle-focused-test-claim-authority-{}-{}",
        std::process::id(),
        Digest::blake3(format!("{:?}", Instant::now())).to_hex()
    ));
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[test]]\nname = \"answer\"\npath = \"tests/answer.rs\"\n",
    )
    .unwrap();
    fs::write(root.join("tests/answer.rs"), "#[test]\nfn answer() {}\n").unwrap();

    let store = RuntimeStore::new(root.join("needle.sqlite3"));
    store.initialize().unwrap();
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "test".to_owned(),
            worker_reasoning: "low".to_owned(),
            worker_timeout_seconds: 1,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .unwrap();

    let ir = NeedIr::parse(
        "@@need\n\
         @route tests.relevant\n\
         @subject symbol:\"answer\"\n\
         @require focused-tests selection=representative completeness=open-world polarity=positive\n\
         @world source=current features=default\n\
         \n\
         Find the relevant focused test.\n\
         @@end",
    )
    .unwrap()
    .unwrap();
    let route = built_in_route_contracts()
        .into_iter()
        .find(|route| route.route.as_str() == "tests.relevant")
        .unwrap();
    let need =
        needle_core::compile_need(&ir, Digest::blake3(b"focused-test-repository"), &route).unwrap();
    let fragment = need_fragment(&need, need.required.clone(), Vec::new());
    store
        .record_need_shadow(NeedShadowWrite {
            session_id: "focused-test-claim-session",
            turn_id: "focused-test-claim-turn",
            transport_digest: Digest::blake3(b"focused-test-transport"),
            parser_definition_digest: Digest::blake3(b"parser"),
            prompt_profile_digest: Digest::blake3(b"profile"),
            need_ir: &ir,
            need: &need,
            fragments: std::slice::from_ref(&fragment),
        })
        .unwrap();

    let source_snapshot_digest = Digest::blake3(b"focused-test-source");
    let argv = vec![
        "cargo".to_owned(),
        "test".to_owned(),
        "--test".to_owned(),
        "answer".to_owned(),
        "answer".to_owned(),
        "--".to_owned(),
        "--exact".to_owned(),
    ];
    let worker_artifact = SemanticWorkerArtifact::TestPlan {
        runner: "cargo".to_owned(),
        argv: argv.clone(),
        cwd_relative: ".".to_owned(),
        identifiers: vec!["answer".to_owned()],
        selection: "representative".to_owned(),
        evidence_paths: vec!["Cargo.toml".to_owned()],
    };
    let request = ArtifactRequest {
        contract_id: "needle.semantic.test-plan".to_owned(),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest,
        route_key: route.route.clone(),
        normalized_request: "focused test wording".to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    };
    let declared_plan = TestPlan {
        runner: "cargo".to_owned(),
        argv: argv.clone(),
        cwd_relative: ".".to_owned(),
        test_identifier: "answer".to_owned(),
        requires_approval: true,
        execution_evidence_id: None,
    };

    let validated_broad = validate_semantic_artifact_with_trace(
        &fragment,
        &worker_artifact,
        &root,
        request.semantic_id().digest(),
        None,
    )
    .unwrap();
    let broad_paths = validated_broad
        .artifact
        .dependency_manifest
        .dependencies
        .iter()
        .map(|dependency| dependency.path.as_str())
        .collect::<Vec<_>>();
    assert!(broad_paths.contains(&"Cargo.toml"));
    assert!(broad_paths.contains(&"tests/answer.rs"));

    let validated_narrow = validate_semantic_test_plan(
        &fragment,
        &worker_artifact,
        &root,
        request.semantic_id().digest(),
        None,
        &declared_plan,
    )
    .unwrap();
    assert_eq!(validated_broad.artifact.id, validated_narrow.artifact.id);
    assert_ne!(validated_broad.certificate.id, validated_narrow.certificate.id);
    assert_eq!(
        validated_narrow
            .artifact
            .dependency_manifest
            .dependencies
            .iter()
            .map(|dependency| dependency.path.as_str())
            .collect::<Vec<_>>(),
        vec!["Cargo.toml"]
    );
    assert_eq!(validated_broad.claims.claims, validated_narrow.claims.claims);
    assert_eq!(validated_broad.claims.certificates.len(), 1);
    assert_eq!(validated_narrow.claims.certificates.len(), 1);
    let broad_claim_certificate_id = validated_broad.claims.certificates[0].id;
    let narrow_claim_certificate = &validated_narrow.claims.certificates[0];
    assert_ne!(broad_claim_certificate_id, narrow_claim_certificate.id);
    assert_eq!(
        narrow_claim_certificate
            .dependencies
            .iter()
            .map(|dependency| dependency.path.as_str())
            .collect::<Vec<_>>(),
        vec!["Cargo.toml"]
    );

    store
        .publish_semantic_artifact(
            &request,
            &need,
            &validated_broad.artifact,
            &validated_broad.certificate,
        )
        .unwrap();
    store
        .publish_claims_shadow(
            &validated_broad.artifact,
            &validated_broad.certificate,
            &validated_broad.claims.claims,
            &validated_broad.claims.origins,
            &validated_broad.claims.relations,
            &validated_broad.claims.certificates,
        )
        .unwrap();
    store
        .publish_semantic_artifact(
            &request,
            &need,
            &validated_narrow.artifact,
            &validated_narrow.certificate,
        )
        .unwrap();
    store
        .publish_claims_shadow(
            &validated_narrow.artifact,
            &validated_narrow.certificate,
            &validated_narrow.claims.claims,
            &validated_narrow.claims.origins,
            &validated_narrow.claims.relations,
            &validated_narrow.claims.certificates,
        )
        .unwrap();
    let persisted = store
        .semantic_artifact(&validated_broad.artifact.id.to_string())
        .unwrap()
        .expect("persisted first artifact");
    assert_eq!(persisted.dependency_manifest, validated_broad.artifact.dependency_manifest);

    let resolver = SemanticResolver::new(store.clone());
    let costs = SemanticCostEstimates {
        fresh_microusd: Some(100),
        artifact_reuse_microusd: Some(1),
        claim_reuse_microusd: Some(1),
        claim_partial_reuse_microusd: None,
    };
    let default_shadow = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!default_shadow.authoritative);
    assert!(matches!(default_shadow.resolution, CacheResolution::Bypass { .. }));

    fs::write(root.join("tests/answer.rs"), "#[test]\nfn answer() { assert!(true); }\n").unwrap();
    assert!(!artifact_and_certificate_are_fresh(
        &validated_broad.artifact,
        &validated_broad.certificate,
        &root
    ));
    assert!(artifact_and_certificate_are_fresh(
        &validated_narrow.artifact,
        &validated_narrow.certificate,
        &root
    ));
    let shadow_claim = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!shadow_claim.authoritative);
    assert!(matches!(shadow_claim.resolution, CacheResolution::Stale { .. }));
    assert_eq!(
        shadow_claim.claim_advisory.as_ref().map(|advisory| advisory.resolution),
        Some(ClaimAdvisoryResolutionKind::ClaimHit)
    );

    let claim_class = store
        .capability_classes()
        .unwrap()
        .into_iter()
        .find(|class| {
            class.reuse_unit == ReuseUnit::Claim && class.predicate == PredicateKind::FocusedTests
        })
        .expect("focused-test claim capability");
    assert_eq!(claim_class.mode, CapabilityMode::Shadow);
    store
        .set_capability_mode(
            &claim_class.id,
            claim_class.definition_digest,
            CapabilityMode::Authoritative,
            Some(Digest::blake3(b"focused-test-claim-offline-evidence")),
        )
        .unwrap();
    let authoritative = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(authoritative.authoritative);
    assert!(authoritative.artifacts.is_empty());
    let claim_material = authoritative.claim_material.as_ref().expect("claim material");
    assert_eq!(claim_material.claims.len(), 1);
    let selected_claim_certificate = authoritative
        .claim_certificate
        .as_ref()
        .expect("claim-set certificate")
        .validation_certificates
        .first()
        .copied()
        .expect("selected claim certificate");
    assert_eq!(selected_claim_certificate, narrow_claim_certificate.id);
    assert_ne!(selected_claim_certificate, broad_claim_certificate_id);
    let selected_certificate = claim_material
        .certificates
        .iter()
        .find(|certificate| certificate.id == selected_claim_certificate)
        .expect("selected narrow claim certificate");
    assert_eq!(
        selected_certificate
            .dependencies
            .iter()
            .map(|dependency| dependency.path.as_str())
            .collect::<Vec<_>>(),
        vec!["Cargo.toml"]
    );
    assert_eq!(
        authoritative.plan.as_ref().and_then(|plan| plan.economics.expected_net_microusd),
        Some(99)
    );
    match &authoritative.resolution {
        CacheResolution::ClaimHit {
            artifact_ids,
            claim_ids,
            claim_set_certificate_id,
            selected_plan_id,
            ..
        } => {
            assert_eq!(artifact_ids, &vec![validated_broad.artifact.id]);
            assert_eq!(claim_ids, &vec![validated_narrow.claims.claims[0].id]);
            assert_eq!(
                Some(*claim_set_certificate_id),
                authoritative.claim_certificate.as_ref().map(|certificate| certificate.id)
            );
            assert_eq!(Some(*selected_plan_id), authoritative.plan.as_ref().map(|plan| plan.id));
        }
        other => panic!("expected claim-only ClaimHit, got {other:?}"),
    }

    let request = needle_core::NeedRequest::parse(
        "@@need:tests.relevant\nFind the relevant focused test.\n@@end",
    )
    .unwrap()
    .unwrap();
    let projected = crate::semantic_claim_projection::project_claim_brief(
        &request,
        need.world.repository_lineage,
        source_snapshot_digest,
        &root,
        claim_material,
        EvidenceBrief {
            summary: "focused test".to_owned(),
            locations: Vec::new(),
            behavior: None,
            test_plan: None,
            claims: Default::default(),
        },
        &authoritative.artifacts,
    )
    .unwrap();
    let brief: EvidenceBrief = serde_json::from_value(projected.payload).unwrap();
    assert_eq!(brief.test_plan, Some(declared_plan));
    assert!(brief.claims.contains_key("answer"));

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.1\"\nedition = \"2021\"\n\n[[test]]\nname = \"answer\"\npath = \"tests/answer.rs\"\n",
    )
    .unwrap();
    let stale = resolver
        .resolve_for_route(&need, &route.route, &root, source_snapshot_digest, costs, &[])
        .unwrap();
    assert!(!stale.authoritative);
    assert!(matches!(stale.resolution, CacheResolution::Stale { .. }));
    assert!(stale.claim_advisory.is_none());
    let _ = fs::remove_dir_all(root);
}
