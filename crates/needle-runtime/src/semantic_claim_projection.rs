use crate::store::now_ms;
use crate::{ClaimProofMaterial, RuntimeError, StoreError};
use needle_core::{
    Artifact, ArtifactContract, ArtifactKind, ArtifactRequest, BehaviorStep, BehaviorTrace,
    CacheScope, ClaimId, ClaimPayload, ClaimRelationKind, CodeLocation, Dependency,
    DependencyManifest, Digest, EvidenceBrief, NeedRequest, TestPlan, ValidationRecord,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub(crate) fn project_claim_brief(
    need: &NeedRequest,
    repository_id: Digest,
    source_snapshot_digest: Digest,
    repository_root: &Path,
    material: &ClaimProofMaterial,
    mut brief: EvidenceBrief,
    artifact_inputs: &[Artifact],
) -> Result<Artifact, RuntimeError> {
    if material.claims.is_empty()
        || material.claims.len() > needle_core::MAX_SELECTED_CLAIMS
        || material.certificates.is_empty()
    {
        return Err(protocol("authoritative claim projection has no bounded claim material"));
    }

    let mut claim_dependencies = Vec::new();
    let mut flow_claims = Vec::new();
    let mut test_plan = None;
    for claim in &material.claims {
        let certificates = material
            .certificates
            .iter()
            .filter(|certificate| certificate.claim == claim.id)
            .collect::<Vec<_>>();
        if certificates.len() != 1 {
            return Err(protocol("authoritative claim projection is not certificate-exact"));
        }
        let certificate = certificates[0];
        for dependency in &certificate.dependencies {
            let mut dependency = dependency.clone();
            dependency.claims = vec![claim.id.to_string()];
            claim_dependencies.push(dependency);
        }
        match &claim.payload {
            ClaimPayload::ImplementationLocation { location } => {
                if location.role != needle_core::LocationRole::Primary {
                    return Err(protocol("authoritative implementation claim is not primary"));
                }
                let projected = project_location(repository_root, location)?;
                let key = projected.symbol.clone().unwrap_or_else(|| projected.path.clone());
                brief
                    .claims
                    .entry(key)
                    .or_default()
                    .push("primary implementation location".to_owned());
                brief.locations.push(projected);
            }
            ClaimPayload::RuntimeFlowStep { scenario, flow_anchor, step } => {
                flow_claims.push((claim.id, scenario.as_str(), *flow_anchor, step));
            }
            ClaimPayload::FocusedTest {
                runner, argv, cwd_relative, identifier, selection, ..
            } => {
                let projected = TestPlan {
                    runner: runner.clone(),
                    argv: argv.clone(),
                    cwd_relative: cwd_relative.clone(),
                    test_identifier: identifier.clone(),
                    requires_approval: true,
                    execution_evidence_id: None,
                };
                projected.test_command().map_err(|_| {
                    protocol("authoritative focused-test claim is not a canonical safe command")
                })?;
                if test_plan.as_ref().is_some_and(|existing| existing != &projected) {
                    return Err(protocol("authoritative claim projection has conflicting tests"));
                }
                test_plan = Some(projected);
                brief
                    .claims
                    .entry(identifier.clone())
                    .or_default()
                    .push(format!("{selection} focused test"));
            }
        }
    }

    if !flow_claims.is_empty() {
        if brief.behavior.is_some() {
            return Err(protocol("authoritative claim projection duplicates runtime flow"));
        }
        let ordered = order_flow_claims(&flow_claims, material)?;
        let mut steps = Vec::with_capacity(ordered.len());
        for (ordinal, (_, _, _, step)) in ordered.into_iter().enumerate() {
            let location = project_location(repository_root, &step.location)?;
            let key = location.symbol.clone().unwrap_or_else(|| location.path.clone());
            brief
                .claims
                .entry(key)
                .or_default()
                .push(format!("{:?}: {}", step.role, step.description));
            steps.push(BehaviorStep {
                ordinal: ordinal.try_into().unwrap_or(u32::MAX),
                location,
                description: step.description.clone(),
            });
        }
        let entrypoint =
            steps[0].location.symbol.clone().unwrap_or_else(|| steps[0].location.path.clone());
        brief.behavior = Some(BehaviorTrace { entrypoint, steps, uncertainty: Vec::new() });
    }
    if let Some(projected) = test_plan {
        if brief.test_plan.as_ref().is_some_and(|existing| existing != &projected) {
            return Err(protocol("authoritative claim projection duplicates focused tests"));
        }
        brief.test_plan = Some(projected);
    }

    brief.locations.sort_by(|left, right| {
        (&left.path, &left.symbol, left.byte_start, left.byte_end).cmp(&(
            &right.path,
            &right.symbol,
            right.byte_start,
            right.byte_end,
        ))
    });
    brief.locations.dedup_by(|left, right| {
        left.path == right.path
            && left.symbol == right.symbol
            && left.byte_start == right.byte_start
            && left.byte_end == right.byte_end
    });
    for facts in brief.claims.values_mut() {
        facts.sort();
        facts.dedup();
    }
    let fact_count = brief.claims.values().map(Vec::len).sum::<usize>();
    brief.summary =
        format!("{fact_count} proof-certified claim facts satisfy the declared obligations.");

    let payload = serde_json::to_value(&brief).map_err(StoreError::from)?;
    let contract = ArtifactContract::semantic(
        "needle.semantic.evidence-brief",
        2,
        ArtifactKind::evidence_brief(),
        CacheScope::WorktreeSemantic,
    );
    let request = ArtifactRequest {
        contract_id: "needle.evidence-brief".to_owned(),
        contract_revision: 1,
        repository_id,
        source_snapshot_digest,
        route_key: need.key.clone(),
        normalized_request: need.body.clone(),
        semantic_fragment_id: None,
        input_artifact_ids: Vec::new(),
    };
    let id = Artifact::compute_content_id(&contract, &payload).map_err(StoreError::from)?.digest();
    let created = now_ms();
    Ok(Artifact {
        id,
        request_id: request.id(),
        contract,
        payload,
        dependency_manifest: projection_manifest(artifact_inputs, claim_dependencies),
        validations: vec![ValidationRecord {
            validator: "needle.semantic-claim-projection".to_owned(),
            validator_revision: 2,
            status: "passed".to_owned(),
            evidence_digest: id,
            validated_unix_ms: created,
        }],
        created_unix_ms: created,
    })
}

fn project_location(
    repository_root: &Path,
    location: &needle_core::SemanticLocation,
) -> Result<CodeLocation, RuntimeError> {
    let bytes = fs::read(repository_root.join(&location.path)).map_err(|error| {
        protocol(&format!("cannot project authoritative claim {}: {error}", location.path))
    })?;
    Ok(CodeLocation {
        path: location.path.clone(),
        symbol: location.symbol.clone(),
        byte_start: location.byte_start,
        byte_end: location.byte_end,
        content_digest: Digest::blake3(bytes),
    })
}

type FlowProjection<'a> = (ClaimId, &'a str, Digest, &'a needle_core::SemanticFlowStep);

fn order_flow_claims<'a>(
    claims: &[FlowProjection<'a>],
    material: &ClaimProofMaterial,
) -> Result<Vec<FlowProjection<'a>>, RuntimeError> {
    let scenario = claims[0].1;
    let anchor = claims[0].2;
    if claims.iter().any(|(_, candidate_scenario, candidate_anchor, _)| {
        *candidate_scenario != scenario || *candidate_anchor != anchor
    }) {
        return Err(protocol("authoritative runtime-flow claims do not share one flow anchor"));
    }
    if claims.len() == 1 {
        return Ok(vec![claims[0]]);
    }
    let ids = claims.iter().map(|(id, _, _, _)| *id).collect::<BTreeSet<_>>();
    let relations = material
        .relations
        .iter()
        .filter(|relation| {
            relation.kind == ClaimRelationKind::Precedes
                && ids.contains(&relation.from)
                && ids.contains(&relation.to)
        })
        .collect::<Vec<_>>();
    if relations.len() + 1 != claims.len() {
        return Err(protocol("authoritative runtime-flow claim chain is incomplete"));
    }
    let targets = relations.iter().map(|relation| relation.to).collect::<BTreeSet<_>>();
    let starts = ids.iter().filter(|id| !targets.contains(id)).copied().collect::<Vec<_>>();
    if starts.len() != 1 {
        return Err(protocol("authoritative runtime-flow claim chain has no unique start"));
    }
    let mut ordered = Vec::with_capacity(claims.len());
    let mut current = starts[0];
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Err(protocol("authoritative runtime-flow claim chain is cyclic"));
        }
        let claim = claims
            .iter()
            .find(|(id, _, _, _)| *id == current)
            .copied()
            .ok_or_else(|| protocol("authoritative runtime-flow claim is missing"))?;
        ordered.push(claim);
        let next = relations
            .iter()
            .filter(|relation| relation.from == current)
            .map(|relation| relation.to)
            .collect::<Vec<_>>();
        match next.as_slice() {
            [] => break,
            [next] => current = *next,
            _ => return Err(protocol("authoritative runtime-flow claim chain branches")),
        }
    }
    if ordered.len() != claims.len() {
        return Err(protocol("authoritative runtime-flow claim chain is disconnected"));
    }
    Ok(ordered)
}

