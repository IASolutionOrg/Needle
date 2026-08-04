use needle_core::{
    Artifact, ArtifactContract, ArtifactId, ArtifactKind, ArtifactValidationCertificate,
    ArtifactValidationCertificateId, CacheScope, CanonicalHasher, CommandExecutionEvidence,
    CoverageEntry, CoverageManifest, Dependency, DependencyManifest, Digest, Facet, FlowStepRole,
    NeedFragment, Obligation, PredicateKind, SemanticArtifactResult, SemanticWorkerArtifact,
    TestCommand, TestPlan, TestPlanEvidenceStatus, ValidationRecord, WorkerObservationTrace,
    built_in_predicate_contracts,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "semantic_validation/claims.rs"]
mod claims;
#[path = "semantic_validation/test_plan.rs"]
mod test_plan;

pub use claims::ValidatedClaimSet;
use claims::extract_validated_claims;
use test_plan::{TestPlanProof, validate_test_plan_proof};

pub const SEMANTIC_VALIDATOR_REVISION: u32 = 12;

#[derive(Clone, Debug)]
pub struct ValidatedSemanticArtifact {
    pub artifact: Artifact,
    pub semantic_id: ArtifactId,
    pub certificate: ArtifactValidationCertificate,
    pub claims: ValidatedClaimSet,
}

struct TestPlanCertification<'a> {
    evidence_ids: &'a [String],
    status: Option<TestPlanEvidenceStatus>,
    require_declared_target: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticValidationError {
    #[error("semantic worker result uses an unsupported schema")]
    Schema,
    #[error("semantic artifact `{0}` has no validator-derived coverage")]
    Insufficient(String),
    #[error("semantic artifact evidence is invalid: {0}")]
    Evidence(String),
    #[error("semantic artifact serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn validate_semantic_result(
    fragment: &NeedFragment,
    result: &SemanticArtifactResult,
    repository_root: &Path,
    origin_request_id: Digest,
) -> Result<Vec<ValidatedSemanticArtifact>, SemanticValidationError> {
    if result.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID {
        return Err(SemanticValidationError::Schema);
    }
    result
        .artifacts
        .iter()
        .map(|worker_artifact| {
            let kind = worker_artifact.kind();
            let trace = result.artifact_traces.get(&kind).unwrap_or(&result.observation_trace);
            validate_semantic_artifact_with_trace(
                fragment,
                worker_artifact,
                repository_root,
                origin_request_id,
                Some(trace),
            )
        })
        .collect()
}

pub fn validate_semantic_artifact(
    fragment: &NeedFragment,
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    origin_request_id: Digest,
) -> Result<ValidatedSemanticArtifact, SemanticValidationError> {
    validate_semantic_artifact_with_trace(
        fragment,
        worker_artifact,
        repository_root,
        origin_request_id,
        None,
    )
}

pub fn validate_semantic_artifact_with_trace(
    fragment: &NeedFragment,
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    origin_request_id: Digest,
    observation_trace: Option<&WorkerObservationTrace>,
) -> Result<ValidatedSemanticArtifact, SemanticValidationError> {
    let test_plan_evidence = if matches!(worker_artifact, SemanticWorkerArtifact::TestPlan { .. }) {
        Some(TestPlanEvidenceStatus::Located)
    } else {
        None
    };
    validate_semantic_artifact_with_evidence(
        fragment,
        worker_artifact,
        repository_root,
        origin_request_id,
        observation_trace,
        TestPlanCertification {
            evidence_ids: &[],
            status: test_plan_evidence,
            require_declared_target: true,
        },
    )
}

pub fn validate_semantic_test_plan_with_evidence(
    fragment: &NeedFragment,
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    origin_request_id: Digest,
    observation_trace: Option<&WorkerObservationTrace>,
    declared_plan: &TestPlan,
    command_evidence: &CommandExecutionEvidence,
) -> Result<ValidatedSemanticArtifact, SemanticValidationError> {
    validate_parent_owned_test_plan_binding(worker_artifact, declared_plan)?;
    crate::validate_test_evidence(declared_plan, command_evidence)
        .map_err(|error| SemanticValidationError::Evidence(error.to_string()))?;
    validate_semantic_artifact_with_evidence(
        fragment,
        worker_artifact,
        repository_root,
        origin_request_id,
        observation_trace,
        TestPlanCertification {
            evidence_ids: std::slice::from_ref(&command_evidence.id),
            status: Some(TestPlanEvidenceStatus::Executed),
            require_declared_target: false,
        },
    )
}

pub fn validate_semantic_test_plan(
    fragment: &NeedFragment,
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    origin_request_id: Digest,
    observation_trace: Option<&WorkerObservationTrace>,
    declared_plan: &TestPlan,
) -> Result<ValidatedSemanticArtifact, SemanticValidationError> {
    validate_parent_owned_test_plan_binding(worker_artifact, declared_plan)?;
    validate_semantic_artifact_with_evidence(
        fragment,
        worker_artifact,
        repository_root,
        origin_request_id,
        observation_trace,
        TestPlanCertification {
            evidence_ids: &[],
            status: Some(TestPlanEvidenceStatus::Located),
            require_declared_target: false,
        },
    )
}

pub(crate) fn validate_parent_owned_test_plan_binding(
    worker_artifact: &SemanticWorkerArtifact,
    declared_plan: &TestPlan,
) -> Result<(), SemanticValidationError> {
    let SemanticWorkerArtifact::TestPlan { runner, argv, cwd_relative, identifiers, .. } =
        worker_artifact
    else {
        return Err(SemanticValidationError::Evidence(
            "parent-declared test plan may certify only a test-plan artifact".to_owned(),
        ));
    };
    let Some(identifier) = identifiers.first().filter(|_| identifiers.len() == 1) else {
        return Err(SemanticValidationError::Evidence(
            "semantic test plan has no singular test identifier".to_owned(),
        ));
    };
    let worker_command =
        TestCommand::from_canonical_parts(runner, argv, identifier).map_err(|violations| {
            SemanticValidationError::Evidence(format!(
                "semantic test plan command is invalid: {}",
                violations.iter().map(|violation| violation.code()).collect::<Vec<_>>().join(",")
            ))
        })?;
    let declared_command = declared_plan.test_command().map_err(|violations| {
        SemanticValidationError::Evidence(format!(
            "parent-declared test command is invalid: {}",
            violations.iter().map(|violation| violation.code()).collect::<Vec<_>>().join(",")
        ))
    })?;
    if worker_command != declared_command || cwd_relative != &declared_plan.cwd_relative {
        return Err(SemanticValidationError::Evidence(
            "semantic test plan differs from the parent-declared test plan".to_owned(),
        ));
    }
    Ok(())
}

