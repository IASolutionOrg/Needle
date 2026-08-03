use crate::semantic_validation::{dependency_is_fresh, validator_definition};
use arrayvec::ArrayVec;
use needle_core::claim::Claim as SemanticClaim;
use needle_core::{
    ArtifactId, ArtifactKind, CanonicalHasher, ClaimId, ClaimKind, ClaimPayload, ClaimRelation,
    ClaimRelationKind, ClaimSetCertificate, ClaimValidationCertificate,
    ClaimValidationCertificateId, Digest, FlowStepRole, LocationRole, MAX_CLAIM_CANDIDATES,
    MAX_CLAIM_ORIGINS, MAX_SELECTED_CLAIMS, Need, Obligation, ObligationId, PredicateKind,
    built_in_predicate_contracts,
};
use std::path::Path;
use std::sync::OnceLock;
use thiserror::Error;

const CLAIM_PROOF_ENGINE_REVISION: u32 = 1;
static CLAIM_PROOF_ENGINE_DEFINITION: OnceLock<Digest> = OnceLock::new();
static CLAIM_CONTRACT_DEFINITIONS: OnceLock<[Digest; 3]> = OnceLock::new();
static CLAIM_VALIDATOR_DEFINITIONS: OnceLock<[Digest; 3]> = OnceLock::new();

