use needle_core::{
    ArtifactId, ArtifactValidationCertificateId, CanonicalHasher, ClaimId, ClaimSetCertificate,
    ClaimSetCertificateId, ClaimValidationCertificateId, MAX_PROOF_ARTIFACTS, MAX_PROOF_CANDIDATES,
    Need, Obligation, PlanEconomics, ProofBudget, ReuseSufficiencyCertificate,
    ReuseSufficiencyCertificateId, SatisfactionStep, SelectedPlan, SelectedPlanId,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

const MAX_OBLIGATIONS: usize = 16;
const STATE_COUNT: usize = 1 << MAX_OBLIGATIONS;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofCandidate {
    pub artifact: ArtifactId,
    pub validation_certificate: ArtifactValidationCertificateId,
    pub coverage: Vec<Obligation>,
    pub exact_request: bool,
    pub expected_reuse_microusd: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_ids: Vec<ClaimId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_validation_certificate_ids: Vec<ClaimValidationCertificateId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_set_certificate_id: Option<ClaimSetCertificateId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofResolutionKind {
    ExactHit,
    CoverageHit,
    CompositeHit,
    PartialHit,
    Miss,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofPlan {
    pub resolution: ProofResolutionKind,
    pub selected: SelectedPlan,
    pub certificate: Option<ReuseSufficiencyCertificate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimAdvisoryResolutionKind {
    ClaimHit,
    ClaimCompositeHit,
    PartialHit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimAdvisoryPlan {
    pub resolution: ClaimAdvisoryResolutionKind,
    pub selected: SelectedPlan,
    pub certificate: ClaimSetCertificate,
    pub selected_bits: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SemanticCostEstimates {
    pub fresh_microusd: Option<u64>,
    pub artifact_reuse_microusd: Option<u64>,
    pub claim_reuse_microusd: Option<u64>,
    pub claim_partial_reuse_microusd: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValiditySelection {
    pub covered_mask: u16,
    pub selected_bits: u64,
    pub selected_count: u8,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ProofError {
    #[error("proof planning supports between 1 and 16 required obligations")]
    ObligationBound,
    #[error("proof candidate count exceeds the configured bound")]
    CandidateBound,
    #[error("proof budget is invalid")]
    Budget,
    #[error("proof planner scratch lock was poisoned")]
    ScratchPoisoned,
    #[error("claim advisory selection cannot form a canonical claim-set certificate")]
    ClaimCertificate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlanState {
    selected: u64,
    count: u8,
}

impl PlanState {
    const ROOT: Self = Self { selected: 0, count: 0 };

    fn better_than(self, other: Self) -> bool {
        (self.count, self.selected) < (other.count, other.selected)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EconomicState {
    selected: u64,
    count: u8,
    cost_microusd: u64,
}

impl EconomicState {
    const ROOT: Self = Self { selected: 0, count: 0, cost_microusd: 0 };

    fn better_than(self, other: Self) -> bool {
        (self.cost_microusd, self.count, self.selected)
            < (other.cost_microusd, other.count, other.selected)
    }
}

struct PlannerScratch {
    states: Vec<Option<PlanState>>,
    economic_states: Vec<Option<EconomicState>>,
    candidate_masks: [u16; MAX_PROOF_CANDIDATES],
}

impl PlannerScratch {
    fn new() -> Self {
        Self {
            states: vec![None; STATE_COUNT],
            economic_states: vec![None; STATE_COUNT],
            candidate_masks: [0; MAX_PROOF_CANDIDATES],
        }
    }

    fn clear(&mut self, used_states: usize, candidate_count: usize) {
        self.states[..used_states].fill(None);
        self.economic_states[..used_states].fill(None);
        self.candidate_masks[..candidate_count].fill(0);
    }
}

pub struct ProofPlanner {
    engine_definition: needle_core::Digest,
    scratch: Mutex<PlannerScratch>,
}

impl Default for ProofPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ProofPlanner {
    pub fn new() -> Self {
        let mut hash = CanonicalHasher::new(b"proof-engine");
        hash.field_str("bounded-bitmask-dp");
        hash.field_u16(MAX_OBLIGATIONS as u16);
        hash.field_u16(MAX_PROOF_CANDIDATES as u16);
        Self { engine_definition: hash.finish(), scratch: Mutex::new(PlannerScratch::new()) }
    }

    pub fn engine_definition(&self) -> needle_core::Digest {
        self.engine_definition
    }

    pub fn plan(
        &self,
        need: &Need,
        candidates: &[ProofCandidate],
        proof_overhead_micros: u64,
        expected_fresh_microusd: Option<u64>,
        budget: &ProofBudget,
    ) -> Result<ProofPlan, ProofError> {
        let validity = self.plan_validity(need, candidates, budget)?;
        let selected_bits = self.plan_economics(need, candidates, budget, validity.covered_mask)?;
        let obligation_count = need.required.len();
        let full_mask = ((1_usize << obligation_count) - 1) as u16;
        let chosen_mask = validity.covered_mask;
        let mut selected_candidates = Vec::with_capacity(validity.selected_count as usize);
        for (index, candidate) in candidates.iter().enumerate() {
            if selected_bits & (1_u64 << index) != 0 {
                selected_candidates.push(candidate);
            }
        }

        let mandatory_residual = need.residual.as_ref().is_some_and(|residual| residual.mandatory);
        let full = chosen_mask == full_mask && !mandatory_residual;
        let resolution = if selected_candidates.is_empty() {
            ProofResolutionKind::Miss
        } else if !full {
            ProofResolutionKind::PartialHit
        } else if selected_candidates.len() > 1 {
            ProofResolutionKind::CompositeHit
        } else if selected_candidates[0].exact_request {
            ProofResolutionKind::ExactHit
        } else {
            ProofResolutionKind::CoverageHit
        };
        let expected_selected = selected_candidates
            .iter()
            .map(|candidate| candidate.expected_reuse_microusd)
            .try_fold(0_u64, u64::checked_add);
        let expected_net =
            expected_fresh_microusd.zip(expected_selected).map(|(fresh, selected)| {
                i64::try_from(fresh).unwrap_or(i64::MAX)
                    - i64::try_from(selected).unwrap_or(i64::MAX)
            });
        let economics = PlanEconomics {
            expected_fresh_microusd,
            expected_selected_microusd: expected_selected,
            proof_overhead_micros,
            expected_net_microusd: expected_net,
        };
        let selected = selected_plan(
            need,
            &selected_candidates,
            chosen_mask,
            full_mask ^ chosen_mask,
            economics,
            budget.clone(),
            resolution,
        );
        let certificate = full.then(|| {
            sufficiency_certificate(
                need,
                &selected_candidates,
                selected.covered_mask,
                self.engine_definition,
            )
        });
        Ok(ProofPlan { resolution, selected, certificate })
    }

    pub fn plan_claim_advisory(
        &self,
        need: &Need,
        candidates: &[ProofCandidate],
        proof_overhead_micros: u64,
        costs: SemanticCostEstimates,
        budget: &ProofBudget,
        created_unix_ms: u64,
    ) -> Result<Option<ClaimAdvisoryPlan>, ProofError> {
        let validity = self.plan_validity(need, candidates, budget)?;
        if validity.selected_bits == 0 {
            return Ok(None);
        }
        let selected_candidates = candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| validity.selected_bits & (1_u64 << index) != 0)
            .map(|(_, candidate)| candidate)
            .collect::<Vec<_>>();
        let claim_candidates = selected_candidates
            .iter()
            .filter(|candidate| !candidate.claim_ids.is_empty())
            .copied()
            .collect::<Vec<_>>();
        if claim_candidates.is_empty() {
            return Ok(None);
        }
        let full_mask = ((1_usize << need.required.len()) - 1) as u16;
        let missing_mask = full_mask ^ validity.covered_mask;
        let artifact_candidate_count =
            selected_candidates.len().saturating_sub(claim_candidates.len());
        let resolution = if missing_mask != 0 {
            ClaimAdvisoryResolutionKind::PartialHit
        } else if artifact_candidate_count == 0 && claim_candidates.len() == 1 {
            ClaimAdvisoryResolutionKind::ClaimHit
        } else {
            ClaimAdvisoryResolutionKind::ClaimCompositeHit
        };
        let mut artifacts =
            selected_candidates.iter().map(|candidate| candidate.artifact).collect::<Vec<_>>();
        artifacts.sort_unstable();
        artifacts.dedup();
        let mut members = BTreeMap::<ClaimId, ClaimValidationCertificateId>::new();
        let mut component_certificates = BTreeSet::<ClaimSetCertificateId>::new();
        for candidate in &claim_candidates {
            if candidate.claim_ids.len() != candidate.claim_validation_certificate_ids.len() {
                return Err(ProofError::ClaimCertificate);
            }
            for (claim, certificate) in
                candidate.claim_ids.iter().zip(&candidate.claim_validation_certificate_ids)
            {
                members
                    .entry(*claim)
                    .and_modify(|current| *current = (*current).min(*certificate))
                    .or_insert(*certificate);
            }
            if let Some(certificate) = candidate.claim_set_certificate_id {
                component_certificates.insert(certificate);
            }
        }
        let claim_obligations = need
            .required
            .iter()
            .filter(|requested| {
                claim_candidates.iter().any(|candidate| {
                    candidate.coverage.iter().any(|provided| provided.satisfies(requested))
                })
            })
            .map(|obligation| obligation.id)
            .collect::<Vec<_>>();
        let certificate = ClaimSetCertificate::new(
            need.id,
            members.iter().map(|(claim, certificate)| (*claim, *certificate)).collect(),
            claim_obligations,
            need.world.id(),
            crate::claim_proof_engine_definition(),
            created_unix_ms,
        )
        .ok_or(ProofError::ClaimCertificate)?;
        let claim_ids = members.keys().copied().collect::<Vec<_>>();
        let claim_validation_certificate_ids = members.values().copied().collect::<Vec<_>>();
        let mut claim_set_certificate_ids = component_certificates.into_iter().collect::<Vec<_>>();
        claim_set_certificate_ids.push(certificate.id);
        claim_set_certificate_ids.sort_unstable();
        claim_set_certificate_ids.dedup();
        let mut hash = CanonicalHasher::new(b"selected-claim-advisory-plan");
        hash.field_digest(need.id.digest());
        hash.field_u16(validity.covered_mask);
        hash.field_u16(missing_mask);
        for artifact in &artifacts {
            hash.field_digest(artifact.digest());
        }
        for claim in &claim_ids {
            hash.field_digest(claim.digest());
        }
        hash.field_u8(match resolution {
            ClaimAdvisoryResolutionKind::ClaimHit => 0,
            ClaimAdvisoryResolutionKind::ClaimCompositeHit => 1,
            ClaimAdvisoryResolutionKind::PartialHit => 2,
        });
        let selected_cost = match resolution {
            ClaimAdvisoryResolutionKind::PartialHit => costs.claim_partial_reuse_microusd,
            ClaimAdvisoryResolutionKind::ClaimHit
            | ClaimAdvisoryResolutionKind::ClaimCompositeHit => costs.claim_reuse_microusd,
        };
        let selected = SelectedPlan {
            id: SelectedPlanId(hash.finish()),
            need: need.id,
            artifact_ids: artifacts,
            claim_ids,
            claim_validation_certificate_ids,
            claim_set_certificate_ids,
            covered_mask: validity.covered_mask,
            missing_mask,
            economics: PlanEconomics {
                expected_fresh_microusd: costs.fresh_microusd,
                expected_selected_microusd: selected_cost,
                proof_overhead_micros,
                expected_net_microusd: costs.fresh_microusd.zip(selected_cost).map(
                    |(fresh, selected)| {
                        i64::try_from(fresh).unwrap_or(i64::MAX)
                            - i64::try_from(selected).unwrap_or(i64::MAX)
                    },
                ),
            },
            proof_budget: budget.clone(),
            decision_reason: format!("Advisory::{resolution:?}"),
        };
        Ok(Some(ClaimAdvisoryPlan {
            resolution,
            selected,
            certificate,
            selected_bits: validity.selected_bits,
        }))
    }

    pub fn replay_claim_advisory(
        &self,
        need: &Need,
        advisory: &ClaimAdvisoryPlan,
        candidates: &[ProofCandidate],
    ) -> bool {
        self.plan_claim_advisory(
            need,
            candidates,
            advisory.selected.economics.proof_overhead_micros,
            SemanticCostEstimates {
                fresh_microusd: advisory.selected.economics.expected_fresh_microusd,
                artifact_reuse_microusd: None,
                claim_reuse_microusd: advisory.selected.economics.expected_selected_microusd,
                claim_partial_reuse_microusd: advisory
                    .selected
                    .economics
                    .expected_selected_microusd,
            },
            &advisory.selected.proof_budget,
            advisory.certificate.created_unix_ms,
        )
        .is_ok_and(|replayed| replayed.as_ref() == Some(advisory))
    }

    fn plan_economics(
        &self,
        need: &Need,
        candidates: &[ProofCandidate],
        budget: &ProofBudget,
        target_mask: u16,
    ) -> Result<u64, ProofError> {
        let state_count = 1_usize << need.required.len();
        let mut scratch = self.scratch.lock().map_err(|_| ProofError::ScratchPoisoned)?;
        scratch.clear(state_count, candidates.len());
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let mut mask = 0_u16;
            for (obligation_index, requested) in need.required.iter().enumerate() {
                if candidate.coverage.iter().any(|provided| provided.satisfies(requested)) {
                    mask |= 1_u16 << obligation_index;
                }
            }
            scratch.candidate_masks[candidate_index] = mask;
        }
        scratch.economic_states[0] = Some(EconomicState::ROOT);
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let coverage = scratch.candidate_masks[candidate_index];
            if coverage == 0 {
                continue;
            }
            for mask in (0..state_count).rev() {
                let Some(existing) = scratch.economic_states[mask] else {
                    continue;
                };
                if existing.count >= budget.max_artifacts {
                    continue;
                }
                let next_mask = (mask as u16 | coverage) as usize;
                let next = EconomicState {
                    selected: existing.selected | (1_u64 << candidate_index),
                    count: existing.count.saturating_add(1),
                    cost_microusd: existing
                        .cost_microusd
                        .saturating_add(candidate.expected_reuse_microusd),
                };
                match scratch.economic_states[next_mask] {
                    Some(current) if !next.better_than(current) => {}
                    _ => scratch.economic_states[next_mask] = Some(next),
                }
            }
        }
        Ok(scratch.economic_states[target_mask as usize].map(|state| state.selected).unwrap_or(0))
    }

    /// Validity-only exact set cover. After the resident scratch allocation,
    /// this method performs no heap allocation and reads no economic input.
    pub fn plan_validity(
        &self,
        need: &Need,
        candidates: &[ProofCandidate],
        budget: &ProofBudget,
    ) -> Result<ValiditySelection, ProofError> {
        let obligation_count = need.required.len();
        if obligation_count == 0 || obligation_count > MAX_OBLIGATIONS {
            return Err(ProofError::ObligationBound);
        }
        if candidates.len() > MAX_PROOF_CANDIDATES
            || candidates.len() > usize::from(budget.max_candidates)
        {
            return Err(ProofError::CandidateBound);
        }
        if budget.max_artifacts == 0
            || usize::from(budget.max_artifacts) > MAX_PROOF_ARTIFACTS
            || budget.max_plan_nodes == 0
            || usize::from(budget.max_plan_nodes) > MAX_OBLIGATIONS
            || obligation_count > usize::from(budget.max_plan_nodes)
            || budget.max_derivation_depth == 0
            || usize::from(budget.max_derivation_depth) > needle_core::MAX_DERIVATION_DEPTH
            || budget.max_validation_millis == 0
            || budget.max_projection_tokens == 0
        {
            return Err(ProofError::Budget);
        }

        let state_count = 1_usize << obligation_count;
        let full_mask = (state_count - 1) as u16;
        let mut scratch = self.scratch.lock().map_err(|_| ProofError::ScratchPoisoned)?;
        scratch.clear(state_count, candidates.len());
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let mut mask = 0_u16;
            for (obligation_index, requested) in need.required.iter().enumerate() {
                if candidate.coverage.iter().any(|provided| provided.satisfies(requested)) {
                    mask |= 1_u16 << obligation_index;
                }
            }
            scratch.candidate_masks[candidate_index] = mask;
        }
        scratch.states[0] = Some(PlanState::ROOT);
        for candidate_index in 0..candidates.len() {
            let coverage = scratch.candidate_masks[candidate_index];
            if coverage == 0 {
                continue;
            }
            for mask in (0..state_count).rev() {
                let Some(existing) = scratch.states[mask] else {
                    continue;
                };
                if existing.count >= budget.max_artifacts {
                    continue;
                }
                let next_mask = (mask as u16 | coverage) as usize;
                let next = PlanState {
                    selected: existing.selected | (1_u64 << candidate_index),
                    count: existing.count.saturating_add(1),
                };
                match scratch.states[next_mask] {
                    Some(current) if !next.better_than(current) => {}
                    _ => scratch.states[next_mask] = Some(next),
                }
            }
        }

        let mandatory_residual = need.residual.as_ref().is_some_and(|residual| residual.mandatory);
        let chosen_mask = if !mandatory_residual && scratch.states[full_mask as usize].is_some() {
            full_mask
        } else {
            best_partial_mask(&scratch.states[..state_count])
        };
        let state = scratch.states[chosen_mask as usize].unwrap_or(PlanState::ROOT);
        Ok(ValiditySelection {
            covered_mask: chosen_mask,
            selected_bits: state.selected,
            selected_count: state.count,
        })
    }

    pub fn replay(
        &self,
        need: &Need,
        certificate: &ReuseSufficiencyCertificate,
        candidates: &[ProofCandidate],
    ) -> bool {
        if certificate.need != need.id
            || certificate.engine_definition != self.engine_definition
            || certificate.id != sufficiency_certificate_id(certificate)
            || certificate.residual.as_ref().is_some_and(|residual| residual.mandatory)
            || certificate.world_digest != need.world.id()
            || certificate.obligations.len() != need.required.len()
            || !certificate
                .obligations
                .iter()
                .copied()
                .eq(need.required.iter().map(|obligation| obligation.id))
            || certificate.artifacts.len() != certificate.validation_certificates.len()
            || certificate.freshness_digest != relation_digest(b"freshness", &certificate.artifacts)
            || certificate.contradiction_digest
                != relation_digest(b"contradictions-clear", &certificate.artifacts)
        {
            return false;
        }
        for requested in &need.required {
            let Some(step) =
                certificate.satisfaction_steps.iter().find(|step| step.obligation == requested.id)
            else {
                return false;
            };
            let Some(candidate) = candidates.iter().find(|candidate| {
                candidate.artifact == step.artifact
                    && candidate.validation_certificate == step.validation_certificate
            }) else {
                return false;
            };
            if !certificate.artifacts.contains(&step.artifact)
                || !certificate.validation_certificates.contains(&step.validation_certificate)
            {
                return false;
            }
            if !candidate.coverage.iter().any(|provided| provided.satisfies(requested)) {
                return false;
            }
        }
        true
    }
}

fn best_partial_mask(states: &[Option<PlanState>]) -> u16 {
    let mut best_mask = 0_u16;
    let mut best_state = PlanState::ROOT;
    for (mask, state) in states.iter().enumerate().skip(1) {
        let Some(state) = *state else {
            continue;
        };
        let ordering = (mask.count_ones(), std::cmp::Reverse(state.count))
            .cmp(&(best_mask.count_ones(), std::cmp::Reverse(best_state.count)));
        if ordering == Ordering::Greater
            || (ordering == Ordering::Equal && (mask as u16) < best_mask)
        {
            best_mask = mask as u16;
            best_state = state;
        }
    }
    best_mask
}

fn selected_plan(
    need: &Need,
    selected: &[&ProofCandidate],
    covered_mask: u16,
    missing_mask: u16,
    economics: PlanEconomics,
    proof_budget: ProofBudget,
    resolution: ProofResolutionKind,
) -> SelectedPlan {
    let mut hash = CanonicalHasher::new(b"selected-plan");
    hash.field_digest(need.id.digest());
    hash.field_u16(covered_mask);
    hash.field_u16(missing_mask);
    for candidate in selected {
        hash.field_digest(candidate.artifact.digest());
    }
    hash.field_u8(resolution as u8);
    SelectedPlan {
        id: SelectedPlanId(hash.finish()),
        need: need.id,
        artifact_ids: selected.iter().map(|candidate| candidate.artifact).collect(),
        claim_ids: Vec::new(),
        claim_validation_certificate_ids: Vec::new(),
        claim_set_certificate_ids: Vec::new(),
        covered_mask,
        missing_mask,
        economics,
        proof_budget,
        decision_reason: format!("{resolution:?}"),
    }
}

fn sufficiency_certificate(
    need: &Need,
    selected: &[&ProofCandidate],
    covered_mask: u16,
    engine_definition: needle_core::Digest,
) -> ReuseSufficiencyCertificate {
    let mut steps = Vec::with_capacity(need.required.len());
    for (index, requested) in need.required.iter().enumerate() {
        if covered_mask & (1_u16 << index) == 0 {
            continue;
        }
        let candidate = selected
            .iter()
            .find(|candidate| {
                candidate.coverage.iter().any(|provided| provided.satisfies(requested))
            })
            .expect("selected coverage mask has a satisfying candidate");
        steps.push(SatisfactionStep {
            obligation: requested.id,
            artifact: candidate.artifact,
            validation_certificate: candidate.validation_certificate,
        });
    }
    let artifacts = selected.iter().map(|candidate| candidate.artifact).collect::<Vec<_>>();
    let validation_certificates =
        selected.iter().map(|candidate| candidate.validation_certificate).collect::<Vec<_>>();
    let freshness_digest = relation_digest(b"freshness", &artifacts);
    let contradiction_digest = relation_digest(b"contradictions-clear", &artifacts);
    let mut certificate = ReuseSufficiencyCertificate {
        id: ReuseSufficiencyCertificateId(needle_core::Digest::blake3(b"pending")),
        need: need.id,
        obligations: need.required.iter().map(|obligation| obligation.id).collect(),
        artifacts,
        validation_certificates,
        satisfaction_steps: steps,
        world_digest: need.world.id(),
        freshness_digest,
        contradiction_digest,
        residual: need.residual.clone(),
        engine_definition,
    };
    certificate.id = sufficiency_certificate_id(&certificate);
    certificate
}

fn sufficiency_certificate_id(
    certificate: &ReuseSufficiencyCertificate,
) -> ReuseSufficiencyCertificateId {
    let mut hash = CanonicalHasher::new(b"reuse-sufficiency-certificate");
    hash.field_digest(certificate.need.digest());
    for obligation in &certificate.obligations {
        hash.field_digest(obligation.digest());
    }
    for artifact in &certificate.artifacts {
        hash.field_digest(artifact.digest());
    }
    for validation in &certificate.validation_certificates {
        hash.field_digest(validation.digest());
    }
    for step in &certificate.satisfaction_steps {
        hash.field_digest(step.obligation.digest());
        hash.field_digest(step.artifact.digest());
        hash.field_digest(step.validation_certificate.digest());
    }
    hash.field_digest(certificate.world_digest);
    hash.field_digest(certificate.freshness_digest);
    hash.field_digest(certificate.contradiction_digest);
    match &certificate.residual {
        Some(residual) => {
            hash.field_u8(1);
            hash.field_digest(residual.raw_digest);
            hash.field_u8(u8::from(residual.mandatory));
        }
        None => hash.field_u8(0),
    }
    hash.field_digest(certificate.engine_definition);
    ReuseSufficiencyCertificateId(hash.finish())
}

fn relation_digest(domain: &'static [u8], artifacts: &[ArtifactId]) -> needle_core::Digest {
    let mut hash = CanonicalHasher::new(domain);
    for artifact in artifacts {
        hash.field_digest(artifact.digest());
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        ClaimId, ClaimSetCertificateId, ClaimValidationCertificateId, Digest, Facet, NeedId,
        ObligationId, PredicateKind, ResidualIntent, SemanticWorld, SubjectId,
    };

    fn obligation(index: u8) -> Obligation {
        let subject = SubjectId(Digest::blake3(b"subject"));
        let predicate = match index {
            0 => PredicateKind::ImplementationLocation,
            1 => PredicateKind::RuntimeFlow,
            _ => PredicateKind::FocusedTests,
        };
        Obligation::new(
            predicate,
            subject,
            vec![Facet { key: "slot".to_owned(), value: index.to_string() }],
        )
    }

    fn need(count: u8) -> Need {
        let required = (0..count).map(obligation).collect::<Vec<_>>();
        Need {
            id: NeedId(Digest::blake3(b"need")),
            subjects: Vec::new(),
            required,
            preferred: Vec::new(),
            semantic_constraints: Vec::new(),
            world: SemanticWorld {
                repository_lineage: Digest::blake3(b"repo"),
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

    fn candidate(index: u8, coverage: Vec<Obligation>, exact: bool, cost: u64) -> ProofCandidate {
        ProofCandidate {
            artifact: ArtifactId(Digest::blake3([index])),
            validation_certificate: ArtifactValidationCertificateId(Digest::blake3([index, 1])),
            coverage,
            exact_request: exact,
            expected_reuse_microusd: cost,
            claim_ids: Vec::new(),
            claim_validation_certificate_ids: Vec::new(),
            claim_set_certificate_id: None,
        }
    }

    fn claim_candidate(index: u8, coverage: Vec<Obligation>) -> ProofCandidate {
        let mut candidate = candidate(index, coverage, false, 0);
        candidate.claim_ids = vec![ClaimId(Digest::blake3([index, 2]))];
        candidate.claim_validation_certificate_ids =
            vec![ClaimValidationCertificateId(Digest::blake3([index, 3]))];
        candidate.claim_set_certificate_id =
            Some(ClaimSetCertificateId(Digest::blake3([index, 4])));
        candidate
    }

    #[test]
    fn exact_coverage_composite_and_partial_are_distinct() {
        let planner = ProofPlanner::new();
        let need = need(2);
        let exact = candidate(0, need.required.clone(), true, 1);
        assert_eq!(
            planner.plan(&need, &[exact], 1, Some(10), &ProofBudget::default()).unwrap().resolution,
            ProofResolutionKind::ExactHit
        );
        let coverage = candidate(1, need.required.clone(), false, 1);
        assert_eq!(
            planner
                .plan(&need, &[coverage], 1, Some(10), &ProofBudget::default())
                .unwrap()
                .resolution,
            ProofResolutionKind::CoverageHit
        );
        let left = candidate(2, vec![need.required[0].clone()], false, 1);
        let right = candidate(3, vec![need.required[1].clone()], false, 1);
        assert_eq!(
            planner
                .plan(&need, &[left.clone(), right], 1, Some(10), &ProofBudget::default())
                .unwrap()
                .resolution,
            ProofResolutionKind::CompositeHit
        );
        assert_eq!(
            planner.plan(&need, &[left], 1, Some(10), &ProofBudget::default()).unwrap().resolution,
            ProofResolutionKind::PartialHit
        );
    }

    #[test]
    fn claim_advisory_distinguishes_claim_mixed_and_partial_plans() {
        let planner = ProofPlanner::new();
        let need = need(2);
        let budget = ProofBudget::default();

        let claim_hit = planner
            .plan_claim_advisory(
                &need,
                &[claim_candidate(10, need.required.clone())],
                7,
                SemanticCostEstimates::default(),
                &budget,
                1,
            )
            .unwrap()
            .unwrap();
        assert_eq!(claim_hit.resolution, ClaimAdvisoryResolutionKind::ClaimHit);
        assert_eq!(claim_hit.selected.covered_mask, 0b11);
        assert_eq!(claim_hit.selected.missing_mask, 0);
        assert!(planner.replay_claim_advisory(
            &need,
            &claim_hit,
            &[claim_candidate(10, need.required.clone())],
        ));

        let artifact = candidate(11, vec![need.required[0].clone()], false, 1);
        let claim = claim_candidate(12, vec![need.required[1].clone()]);
        let mixed = planner
            .plan_claim_advisory(
                &need,
                &[artifact.clone(), claim.clone()],
                8,
                SemanticCostEstimates::default(),
                &budget,
                2,
            )
            .unwrap()
            .unwrap();
        assert_eq!(mixed.resolution, ClaimAdvisoryResolutionKind::ClaimCompositeHit);
        assert_eq!(mixed.selected.covered_mask, 0b11);
        assert_eq!(mixed.selected.artifact_ids.len(), 2);
        assert_eq!(mixed.selected.claim_ids, claim.claim_ids);

        let partial = planner
            .plan_claim_advisory(
                &need,
                &[claim_candidate(13, vec![need.required[0].clone()])],
                9,
                SemanticCostEstimates::default(),
                &budget,
                3,
            )
            .unwrap()
            .unwrap();
        assert_eq!(partial.resolution, ClaimAdvisoryResolutionKind::PartialHit);
        assert_eq!(partial.selected.covered_mask, 0b01);
        assert_eq!(partial.selected.missing_mask, 0b10);
    }

    #[test]
    fn stronger_coverage_satisfies_a_request_with_fewer_facets() {
        let mut need = need(1);
        let mut provided = need.required[0].clone();
        provided
            .facets
            .push(Facet { key: "granularity".to_owned(), value: "exact-location".to_owned() });
        need.required[0].facets.clear();
        need.required[0].id = ObligationId(Digest::blake3(b"weaker"));
        let plan = ProofPlanner::new()
            .plan(
                &need,
                &[candidate(1, vec![provided], false, 1)],
                1,
                Some(10),
                &ProofBudget::default(),
            )
            .unwrap();
        assert_eq!(plan.resolution, ProofResolutionKind::CoverageHit);
    }

    #[test]
    fn economics_selects_the_cheapest_semantically_valid_plan() {
        let planner = ProofPlanner::new();
        let need = need(2);
        let expensive = candidate(0, need.required.clone(), false, 50);
        let left = candidate(1, vec![need.required[0].clone()], false, 4);
        let right = candidate(2, vec![need.required[1].clone()], false, 5);
        let plan = planner
            .plan(
                &need,
                &[expensive, left.clone(), right.clone()],
                1,
                Some(100),
                &ProofBudget::default(),
            )
            .unwrap();
        assert_eq!(plan.resolution, ProofResolutionKind::CompositeHit);
        assert_eq!(plan.selected.artifact_ids, vec![left.artifact, right.artifact]);
        assert_eq!(plan.selected.economics.expected_selected_microusd, Some(9));
    }

    #[test]
    fn validity_dp_matches_brute_force_on_reduced_problems() {
        let planner = ProofPlanner::new();
        let need = need(5);
        for seed in 0_u8..16 {
            let candidates = (0_u8..8)
                .map(|candidate_index| {
                    let mut coverage = need
                        .required
                        .iter()
                        .enumerate()
                        .filter(|(obligation_index, _)| {
                            (usize::from(seed)
                                + usize::from(candidate_index) * 3
                                + obligation_index * 5)
                                % 4
                                == 0
                        })
                        .map(|(_, obligation)| obligation.clone())
                        .collect::<Vec<_>>();
                    if coverage.is_empty() {
                        coverage.push(
                            need.required[usize::from(candidate_index) % need.required.len()]
                                .clone(),
                        );
                    }
                    candidate(candidate_index, coverage, false, u64::from(candidate_index))
                })
                .collect::<Vec<_>>();
            let actual =
                planner.plan_validity(&need, &candidates, &ProofBudget::default()).unwrap();
            let expected = brute_force_validity(&need, &candidates, &ProofBudget::default());
            assert_eq!(actual, expected, "seed {seed}");
        }
    }

    #[test]
    fn mandatory_residual_blocks_full_reuse() {
        let mut need = need(1);
        need.residual = Some(ResidualIntent {
            raw_digest: Digest::blake3(b"residual"),
            reason: needle_core::ResidualReason::UnparsedConstraint,
            mandatory: true,
        });
        let candidate = candidate(1, need.required.clone(), true, 1);
        let plan = ProofPlanner::new()
            .plan(&need, &[candidate], 1, Some(10), &ProofBudget::default())
            .unwrap();
        assert_eq!(plan.resolution, ProofResolutionKind::PartialHit);
        assert!(plan.certificate.is_none());
    }

    #[test]
    fn replay_detects_tampering_without_replanning() {
        let planner = ProofPlanner::new();
        let need = need(1);
        let candidate = candidate(1, need.required.clone(), false, 1);
        let plan = planner
            .plan(&need, std::slice::from_ref(&candidate), 1, Some(10), &ProofBudget::default())
            .unwrap();
        let mut certificate = plan.certificate.unwrap();
        assert!(planner.replay(&need, &certificate, std::slice::from_ref(&candidate)));
        certificate.engine_definition = Digest::blake3(b"tampered");
        assert!(!planner.replay(&need, &certificate, &[candidate]));
    }

    #[test]
    fn replay_rejects_certificate_relationship_tampering() {
        let planner = ProofPlanner::new();
        let need = need(1);
        let candidate = candidate(1, need.required.clone(), false, 1);
        let plan = planner
            .plan(&need, std::slice::from_ref(&candidate), 1, Some(10), &ProofBudget::default())
            .unwrap();
        let mut certificate = plan.certificate.unwrap();
        certificate.artifacts.clear();
        assert!(!planner.replay(&need, &certificate, &[candidate]));
    }

    fn brute_force_validity(
        need: &Need,
        candidates: &[ProofCandidate],
        budget: &ProofBudget,
    ) -> ValiditySelection {
        let state_count = 1_usize << need.required.len();
        let full_mask = (state_count - 1) as u16;
        let mut states = vec![None; state_count];
        states[0] = Some(PlanState::ROOT);
        for selected in 1_u64..(1_u64 << candidates.len()) {
            let count = selected.count_ones() as u8;
            if count > budget.max_artifacts {
                continue;
            }
            let mut coverage = 0_u16;
            for (candidate_index, candidate) in candidates.iter().enumerate() {
                if selected & (1_u64 << candidate_index) == 0 {
                    continue;
                }
                for (obligation_index, requested) in need.required.iter().enumerate() {
                    if candidate.coverage.iter().any(|provided| provided.satisfies(requested)) {
                        coverage |= 1_u16 << obligation_index;
                    }
                }
            }
            let state = PlanState { selected, count };
            match states[coverage as usize] {
                Some(current) if !state.better_than(current) => {}
                _ => states[coverage as usize] = Some(state),
            }
        }
        let covered_mask = if states[full_mask as usize].is_some() {
            full_mask
        } else {
            best_partial_mask(&states)
        };
        let state = states[covered_mask as usize].unwrap_or(PlanState::ROOT);
        ValiditySelection {
            covered_mask,
            selected_bits: state.selected,
            selected_count: state.count,
        }
    }
}