fn validate_semantic_artifact_with_evidence(
    fragment: &NeedFragment,
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    origin_request_id: Digest,
    observation_trace: Option<&WorkerObservationTrace>,
    test_plan_certification: TestPlanCertification<'_>,
) -> Result<ValidatedSemanticArtifact, SemanticValidationError> {
    let kind = worker_artifact.kind();
    if !fragment.is_consistent() {
        return Err(SemanticValidationError::Evidence(
            "semantic fragment identity or subject definition is inconsistent".to_owned(),
        ));
    }
    let evidence = evidence_labels(worker_artifact);
    let Some(subject) = fragment.subject_definitions.first() else {
        return Err(SemanticValidationError::Insufficient(kind.0));
    };
    let test_plan_proof = validate_test_plan_proof(
        worker_artifact,
        repository_root,
        subject,
        test_plan_certification.require_declared_target,
    )?;
    validate_artifact_locations(worker_artifact, repository_root)?;
    let Some(derived) =
        derive_coverage(worker_artifact, subject, repository_root, test_plan_proof.as_ref())?
    else {
        return Err(SemanticValidationError::Insufficient(kind.0));
    };
    if !fragment.obligations.iter().any(|requested| derived.satisfies(requested)) {
        return Err(SemanticValidationError::Insufficient(kind.0));
    }
    let coverage = vec![CoverageEntry { obligation: derived, evidence }];
    let dependency_manifest = dependency_manifest(
        worker_artifact,
        &coverage,
        repository_root,
        observation_trace,
        test_plan_proof.as_ref(),
    )?;
    let dependency_manifest_digest = manifest_digest(&dependency_manifest);
    let payload = serde_json::to_value(worker_artifact)?;
    let contract = ArtifactContract::semantic(
        format!("needle.semantic.{}", kind.0),
        2,
        kind.clone(),
        dependency_manifest.scope,
    );
    let semantic_id =
        worker_artifact.canonical_artifact_id(contract.definition_digest).ok_or_else(|| {
            SemanticValidationError::Evidence(
                "semantic artifact exceeds canonical bounds".to_owned(),
            )
        })?;
    let validator_definition = validator_definition(&kind);
    let issued_unix_ms = now_ms();
    let coverage_manifest = CoverageManifest {
        entries: coverage,
        world: fragment.world.clone(),
        dependency_manifest_digest,
    };
    let certificate_id = validation_certificate_id(
        semantic_id,
        &fragment.semantic_inputs,
        test_plan_certification.evidence_ids,
        &coverage_manifest,
        validator_definition,
        test_plan_certification.status,
    );
    let artifact = Artifact {
        id: semantic_id.digest(),
        request_id: origin_request_id,
        contract,
        payload,
        dependency_manifest,
        validations: vec![ValidationRecord {
            validator: "needle.semantic-validator".to_owned(),
            validator_revision: SEMANTIC_VALIDATOR_REVISION,
            status: "passed".to_owned(),
            evidence_digest: certificate_id.digest(),
            validated_unix_ms: issued_unix_ms,
        }],
        created_unix_ms: issued_unix_ms,
    };
    let certificate = ArtifactValidationCertificate {
        id: certificate_id,
        artifact: semantic_id,
        input_artifacts: fragment.semantic_inputs.clone(),
        evidence_ids: test_plan_certification.evidence_ids.to_vec(),
        test_plan_evidence: test_plan_certification.status,
        coverage: coverage_manifest,
        validator_definition,
        dependency_checks_digest: dependency_manifest_digest,
        issued_unix_ms,
    };
    let claims = extract_validated_claims(worker_artifact, &artifact, &certificate, subject)
        .unwrap_or_else(|error| ValidatedClaimSet::rejected(&error));
    Ok(ValidatedSemanticArtifact { artifact, semantic_id, certificate, claims })
}

pub fn validator_definition(kind: &ArtifactKind) -> Digest {
    static DEFINITIONS: OnceLock<[Digest; 3]> = OnceLock::new();
    let definitions = DEFINITIONS.get_or_init(|| {
        let contracts = built_in_predicate_contracts();
        [
            validator_definition_for(
                &ArtifactKind::code_location(),
                PredicateKind::ImplementationLocation,
                &contracts,
            ),
            validator_definition_for(
                &ArtifactKind::behavior_trace(),
                PredicateKind::RuntimeFlow,
                &contracts,
            ),
            validator_definition_for(
                &ArtifactKind::test_plan(),
                PredicateKind::FocusedTests,
                &contracts,
            ),
        ]
    });
    if kind == &ArtifactKind::code_location() {
        definitions[0]
    } else if kind == &ArtifactKind::behavior_trace() {
        definitions[1]
    } else {
        definitions[2]
    }
}

fn validator_definition_for(
    kind: &ArtifactKind,
    predicate: PredicateKind,
    contracts: &[needle_core::PredicateContract],
) -> Digest {
    let mut hasher = CanonicalHasher::new(b"semantic-validator-definition");
    hasher.field_str(needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID);
    hasher.field_u32(SEMANTIC_VALIDATOR_REVISION);
    hasher.field_str(&kind.0);
    if let Some(contract) = contracts.iter().find(|item| item.predicate == predicate) {
        hasher.field_digest(contract.definition_digest);
    }
    hasher.finish()
}