fn projection_manifest(
    artifact_inputs: &[Artifact],
    claim_dependencies: Vec<Dependency>,
) -> DependencyManifest {
    let mut dependencies = artifact_inputs
        .iter()
        .flat_map(|artifact| artifact.dependency_manifest.dependencies.iter().cloned())
        .chain(claim_dependencies)
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| {
        (&left.path, left.content_digest, left.byte_start, left.byte_end).cmp(&(
            &right.path,
            right.content_digest,
            right.byte_start,
            right.byte_end,
        ))
    });
    let mut merged = Vec::<Dependency>::new();
    for dependency in dependencies {
        if let Some(previous) = merged.last_mut()
            && previous.path == dependency.path
            && previous.content_digest == dependency.content_digest
            && previous.byte_start == dependency.byte_start
            && previous.byte_end == dependency.byte_end
        {
            previous.claims.extend(dependency.claims);
            previous.claims.sort();
            previous.claims.dedup();
        } else {
            merged.push(dependency);
        }
    }
    let scope = if artifact_inputs
        .iter()
        .any(|artifact| artifact.dependency_manifest.scope == CacheScope::SnapshotExact)
    {
        CacheScope::SnapshotExact
    } else {
        CacheScope::WorktreeSemantic
    };
    DependencyManifest {
        scope,
        observed_files_complete: true,
        dependencies: merged,
        gaps: Vec::new(),
    }
}