#[derive(Clone, Debug, Default)]
pub struct ClaimProofMaterial {
    pub claims: Vec<SemanticClaim>,
    pub relations: Vec<ClaimRelation>,
    pub certificates: Vec<ClaimValidationCertificate>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ClaimProofError {
    #[error("claim proof input exceeds a bounded limit")]
    Bound,
    #[error("claim proof input is non-canonical or internally inconsistent")]
    NonCanonical,
    #[error("claim proof input is stale")]
    Stale,
    #[error("claim proof world or subject is incompatible with the need")]
    Incompatible,
    #[error("claim proof cannot satisfy required obligation `{0}`")]
    Insufficient(&'static str),
    #[error("claim proof certificate does not replay to the selected proof")]
    ReplayMismatch,
}

#[derive(Clone, Debug)]
struct SelectedClaimProof {
    members: ArrayVec<(ClaimId, ClaimValidationCertificateId), MAX_SELECTED_CLAIMS>,
    obligations: ArrayVec<ObligationId, 16>,
}

pub fn claim_proof_engine_definition() -> Digest {
    *CLAIM_PROOF_ENGINE_DEFINITION.get_or_init(|| {
        let mut hasher = CanonicalHasher::new(b"claim-proof-engine-definition");
        hasher.field_u32(CLAIM_PROOF_ENGINE_REVISION);
        for definition in contract_definitions() {
            hasher.field_digest(*definition);
        }
        for definition in validator_definitions() {
            hasher.field_digest(*definition);
        }
        hasher.finish()
    })
}

pub fn build_claim_set_certificate(
    need: &Need,
    material: &ClaimProofMaterial,
    repository_root: &Path,
    created_unix_ms: u64,
) -> Result<ClaimSetCertificate, ClaimProofError> {
    build_claim_component_certificate(
        need,
        &need.required,
        material,
        repository_root,
        created_unix_ms,
    )
}

pub fn build_claim_component_certificate(
    need: &Need,
    obligations: &[Obligation],
    material: &ClaimProofMaterial,
    repository_root: &Path,
    created_unix_ms: u64,
) -> Result<ClaimSetCertificate, ClaimProofError> {
    let selected = select_claim_proof(need, obligations, material, repository_root)?;
    ClaimSetCertificate::new(
        need.id,
        selected.members.into_iter().collect(),
        selected.obligations.into_iter().collect(),
        need.world.id(),
        claim_proof_engine_definition(),
        created_unix_ms,
    )
    .ok_or(ClaimProofError::NonCanonical)
}

pub fn replay_claim_set_certificate(
    certificate: &ClaimSetCertificate,
    need: &Need,
    material: &ClaimProofMaterial,
    repository_root: &Path,
) -> Result<(), ClaimProofError> {
    if !certificate.is_canonical()
        || certificate.need != need.id
        || certificate.world != need.world.id()
        || certificate.engine_definition != claim_proof_engine_definition()
    {
        return Err(ClaimProofError::ReplayMismatch);
    }
    let mut obligations = ArrayVec::<Obligation, 16>::new();
    for obligation_id in &certificate.obligations {
        let obligation = need
            .required
            .iter()
            .find(|obligation| obligation.id == *obligation_id)
            .ok_or(ClaimProofError::ReplayMismatch)?;
        obligations.try_push(obligation.clone()).map_err(|_| ClaimProofError::Bound)?;
    }
    let selected = select_claim_proof(need, &obligations, material, repository_root)?;
    if !certificate.claims.iter().copied().eq(selected.members.iter().map(|member| member.0))
        || !certificate
            .validation_certificates
            .iter()
            .copied()
            .eq(selected.members.iter().map(|member| member.1))
        || certificate.obligations.as_slice() != selected.obligations.as_slice()
    {
        return Err(ClaimProofError::ReplayMismatch);
    }
    Ok(())
}

fn select_claim_proof(
    need: &Need,
    obligations: &[Obligation],
    material: &ClaimProofMaterial,
    repository_root: &Path,
) -> Result<SelectedClaimProof, ClaimProofError> {
    validate_material(need, material)?;
    let mut selected =
        SelectedClaimProof { members: ArrayVec::new(), obligations: ArrayVec::new() };
    if obligations.is_empty() || obligations.len() > 16 {
        return Err(ClaimProofError::Bound);
    }
    for obligation in obligations {
        match obligation.predicate {
            PredicateKind::ImplementationLocation => {
                select_single_claim(
                    material,
                    obligation,
                    ClaimKind::ImplementationLocation,
                    |claim| {
                        matches!(
                            &claim.payload,
                            ClaimPayload::ImplementationLocation { location }
                                if location.role == LocationRole::Primary
                        )
                    },
                    repository_root,
                    &mut selected.members,
                )?;
            }
            PredicateKind::FocusedTests => {
                select_single_claim(
                    material,
                    obligation,
                    ClaimKind::FocusedTest,
                    |claim| matches!(&claim.payload, ClaimPayload::FocusedTest { .. }),
                    repository_root,
                    &mut selected.members,
                )?;
            }
            PredicateKind::RuntimeFlow => {
                select_runtime_flow(material, obligation, repository_root, &mut selected.members)?;
            }
        }
        selected.obligations.try_push(obligation.id).map_err(|_| ClaimProofError::Bound)?;
    }
    selected.members.sort_unstable();
    selected.obligations.sort_unstable();
    if selected.members.windows(2).any(|pair| pair[0].0 == pair[1].0)
        || selected.obligations.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(ClaimProofError::NonCanonical);
    }
    validate_selected_members(&selected.members, material, repository_root)?;
    Ok(selected)
}

fn validate_material(need: &Need, material: &ClaimProofMaterial) -> Result<(), ClaimProofError> {
    if need.subjects.is_empty()
        || need.subjects.len() > 8
        || need.subjects.iter().any(|subject| {
            !subject.is_canonical() || subject.repository_lineage != need.world.repository_lineage
        })
        || need.required.iter().any(|obligation| {
            !need.subjects.iter().any(|subject| subject.id == obligation.subject)
                || Obligation::new(
                    obligation.predicate,
                    obligation.subject,
                    obligation.facets.clone(),
                ) != *obligation
        })
    {
        return Err(ClaimProofError::NonCanonical);
    }
    if need.required.is_empty()
        || need.required.len() > 16
        || need.residual.as_ref().is_some_and(|residual| residual.mandatory)
        || material.claims.is_empty()
        || material.claims.len() > MAX_CLAIM_CANDIDATES
        || material.certificates.is_empty()
        || material.certificates.len() > MAX_CLAIM_CANDIDATES
        || material.relations.len() > MAX_CLAIM_CANDIDATES
    {
        return Err(ClaimProofError::Bound);
    }
    let world = need.world.id();
    for (index, claim) in material.claims.iter().enumerate() {
        if !claim.is_canonical()
            || material.claims[..index].iter().any(|candidate| candidate.id == claim.id)
            || current_contract_definition(claim.kind) != claim.contract_definition
        {
            return Err(ClaimProofError::NonCanonical);
        }
    }
    for (index, certificate) in material.certificates.iter().enumerate() {
        let Some(claim) = material.claims.iter().find(|claim| claim.id == certificate.claim) else {
            return Err(ClaimProofError::NonCanonical);
        };
        if !certificate.is_canonical()
            || material.certificates[..index].iter().any(|candidate| candidate.id == certificate.id)
            || certificate.world != world
            || !need.subjects.iter().any(|subject| subject.id == certificate.subject)
            || certificate.validator_definition != current_validator_definition(claim.kind)
            || certificate.obligations.iter().any(|obligation| {
                obligation.subject != certificate.subject
                    || predicate_for_claim_kind(claim.kind) != obligation.predicate
            })
        {
            return Err(ClaimProofError::Incompatible);
        }
    }
    for (index, relation) in material.relations.iter().enumerate() {
        if !relation.is_canonical()
            || material.relations[..index].iter().any(|candidate| candidate.id == relation.id)
            || !material.claims.iter().any(|claim| claim.id == relation.from)
            || !material.claims.iter().any(|claim| claim.id == relation.to)
        {
            return Err(ClaimProofError::NonCanonical);
        }
    }
    Ok(())
}

fn select_single_claim<F>(
    material: &ClaimProofMaterial,
    obligation: &Obligation,
    kind: ClaimKind,
    predicate: F,
    repository_root: &Path,
    selected: &mut ArrayVec<(ClaimId, ClaimValidationCertificateId), MAX_SELECTED_CLAIMS>,
) -> Result<(), ClaimProofError>
where
    F: Fn(&SemanticClaim) -> bool,
{
    let matching = material
        .claims
        .iter()
        .filter(|claim| claim.kind == kind && predicate(claim))
        .flat_map(|claim| {
            material.certificates.iter().filter(move |certificate| {
                certificate.claim == claim.id && certificate_covers(certificate, obligation)
            })
        })
        .count();
    let candidate = material
        .claims
        .iter()
        .filter(|claim| claim.kind == kind && predicate(claim))
        .filter_map(|claim| {
            certificate_satisfying(material, claim.id, obligation, repository_root)
                .map(|certificate| (claim.id, certificate.id))
        })
        .min();
    let Some(candidate) = candidate else {
        return Err(if matching == 0 {
            ClaimProofError::Insufficient(obligation.predicate.as_str())
        } else {
            ClaimProofError::Stale
        });
    };
    push_member(selected, candidate)
}

fn select_runtime_flow(
    material: &ClaimProofMaterial,
    obligation: &Obligation,
    repository_root: &Path,
    selected: &mut ArrayVec<(ClaimId, ClaimValidationCertificateId), MAX_SELECTED_CLAIMS>,
) -> Result<(), ClaimProofError> {
    let mut matching = false;
    let mut fresh = false;
    let mut anchors = ArrayVec::<Digest, MAX_CLAIM_CANDIDATES>::new();
    for claim in &material.claims {
        if let ClaimPayload::RuntimeFlowStep { flow_anchor, .. } = &claim.payload {
            matching |= material.certificates.iter().any(|certificate| {
                certificate.claim == claim.id && certificate_covers(certificate, obligation)
            });
            if certificate_satisfying(material, claim.id, obligation, repository_root).is_some() {
                fresh = true;
                if !anchors.contains(flow_anchor) {
                    anchors.try_push(*flow_anchor).map_err(|_| ClaimProofError::Bound)?;
                }
            }
        }
    }
    anchors.sort_unstable();
    for anchor in anchors {
        if let Some(flow) = connected_flow(material, obligation, repository_root, anchor)? {
            for member in flow {
                push_member(selected, member)?;
            }
            return Ok(());
        }
    }
    Err(if matching && !fresh {
        ClaimProofError::Stale
    } else {
        ClaimProofError::Insufficient(obligation.predicate.as_str())
    })
}

fn connected_flow(
    material: &ClaimProofMaterial,
    obligation: &Obligation,
    repository_root: &Path,
    anchor: Digest,
) -> Result<
    Option<ArrayVec<(ClaimId, ClaimValidationCertificateId), MAX_SELECTED_CLAIMS>>,
    ClaimProofError,
> {
    let mut members = ArrayVec::<_, MAX_SELECTED_CLAIMS>::new();
    let mut roles = [false; 5];
    let mut scenario: Option<&str> = None;
    for claim in &material.claims {
        let ClaimPayload::RuntimeFlowStep { scenario: candidate_scenario, flow_anchor, step } =
            &claim.payload
        else {
            continue;
        };
        if *flow_anchor != anchor {
            continue;
        }
        let Some(certificate) =
            certificate_satisfying(material, claim.id, obligation, repository_root)
        else {
            continue;
        };
        if scenario.is_some_and(|value| value != candidate_scenario) {
            return Ok(None);
        }
        scenario = Some(candidate_scenario);
        roles[flow_role_index(step.role)] = true;
        members.try_push((claim.id, certificate.id)).map_err(|_| ClaimProofError::Bound)?;
    }
    if members.len() < roles.len() || roles.iter().any(|present| !present) {
        return Ok(None);
    }
    let mut in_degree = [0_u8; MAX_SELECTED_CLAIMS];
    let mut out_degree = [0_u8; MAX_SELECTED_CLAIMS];
    let mut next = [None; MAX_SELECTED_CLAIMS];
    let mut edge_count = 0_usize;
    for relation in &material.relations {
        if relation.kind != ClaimRelationKind::Precedes {
            continue;
        }
        let Some(from) = members.iter().position(|member| member.0 == relation.from) else {
            continue;
        };
        let Some(to) = members.iter().position(|member| member.0 == relation.to) else {
            continue;
        };
        in_degree[to] = in_degree[to].saturating_add(1);
        out_degree[from] = out_degree[from].saturating_add(1);
        next[from] = Some(to);
        edge_count += 1;
    }
    if edge_count + 1 != members.len()
        || in_degree[..members.len()].iter().filter(|degree| **degree == 0).count() != 1
        || out_degree[..members.len()].iter().filter(|degree| **degree == 0).count() != 1
        || in_degree[..members.len()].iter().any(|degree| *degree > 1)
        || out_degree[..members.len()].iter().any(|degree| *degree > 1)
    {
        return Ok(None);
    }
    let mut cursor = in_degree[..members.len()].iter().position(|degree| *degree == 0).unwrap();
    let mut visited = [false; MAX_SELECTED_CLAIMS];
    for _ in 0..members.len() {
        if visited[cursor] {
            return Ok(None);
        }
        visited[cursor] = true;
        let Some(following) = next[cursor] else {
            break;
        };
        cursor = following;
    }
    if visited[..members.len()].iter().any(|seen| !seen) {
        return Ok(None);
    }
    Ok(Some(members))
}

fn certificate_satisfying<'a>(
    material: &'a ClaimProofMaterial,
    claim: ClaimId,
    obligation: &Obligation,
    repository_root: &Path,
) -> Option<&'a ClaimValidationCertificate> {
    material
        .certificates
        .iter()
        .filter(|certificate| certificate.claim == claim)
        .filter(|certificate| certificate_covers(certificate, obligation))
        .filter(|certificate| claim_validation_certificate_is_fresh(certificate, repository_root))
        .min_by_key(|certificate| certificate.id)
}