pub fn artifact_and_certificate_are_fresh(
    artifact: &Artifact,
    certificate: &ArtifactValidationCertificate,
    repository_root: &Path,
) -> bool {
    if artifact.contract.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
        || artifact.contract.cache_scope != artifact.dependency_manifest.scope
        || (artifact.contract.cache_scope == CacheScope::WorktreeSemantic
            && !artifact.dependency_manifest.supports_worktree_semantic())
        || certificate.artifact.digest() != artifact.id
        || certificate.validator_definition != validator_definition(&artifact.contract.kind)
        || certificate.coverage.dependency_manifest_digest
            != manifest_digest(&artifact.dependency_manifest)
        || certificate.dependency_checks_digest != certificate.coverage.dependency_manifest_digest
        || certificate.id
            != validation_certificate_id(
                certificate.artifact,
                &certificate.input_artifacts,
                &certificate.evidence_ids,
                &certificate.coverage,
                certificate.validator_definition,
                certificate.test_plan_evidence,
            )
    {
        return false;
    }
    let Ok(payload) = serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone())
    else {
        return false;
    };
    if payload.canonical_artifact_id(artifact.contract.definition_digest).map(|id| id.digest())
        != Some(artifact.id)
    {
        return false;
    }
    artifact
        .dependency_manifest
        .dependencies
        .iter()
        .all(|dependency| dependency_is_fresh(repository_root, dependency))
}

pub(crate) fn dependency_is_fresh(repository_root: &Path, dependency: &Dependency) -> bool {
    read_evidence(repository_root, &dependency.path).is_ok_and(|bytes| {
        let range_is_valid = match (dependency.byte_start, dependency.byte_end) {
            (None, None) => true,
            (Some(start), Some(end)) => {
                start < end && usize::try_from(end).is_ok_and(|end| end <= bytes.len())
            }
            _ => false,
        };
        range_is_valid && Digest::blake3(bytes) == dependency.content_digest
    })
}

fn derive_coverage(
    artifact: &SemanticWorkerArtifact,
    subject: &needle_core::Subject,
    repository_root: &Path,
    test_plan_proof: Option<&TestPlanProof>,
) -> Result<Option<Obligation>, SemanticValidationError> {
    match artifact {
        SemanticWorkerArtifact::CodeLocation { locations, gaps } => Ok((gaps.is_empty()
            && locations.iter().any(|location| {
                location.role == needle_core::LocationRole::Primary
                    && implementation_location_path_allowed(&location.path, subject.kind)
                    && (location
                        .byte_start
                        .zip(location.byte_end)
                        .is_some_and(|(start, end)| start < end)
                        || (location.byte_start.is_none()
                            && location.byte_end.is_none()
                            && location_symbol_is_observed(repository_root, location)))
                    && location_matches_subject(repository_root, location, subject, true)
                        .unwrap_or(false)
            }))
        .then(|| {
            Obligation::new(
                PredicateKind::ImplementationLocation,
                subject.id,
                facets(&[
                    ("granularity", "exact-location"),
                    ("polarity", "positive"),
                    ("selection", "primary"),
                ]),
            )
        })),
        SemanticWorkerArtifact::BehaviorTrace { scenario, steps, gaps } => {
            let canonical_scenario = canonical_runtime_scenario(scenario);
            let roles = steps.iter().map(|step| step.role).collect::<BTreeSet<_>>();
            let complete = [
                FlowStepRole::Producer,
                FlowStepRole::Carrier,
                FlowStepRole::Transformation,
                FlowStepRole::Precedence,
                FlowStepRole::Consumer,
            ]
            .iter()
            .all(|role| roles.contains(role));
            let tied_to_subject = steps.iter().any(|step| {
                location_matches_subject(repository_root, &step.location, subject, false)
                    .unwrap_or(false)
            });
            let all_symbols_observed = steps
                .iter()
                .all(|step| location_symbol_is_observed(repository_root, &step.location));
            Ok((!steps.is_empty()
                && complete
                && gaps.is_empty()
                && tied_to_subject
                && all_symbols_observed
                && canonical_scenario.is_some())
            .then(|| {
                Obligation::new(
                    PredicateKind::RuntimeFlow,
                    subject.id,
                    facets(&[
                        ("completeness", "contract-complete"),
                        ("granularity", "stepwise"),
                        ("scenario", canonical_scenario.expect("checked canonical scenario")),
                    ]),
                )
            }))
        }
        SemanticWorkerArtifact::TestPlan {
            runner,
            argv,
            cwd_relative,
            identifiers,
            selection,
            evidence_paths,
        } => Ok((runner == "cargo"
            && argv.first().map(String::as_str) == Some("cargo")
            && argv.get(1).map(String::as_str) == Some("test")
            && is_safe_relative_path(cwd_relative)
            && !identifiers.is_empty()
            && !evidence_paths.is_empty()
            && test_plan_proof.is_some_and(test_plan_matches_subject))
        .then(|| {
            Obligation::new(
                PredicateKind::FocusedTests,
                subject.id,
                facets(&[
                    ("completeness", "open-world"),
                    ("polarity", "positive"),
                    ("selection", selection),
                ]),
            )
        })),
    }
}

fn canonical_runtime_scenario(scenario: &str) -> Option<&'static str> {
    let scenario = scenario.trim();
    let bytes = scenario.as_bytes();
    let prefix = bytes.get(..7)?;
    let remainder = &bytes[7..];
    if !prefix.eq_ignore_ascii_case(b"default") {
        return None;
    }
    if remainder.is_empty()
        || remainder
            .first()
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b':' | b'-' | b'('))
    {
        Some("default")
    } else {
        None
    }
}

fn location_matches_subject(
    repository_root: &Path,
    location: &needle_core::SemanticLocation,
    subject: &needle_core::Subject,
    range_scoped: bool,
) -> Result<bool, SemanticValidationError> {
    if subject.kind == needle_core::SubjectKind::File {
        let normalized = location.path.replace('\\', "/");
        let canonical = subject.canonical_name.replace('\\', "/");
        return Ok(normalized == canonical);
    }
    let bytes = read_evidence(repository_root, &location.path)?;
    if !range_scoped {
        return Ok(contains_bytes(&bytes, subject.canonical_name.as_bytes()));
    }
    let (start, end) = location
        .byte_start
        .zip(location.byte_end)
        .and_then(|(start, end)| Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?)))
        .unwrap_or((0, bytes.len()));
    let Some(evidence) = bytes.get(start..end) else {
        return Ok(false);
    };
    Ok(contains_bytes(evidence, subject.canonical_name.as_bytes()))
}