fn protocol(message: &str) -> RuntimeError {
    RuntimeError::ArtifactProtocol(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::claim::Claim;
    use needle_core::{
        ArtifactId, ArtifactValidationCertificateId, ClaimRelation, ClaimValidationCertificate,
        Facet, FlowStepRole, LocationRole, NeedKey, Obligation, PredicateKind, SemanticFlowStep,
        SemanticLocation, Subject, SubjectKind,
    };
    use std::collections::BTreeMap;

    fn certificate_for(
        claim: &Claim,
        obligation: &Obligation,
        subject: &Subject,
        world: Digest,
        path: &str,
        content_digest: Digest,
        ordinal: u8,
    ) -> ClaimValidationCertificate {
        ClaimValidationCertificate::new(
            claim.id,
            ArtifactId(Digest::blake3(format!("origin-artifact-{ordinal}"))),
            ArtifactValidationCertificateId(Digest::blake3(format!(
                "origin-certificate-{ordinal}"
            ))),
            subject.id,
            world,
            Digest::blake3(b"projection-validator"),
            vec![Dependency {
                path: path.to_owned(),
                content_digest,
                byte_start: None,
                byte_end: None,
                claims: Vec::new(),
            }],
            vec![obligation.clone()],
            u64::from(ordinal),
        )
        .expect("projection test certificate is canonical")
    }

    fn brief() -> EvidenceBrief {
        EvidenceBrief {
            summary: "seed".to_owned(),
            locations: Vec::new(),
            behavior: None,
            test_plan: None,
            claims: BTreeMap::new(),
        }
    }

    #[test]
    fn projects_runtime_flow_claims_in_precedes_order_and_rejects_disconnected_chain() {
        let root = std::env::temp_dir().join(format!(
            "needle-claim-projection-flow-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let path = "src/flow.rs";
        fs::write(root.join(path), "pub fn answer() { let _ = 1; }\n").unwrap();
        let content_digest = Digest::blake3(fs::read(root.join(path)).unwrap());
        let repository_id = Digest::blake3(b"projection-flow-repository");
        let subject = Subject::exact(repository_id, SubjectKind::Symbol, "answer");
        let obligation = Obligation::new(
            PredicateKind::RuntimeFlow,
            subject.id,
            vec![
                Facet { key: "completeness".to_owned(), value: "contract-complete".to_owned() },
                Facet { key: "granularity".to_owned(), value: "stepwise".to_owned() },
                Facet { key: "scenario".to_owned(), value: "default".to_owned() },
            ],
        );
        let anchor = Digest::blake3(b"projection-flow-anchor");
        let roles = [
            FlowStepRole::Producer,
            FlowStepRole::Carrier,
            FlowStepRole::Transformation,
            FlowStepRole::Precedence,
            FlowStepRole::Consumer,
        ];
        let contract = needle_core::built_in_predicate_contracts()
            .into_iter()
            .find(|contract| contract.predicate == PredicateKind::RuntimeFlow)
            .expect("runtime-flow contract")
            .definition_digest;
        let mut claims = Vec::new();
        let mut certificates = Vec::new();
        for (ordinal, role) in roles.iter().enumerate() {
            let step = SemanticFlowStep {
                role: *role,
                location: SemanticLocation {
                    role: LocationRole::Supporting,
                    path: path.to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(ordinal as u64),
                    byte_end: Some(ordinal as u64 + 1),
                },
                description: format!("{ordinal}: {role:?}"),
            };
            let claim = Claim::new(
                contract,
                ClaimPayload::RuntimeFlowStep {
                    scenario: "default".to_owned(),
                    flow_anchor: anchor,
                    step,
                },
            )
            .expect("runtime-flow claim is canonical");
            certificates.push(certificate_for(
                &claim,
                &obligation,
                &subject,
                Digest::blake3(b"projection-flow-world"),
                path,
                content_digest,
                ordinal as u8,
            ));
            claims.push(claim);
        }
        let relations = claims
            .windows(2)
            .map(|pair| ClaimRelation::new(pair[0].id, pair[1].id, ClaimRelationKind::Precedes))
            .collect::<Vec<_>>();
        let mut reversed_claims = claims.clone();
        reversed_claims.reverse();
        let mut reversed_certificates = certificates.clone();
        reversed_certificates.reverse();
        let material = ClaimProofMaterial {
            claims: reversed_claims,
            relations,
            certificates: reversed_certificates,
        };
        let need = NeedRequest {
            key: NeedKey::new("trace.state-flow").unwrap(),
            body: "Trace the runtime flow.".to_owned(),
        };
        let projected = project_claim_brief(
            &need,
            repository_id,
            Digest::blake3(b"projection-flow-source"),
            &root,
            &material,
            brief(),
            &[],
        )
        .unwrap();
        let projected_brief: EvidenceBrief = serde_json::from_value(projected.payload).unwrap();
        let behavior = projected_brief.behavior.expect("runtime flow is projected");
        assert_eq!(behavior.steps.len(), roles.len());
        assert_eq!(
            behavior.steps.iter().map(|step| step.description.as_str()).collect::<Vec<_>>(),
            vec!["0: Producer", "1: Carrier", "2: Transformation", "3: Precedence", "4: Consumer"]
        );
        assert_eq!(projected.dependency_manifest.dependencies.len(), 1);
        assert_eq!(projected.dependency_manifest.dependencies[0].path, path);
        assert_eq!(projected.dependency_manifest.dependencies[0].claims.len(), roles.len());

        let malformed = ClaimProofMaterial { relations: Vec::new(), ..material };
        let error = project_claim_brief(
            &need,
            repository_id,
            Digest::blake3(b"projection-flow-source"),
            &root,
            &malformed,
            brief(),
            &[],
        )
        .expect_err("a disconnected flow must not be projected");
        assert!(matches!(
            error,
            RuntimeError::ArtifactProtocol(message)
                if message.contains("flow claim chain is incomplete")
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn projects_focused_test_claim_to_canonical_plan() {
        let root = std::env::temp_dir().join(format!(
            "needle-claim-projection-tests-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("tests")).unwrap();
        let path = "tests/answer.rs";
        fs::write(root.join(path), "#[test]\nfn answer() {}\n").unwrap();
        let repository_id = Digest::blake3(b"projection-tests-repository");
        let subject = Subject::exact(repository_id, SubjectKind::Symbol, "answer");
        let obligation = Obligation::new(
            PredicateKind::FocusedTests,
            subject.id,
            vec![
                Facet { key: "completeness".to_owned(), value: "open-world".to_owned() },
                Facet { key: "polarity".to_owned(), value: "positive".to_owned() },
                Facet { key: "selection".to_owned(), value: "representative".to_owned() },
            ],
        );
        let claim = Claim::new(
            needle_core::built_in_predicate_contracts()
                .into_iter()
                .find(|contract| contract.predicate == PredicateKind::FocusedTests)
                .expect("focused-tests contract")
                .definition_digest,
            ClaimPayload::FocusedTest {
                runner: "cargo".to_owned(),
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "answer".to_owned(),
                    "--".to_owned(),
                    "--exact".to_owned(),
                ],
                cwd_relative: ".".to_owned(),
                identifier: "answer".to_owned(),
                selection: "representative".to_owned(),
                evidence_paths: vec![path.to_owned()],
            },
        )
        .expect("focused-test claim is canonical");
        let material = ClaimProofMaterial {
            claims: vec![claim.clone()],
            relations: Vec::new(),
            certificates: vec![certificate_for(
                &claim,
                &obligation,
                &subject,
                Digest::blake3(b"projection-tests-world"),
                path,
                Digest::blake3(fs::read(root.join(path)).unwrap()),
                0,
            )],
        };
        let need = NeedRequest {
            key: NeedKey::new("tests.relevant").unwrap(),
            body: "Find the relevant focused test.".to_owned(),
        };
        let projected = project_claim_brief(
            &need,
            repository_id,
            Digest::blake3(b"projection-tests-source"),
            &root,
            &material,
            brief(),
            &[],
        )
        .unwrap();
        let projected_brief: EvidenceBrief = serde_json::from_value(projected.payload).unwrap();
        assert!(projected_brief.behavior.is_none());
        let test_plan = projected_brief.test_plan.expect("focused test is projected");
        assert_eq!(test_plan.runner, "cargo");
        assert_eq!(test_plan.argv, claim_test_argv());
        assert_eq!(test_plan.cwd_relative, ".");
        assert_eq!(test_plan.test_identifier, "answer");
        assert!(test_plan.requires_approval);
        assert!(test_plan.execution_evidence_id.is_none());
        assert_eq!(projected_brief.claims["answer"], vec!["representative focused test"]);
        assert_eq!(projected.dependency_manifest.dependencies.len(), 1);
        assert_eq!(projected.dependency_manifest.dependencies[0].path, path);
        let _ = fs::remove_dir_all(root);
    }

    fn claim_test_argv() -> Vec<String> {
        ["cargo", "test", "answer", "--", "--exact"].into_iter().map(str::to_owned).collect()
    }
}
