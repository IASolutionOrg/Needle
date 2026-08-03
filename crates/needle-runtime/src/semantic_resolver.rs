use crate::store::now_ms;
use crate::{
    ClaimAdvisoryPlan, ClaimProofError, ClaimProofMaterial, ProofAccountingRecord, ProofCandidate,
    ProofError, ProofPlan, ProofPlanner, ProofResolutionKind, RuntimeStore, SemanticCostEstimates,
    StoreError, artifact_and_certificate_are_fresh, build_claim_component_certificate,
    replay_claim_set_certificate,
};
use needle_core::{
    Artifact, ArtifactValidationCertificate, CacheResolution, CacheScope, CapabilityMode,
    ClaimSetCertificate, MAX_DERIVATION_DEPTH, Need, NeedKey, PredicateKind,
    ReuseSufficiencyCertificate, ReuseUnit, SelectedPlan, built_in_capability_classes,
    built_in_claim_capability_classes,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

pub const PROOF_RESOLUTION_FORMAT_REVISION: u16 = 1;
pub const CLAIM_PROOF_RESOLUTION_FORMAT_REVISION: u16 = 2;

#[derive(Clone, Debug)]
pub struct SemanticReuseDecision {
    pub resolution: CacheResolution,
    /// Proof-valid resolution before the economics gate. This is retained for
    /// deterministic experiment calibration and is never authoritative by itself.
    pub validated_resolution: Option<CacheResolution>,
    pub plan: Option<SelectedPlan>,
    pub certificate: Option<ReuseSufficiencyCertificate>,
    /// Claim-aware validity plan retained for audit. Only the bounded
    /// single-claim authority class can consume it in production.
    pub claim_advisory: Option<ClaimAdvisoryPlan>,
    pub claim_certificate: Option<ClaimSetCertificate>,
    pub claim_material: Option<ClaimProofMaterial>,
    pub artifacts: Vec<Artifact>,
    pub authoritative: bool,
    pub calibration_eligible: bool,
    pub stale_candidates: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticResolverError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Proof(#[from] ProofError),
}

pub struct SemanticResolver {
    store: RuntimeStore,
    planner: ProofPlanner,
}

struct ClaimSelection {
    advisory: ClaimAdvisoryPlan,
    material: ClaimProofMaterial,
    candidates: Vec<ProofCandidate>,
}

impl SemanticResolver {
    pub fn new(store: RuntimeStore) -> Self {
        Self { store, planner: ProofPlanner::new() }
    }

    pub fn resolve(
        &self,
        need: &Need,
        repository_root: &Path,
        source_snapshot_digest: needle_core::Digest,
        expected_fresh_microusd: Option<u64>,
        expected_reuse_microusd: Option<u64>,
        exact_request_ids: &[needle_core::Digest],
    ) -> Result<SemanticReuseDecision, SemanticResolverError> {
        let route_key = inferred_route_key(need);
        self.resolve_for_route(
            need,
            &route_key,
            repository_root,
            source_snapshot_digest,
            SemanticCostEstimates {
                fresh_microusd: expected_fresh_microusd,
                artifact_reuse_microusd: expected_reuse_microusd,
                claim_reuse_microusd: None,
                claim_partial_reuse_microusd: None,
            },
            exact_request_ids,
        )
    }

    pub fn resolve_for_route(
        &self,
        need: &Need,
        route_key: &NeedKey,
        repository_root: &Path,
        source_snapshot_digest: needle_core::Digest,
        costs: SemanticCostEstimates,
        exact_request_ids: &[needle_core::Digest],
    ) -> Result<SemanticReuseDecision, SemanticResolverError> {
        let expected_fresh_microusd = costs.fresh_microusd;
        let expected_reuse_microusd = costs.artifact_reuse_microusd;
        if need.residual.as_ref().is_some_and(|residual| residual.mandatory) {
            return Ok(SemanticReuseDecision {
                resolution: CacheResolution::Bypass {
                    reason: "mandatory residual intent is not structurally representable"
                        .to_owned(),
                },
                validated_resolution: None,
                plan: None,
                certificate: None,
                claim_advisory: None,
                claim_certificate: None,
                claim_material: None,
                artifacts: Vec::new(),
                authoritative: false,
                calibration_eligible: false,
                stale_candidates: 0,
            });
        }
        if self.store.active_contradiction(need)? {
            return Ok(SemanticReuseDecision {
                resolution: CacheResolution::Contradicted {
                    reason: "an active contradiction exists for a required obligation".to_owned(),
                },
                validated_resolution: None,
                plan: None,
                certificate: None,
                claim_advisory: None,
                claim_certificate: None,
                claim_material: None,
                artifacts: Vec::new(),
                authoritative: false,
                calibration_eligible: false,
                stale_candidates: 0,
            });
        }

        let lookup_started = Instant::now();
        let stored =
            self.store.semantic_candidates(need, exact_request_ids, source_snapshot_digest)?;
        let lookup_micros = elapsed_micros(lookup_started);
        let validation_started = Instant::now();
        let mut artifacts = Vec::new();
        let mut certificates = Vec::new();
        let mut candidates = Vec::new();
        let mut stale_candidates = 0;
        let mut first_stale = None;
        for (artifact, certificate, exact_request, same_source) in stored {
            if (artifact.contract.cache_scope == CacheScope::SnapshotExact && !same_source)
                || certificate.coverage.world != need.world
                || !artifact_and_certificate_are_fresh(&artifact, &certificate, repository_root)
                || !certificate_inputs_are_fresh(
                    &self.store,
                    &certificate,
                    repository_root,
                    source_snapshot_digest,
                    0,
                )?
            {
                stale_candidates += 1;
                first_stale.get_or_insert(artifact.id);
                continue;
            }
            candidates.push(ProofCandidate {
                artifact: certificate.artifact,
                validation_certificate: certificate.id,
                coverage: certificate
                    .coverage
                    .entries
                    .iter()
                    .map(|entry| entry.obligation.clone())
                    .collect(),
                exact_request,
                expected_reuse_microusd: 0,
                claim_ids: Vec::new(),
                claim_validation_certificate_ids: Vec::new(),
                claim_set_certificate_id: None,
            });
            artifacts.push(artifact);
            certificates.push(certificate);
        }
        let artifact_candidates_cover_need = !candidates.is_empty()
            && need.required.iter().all(|requested| {
                candidates.iter().any(|candidate| {
                    candidate.coverage.iter().any(|provided| provided.satisfies(requested))
                })
            });
        let mut claim_selection = if artifact_candidates_cover_need {
            None
        } else {
            self.claim_advisory(
                need,
                repository_root,
                source_snapshot_digest,
                &candidates,
                lookup_micros.saturating_add(elapsed_micros(validation_started)),
                costs,
            )
        };
        let validation_micros = elapsed_micros(validation_started);
        let mut validated_claim_resolution = None;
        let mut claim_calibration_eligible = false;
        if let Some(selection) = claim_selection.as_mut() {
            let capability_allowed = claim_capability_allows(
                &self.store,
                route_key,
                need,
                &selection.advisory,
                &selection.candidates,
            )?;
            let authoritative =
                capability_allowed && economics_allow_selected(&selection.advisory.selected);
            if capability_allowed {
                validated_claim_resolution = claim_cache_resolution(&selection.advisory);
                claim_calibration_eligible = validated_claim_resolution.is_some();
            }
            selection.advisory.selected.decision_reason = format!(
                "{}::{:?}",
                if authoritative { "Authoritative" } else { "Advisory" },
                selection.advisory.resolution
            );
            self.store.record_claim_advisory_plan(&selection.advisory, &selection.candidates)?;
            if authoritative {
                let resolution = validated_claim_resolution
                    .clone()
                    .expect("authoritative claim reuse passed its capability gate");
                let selected_artifacts = selected_artifact_components(selection, &artifacts);
                self.store.record_proof_accounting(&ProofAccountingRecord {
                    need_id: need.id,
                    plan_id: Some(selection.advisory.selected.id),
                    parse_micros: 0,
                    lookup_micros,
                    validation_micros,
                    planning_micros: 0,
                    projection_micros: 0,
                    allocation_count: None,
                    allocated_bytes: None,
                    stale_candidates: stale_candidates as u64,
                    created_unix_ms: now_ms(),
                })?;
                return Ok(SemanticReuseDecision {
                    resolution: resolution.clone(),
                    validated_resolution: Some(resolution),
                    plan: Some(selection.advisory.selected.clone()),
                    certificate: None,
                    claim_advisory: Some(selection.advisory.clone()),
                    claim_certificate: Some(selection.advisory.certificate.clone()),
                    claim_material: Some(selection.material.clone()),
                    artifacts: selected_artifacts,
                    authoritative: true,
                    calibration_eligible: true,
                    stale_candidates,
                });
            }
        }
        if candidates.is_empty() {
            let claim_advisory =
                claim_selection.as_ref().map(|selection| selection.advisory.clone());
            let claim_certificate =
                claim_selection.as_ref().map(|selection| selection.advisory.certificate.clone());
            let claim_plan = claim_selection
                .as_ref()
                .filter(|_| claim_calibration_eligible)
                .map(|selection| selection.advisory.selected.clone());
            let claim_material = claim_selection.map(|selection| selection.material);
            let resolution = if stale_candidates == 0 {
                CacheResolution::Miss
            } else {
                CacheResolution::Stale {
                    artifact_id: first_stale.expect("stale candidates are counted"),
                    reason: format!("{stale_candidates} semantic candidate(s) failed freshness"),
                }
            };
            self.store.record_proof_accounting(&ProofAccountingRecord {
                need_id: need.id,
                plan_id: claim_plan.as_ref().map(|plan| plan.id),
                parse_micros: 0,
                lookup_micros,
                validation_micros,
                planning_micros: 0,
                projection_micros: 0,
                allocation_count: None,
                allocated_bytes: None,
                stale_candidates: stale_candidates as u64,
                created_unix_ms: now_ms(),
            })?;
            return Ok(SemanticReuseDecision {
                resolution,
                validated_resolution: validated_claim_resolution,
                plan: claim_plan,
                certificate: None,
                claim_advisory,
                claim_certificate,
                claim_material,
                artifacts,
                authoritative: false,
                calibration_eligible: claim_calibration_eligible,
                stale_candidates,
            });
        }

        let claim_advisory = claim_selection.as_ref().map(|selection| selection.advisory.clone());
        let claim_certificate =
            claim_selection.as_ref().map(|selection| selection.advisory.certificate.clone());
        let claim_material = claim_selection.map(|selection| selection.material);

        let planning_started = Instant::now();
        let overhead_micros = lookup_micros.saturating_add(validation_micros);
        let mut proof = self.planner.plan(
            need,
            &candidates,
            overhead_micros,
            expected_fresh_microusd,
            &needle_core::ProofBudget::default(),
        )?;
        let planning_micros = elapsed_micros(planning_started);
        proof.selected.economics.proof_overhead_micros =
            overhead_micros.saturating_add(planning_micros);
        proof.selected.economics.expected_selected_microusd = expected_reuse_microusd;
        proof.selected.economics.expected_net_microusd =
            expected_fresh_microusd.zip(expected_reuse_microusd).map(|(fresh, reuse)| {
                i64::try_from(fresh).unwrap_or(i64::MAX) - i64::try_from(reuse).unwrap_or(i64::MAX)
            });
        let replayed = proof
            .certificate
            .as_ref()
            .is_none_or(|certificate| self.planner.replay(need, certificate, &candidates));
        let capability_allowed = capability_allows(&self.store, need, &proof)?;
        let calibration_eligible = replayed && capability_allowed;
        let authoritative = calibration_eligible && economics_allow(&proof);
        proof.selected.decision_reason = format!(
            "{}::{:?}",
            if authoritative { "Authoritative" } else { "Advisory" },
            proof.resolution
        );
        let validated_resolution = calibration_eligible.then(|| cache_resolution(&proof, true));
        let resolution = cache_resolution(&proof, authoritative);
        self.store.record_proof_plan(
            &proof.selected,
            &proof.selected.decision_reason,
            proof.certificate.as_ref(),
            &candidates,
        )?;
        self.store.record_proof_accounting(&ProofAccountingRecord {
            need_id: need.id,
            plan_id: Some(proof.selected.id),
            parse_micros: 0,
            lookup_micros,
            validation_micros,
            planning_micros,
            projection_micros: 0,
            allocation_count: None,
            allocated_bytes: None,
            stale_candidates: stale_candidates as u64,
            created_unix_ms: now_ms(),
        })?;
        let selected = proof
            .selected
            .artifact_ids
            .iter()
            .filter_map(|selected| {
                artifacts.iter().find(|artifact| artifact.id == selected.digest()).cloned()
            })
            .collect();
        Ok(SemanticReuseDecision {
            resolution,
            validated_resolution,
            plan: Some(proof.selected),
            certificate: proof.certificate,
            claim_advisory,
            claim_certificate,
            claim_material,
            artifacts: selected,
            authoritative,
            calibration_eligible,
            stale_candidates,
        })
    }

    fn claim_advisory(
        &self,
        need: &Need,
        repository_root: &Path,
        source_snapshot_digest: needle_core::Digest,
        artifact_candidates: &[ProofCandidate],
        proof_overhead_micros: u64,
        costs: SemanticCostEstimates,
    ) -> Option<ClaimSelection> {
        let claim_started = Instant::now();
        let origins = match self.store.claim_origin_artifacts_for_need(need) {
            Ok(origins) => origins,
            Err(error) => {
                eprintln!("needle: claim advisory lookup remained Shadow-only ({error})");
                return None;
            }
        };
        let mut candidates = artifact_candidates.to_vec();
        let artifact_candidate_count = candidates.len();
        for artifact in origins {
            let origin_is_eligible =
                self.store.semantic_artifact(&artifact.to_string()).and_then(|stored| {
                    let Some(stored) = stored else {
                        return Ok(false);
                    };
                    let Some(certificate) =
                        self.store.validation_certificate_for_artifact(&artifact.to_string())?
                    else {
                        return Ok(false);
                    };
                    Ok(stored.contract.cache_scope == CacheScope::WorktreeSemantic
                        && certificate.coverage.world == need.world
                        && certificate_inputs_are_fresh(
                            &self.store,
                            &certificate,
                            repository_root,
                            source_snapshot_digest,
                            0,
                        )?)
                });
            match origin_is_eligible {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    eprintln!(
                        "needle: claim advisory origin validation remained Shadow-only ({error})"
                    );
                    continue;
                }
            }
            let material = match self.store.claim_proof_material_for_artifacts(&[artifact]) {
                Ok(material) => filter_claim_material_for_need(material, need),
                Err(error) => {
                    eprintln!("needle: claim advisory material remained Shadow-only ({error})");
                    continue;
                }
            };
            if material.claims.is_empty() || material.certificates.is_empty() {
                continue;
            }
            for obligation in &need.required {
                if candidates.len() >= needle_core::MAX_PROOF_CANDIDATES {
                    break;
                }
                let certificate = match build_claim_component_certificate(
                    need,
                    std::slice::from_ref(obligation),
                    &material,
                    repository_root,
                    now_ms(),
                ) {
                    Ok(certificate) => certificate,
                    Err(ClaimProofError::Insufficient(_) | ClaimProofError::Stale) => continue,
                    Err(error) => {
                        eprintln!(
                            "needle: claim advisory validation remained Shadow-only ({error})"
                        );
                        continue;
                    }
                };
                if let Err(error) =
                    replay_claim_set_certificate(&certificate, need, &material, repository_root)
                {
                    eprintln!("needle: claim advisory replay remained Shadow-only ({error})");
                    continue;
                }
                let selected_certificate_ids =
                    certificate.validation_certificates.iter().copied().collect::<BTreeSet<_>>();
                let selected_certificates = material
                    .certificates
                    .iter()
                    .filter(|candidate| selected_certificate_ids.contains(&candidate.id))
                    .collect::<Vec<_>>();
                if selected_certificates.is_empty()
                    || selected_certificates
                        .iter()
                        .any(|candidate| candidate.origin_artifact != artifact)
                {
                    eprintln!("needle: claim advisory origin binding remained Shadow-only");
                    continue;
                }
                let mut coverage = selected_certificates
                    .iter()
                    .flat_map(|candidate| candidate.obligations.iter().cloned())
                    .collect::<Vec<_>>();
                coverage.sort_unstable();
                coverage.dedup_by_key(|provided| provided.id);
                let validation_certificate = selected_certificates
                    .iter()
                    .map(|candidate| candidate.origin_validation_certificate)
                    .min()
                    .expect("selected claim certificates are non-empty");
                if let Err(error) = self.store.publish_claim_set_shadow(&certificate) {
                    eprintln!("needle: claim advisory certificate remained Shadow-only ({error})");
                    continue;
                }
                candidates.push(ProofCandidate {
                    artifact,
                    validation_certificate,
                    coverage,
                    exact_request: false,
                    expected_reuse_microusd: 0,
                    claim_ids: certificate.claims.clone(),
                    claim_validation_certificate_ids: certificate.validation_certificates.clone(),
                    claim_set_certificate_id: Some(certificate.id),
                });
            }
        }
        if candidates.len() == artifact_candidate_count {
            return None;
        }
        let mut advisory = match self.planner.plan_claim_advisory(
            need,
            &candidates,
            proof_overhead_micros.saturating_add(elapsed_micros(claim_started)),
            costs,
            &needle_core::ProofBudget::default(),
            now_ms(),
        ) {
            Ok(advisory) => advisory,
            Err(error) => {
                eprintln!("needle: claim advisory planning remained Shadow-only ({error})");
                return None;
            }
        }?;
        advisory.selected.economics.proof_overhead_micros =
            proof_overhead_micros.saturating_add(elapsed_micros(claim_started));
        if !self.planner.replay_claim_advisory(need, &advisory, &candidates) {
            eprintln!("needle: claim advisory aggregate replay remained Shadow-only");
            return None;
        }
        if let Err(error) = self.store.record_claim_advisory_plan(&advisory, &candidates) {
            eprintln!("needle: claim advisory persistence remained Shadow-only ({error})");
            return None;
        }
        let material = match self.store.claim_proof_material_for_certificate(&advisory.certificate)
        {
            Ok(material) => material,
            Err(error) => {
                eprintln!("needle: selected claim material remained Shadow-only ({error})");
                return None;
            }
        };
        Some(ClaimSelection { advisory, material, candidates })
    }
}

fn inferred_route_key(need: &Need) -> NeedKey {
    let route = if need.required.len() == 1
        && need.required[0].predicate == PredicateKind::ImplementationLocation
    {
        "locate.implementation"
    } else if need.required.len() == 1 && need.required[0].predicate == PredicateKind::FocusedTests
    {
        "tests.relevant"
    } else {
        "trace.state-flow"
    };
    NeedKey::new(route).expect("built-in semantic route key")
}

fn claim_capability_allows(
    store: &RuntimeStore,
    route_key: &NeedKey,
    need: &Need,
    advisory: &ClaimAdvisoryPlan,
    candidates: &[ProofCandidate],
) -> Result<bool, StoreError> {
    let resolution_shape_valid = match advisory.resolution {
        crate::ClaimAdvisoryResolutionKind::ClaimHit
        | crate::ClaimAdvisoryResolutionKind::ClaimCompositeHit => {
            advisory.selected.missing_mask == 0
        }
        crate::ClaimAdvisoryResolutionKind::PartialHit => {
            advisory.selected.covered_mask != 0 && advisory.selected.missing_mask != 0
        }
    };
    if !matches!(
        route_key.as_str(),
        "locate.implementation" | "trace.state-flow" | "tests.relevant"
    ) || !resolution_shape_valid
        || advisory.selected.claim_ids.is_empty()
        || advisory.selected.claim_ids.len() > needle_core::MAX_SELECTED_CLAIMS
        || advisory.selected.artifact_ids.is_empty()
        || advisory.selected.artifact_ids.len() > needle_core::MAX_CLAIM_ORIGINS
        || advisory.certificate.claims != advisory.selected.claim_ids
        || need.subjects.len() != 1
        || !need.subjects[0].is_canonical()
        || !need.input_artifacts.is_empty()
        || need.required.is_empty()
        || need.residual.is_some()
        || advisory.selected.economics.proof_overhead_micros > 100_000
    {
        return Ok(false);
    }
    let mut claim_obligation_ids = candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| {
            advisory.selected_bits & (1_u64 << index) != 0 && !candidate.claim_ids.is_empty()
        })
        .flat_map(|(_, candidate)| {
            need.required.iter().filter_map(|required| {
                candidate
                    .coverage
                    .iter()
                    .any(|provided| provided.satisfies(required))
                    .then_some(required.id)
            })
        })
        .collect::<Vec<_>>();
    claim_obligation_ids.sort_unstable();
    claim_obligation_ids.dedup();
    if advisory.certificate.obligations != claim_obligation_ids
        || claim_obligation_ids.is_empty()
        || need.required.iter().any(|obligation| {
            obligation.subject != need.subjects[0].id || !authoritative_facets(obligation)
        })
    {
        return Ok(false);
    }
    let classes = store.capability_classes()?;
    for (index, candidate) in candidates.iter().enumerate() {
        if advisory.selected_bits & (1_u64 << index) == 0 {
            continue;
        }
        let reuse_unit =
            if candidate.claim_ids.is_empty() { ReuseUnit::Artifact } else { ReuseUnit::Claim };
        for obligation in &need.required {
            if candidate.coverage.iter().any(|provided| provided.satisfies(obligation))
                && !current_capability_is_authoritative(&classes, reuse_unit, obligation.predicate)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn authoritative_facets(obligation: &needle_core::Obligation) -> bool {
    match obligation.predicate {
        PredicateKind::ImplementationLocation => {
            has_exact_facet(obligation, "polarity", "positive")
                && has_exact_facet(obligation, "selection", "primary")
                && has_exact_facet(obligation, "granularity", "exact-location")
        }
        PredicateKind::RuntimeFlow => {
            has_exact_facet(obligation, "scenario", "default")
                && has_exact_facet(obligation, "completeness", "contract-complete")
                && has_exact_facet(obligation, "granularity", "stepwise")
        }
        PredicateKind::FocusedTests => {
            has_exact_facet(obligation, "polarity", "positive")
                && has_exact_facet(obligation, "selection", "representative")
                && has_exact_facet(obligation, "completeness", "open-world")
        }
    }
}

fn current_capability_is_authoritative(
    stored: &[needle_core::CapabilityClass],
    reuse_unit: ReuseUnit,
    predicate: PredicateKind,
) -> bool {
    let expected = match reuse_unit {
        ReuseUnit::Artifact => built_in_capability_classes(),
        ReuseUnit::Claim => built_in_claim_capability_classes(),
    }
    .into_iter()
    .find(|class| class.predicate == predicate)
    .expect("built-in capability exists");
    stored.iter().any(|class| {
        class.id == expected.id
            && class.definition_digest == expected.definition_digest
            && class.reuse_unit == reuse_unit
            && class.predicate == predicate
            && class.mode == CapabilityMode::Authoritative
            && class.exact_subject_only
            && class.positive_only
            && class.single_world_only
    })
}

fn has_exact_facet(obligation: &needle_core::Obligation, key: &str, value: &str) -> bool {
    obligation.facets.iter().any(|facet| facet.key == key && facet.value == value)
}

fn economics_allow_selected(plan: &SelectedPlan) -> bool {
    plan.economics
        .expected_net_microusd
        .is_some_and(|net| net > plan.proof_budget.minimum_expected_net_microusd)
}

fn claim_cache_resolution(advisory: &ClaimAdvisoryPlan) -> Option<CacheResolution> {
    let common = || {
        (
            advisory.selected.artifact_ids.iter().map(|artifact| artifact.digest()).collect(),
            advisory.selected.claim_ids.clone(),
            advisory.certificate.id,
            advisory.selected.id,
        )
    };
    let (artifact_ids, claim_ids, claim_set_certificate_id, selected_plan_id) = common();
    match advisory.resolution {
        crate::ClaimAdvisoryResolutionKind::ClaimHit => Some(CacheResolution::ClaimHit {
            artifact_ids,
            claim_ids,
            claim_set_certificate_id,
            selected_plan_id,
            resolution_format_revision: CLAIM_PROOF_RESOLUTION_FORMAT_REVISION,
        }),
        crate::ClaimAdvisoryResolutionKind::ClaimCompositeHit => {
            Some(CacheResolution::ClaimCompositeHit {
                artifact_ids,
                claim_ids,
                claim_set_certificate_id,
                selected_plan_id,
                resolution_format_revision: CLAIM_PROOF_RESOLUTION_FORMAT_REVISION,
            })
        }
        crate::ClaimAdvisoryResolutionKind::PartialHit => Some(CacheResolution::PartialHit {
            reused: artifact_ids,
            reused_claim_ids: claim_ids,
            invalidated_nodes: missing_predicates(&advisory.selected),
            selected_plan_id: Some(selected_plan_id),
            resolution_format_revision: Some(CLAIM_PROOF_RESOLUTION_FORMAT_REVISION),
        }),
    }
}

fn selected_artifact_components(
    selection: &ClaimSelection,
    artifacts: &[Artifact],
) -> Vec<Artifact> {
    selection
        .candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| {
            selection.advisory.selected_bits & (1_u64 << index) != 0
                && candidate.claim_ids.is_empty()
        })
        .filter_map(|(_, candidate)| {
            artifacts.iter().find(|artifact| artifact.id == candidate.artifact.digest()).cloned()
        })
        .collect()
}

fn filter_claim_material_for_need(
    mut material: ClaimProofMaterial,
    need: &Need,
) -> ClaimProofMaterial {
    material.certificates.retain(|certificate| {
        certificate.world == need.world.id()
            && need.subjects.iter().any(|subject| subject.id == certificate.subject)
            && certificate
                .obligations
                .iter()
                .any(|provided| need.required.iter().any(|requested| provided.satisfies(requested)))
    });
    let claim_ids =
        material.certificates.iter().map(|certificate| certificate.claim).collect::<BTreeSet<_>>();
    material.claims.retain(|claim| claim_ids.contains(&claim.id));
    material
        .relations
        .retain(|relation| claim_ids.contains(&relation.from) && claim_ids.contains(&relation.to));
    material
}

fn certificate_inputs_are_fresh(
    store: &RuntimeStore,
    certificate: &ArtifactValidationCertificate,
    repository_root: &Path,
    source_snapshot_digest: needle_core::Digest,
    depth: usize,
) -> Result<bool, StoreError> {
    if certificate.input_artifacts.is_empty() {
        return Ok(true);
    }
    if depth >= MAX_DERIVATION_DEPTH {
        return Ok(false);
    }
    for input in &certificate.input_artifacts {
        let Some(artifact) = store.semantic_artifact(&input.to_string())? else {
            return Ok(false);
        };
        let Some(input_certificate) =
            store.validation_certificate_for_artifact(&input.to_string())?
        else {
            return Ok(false);
        };
        if (artifact.contract.cache_scope == CacheScope::SnapshotExact
            && !store.semantic_artifact_has_source(&input.to_string(), source_snapshot_digest)?)
            || input_certificate.coverage.world != certificate.coverage.world
            || !artifact_and_certificate_are_fresh(&artifact, &input_certificate, repository_root)
            || !certificate_inputs_are_fresh(
                store,
                &input_certificate,
                repository_root,
                source_snapshot_digest,
                depth + 1,
            )?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn capability_allows(
    store: &RuntimeStore,
    need: &Need,
    proof: &ProofPlan,
) -> Result<bool, StoreError> {
    if proof.resolution == ProofResolutionKind::Miss {
        return Ok(false);
    }
    let classes = store.capability_classes()?;
    let composition = proof.selected.artifact_ids.len() > 1;
    Ok(need.required.iter().all(|obligation| {
        if proof.selected.missing_mask
            & (1_u16
                << need.required.iter().position(|item| item.id == obligation.id).unwrap_or(16))
            != 0
        {
            return true;
        }
        classes.iter().any(|class| {
            class.reuse_unit == ReuseUnit::Artifact
                && class.predicate == obligation.predicate
                && class.mode == CapabilityMode::Authoritative
                && (!composition || class.composition)
                && positive_obligation(obligation.predicate, &obligation.facets)
        })
    }))
}

fn positive_obligation(predicate: PredicateKind, facets: &[needle_core::Facet]) -> bool {
    match predicate {
        PredicateKind::ImplementationLocation | PredicateKind::FocusedTests => facets
            .iter()
            .find(|facet| facet.key == "polarity")
            .is_none_or(|facet| facet.value == "positive"),
        PredicateKind::RuntimeFlow => true,
    }
}

fn economics_allow(proof: &ProofPlan) -> bool {
    proof
        .selected
        .economics
        .expected_net_microusd
        .is_some_and(|net| net > proof.selected.proof_budget.minimum_expected_net_microusd)
}

fn cache_resolution(proof: &ProofPlan, authoritative: bool) -> CacheResolution {
    if !authoritative {
        return CacheResolution::Bypass {
            reason: "proof is shadow/advisory or has no positive measured net value".to_owned(),
        };
    }
    let plan_id = proof.selected.id;
    match proof.resolution {
        ProofResolutionKind::ExactHit => CacheResolution::ExactHit {
            artifact_id: proof.selected.artifact_ids[0].digest(),
            sufficiency_certificate_id: proof.certificate.as_ref().map(|item| item.id),
            selected_plan_id: Some(plan_id),
            resolution_format_revision: Some(PROOF_RESOLUTION_FORMAT_REVISION),
        },
        ProofResolutionKind::CoverageHit => CacheResolution::CoverageHit {
            artifact_id: proof.selected.artifact_ids[0].digest(),
            sufficiency_certificate_id: proof.certificate.as_ref().expect("full proof").id,
            selected_plan_id: plan_id,
            resolution_format_revision: PROOF_RESOLUTION_FORMAT_REVISION,
        },
        ProofResolutionKind::CompositeHit => CacheResolution::CompositeHit {
            artifact_ids: proof
                .selected
                .artifact_ids
                .iter()
                .map(|artifact| artifact.digest())
                .collect(),
            sufficiency_certificate_id: proof.certificate.as_ref().map(|item| item.id),
            selected_plan_id: Some(plan_id),
            resolution_format_revision: Some(PROOF_RESOLUTION_FORMAT_REVISION),
        },
        ProofResolutionKind::PartialHit => CacheResolution::PartialHit {
            reused: proof.selected.artifact_ids.iter().map(|artifact| artifact.digest()).collect(),
            reused_claim_ids: Vec::new(),
            invalidated_nodes: missing_predicates(&proof.selected),
            selected_plan_id: Some(plan_id),
            resolution_format_revision: Some(PROOF_RESOLUTION_FORMAT_REVISION),
        },
        ProofResolutionKind::Miss => CacheResolution::Miss,
    }
}

fn missing_predicates(plan: &SelectedPlan) -> Vec<String> {
    (0..16)
        .filter(|index| plan.missing_mask & (1_u16 << index) != 0)
        .map(|index| format!("obligation-{index}"))
        .collect()
}

#[cfg(test)]
mod claim_authority_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClaimAdvisoryResolutionKind, NeedShadowWrite, RuntimeSettings, validate_semantic_artifact,
        validate_semantic_artifact_with_trace, validate_semantic_test_plan_with_evidence,
    };
    use needle_core::{
        ArtifactRequest, CapabilityMode, CommandExecutionEvidence, Digest, EvidenceFailurePolicy,
        FlowStepRole, NeedIr, SemanticFlowStep, SemanticLocation, SemanticWorkerArtifact, TestPlan,
        built_in_route_contracts, need_fragment,
    };
    use std::fs;

    #[test]
    fn economics_never_authorizes_without_both_observed_estimates() {
        let plan = SelectedPlan {
            id: needle_core::SelectedPlanId(needle_core::Digest::blake3(b"plan")),
            need: needle_core::NeedId(needle_core::Digest::blake3(b"need")),
            artifact_ids: Vec::new(),
            claim_ids: Vec::new(),
            claim_validation_certificate_ids: Vec::new(),
            claim_set_certificate_ids: Vec::new(),
            covered_mask: 0,
            missing_mask: 1,
            economics: needle_core::PlanEconomics {
                expected_fresh_microusd: Some(100),
                expected_selected_microusd: None,
                proof_overhead_micros: 1,
                expected_net_microusd: None,
            },
            proof_budget: Default::default(),
            decision_reason: "test".to_owned(),
        };
        let proof = ProofPlan {
            resolution: ProofResolutionKind::PartialHit,
            selected: plan,
            certificate: None,
        };
        assert!(!economics_allow(&proof));
    }

    #[test]
    fn stale_artifact_can_retain_a_fresh_primary_claim_in_advisory() {
        let root = std::env::temp_dir().join(format!(
            "needle-claim-advisory-{}-{}",
            std::process::id(),
            needle_core::Digest::blake3(format!("{:?}", Instant::now())).to_hex()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        fs::write(root.join("src/support.rs"), "pub fn support() {}\n").unwrap();

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
             @route locate.implementation\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "locate.implementation")
            .unwrap();
        let need = needle_core::compile_need(&ir, Digest::blake3(b"repo"), &route).unwrap();
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "claim-session",
                turn_id: "claim-turn",
                transport_digest: Digest::blake3(b"transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &ir,
                need: &need,
                fragments: std::slice::from_ref(&fragment),
            })
            .unwrap();
        let worker_artifact = SemanticWorkerArtifact::CodeLocation {
            locations: vec![
                SemanticLocation {
                    role: needle_core::LocationRole::Primary,
                    path: "src/lib.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(29),
                },
                SemanticLocation {
                    role: needle_core::LocationRole::Supporting,
                    path: "src/support.rs".to_owned(),
                    symbol: Some("support".to_owned()),
                    byte_start: None,
                    byte_end: None,
                },
            ],
            gaps: Vec::new(),
        };
        let source_snapshot_digest = Digest::blake3(b"source");
        let request = ArtifactRequest {
            contract_id: "needle.semantic.code-location".to_owned(),
            contract_revision: 2,
            repository_id: need.world.repository_lineage,
            source_snapshot_digest,
            route_key: route.route.clone(),
            normalized_request: "claim wording".to_owned(),
            semantic_fragment_id: Some(fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let validated = validate_semantic_artifact(
            &fragment,
            &worker_artifact,
            &root,
            request.semantic_id().digest(),
        )
        .unwrap();
        assert_eq!(validated.artifact.contract.cache_scope, CacheScope::WorktreeSemantic);
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
        let fresh = resolver
            .resolve(&need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(fresh.claim_advisory.is_none());

        fs::write(root.join("src/support.rs"), "pub fn support() { unreachable!() }\n").unwrap();
        let stale = resolver
            .resolve(&need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(!stale.authoritative);
        assert!(matches!(stale.resolution, CacheResolution::Stale { .. }));
        let advisory = stale.claim_advisory.as_ref().unwrap();
        assert_eq!(advisory.resolution, ClaimAdvisoryResolutionKind::ClaimHit);
        assert_eq!(advisory.selected.covered_mask, 1);
        assert_eq!(advisory.selected.missing_mask, 0);
        assert_eq!(advisory.selected.claim_ids.len(), 1);
        assert_eq!(advisory.selected.artifact_ids, vec![validated.semantic_id]);
        assert_eq!(
            store.selected_plan(&advisory.selected.id.to_string()).unwrap(),
            Some(advisory.selected.clone())
        );

        let claim_capability = store
            .capability_classes()
            .unwrap()
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Claim
                    && class.predicate == PredicateKind::ImplementationLocation
            })
            .unwrap();
        store
            .set_capability_mode(
                &claim_capability.id,
                claim_capability.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"claim-authority-offline-evidence")),
            )
            .unwrap();

        let calibration = resolver
            .resolve_for_route(
                &need,
                &route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(1),
                    claim_reuse_microusd: None,
                    claim_partial_reuse_microusd: None,
                },
                &[],
            )
            .unwrap();
        assert!(!calibration.authoritative);
        assert!(calibration.calibration_eligible);
        assert!(matches!(calibration.resolution, CacheResolution::Stale { .. }));
        assert!(matches!(
            calibration.validated_resolution,
            Some(CacheResolution::ClaimHit {
                resolution_format_revision: CLAIM_PROOF_RESOLUTION_FORMAT_REVISION,
                ..
            })
        ));

        let authoritative = resolver
            .resolve_for_route(
                &need,
                &route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(1),
                    claim_reuse_microusd: Some(1),
                    claim_partial_reuse_microusd: None,
                },
                &[],
            )
            .unwrap();
        assert!(authoritative.authoritative);
        assert!(authoritative.artifacts.is_empty());
        assert!(authoritative.claim_certificate.is_some());
        assert!(authoritative.claim_material.is_some());
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
                resolution_format_revision,
            } => {
                assert_eq!(artifact_ids, &[validated.semantic_id.digest()]);
                assert_eq!(claim_ids.len(), 1);
                assert_eq!(
                    Some(*claim_set_certificate_id),
                    authoritative.claim_certificate.as_ref().map(|certificate| certificate.id)
                );
                assert_eq!(
                    Some(*selected_plan_id),
                    authoritative.plan.as_ref().map(|plan| plan.id)
                );
                assert_eq!(*resolution_format_revision, CLAIM_PROOF_RESOLUTION_FORMAT_REVISION);
            }
            other => panic!("expected ClaimHit, got {other:?}"),
        }

        let uneconomic = resolver
            .resolve_for_route(
                &need,
                &route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(1),
                    claim_reuse_microusd: Some(100),
                    claim_partial_reuse_microusd: None,
                },
                &[],
            )
            .unwrap();
        assert!(!uneconomic.authoritative);
        assert!(matches!(uneconomic.resolution, CacheResolution::Stale { .. }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fresh_claim_and_fresh_artifact_form_authoritative_claim_composite() {
        let root = std::env::temp_dir().join(format!(
            "needle-claim-composite-{}-{}",
            std::process::id(),
            needle_core::Digest::blake3(format!("{:?}", Instant::now())).to_hex()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        fs::write(root.join("src/support.rs"), "pub fn support() {}\n").unwrap();
        fs::write(root.join("src/flow.rs"), "pub fn flow_answer() {}\n").unwrap();

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

        let repository_lineage = Digest::blake3(b"repo");
        let source_snapshot_digest = Digest::blake3(b"source");
        let locate_route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "locate.implementation")
            .unwrap();
        let locate_ir = NeedIr::parse(
            "@@need\n\
             @route locate.implementation\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let locate_need =
            needle_core::compile_need(&locate_ir, repository_lineage, &locate_route).unwrap();
        let locate_fragment = need_fragment(&locate_need, locate_need.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "claim-composite-locate-session",
                turn_id: "claim-composite-locate-turn",
                transport_digest: Digest::blake3(b"locate-transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &locate_ir,
                need: &locate_need,
                fragments: std::slice::from_ref(&locate_fragment),
            })
            .unwrap();
        let location_request = ArtifactRequest {
            contract_id: "needle.semantic.code-location".to_owned(),
            contract_revision: 2,
            repository_id: repository_lineage,
            source_snapshot_digest,
            route_key: locate_route.route.clone(),
            normalized_request: "locate wording".to_owned(),
            semantic_fragment_id: Some(locate_fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let validated_location = validate_semantic_artifact(
            &locate_fragment,
            &SemanticWorkerArtifact::CodeLocation {
                locations: vec![
                    SemanticLocation {
                        role: needle_core::LocationRole::Primary,
                        path: "src/lib.rs".to_owned(),
                        symbol: Some("answer".to_owned()),
                        byte_start: Some(0),
                        byte_end: Some(29),
                    },
                    SemanticLocation {
                        role: needle_core::LocationRole::Supporting,
                        path: "src/support.rs".to_owned(),
                        symbol: Some("support".to_owned()),
                        byte_start: None,
                        byte_end: None,
                    },
                ],
                gaps: Vec::new(),
            },
            &root,
            location_request.semantic_id().digest(),
        )
        .unwrap();
        store
            .publish_semantic_artifact(
                &location_request,
                &locate_need,
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

        let trace_route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "trace.state-flow")
            .unwrap();
        let trace_ir = NeedIr::parse(
            "@@need\n\
             @route trace.state-flow\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
             @world source=current features=default\n\
             \n\
             Trace the runtime flow.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let trace_need =
            needle_core::compile_need(&trace_ir, repository_lineage, &trace_route).unwrap();
        let trace_fragment = need_fragment(&trace_need, trace_need.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "claim-composite-trace-session",
                turn_id: "claim-composite-trace-turn",
                transport_digest: Digest::blake3(b"trace-transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &trace_ir,
                need: &trace_need,
                fragments: std::slice::from_ref(&trace_fragment),
            })
            .unwrap();
        let behavior_request = ArtifactRequest {
            contract_id: "needle.semantic.behavior-trace".to_owned(),
            contract_revision: 2,
            repository_id: repository_lineage,
            source_snapshot_digest,
            route_key: trace_route.route.clone(),
            normalized_request: "trace wording".to_owned(),
            semantic_fragment_id: Some(trace_fragment.id),
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
            .map(|(index, role)| SemanticFlowStep {
                role,
                location: SemanticLocation {
                    role: needle_core::LocationRole::Supporting,
                    path: "src/flow.rs".to_owned(),
                    symbol: Some("flow".to_owned()),
                    byte_start: Some(index as u64),
                    byte_end: Some(index as u64 + 1),
                },
                description: format!("{role:?} step"),
            })
            .collect(),
            gaps: Vec::new(),
        };
        let validated_behavior = validate_semantic_artifact(
            &trace_fragment,
            &behavior,
            &root,
            behavior_request.semantic_id().digest(),
        )
        .unwrap();
        store
            .publish_semantic_artifact(
                &behavior_request,
                &trace_need,
                &validated_behavior.artifact,
                &validated_behavior.certificate,
            )
            .unwrap();

        for (reuse_unit, predicate, evidence) in [
            (
                ReuseUnit::Claim,
                PredicateKind::ImplementationLocation,
                b"claim-location-evidence".as_slice(),
            ),
            (ReuseUnit::Artifact, PredicateKind::RuntimeFlow, b"artifact-flow-evidence".as_slice()),
        ] {
            let class = store
                .capability_classes()
                .unwrap()
                .into_iter()
                .find(|class| class.reuse_unit == reuse_unit && class.predicate == predicate)
                .unwrap();
            store
                .set_capability_mode(
                    &class.id,
                    class.definition_digest,
                    CapabilityMode::Authoritative,
                    Some(Digest::blake3(evidence)),
                )
                .unwrap();
        }

        fs::write(root.join("src/support.rs"), "pub fn support() { unreachable!() }\n").unwrap();
        fs::write(root.join("src/flow.rs"), "pub fn changed_flow() {}\n").unwrap();
        let resolver = SemanticResolver::new(store.clone());
        let uncalibrated_partial = resolver
            .resolve_for_route(
                &trace_need,
                &trace_route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(2),
                    claim_reuse_microusd: Some(1),
                    claim_partial_reuse_microusd: None,
                },
                &[],
            )
            .unwrap();
        assert!(!uncalibrated_partial.authoritative);
        assert!(uncalibrated_partial.calibration_eligible);
        assert!(matches!(
            uncalibrated_partial.validated_resolution,
            Some(CacheResolution::PartialHit {
                ref reused_claim_ids,
                resolution_format_revision: Some(CLAIM_PROOF_RESOLUTION_FORMAT_REVISION),
                ..
            }) if reused_claim_ids.len() == 1
        ));
        let authoritative_partial = resolver
            .resolve_for_route(
                &trace_need,
                &trace_route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(2),
                    claim_reuse_microusd: Some(1),
                    claim_partial_reuse_microusd: Some(20),
                },
                &[],
            )
            .unwrap();
        assert!(authoritative_partial.authoritative);
        assert!(authoritative_partial.artifacts.is_empty());
        assert!(matches!(
            authoritative_partial.resolution,
            CacheResolution::PartialHit {
                ref reused_claim_ids,
                resolution_format_revision: Some(CLAIM_PROOF_RESOLUTION_FORMAT_REVISION),
                ..
            } if reused_claim_ids.len() == 1
        ));

        fs::write(root.join("src/flow.rs"), "pub fn flow_answer() {}\n").unwrap();
        let decision = resolver
            .resolve_for_route(
                &trace_need,
                &trace_route.route,
                &root,
                source_snapshot_digest,
                SemanticCostEstimates {
                    fresh_microusd: Some(100),
                    artifact_reuse_microusd: Some(2),
                    claim_reuse_microusd: Some(1),
                    claim_partial_reuse_microusd: None,
                },
                &[],
            )
            .unwrap();

        assert_eq!(
            decision.claim_advisory.as_ref().map(|advisory| advisory.resolution),
            Some(ClaimAdvisoryResolutionKind::ClaimCompositeHit),
            "{decision:#?}"
        );
        assert!(
            matches!(
                decision.validated_resolution,
                Some(CacheResolution::ClaimCompositeHit { .. })
            ),
            "{decision:#?}"
        );
        assert!(decision.authoritative);
        assert_eq!(decision.artifacts, vec![validated_behavior.artifact]);
        assert_eq!(decision.claim_material.as_ref().map(|material| material.claims.len()), Some(1));
        match &decision.resolution {
            CacheResolution::ClaimCompositeHit {
                artifact_ids,
                claim_ids,
                claim_set_certificate_id,
                selected_plan_id,
                resolution_format_revision,
            } => {
                assert_eq!(
                    artifact_ids,
                    &[
                        validated_location.semantic_id.digest(),
                        validated_behavior.semantic_id.digest(),
                    ]
                );
                assert_eq!(claim_ids.len(), 1);
                assert_eq!(
                    Some(*claim_set_certificate_id),
                    decision.claim_certificate.as_ref().map(|certificate| certificate.id)
                );
                assert_eq!(Some(*selected_plan_id), decision.plan.as_ref().map(|plan| plan.id));
                assert_eq!(*resolution_format_revision, CLAIM_PROOF_RESOLUTION_FORMAT_REVISION);
            }
            other => panic!("expected ClaimCompositeHit, got {other:?}"),
        }
        let projected_flow = match &behavior {
            SemanticWorkerArtifact::BehaviorTrace { steps, .. } => steps
                .iter()
                .enumerate()
                .map(|(ordinal, step)| needle_core::BehaviorStep {
                    ordinal: ordinal.try_into().unwrap(),
                    location: needle_core::CodeLocation {
                        path: step.location.path.clone(),
                        symbol: step.location.symbol.clone(),
                        byte_start: step.location.byte_start,
                        byte_end: step.location.byte_end,
                        content_digest: Digest::blake3(fs::read(root.join("src/flow.rs")).unwrap()),
                    },
                    description: step.description.clone(),
                })
                .collect(),
            other => panic!("expected behavior trace, got {other:?}"),
        };
        let compatibility_request = needle_core::NeedRequest::parse(
            "@@need:trace.state-flow\nTrace the runtime flow.\n@@end",
        )
        .unwrap()
        .unwrap();
        let projected = crate::semantic_claim_projection::project_claim_brief(
            &compatibility_request,
            repository_lineage,
            source_snapshot_digest,
            &root,
            decision.claim_material.as_ref().unwrap(),
            needle_core::EvidenceBrief {
                summary: "runtime flow".to_owned(),
                locations: Vec::new(),
                behavior: Some(needle_core::BehaviorTrace {
                    entrypoint: "flow".to_owned(),
                    steps: projected_flow,
                    uncertainty: Vec::new(),
                }),
                test_plan: None,
                claims: Default::default(),
            },
            &decision.artifacts,
        )
        .unwrap();
        let projected_brief: needle_core::EvidenceBrief =
            serde_json::from_value(projected.payload.clone()).unwrap();
        assert!(projected_brief.behavior.is_some());
        assert!(projected_brief.locations.iter().any(|location| location.path == "src/lib.rs"));
        assert!(
            !projected_brief.locations.iter().any(|location| location.path == "src/support.rs")
        );
        assert!(
            projected
                .dependency_manifest
                .dependencies
                .iter()
                .any(|dependency| dependency.path == "src/lib.rs")
        );
        assert!(
            projected
                .dependency_manifest
                .dependencies
                .iter()
                .any(|dependency| dependency.path == "src/flow.rs")
        );
        assert!(
            projected
                .dependency_manifest
                .dependencies
                .iter()
                .all(|dependency| dependency.path != "src/support.rs")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn authoritative_exact_coverage_and_stale_paths_are_distinct() {
        let root = std::env::temp_dir().join(format!(
            "needle-semantic-resolver-{}-{}",
            std::process::id(),
            needle_core::Digest::blake3(format!("{:?}", Instant::now())).to_hex()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        fs::write(root.join("src/flow.rs"), "pub fn flow_answer() {}\n").unwrap();
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
             @route locate.implementation\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "locate.implementation")
            .unwrap();
        let need = needle_core::compile_need(&ir, Digest::blake3(b"repo"), &route).unwrap();
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "session",
                turn_id: "turn",
                transport_digest: Digest::blake3(b"transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &ir,
                need: &need,
                fragments: std::slice::from_ref(&fragment),
            })
            .unwrap();
        let worker_artifact = SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: needle_core::LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: Some(0),
                byte_end: Some(29),
            }],
            gaps: Vec::new(),
        };
        let source_snapshot_digest = Digest::blake3(b"source");
        let request = ArtifactRequest {
            contract_id: "needle.semantic.code-location".to_owned(),
            contract_revision: 2,
            repository_id: need.world.repository_lineage,
            source_snapshot_digest,
            route_key: route.route.clone(),
            normalized_request: "first wording".to_owned(),
            semantic_fragment_id: Some(fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let incomplete_trace = needle_core::WorkerObservationTrace {
            observed_files: vec!["src/lib.rs".to_owned()],
            gaps: vec!["unknown_command_action".to_owned()],
        };
        let validated = validate_semantic_artifact_with_trace(
            &fragment,
            &worker_artifact,
            &root,
            request.semantic_id().digest(),
            Some(&incomplete_trace),
        )
        .unwrap();
        assert_eq!(validated.artifact.contract.cache_scope, CacheScope::SnapshotExact);
        store
            .publish_semantic_artifact(&request, &need, &validated.artifact, &validated.certificate)
            .unwrap();
        let resolver = SemanticResolver::new(store.clone());
        let shadow = resolver
            .resolve(
                &need,
                &root,
                source_snapshot_digest,
                Some(100),
                Some(1),
                &[request.semantic_id().digest()],
            )
            .unwrap();
        assert!(!shadow.authoritative);
        assert!(matches!(shadow.resolution, CacheResolution::Bypass { .. }));

        let class = store
            .capability_classes()
            .unwrap()
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == PredicateKind::ImplementationLocation
            })
            .unwrap();
        store
            .set_capability_mode(
                &class.id,
                class.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"evidence")),
            )
            .unwrap();
        let exact = resolver
            .resolve(
                &need,
                &root,
                source_snapshot_digest,
                Some(100),
                Some(1),
                &[request.semantic_id().digest()],
            )
            .unwrap();
        assert!(exact.authoritative);
        assert!(matches!(exact.resolution, CacheResolution::ExactHit { .. }));

        let different_source = resolver
            .resolve(&need, &root, Digest::blake3(b"different-source"), Some(100), Some(1), &[])
            .unwrap();
        assert!(!different_source.authoritative);
        assert!(matches!(different_source.resolution, CacheResolution::Stale { .. }));

        let coverage = resolver
            .resolve(&need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(coverage.authoritative);
        assert!(matches!(coverage.resolution, CacheResolution::CoverageHit { .. }));

        let trace_ir = NeedIr::parse(
            "@@need\n\
             @route trace.state-flow\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
             @world source=current features=default\n\
             \n\
             Trace the runtime flow.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let trace_route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "trace.state-flow")
            .unwrap();
        let trace_need =
            needle_core::compile_need(&trace_ir, Digest::blake3(b"repo"), &trace_route).unwrap();
        let mut trace_obligations = trace_need.required.clone();
        trace_obligations.extend(trace_need.preferred.clone());
        let trace_fragment = need_fragment(&trace_need, trace_obligations, Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "trace-session",
                turn_id: "trace-turn",
                transport_digest: Digest::blake3(b"trace-transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &trace_ir,
                need: &trace_need,
                fragments: std::slice::from_ref(&trace_fragment),
            })
            .unwrap();
        let partial = resolver
            .resolve(&trace_need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(partial.authoritative);
        assert!(matches!(partial.resolution, CacheResolution::PartialHit { .. }));
        assert_eq!(partial.artifacts.len(), 1);

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
            .map(|(index, role)| SemanticFlowStep {
                role,
                location: SemanticLocation {
                    role: needle_core::LocationRole::Supporting,
                    path: "src/flow.rs".to_owned(),
                    symbol: Some("flow".to_owned()),
                    byte_start: Some(index as u64),
                    byte_end: Some(index as u64 + 1),
                },
                description: format!("{role:?} step"),
            })
            .collect(),
            gaps: Vec::new(),
        };
        let behavior_request = ArtifactRequest {
            contract_id: "needle.semantic.behavior-trace".to_owned(),
            contract_revision: 2,
            repository_id: trace_need.world.repository_lineage,
            source_snapshot_digest: Digest::blake3(b"source"),
            route_key: trace_route.route.clone(),
            normalized_request: "trace wording".to_owned(),
            semantic_fragment_id: Some(trace_fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let validated_behavior = validate_semantic_artifact(
            &trace_fragment,
            &behavior,
            &root,
            behavior_request.semantic_id().digest(),
        )
        .unwrap();
        store
            .publish_semantic_artifact(
                &behavior_request,
                &trace_need,
                &validated_behavior.artifact,
                &validated_behavior.certificate,
            )
            .unwrap();
        let runtime_class = store
            .capability_classes()
            .unwrap()
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == PredicateKind::RuntimeFlow
            })
            .unwrap();
        store
            .set_capability_mode(
                &runtime_class.id,
                runtime_class.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"runtime-evidence")),
            )
            .unwrap();
        let composite = resolver
            .resolve(&trace_need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(composite.authoritative);
        assert!(matches!(composite.resolution, CacheResolution::CompositeHit { .. }));
        assert_eq!(composite.artifacts.len(), 2);

        let test_plan = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "answer".to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            identifiers: vec!["answer".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec!["tests/answer.rs".to_owned()],
        };
        let test_request = ArtifactRequest {
            contract_id: "needle.semantic.test-plan".to_owned(),
            contract_revision: 2,
            repository_id: trace_need.world.repository_lineage,
            source_snapshot_digest: Digest::blake3(b"source"),
            route_key: trace_route.route.clone(),
            normalized_request: "trace with focused tests".to_owned(),
            semantic_fragment_id: Some(trace_fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let declared_test_plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "answer".to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            test_identifier: "answer".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let command_evidence = CommandExecutionEvidence {
            id: "resolver-command-evidence".to_owned(),
            approval_id: "resolver-approval".to_owned(),
            argv: declared_test_plan.argv.clone(),
            cwd: root.display().to_string(),
            source_snapshot_digest,
            runner: "cargo".to_owned(),
            runner_version: None,
            exit_status: Some(0),
            duration_ms: 1,
            output_digest: Digest::blake3(b"output"),
            output_preview: "test answer ... ok\ntest result: ok. 1 passed".to_owned(),
            test_identifier: Some("answer".to_owned()),
            tests_executed: Some(1),
            infrastructure_failure: None,
        };
        store.record_command_evidence(None, &command_evidence).unwrap();
        let validated_test = validate_semantic_test_plan_with_evidence(
            &trace_fragment,
            &test_plan,
            &root,
            test_request.semantic_id().digest(),
            None,
            &declared_test_plan,
            &command_evidence,
        )
        .unwrap();
        store
            .publish_semantic_artifact(
                &test_request,
                &trace_need,
                &validated_test.artifact,
                &validated_test.certificate,
            )
            .unwrap();
        let focused_class = store
            .capability_classes()
            .unwrap()
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == PredicateKind::FocusedTests
            })
            .unwrap();
        store
            .set_capability_mode(
                &focused_class.id,
                focused_class.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"focused-evidence")),
            )
            .unwrap();
        let tests_ir = NeedIr::parse(
            "@@need\n\
             @route tests.relevant\n\
             @subject symbol:\"answer\"\n\
             @require focused-tests selection=representative completeness=open-world polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Find the relevant focused tests.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let tests_route = built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "tests.relevant")
            .unwrap();
        let tests_need =
            needle_core::compile_need(&tests_ir, Digest::blake3(b"repo"), &tests_route).unwrap();
        let tests_fragment = need_fragment(&tests_need, tests_need.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "tests-session",
                turn_id: "tests-turn",
                transport_digest: Digest::blake3(b"tests-transport"),
                parser_definition_digest: Digest::blake3(b"parser"),
                prompt_profile_digest: Digest::blake3(b"profile"),
                need_ir: &tests_ir,
                need: &tests_need,
                fragments: std::slice::from_ref(&tests_fragment),
            })
            .unwrap();
        let tests_hit = resolver
            .resolve(&tests_need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(tests_hit.authoritative);
        assert!(matches!(tests_hit.resolution, CacheResolution::CoverageHit { .. }));
        assert_eq!(tests_hit.artifacts.len(), 1);

        fs::write(root.join("notes.txt"), "unrelated\n").unwrap();
        let irrelevant_mutation = resolver
            .resolve(&trace_need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(irrelevant_mutation.authoritative);
        assert!(matches!(irrelevant_mutation.resolution, CacheResolution::CompositeHit { .. }));

        let derived_fragment = need_fragment(
            &trace_need,
            trace_fragment.obligations.clone(),
            vec![validated.semantic_id],
        );
        let mut derived_behavior = behavior.clone();
        if let SemanticWorkerArtifact::BehaviorTrace { steps, .. } = &mut derived_behavior {
            steps[0].description.push_str(" derived");
        }
        let derived_request = ArtifactRequest {
            contract_id: "needle.semantic.behavior-trace".to_owned(),
            contract_revision: 2,
            repository_id: trace_need.world.repository_lineage,
            source_snapshot_digest: Digest::blake3(b"source"),
            route_key: trace_route.route.clone(),
            normalized_request: "derived trace wording".to_owned(),
            semantic_fragment_id: Some(derived_fragment.id),
            input_artifact_ids: vec![validated.semantic_id.digest()],
        };
        let validated_derived = validate_semantic_artifact(
            &derived_fragment,
            &derived_behavior,
            &root,
            derived_request.semantic_id().digest(),
        )
        .unwrap();
        store
            .publish_semantic_artifact(
                &derived_request,
                &trace_need,
                &validated_derived.artifact,
                &validated_derived.certificate,
            )
            .unwrap();
        assert!(
            certificate_inputs_are_fresh(
                &store,
                &validated_derived.certificate,
                &root,
                source_snapshot_digest,
                0,
            )
            .unwrap()
        );

        store
            .record_contradiction(
                PredicateKind::ImplementationLocation,
                need.required[0].subject,
                need.world.id(),
                &[validated.semantic_id],
                true,
            )
            .unwrap();
        let contradicted = resolver
            .resolve(&need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(matches!(contradicted.resolution, CacheResolution::Contradicted { .. }));
        assert!(contradicted.claim_advisory.is_none());
        store
            .record_contradiction(
                PredicateKind::ImplementationLocation,
                need.required[0].subject,
                need.world.id(),
                &[validated.semantic_id],
                false,
            )
            .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 7 }\n").unwrap();
        assert!(
            !certificate_inputs_are_fresh(
                &store,
                &validated_derived.certificate,
                &root,
                source_snapshot_digest,
                0,
            )
            .unwrap()
        );
        let stale = resolver
            .resolve(&need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(!stale.authoritative);
        assert!(matches!(stale.resolution, CacheResolution::Stale { .. }));
        assert!(stale.claim_advisory.is_none());
        let relevant_partial = resolver
            .resolve(&trace_need, &root, source_snapshot_digest, Some(100), Some(1), &[])
            .unwrap();
        assert!(relevant_partial.authoritative);
        assert!(matches!(relevant_partial.resolution, CacheResolution::PartialHit { .. }));
        assert_eq!(
            relevant_partial.artifacts.iter().map(|artifact| artifact.id).collect::<Vec<_>>(),
            vec![validated_behavior.artifact.id]
        );
        let _ = fs::remove_dir_all(root);
    }
}
