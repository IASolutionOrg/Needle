use super::SemanticValidationError;
use needle_core::claim::Claim as SemanticClaim;
use needle_core::{
    Artifact, ArtifactValidationCertificate, ClaimOrigin, ClaimPayload, ClaimRelation,
    ClaimRelationKind, ClaimValidationCertificate, Dependency, PredicateKind,
    SemanticWorkerArtifact, Subject, built_in_predicate_contracts, runtime_flow_anchor,
};

#[derive(Clone, Debug, Default)]
pub struct ValidatedClaimSet {
    pub claims: Vec<SemanticClaim>,
    pub origins: Vec<ClaimOrigin>,
    pub relations: Vec<ClaimRelation>,
    pub certificates: Vec<ClaimValidationCertificate>,
    pub rejection: Option<String>,
}

impl ValidatedClaimSet {
    pub(super) fn rejected(error: &SemanticValidationError) -> Self {
        Self { rejection: Some(error.to_string()), ..Self::default() }
    }
}

pub(super) fn extract_validated_claims(
    worker_artifact: &SemanticWorkerArtifact,
    artifact: &Artifact,
    certificate: &ArtifactValidationCertificate,
    subject: &Subject,
) -> Result<ValidatedClaimSet, SemanticValidationError> {
    let predicate = match worker_artifact {
        SemanticWorkerArtifact::CodeLocation { .. } => PredicateKind::ImplementationLocation,
        SemanticWorkerArtifact::BehaviorTrace { .. } => PredicateKind::RuntimeFlow,
        SemanticWorkerArtifact::TestPlan { .. } => PredicateKind::FocusedTests,
    };
    let contract_definition = built_in_predicate_contracts()
        .into_iter()
        .find(|contract| contract.predicate == predicate)
        .map(|contract| contract.definition_digest)
        .ok_or_else(|| {
            SemanticValidationError::Evidence(
                "claim extraction has no matching predicate contract".to_owned(),
            )
        })?;
    let obligations = certificate
        .coverage
        .entries
        .iter()
        .map(|entry| entry.obligation.clone())
        .collect::<Vec<_>>();
    let world = certificate.coverage.world.id();
    let mut extracted = ValidatedClaimSet::default();

    match worker_artifact {
        SemanticWorkerArtifact::CodeLocation { locations, .. } => {
            for location in locations {
                let dependencies = dependencies_for_path(artifact, &location.path);
                push_claim(
                    &mut extracted,
                    ClaimPayload::ImplementationLocation { location: location.clone() },
                    contract_definition,
                    artifact,
                    certificate,
                    subject,
                    world,
                    dependencies,
                    &obligations,
                )?;
            }
        }
        SemanticWorkerArtifact::BehaviorTrace { scenario, steps, .. } => {
            let flow_anchor = runtime_flow_anchor(scenario, steps).ok_or_else(|| {
                SemanticValidationError::Evidence(
                    "runtime-flow claim anchor exceeds its structural bound".to_owned(),
                )
            })?;
            for step in steps {
                let dependencies = dependencies_for_path(artifact, &step.location.path);
                push_claim(
                    &mut extracted,
                    ClaimPayload::RuntimeFlowStep {
                        scenario: scenario.clone(),
                        flow_anchor,
                        step: step.clone(),
                    },
                    contract_definition,
                    artifact,
                    certificate,
                    subject,
                    world,
                    dependencies,
                    &obligations,
                )?;
            }
            for pair in extracted.claims.windows(2) {
                extracted.relations.push(ClaimRelation::new(
                    pair[0].id,
                    pair[1].id,
                    ClaimRelationKind::Precedes,
                ));
            }
        }
        SemanticWorkerArtifact::TestPlan {
            runner,
            argv,
            cwd_relative,
            identifiers,
            selection,
            evidence_paths,
        } => {
            let identifier = identifiers.first().ok_or_else(|| {
                SemanticValidationError::Evidence(
                    "validated focused test has no identifier".to_owned(),
                )
            })?;
            push_claim(
                &mut extracted,
                ClaimPayload::FocusedTest {
                    runner: runner.clone(),
                    argv: argv.clone(),
                    cwd_relative: cwd_relative.clone(),
                    identifier: identifier.clone(),
                    selection: selection.clone(),
                    evidence_paths: evidence_paths.clone(),
                },
                contract_definition,
                artifact,
                certificate,
                subject,
                world,
                artifact.dependency_manifest.dependencies.clone(),
                &obligations,
            )?;
        }
    }

    if extracted.claims.is_empty()
        || extracted.claims.len() > needle_core::MAX_CLAIMS_PER_ARTIFACT
        || extracted.claims.len() != extracted.origins.len()
        || extracted.claims.len() != extracted.certificates.len()
    {
        return Err(SemanticValidationError::Evidence(
            "validated claim extraction violates cardinality bounds".to_owned(),
        ));
    }
    Ok(extracted)
}

#[allow(clippy::too_many_arguments)]
fn push_claim(
    extracted: &mut ValidatedClaimSet,
    payload: ClaimPayload,
    contract_definition: needle_core::Digest,
    artifact: &Artifact,
    artifact_certificate: &ArtifactValidationCertificate,
    subject: &Subject,
    world: needle_core::Digest,
    dependencies: Vec<Dependency>,
    obligations: &[needle_core::Obligation],
) -> Result<(), SemanticValidationError> {
    let claim = SemanticClaim::new(contract_definition, payload).ok_or_else(|| {
        SemanticValidationError::Evidence("claim payload exceeds canonical bounds".to_owned())
    })?;
    let ordinal = u16::try_from(extracted.claims.len()).map_err(|_| {
        SemanticValidationError::Evidence("claim ordinal exceeds its bound".to_owned())
    })?;
    let claim_certificate = ClaimValidationCertificate::new(
        claim.id,
        artifact_certificate.artifact,
        artifact_certificate.id,
        subject.id,
        world,
        artifact_certificate.validator_definition,
        dependencies,
        obligations.to_vec(),
        artifact_certificate.issued_unix_ms,
    )
    .ok_or_else(|| {
        SemanticValidationError::Evidence(
            "claim dependency or obligation closure is incomplete".to_owned(),
        )
    })?;
    extracted.origins.push(ClaimOrigin {
        claim: claim.id,
        artifact: artifact_certificate.artifact,
        validation_certificate: artifact_certificate.id,
        subject: subject.id,
        world,
        ordinal,
        created_unix_ms: artifact.created_unix_ms,
    });
    extracted.certificates.push(claim_certificate);
    extracted.claims.push(claim);
    Ok(())
}

fn dependencies_for_path(artifact: &Artifact, path: &str) -> Vec<Dependency> {
    artifact
        .dependency_manifest
        .dependencies
        .iter()
        .filter(|dependency| dependency.path == path)
        .cloned()
        .collect()
}