fn certificate_covers(certificate: &ClaimValidationCertificate, obligation: &Obligation) -> bool {
    certificate.obligations.iter().any(|provided| provided.satisfies(obligation))
}

pub fn claim_validation_certificate_is_fresh(
    certificate: &ClaimValidationCertificate,
    repository_root: &Path,
) -> bool {
    certificate
        .dependencies
        .iter()
        .all(|dependency| dependency_is_fresh(repository_root, dependency))
}

fn validate_selected_members(
    selected: &[(ClaimId, ClaimValidationCertificateId)],
    material: &ClaimProofMaterial,
    repository_root: &Path,
) -> Result<(), ClaimProofError> {
    let mut origins = ArrayVec::<ArtifactId, MAX_CLAIM_ORIGINS>::new();
    for (_, certificate_id) in selected {
        let certificate = material
            .certificates
            .iter()
            .find(|certificate| certificate.id == *certificate_id)
            .ok_or(ClaimProofError::NonCanonical)?;
        if !claim_validation_certificate_is_fresh(certificate, repository_root) {
            return Err(ClaimProofError::Stale);
        }
        if !origins.contains(&certificate.origin_artifact) {
            origins.try_push(certificate.origin_artifact).map_err(|_| ClaimProofError::Bound)?;
        }
    }
    Ok(())
}