fn implementation_location_path_allowed(
    path: &str,
    subject_kind: needle_core::SubjectKind,
) -> bool {
    subject_kind == needle_core::SubjectKind::Test || !is_test_source_path(path)
}

fn is_test_source_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|component| component.eq_ignore_ascii_case("tests"))
}

fn test_plan_matches_subject(proof: &TestPlanProof) -> bool {
    proof.identifier_observed && (proof.identifier_names_subject || proof.subject_observed)
}

fn location_symbol_is_observed(
    repository_root: &Path,
    location: &needle_core::SemanticLocation,
) -> bool {
    let Some(symbol) = location.symbol.as_deref() else {
        return false;
    };
    let Ok(bytes) = read_evidence(repository_root, &location.path) else {
        return false;
    };
    symbol
        .split("::")
        .filter(|segment| !segment.is_empty())
        .all(|segment| contains_bytes(&bytes, segment.as_bytes()))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn is_safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::CurDir | Component::Normal(_)))
}

fn facets(values: &[(&str, &str)]) -> Vec<Facet> {
    values
        .iter()
        .map(|(key, value)| Facet { key: (*key).to_owned(), value: (*value).to_owned() })
        .collect()
}

fn evidence_labels(artifact: &SemanticWorkerArtifact) -> Vec<String> {
    let mut labels = match artifact {
        SemanticWorkerArtifact::CodeLocation { locations, .. } => locations
            .iter()
            .map(|location| {
                format!("{}:{}", location.path, location.symbol.as_deref().unwrap_or_default())
            })
            .collect(),
        SemanticWorkerArtifact::BehaviorTrace { steps, .. } => steps
            .iter()
            .map(|step| {
                format!(
                    "{}:{}",
                    step.location.path,
                    step.location.symbol.as_deref().unwrap_or_default()
                )
            })
            .collect(),
        SemanticWorkerArtifact::TestPlan { evidence_paths, .. } => evidence_paths.clone(),
    };
    labels.sort();
    labels.dedup();
    labels
}

fn dependency_manifest(
    artifact: &SemanticWorkerArtifact,
    coverage: &[CoverageEntry],
    repository_root: &Path,
    observation_trace: Option<&WorkerObservationTrace>,
    test_plan_proof: Option<&TestPlanProof>,
) -> Result<DependencyManifest, SemanticValidationError> {
    let claims = coverage.iter().map(|entry| entry.obligation.id.to_string()).collect::<Vec<_>>();
    if matches!(artifact, SemanticWorkerArtifact::TestPlan { .. }) {
        let Some(proof) = test_plan_proof else {
            return Err(SemanticValidationError::Evidence(
                "test-plan proof evidence was not validated".to_owned(),
            ));
        };
        let dependencies = proof
            .files
            .iter()
            .map(|file| Dependency {
                path: file.path.clone(),
                content_digest: file.content_digest,
                byte_start: None,
                byte_end: None,
                claims: claims.clone(),
            })
            .collect::<Vec<_>>();
        return Ok(DependencyManifest {
            scope: CacheScope::WorktreeSemantic,
            observed_files_complete: true,
            dependencies,
            gaps: Vec::new(),
        });
    }

    let mut paths = match artifact {
        SemanticWorkerArtifact::CodeLocation { locations, .. } => {
            locations.iter().map(|location| location.path.clone()).collect::<BTreeSet<_>>()
        }
        SemanticWorkerArtifact::BehaviorTrace { steps, .. } => {
            steps.iter().map(|step| step.location.path.clone()).collect::<BTreeSet<_>>()
        }
        SemanticWorkerArtifact::TestPlan { .. } => unreachable!("handled above"),
    };
    if let Some(trace) = observation_trace {
        paths.extend(trace.observed_files.iter().cloned());
    }
    let mut dependencies = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = read_evidence(repository_root, &path)?;
        dependencies.push(Dependency {
            path,
            content_digest: Digest::blake3(&bytes),
            byte_start: None,
            byte_end: None,
            claims: claims.clone(),
        });
    }
    if dependencies.is_empty() {
        return Err(SemanticValidationError::Evidence(
            "artifact has no repository evidence".to_owned(),
        ));
    }
    let trace_complete = observation_trace.is_none_or(|trace| trace.gaps.is_empty());
    Ok(DependencyManifest {
        scope: if trace_complete {
            CacheScope::WorktreeSemantic
        } else {
            CacheScope::SnapshotExact
        },
        observed_files_complete: trace_complete,
        dependencies,
        gaps: observation_trace.map_or_else(Vec::new, |trace| trace.gaps.clone()),
    })
}

fn validate_artifact_locations(
    artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
) -> Result<(), SemanticValidationError> {
    match artifact {
        SemanticWorkerArtifact::CodeLocation { locations, .. } => {
            for location in locations {
                validate_location(repository_root, location)?;
            }
        }
        SemanticWorkerArtifact::BehaviorTrace { steps, .. } => {
            for step in steps {
                validate_location(repository_root, &step.location)?;
            }
        }
        SemanticWorkerArtifact::TestPlan { .. } => {}
    }
    Ok(())
}

fn validate_location(
    repository_root: &Path,
    location: &needle_core::SemanticLocation,
) -> Result<(), SemanticValidationError> {
    let bytes = read_evidence(repository_root, &location.path)?;
    if location.byte_start.is_some() != location.byte_end.is_some() {
        return Err(SemanticValidationError::Evidence(format!(
            "partial byte range for {}",
            location.path
        )));
    }
    if let Some((start, end)) = location.byte_start.zip(location.byte_end)
        && (start >= end || end > bytes.len().try_into().unwrap_or(u64::MAX))
    {
        return Err(SemanticValidationError::Evidence(format!(
            "invalid byte range for {}",
            location.path
        )));
    }
    Ok(())
}

fn read_evidence(
    repository_root: &Path,
    relative: &str,
) -> Result<Vec<u8>, SemanticValidationError> {
    let root = fs::canonicalize(repository_root).map_err(|error| {
        SemanticValidationError::Evidence(format!("cannot resolve repository: {error}"))
    })?;
    read_evidence_from_root(&root, relative)
}