fn push_member(
    selected: &mut ArrayVec<(ClaimId, ClaimValidationCertificateId), MAX_SELECTED_CLAIMS>,
    member: (ClaimId, ClaimValidationCertificateId),
) -> Result<(), ClaimProofError> {
    if selected.iter().any(|candidate| candidate.0 == member.0) {
        return Ok(());
    }
    selected.try_push(member).map_err(|_| ClaimProofError::Bound)
}

fn current_contract_definition(kind: ClaimKind) -> Digest {
    contract_definitions()[claim_kind_index(kind)]
}

fn current_validator_definition(kind: ClaimKind) -> Digest {
    validator_definitions()[claim_kind_index(kind)]
}

fn contract_definitions() -> &'static [Digest; 3] {
    CLAIM_CONTRACT_DEFINITIONS.get_or_init(|| {
        let mut definitions = [Digest::blake3(b"missing-contract"); 3];
        for contract in built_in_predicate_contracts() {
            let kind = match contract.predicate {
                PredicateKind::ImplementationLocation => ClaimKind::ImplementationLocation,
                PredicateKind::RuntimeFlow => ClaimKind::RuntimeFlowStep,
                PredicateKind::FocusedTests => ClaimKind::FocusedTest,
            };
            definitions[claim_kind_index(kind)] = contract.definition_digest;
        }
        definitions
    })
}