fn read_evidence_from_root(
    canonical_repository_root: &Path,
    relative: &str,
) -> Result<Vec<u8>, SemanticValidationError> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(SemanticValidationError::Evidence(format!("unsafe evidence path {relative}")));
    }
    let path =
        fs::canonicalize(canonical_repository_root.join(relative_path)).map_err(|error| {
            SemanticValidationError::Evidence(format!("cannot resolve {relative}: {error}"))
        })?;
    if !path.starts_with(canonical_repository_root) || !path.is_file() {
        return Err(SemanticValidationError::Evidence(format!(
            "evidence path escapes repository: {relative}"
        )));
    }
    fs::read(path).map_err(|error| {
        SemanticValidationError::Evidence(format!("cannot read {relative}: {error}"))
    })
}

pub fn manifest_digest(manifest: &DependencyManifest) -> Digest {
    let mut dependencies = manifest.dependencies.iter().collect::<Vec<_>>();
    dependencies.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hasher = CanonicalHasher::new(b"dependency-manifest");
    hasher.field_u8(match manifest.scope {
        CacheScope::SnapshotExact => 0,
        CacheScope::WorktreeSemantic => 1,
    });
    hasher.field_u8(u8::from(manifest.observed_files_complete));
    for dependency in dependencies {
        hasher.field_str(&dependency.path);
        hasher.field_digest(dependency.content_digest);
        for claim in &dependency.claims {
            hasher.field_str(claim);
        }
    }
    for gap in &manifest.gaps {
        hasher.field_str(gap);
    }
    hasher.finish()
}