fn validator_definitions() -> &'static [Digest; 3] {
    CLAIM_VALIDATOR_DEFINITIONS.get_or_init(|| {
        [
            validator_definition(&ArtifactKind::code_location()),
            validator_definition(&ArtifactKind::behavior_trace()),
            validator_definition(&ArtifactKind::test_plan()),
        ]
    })
}

const fn claim_kind_index(kind: ClaimKind) -> usize {
    match kind {
        ClaimKind::ImplementationLocation => 0,
        ClaimKind::RuntimeFlowStep => 1,
        ClaimKind::FocusedTest => 2,
    }
}

const fn predicate_for_claim_kind(kind: ClaimKind) -> PredicateKind {
    match kind {
        ClaimKind::ImplementationLocation => PredicateKind::ImplementationLocation,
        ClaimKind::RuntimeFlowStep => PredicateKind::RuntimeFlow,
        ClaimKind::FocusedTest => PredicateKind::FocusedTests,
    }
}

const fn flow_role_index(role: FlowStepRole) -> usize {
    match role {
        FlowStepRole::Producer => 0,
        FlowStepRole::Carrier => 1,
        FlowStepRole::Transformation => 2,
        FlowStepRole::Precedence => 3,
        FlowStepRole::Consumer => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate_semantic_artifact;
    use needle_core::{
        Facet, NeedId, SemanticFlowStep, SemanticLocation, SemanticWorkerArtifact, SemanticWorld,
        Subject, SubjectKind, need_fragment,
    };
    use std::fs;

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "needle-claim-proof-{name}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn need(
        repository_lineage: Digest,
        subject: Subject,
        predicate: PredicateKind,
        facets: Vec<Facet>,
    ) -> Need {
        Need {
            id: NeedId(Digest::blake3(format!("{predicate:?}-need").as_bytes())),
            subjects: vec![subject.clone()],
            required: vec![Obligation::new(predicate, subject.id, facets)],
            preferred: Vec::new(),
            semantic_constraints: Vec::new(),
            world: SemanticWorld {
                repository_lineage,
                source_selector: "current".to_owned(),
                platform: "current".to_owned(),
                features: "default".to_owned(),
                configuration: None,
                toolchain: None,
            },
            input_artifacts: Vec::new(),
            residual: None,
            body_digest: Digest::blake3(b"body"),
            format_revision: 1,
        }
    }

    fn material(validated: &crate::ValidatedSemanticArtifact) -> ClaimProofMaterial {
        ClaimProofMaterial {
            claims: validated.claims.claims.clone(),
            relations: validated.claims.relations.clone(),
            certificates: validated.claims.certificates.clone(),
        }
    }

    #[test]
    fn implementation_claim_set_replays_and_rejects_a_stale_dependency() {
        let root = fixture_root("location");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        let repository_lineage = Digest::blake3(b"location-repository");
        let subject = Subject::exact(repository_lineage, SubjectKind::Symbol, "answer");
        let need = need(
            repository_lineage,
            subject,
            PredicateKind::ImplementationLocation,
            vec![
                Facet { key: "granularity".to_owned(), value: "exact-location".to_owned() },
                Facet { key: "polarity".to_owned(), value: "positive".to_owned() },
                Facet { key: "selection".to_owned(), value: "primary".to_owned() },
            ],
        );
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let validated = validate_semantic_artifact(
            &fragment,
            &SemanticWorkerArtifact::CodeLocation {
                locations: vec![SemanticLocation {
                    role: LocationRole::Primary,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: None,
                    byte_end: None,
                }],
                gaps: Vec::new(),
            },
            &root,
            Digest::blake3(b"location-origin"),
        )
        .unwrap();
        let material = material(&validated);
        let certificate = build_claim_set_certificate(&need, &material, &root, 1).unwrap();
        assert!(certificate.is_canonical());
        replay_claim_set_certificate(&certificate, &need, &material, &root).unwrap();

        let original = &material.certificates[0];
        let incompatible_certificate = ClaimValidationCertificate::new(
            original.claim,
            original.origin_artifact,
            original.origin_validation_certificate,
            original.subject,
            Digest::blake3(b"another-world"),
            original.validator_definition,
            original.dependencies.clone(),
            original.obligations.clone(),
            original.issued_unix_ms,
        )
        .unwrap();
        let mut incompatible = material.clone();
        incompatible.certificates[0] = incompatible_certificate;
        assert_eq!(
            build_claim_set_certificate(&need, &incompatible, &root, 2),
            Err(ClaimProofError::Incompatible)
        );

        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 43 }\n").unwrap();
        assert_eq!(
            replay_claim_set_certificate(&certificate, &need, &material, &root),
            Err(ClaimProofError::Stale)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_flow_claim_set_requires_one_connected_certified_chain() {
        let root = fixture_root("flow");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        let repository_lineage = Digest::blake3(b"flow-repository");
        let subject = Subject::exact(repository_lineage, SubjectKind::Symbol, "answer");
        let need = need(
            repository_lineage,
            subject,
            PredicateKind::RuntimeFlow,
            vec![
                Facet { key: "completeness".to_owned(), value: "contract-complete".to_owned() },
                Facet { key: "granularity".to_owned(), value: "stepwise".to_owned() },
                Facet { key: "scenario".to_owned(), value: "default".to_owned() },
            ],
        );
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let validated = validate_semantic_artifact(
            &fragment,
            &SemanticWorkerArtifact::BehaviorTrace {
                scenario: "default".to_owned(),
                steps: [
                    FlowStepRole::Producer,
                    FlowStepRole::Carrier,
                    FlowStepRole::Transformation,
                    FlowStepRole::Precedence,
                    FlowStepRole::Consumer,
                ]
                .into_iter()
                .map(|role| SemanticFlowStep {
                    role,
                    location: SemanticLocation {
                        role: LocationRole::Supporting,
                        path: "src/lib.rs".to_owned(),
                        symbol: Some("answer".to_owned()),
                        byte_start: None,
                        byte_end: None,
                    },
                    description: format!("{role:?}"),
                })
                .collect(),
                gaps: Vec::new(),
            },
            &root,
            Digest::blake3(b"flow-origin"),
        )
        .unwrap();
        let mut material = material(&validated);
        let certificate = build_claim_set_certificate(&need, &material, &root, 1).unwrap();
        replay_claim_set_certificate(&certificate, &need, &material, &root).unwrap();

        material.relations.pop();
        assert_eq!(
            build_claim_set_certificate(&need, &material, &root, 2),
            Err(ClaimProofError::Insufficient("runtime-flow"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn focused_test_claim_set_is_sufficient_without_command_execution() {
        let root = fixture_root("focused-test");
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests/misc.rs"), "mod feature;\n").unwrap();
        fs::write(root.join("tests/feature.rs"), "// --crlf\n#[test]\nfn crlf_matching() {}\n")
            .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"integration\"\npath = \"tests/misc.rs\"\n",
        )
        .unwrap();
        let repository_lineage = Digest::blake3(b"focused-test-repository");
        let subject = Subject::exact(repository_lineage, SubjectKind::CliOption, "--crlf");
        let need = need(
            repository_lineage,
            subject,
            PredicateKind::FocusedTests,
            vec![
                Facet { key: "completeness".to_owned(), value: "open-world".to_owned() },
                Facet { key: "polarity".to_owned(), value: "positive".to_owned() },
                Facet { key: "selection".to_owned(), value: "representative".to_owned() },
            ],
        );
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let argv = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--test".to_owned(),
            "integration".to_owned(),
            "crlf_matching".to_owned(),
            "--".to_owned(),
            "--exact".to_owned(),
        ];
        let validated = validate_semantic_artifact(
            &fragment,
            &SemanticWorkerArtifact::TestPlan {
                runner: "cargo".to_owned(),
                argv,
                cwd_relative: ".".to_owned(),
                identifiers: vec!["crlf_matching".to_owned()],
                selection: "representative".to_owned(),
                evidence_paths: vec!["Cargo.toml".to_owned(), "tests/feature.rs".to_owned()],
            },
            &root,
            Digest::blake3(b"focused-test-origin"),
        )
        .unwrap();
        assert_eq!(
            validated.certificate.test_plan_evidence,
            Some(needle_core::TestPlanEvidenceStatus::Located)
        );
        let material = material(&validated);
        let certificate = build_claim_set_certificate(&need, &material, &root, 1).unwrap();
        replay_claim_set_certificate(&certificate, &need, &material, &root).unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