pub(crate) fn validation_certificate_id(
    artifact: ArtifactId,
    input_artifacts: &[ArtifactId],
    evidence_ids: &[String],
    coverage: &CoverageManifest,
    validator_definition: Digest,
    test_plan_evidence: Option<TestPlanEvidenceStatus>,
) -> ArtifactValidationCertificateId {
    let mut hasher = CanonicalHasher::new(b"artifact-validation-certificate");
    hasher.field_digest(artifact.digest());
    hasher.field_digest(validator_definition);
    for input in input_artifacts {
        hasher.field_digest(input.digest());
    }
    for evidence_id in evidence_ids {
        hasher.field_str(evidence_id);
    }
    if let Some(status) = test_plan_evidence {
        hasher.field_u8(match status {
            TestPlanEvidenceStatus::Located => 0,
            TestPlanEvidenceStatus::Executed => 1,
        });
    }
    hasher.field_digest(coverage.world.id());
    hasher.field_digest(coverage.dependency_manifest_digest);
    for entry in &coverage.entries {
        hasher.field_digest(entry.obligation.id.digest());
        for evidence in &entry.evidence {
            hasher.field_str(evidence);
        }
    }
    ArtifactValidationCertificateId(hasher.finish())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        ClaimKind, ClaimPayload, CommandExecutionEvidence, Need, NeedId, Obligation,
        SemanticArtifactResult, SemanticFlowStep, SemanticLocation, SemanticWorld, Subject,
        SubjectKind, TestPlan, need_fragment,
    };
    use std::collections::BTreeMap;

    #[test]
    fn coverage_is_derived_and_freshness_detects_mutation() {
        let root = std::env::temp_dir().join(format!(
            "needle-semantic-validation-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        fs::write(root.join("src/unrelated.rs"), "pub fn unrelated() {}\n").unwrap();
        fs::write(root.join("tests/answer.rs"), "#[test]\nfn answer() { assert_eq!(42, 42); }\n")
            .unwrap();
        let repository_lineage = Digest::blake3(b"repo");
        let subject = Subject::exact(repository_lineage, SubjectKind::Symbol, "answer");
        let obligation = Obligation::new(
            PredicateKind::ImplementationLocation,
            subject.id,
            vec![
                Facet { key: "granularity".to_owned(), value: "exact-location".to_owned() },
                Facet { key: "polarity".to_owned(), value: "positive".to_owned() },
                Facet { key: "selection".to_owned(), value: "primary".to_owned() },
            ],
        );
        let need = Need {
            id: NeedId(Digest::blake3(b"need")),
            subjects: vec![subject.clone()],
            required: vec![obligation],
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
        };
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let result = SemanticArtifactResult {
            schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: vec![SemanticWorkerArtifact::CodeLocation {
                locations: vec![SemanticLocation {
                    role: needle_core::LocationRole::Primary,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(29),
                }],
                gaps: Vec::new(),
            }],
            observation_trace: Default::default(),
            artifact_traces: BTreeMap::new(),
        };
        let validated =
            validate_semantic_result(&fragment, &result, &root, Digest::blake3(b"origin")).unwrap();
        assert_eq!(validated[0].certificate.coverage.entries.len(), 1);
        assert_eq!(validated[0].claims.claims.len(), 1);
        assert_eq!(validated[0].claims.claims[0].kind, ClaimKind::ImplementationLocation);
        assert_eq!(validated[0].claims.certificates[0].subject, subject.id);
        assert_eq!(validated[0].claims.certificates[0].dependencies.len(), 1);
        let unrelated_location = SemanticArtifactResult {
            schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: vec![SemanticWorkerArtifact::CodeLocation {
                locations: vec![SemanticLocation {
                    role: needle_core::LocationRole::Primary,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some("different".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(3),
                }],
                gaps: Vec::new(),
            }],
            observation_trace: Default::default(),
            artifact_traces: BTreeMap::new(),
        };
        assert!(matches!(
            validate_semantic_result(
                &fragment,
                &unrelated_location,
                &root,
                Digest::blake3(b"unrelated-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));

        let exact_symbol_without_range = SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: needle_core::LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: None,
                byte_end: None,
            }],
            gaps: Vec::new(),
        };
        let incomplete_trace = WorkerObservationTrace {
            observed_files: vec!["src/lib.rs".to_owned()],
            gaps: vec!["unknown_command_action".to_owned()],
        };
        let snapshot_exact = validate_semantic_artifact_with_trace(
            &fragment,
            &exact_symbol_without_range,
            &root,
            Digest::blake3(b"snapshot-exact-origin"),
            Some(&incomplete_trace),
        )
        .unwrap();
        assert_eq!(snapshot_exact.artifact.contract.cache_scope, CacheScope::SnapshotExact);
        assert!(!snapshot_exact.artifact.dependency_manifest.observed_files_complete);
        assert_eq!(
            snapshot_exact.artifact.dependency_manifest.gaps,
            vec!["unknown_command_action"]
        );
        assert!(artifact_and_certificate_are_fresh(
            &snapshot_exact.artifact,
            &snapshot_exact.certificate,
            &root
        ));
        let mut scope_tampered = snapshot_exact.artifact.clone();
        scope_tampered.contract.cache_scope = CacheScope::WorktreeSemantic;
        assert!(!artifact_and_certificate_are_fresh(
            &scope_tampered,
            &snapshot_exact.certificate,
            &root
        ));

        let partial_range = SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: needle_core::LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: Some(0),
                byte_end: None,
            }],
            gaps: Vec::new(),
        };
        assert!(matches!(
            validate_semantic_artifact(
                &fragment,
                &partial_range,
                &root,
                Digest::blake3(b"partial-range-origin")
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("partial byte range")
        ));
        let forged_subject_symbol = SemanticArtifactResult {
            schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: vec![SemanticWorkerArtifact::CodeLocation {
                locations: vec![SemanticLocation {
                    role: needle_core::LocationRole::Primary,
                    path: "src/unrelated.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(20),
                }],
                gaps: Vec::new(),
            }],
            observation_trace: Default::default(),
            artifact_traces: BTreeMap::new(),
        };
        assert!(matches!(
            validate_semantic_result(
                &fragment,
                &forged_subject_symbol,
                &root,
                Digest::blake3(b"forged-subject-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));
        let test_location_for_implementation = SemanticArtifactResult {
            schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: vec![SemanticWorkerArtifact::CodeLocation {
                locations: vec![SemanticLocation {
                    role: needle_core::LocationRole::Primary,
                    path: "tests/answer.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(44),
                }],
                gaps: Vec::new(),
            }],
            observation_trace: Default::default(),
            artifact_traces: BTreeMap::new(),
        };
        assert!(matches!(
            validate_semantic_result(
                &fragment,
                &test_location_for_implementation,
                &root,
                Digest::blake3(b"test-location-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));
        assert!(implementation_location_path_allowed("tests/answer.rs", SubjectKind::Test));
        let runtime_fragment = need_fragment(
            &need,
            vec![Obligation::new(
                PredicateKind::RuntimeFlow,
                subject.id,
                vec![
                    Facet { key: "completeness".to_owned(), value: "contract-complete".to_owned() },
                    Facet { key: "granularity".to_owned(), value: "stepwise".to_owned() },
                    Facet { key: "scenario".to_owned(), value: "default".to_owned() },
                ],
            )],
            Vec::new(),
        );
        let forged_flow = SemanticWorkerArtifact::BehaviorTrace {
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
            .map(|(index, role)| SemanticFlowStep {
                role,
                location: SemanticLocation {
                    role: needle_core::LocationRole::Supporting,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some(if index == 3 {
                        "invented_symbol".to_owned()
                    } else {
                        "answer".to_owned()
                    }),
                    byte_start: None,
                    byte_end: None,
                },
                description: format!("{role:?}"),
            })
            .collect(),
            gaps: Vec::new(),
        };
        assert!(matches!(
            validate_semantic_artifact(
                &runtime_fragment,
                &forged_flow,
                &root,
                Digest::blake3(b"forged-flow-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));
        let focused_fragment = need_fragment(
            &need,
            vec![Obligation::new(
                PredicateKind::FocusedTests,
                subject.id,
                vec![
                    Facet { key: "completeness".to_owned(), value: "open-world".to_owned() },
                    Facet { key: "polarity".to_owned(), value: "positive".to_owned() },
                    Facet { key: "selection".to_owned(), value: "representative".to_owned() },
                ],
            )],
            Vec::new(),
        );
        let forged_test = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "invented_test".to_owned()],
            cwd_relative: ".".to_owned(),
            identifiers: vec!["invented_test".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec!["src/lib.rs".to_owned()],
        };
        assert!(matches!(
            validate_semantic_artifact_with_trace(
                &focused_fragment,
                &forged_test,
                &root,
                Digest::blake3(b"forged-test-origin"),
                Some(&WorkerObservationTrace {
                    observed_files: vec!["src/lib.rs".to_owned()],
                    gaps: Vec::new(),
                })
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("test identifier is absent")
        ));
        assert!(artifact_and_certificate_are_fresh(
            &validated[0].artifact,
            &validated[0].certificate,
            &root
        ));
        fs::write(root.join("src/unrelated.rs"), "pub fn unrelated() { let _ = 1; }\n").unwrap();
        assert!(artifact_and_certificate_are_fresh(
            &validated[0].artifact,
            &validated[0].certificate,
            &root
        ));
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 7 }\n").unwrap();
        assert!(!artifact_and_certificate_are_fresh(
            &validated[0].artifact,
            &validated[0].certificate,
            &root
        ));
        let insufficient = need_fragment(
            &need,
            vec![Obligation::new(
                PredicateKind::RuntimeFlow,
                subject.id,
                vec![Facet { key: "scenario".to_owned(), value: "default".to_owned() }],
            )],
            Vec::new(),
        );
        assert!(matches!(
            validate_semantic_result(
                &insufficient,
                &result,
                &root,
                Digest::blake3(b"other-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_runtime_scenario_accepts_a_bounded_description_but_not_another_scenario() {
        let root = std::env::temp_dir().join(format!(
            "needle-semantic-scenario-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();

        let repository_lineage = Digest::blake3(b"scenario-repo");
        let subject = Subject::exact(repository_lineage, SubjectKind::Symbol, "answer");
        let obligation = Obligation::new(
            PredicateKind::RuntimeFlow,
            subject.id,
            vec![
                Facet { key: "completeness".to_owned(), value: "contract-complete".to_owned() },
                Facet { key: "granularity".to_owned(), value: "stepwise".to_owned() },
                Facet { key: "scenario".to_owned(), value: "default".to_owned() },
            ],
        );
        let need = Need {
            id: NeedId(Digest::blake3(b"scenario-need")),
            subjects: vec![subject],
            required: vec![obligation],
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
            body_digest: Digest::blake3(b"scenario-body"),
            format_revision: 1,
        };
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let trace = |scenario: &str| SemanticWorkerArtifact::BehaviorTrace {
            scenario: scenario.to_owned(),
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
                    role: needle_core::LocationRole::Supporting,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: None,
                    byte_end: None,
                },
                description: format!("{role:?}"),
            })
            .collect(),
            gaps: Vec::new(),
        };

        let validated = validate_semantic_artifact(
            &fragment,
            &trace("Default CLI search configuration and the --crlf-enabled CRLF search path"),
            &root,
            Digest::blake3(b"default-described-origin"),
        )
        .unwrap();
        let scenario = validated.certificate.coverage.entries[0]
            .obligation
            .facets
            .iter()
            .find(|facet| facet.key == "scenario")
            .map(|facet| facet.value.as_str());
        assert_eq!(scenario, Some("default"));
        assert_eq!(validated.claims.claims.len(), 5);
        assert!(
            validated
                .claims
                .claims
                .iter()
                .all(|claim| claim.kind == ClaimKind::RuntimeFlowStep && claim.is_canonical())
        );
        assert_eq!(validated.claims.relations.len(), 4);
        assert!(validated.claims.relations.iter().all(|relation| relation.is_canonical()));
        let flow_anchors = validated
            .claims
            .claims
            .iter()
            .filter_map(|claim| match &claim.payload {
                ClaimPayload::RuntimeFlowStep { flow_anchor, .. } => Some(*flow_anchor),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(flow_anchors.len(), 1);

        assert!(matches!(
            validate_semantic_artifact(
                &fragment,
                &trace("error recovery"),
                &root,
                Digest::blake3(b"different-scenario-origin")
            ),
            Err(SemanticValidationError::Insufficient(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parent_owned_test_plan_supports_optional_execution_evidence_and_exact_binding() {
        let root = std::env::temp_dir().join(format!(
            "needle-semantic-test-plan-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests/misc.rs"), "mod feature;\n").unwrap();
        fs::write(
            root.join("tests/feature.rs"),
            "// --glob-case-insensitive\n#[test]\nfn glob_always_case_insensitive() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"integration\"\npath = \"tests/misc.rs\"\n",
        )
        .unwrap();
        let repository_lineage = Digest::blake3(b"test-plan-repo");
        let subject =
            Subject::exact(repository_lineage, SubjectKind::CliOption, "--glob-case-insensitive");
        let obligation = Obligation::new(
            PredicateKind::FocusedTests,
            subject.id,
            facets(&[
                ("completeness", "open-world"),
                ("polarity", "positive"),
                ("selection", "representative"),
            ]),
        );
        let need = Need {
            id: NeedId(Digest::blake3(b"test-plan-need")),
            subjects: vec![subject],
            required: vec![obligation],
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
        };
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let argv = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--test".to_owned(),
            "integration".to_owned(),
            "misc::glob_always_case_insensitive".to_owned(),
            "--".to_owned(),
            "--exact".to_owned(),
        ];
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: argv.clone(),
            cwd_relative: ".".to_owned(),
            test_identifier: "misc::glob_always_case_insensitive".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let artifact = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: argv.clone(),
            cwd_relative: ".".to_owned(),
            identifiers: vec!["misc::glob_always_case_insensitive".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec!["Cargo.toml".to_owned(), "tests/feature.rs".to_owned()],
        };
        let untraced = validate_semantic_artifact(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"untraced-origin"),
        )
        .unwrap();
        assert_eq!(untraced.certificate.test_plan_evidence, Some(TestPlanEvidenceStatus::Located));
        assert_eq!(untraced.claims.claims.len(), 1);
        assert_eq!(untraced.claims.claims[0].kind, ClaimKind::FocusedTest);
        assert_eq!(untraced.claims.certificates[0].dependencies.len(), 3);
        assert_eq!(untraced.artifact.dependency_manifest.scope, CacheScope::WorktreeSemantic);
        assert!(untraced.artifact.dependency_manifest.gaps.is_empty());
        let discovered = validate_semantic_artifact_with_trace(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"worker-discovered-origin"),
            Some(&WorkerObservationTrace {
                observed_files: vec!["unrelated/search-only.rs".to_owned()],
                gaps: Vec::new(),
            }),
        )
        .unwrap();
        assert_eq!(
            discovered.certificate.test_plan_evidence,
            Some(TestPlanEvidenceStatus::Located)
        );
        let discovered_after_search_and_exact_read = validate_semantic_artifact_with_trace(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"worker-discovered-after-search-origin"),
            Some(&WorkerObservationTrace {
                observed_files: Vec::new(),
                gaps: vec!["search_result_closure_unproven".to_owned()],
            }),
        )
        .unwrap();
        assert_eq!(
            discovered_after_search_and_exact_read.certificate.test_plan_evidence,
            Some(TestPlanEvidenceStatus::Located)
        );
        let discovered_with_unparseable_command_trace = validate_semantic_artifact_with_trace(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"worker-discovered-unknown-gap-origin"),
            Some(&WorkerObservationTrace {
                observed_files: Vec::new(),
                gaps: vec!["unknown_command_action".to_owned()],
            }),
        )
        .unwrap();
        assert_eq!(
            discovered_with_unparseable_command_trace
                .artifact
                .dependency_manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.path.as_str())
                .collect::<Vec<_>>(),
            ["Cargo.toml", "tests/feature.rs", "tests/misc.rs"]
        );
        assert!(
            discovered_with_unparseable_command_trace.artifact.dependency_manifest.gaps.is_empty()
        );
        fs::create_dir_all(root.join("unrelated")).unwrap();
        fs::write(root.join("unrelated/search-only.rs"), "pub fn search_only() {}\n").unwrap();
        assert!(artifact_and_certificate_are_fresh(
            &discovered_with_unparseable_command_trace.artifact,
            &discovered_with_unparseable_command_trace.certificate,
            &root
        ));
        fs::write(
            root.join("tests/feature.rs"),
            "// --glob-case-insensitive\n#[test]\nfn glob_always_case_insensitive() { let _ = 1; }\n",
        )
        .unwrap();
        assert!(!artifact_and_certificate_are_fresh(
            &discovered_with_unparseable_command_trace.artifact,
            &discovered_with_unparseable_command_trace.certificate,
            &root
        ));
        fs::write(
            root.join("tests/feature.rs"),
            "// --glob-case-insensitive\n#[test]\nfn glob_always_case_insensitive() {}\n",
        )
        .unwrap();
        fs::write(root.join("tests/misc.rs"), "mod changed;\n").unwrap();
        assert!(!artifact_and_certificate_are_fresh(
            &discovered_with_unparseable_command_trace.artifact,
            &discovered_with_unparseable_command_trace.certificate,
            &root
        ));
        fs::write(root.join("tests/misc.rs"), "mod feature;\n").unwrap();
        let malicious = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "misc".to_owned(), ";".to_owned()],
            cwd_relative: "..".to_owned(),
            identifiers: vec!["misc".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec![
                "Cargo.toml".to_owned(),
                "tests/misc.rs".to_owned(),
                "tests/feature.rs".to_owned(),
            ],
        };
        assert!(matches!(
            validate_semantic_artifact_with_trace(
                &fragment,
                &malicious,
                &root,
                Digest::blake3(b"malicious-origin"),
                Some(&WorkerObservationTrace {
                    observed_files: vec!["tests/misc.rs".to_owned()],
                    gaps: Vec::new(),
                }),
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("argv_contains_shell_syntax")
                    && message.contains("cwd_not_safe_relative")
        ));
        let non_canonical = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: argv[1..].to_vec(),
            cwd_relative: ".".to_owned(),
            identifiers: vec!["misc::glob_always_case_insensitive".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec![
                "Cargo.toml".to_owned(),
                "tests/misc.rs".to_owned(),
                "tests/feature.rs".to_owned(),
            ],
        };
        assert!(matches!(
            validate_semantic_artifact_with_trace(
                &fragment,
                &non_canonical,
                &root,
                Digest::blake3(b"non-canonical-origin"),
                Some(&WorkerObservationTrace {
                    observed_files: vec!["tests/misc.rs".to_owned()],
                    gaps: Vec::new(),
                }),
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("argv_not_canonical")
        ));
        let fabricated_identifier = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "--test".to_owned(),
                "integration".to_owned(),
                "misc::fabricated".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            identifiers: vec!["misc::fabricated".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec![
                "Cargo.toml".to_owned(),
                "tests/misc.rs".to_owned(),
                "tests/feature.rs".to_owned(),
            ],
        };
        assert!(matches!(
            validate_semantic_artifact(
                &fragment,
                &fabricated_identifier,
                &root,
                Digest::blake3(b"fabricated-identifier-origin")
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("test identifier is absent")
        ));
        let fabricated_target = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "--test".to_owned(),
                "fabricated-target".to_owned(),
                "misc::glob_always_case_insensitive".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            identifiers: vec!["misc::glob_always_case_insensitive".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec![
                "Cargo.toml".to_owned(),
                "tests/misc.rs".to_owned(),
                "tests/feature.rs".to_owned(),
            ],
        };
        assert!(matches!(
            validate_semantic_artifact(
                &fragment,
                &fabricated_target,
                &root,
                Digest::blake3(b"fabricated-target-origin")
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("cargo test target is absent")
        ));
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"integration\"\npath = \"../escape.rs\"\n",
        )
        .unwrap();
        assert!(matches!(
            validate_semantic_artifact(
                &fragment,
                &artifact,
                &root,
                Digest::blake3(b"escaping-target-origin")
            ),
            Err(SemanticValidationError::Evidence(message))
                if message.contains("target escapes the repository")
        ));
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"integration\"\npath = \"tests/misc.rs\"\n",
        )
        .unwrap();
        let validated_without_execution = validate_semantic_test_plan(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"unexecuted-origin"),
            None,
            &plan,
        )
        .unwrap();
        assert!(validated_without_execution.certificate.evidence_ids.is_empty());
        assert_eq!(
            validated_without_execution.certificate.test_plan_evidence,
            Some(TestPlanEvidenceStatus::Located)
        );
        assert!(artifact_and_certificate_are_fresh(
            &validated_without_execution.artifact,
            &validated_without_execution.certificate,
            &root
        ));
        let command_evidence = CommandExecutionEvidence {
            id: "command-evidence-r43".to_owned(),
            approval_id: "approval-r43".to_owned(),
            argv,
            cwd: root.display().to_string(),
            source_snapshot_digest: Digest::blake3(b"snapshot"),
            runner: "cargo".to_owned(),
            runner_version: Some("cargo 1.90".to_owned()),
            exit_status: Some(0),
            duration_ms: 1,
            output_digest: Digest::blake3(b"test output"),
            output_preview:
                "test misc::glob_always_case_insensitive ... ok\ntest result: ok. 1 passed"
                    .to_owned(),
            test_identifier: Some("misc::glob_always_case_insensitive".to_owned()),
            tests_executed: Some(1),
            infrastructure_failure: None,
        };
        let validated = validate_semantic_test_plan_with_evidence(
            &fragment,
            &artifact,
            &root,
            Digest::blake3(b"verified-origin"),
            None,
            &plan,
            &command_evidence,
        )
        .unwrap();
        assert_eq!(validated.certificate.evidence_ids, ["command-evidence-r43"]);
        assert_eq!(
            validated.certificate.test_plan_evidence,
            Some(TestPlanEvidenceStatus::Executed)
        );
        assert!(artifact_and_certificate_are_fresh(
            &validated.artifact,
            &validated.certificate,
            &root
        ));
        let mut tampered = validated.certificate.clone();
        tampered.evidence_ids[0] = "different-evidence".to_owned();
        assert!(!artifact_and_certificate_are_fresh(&validated.artifact, &tampered, &root));
        fs::remove_dir_all(root).unwrap();
    }
}
