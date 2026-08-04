use super::{
    LifecycleError, bounded_identifier, safe_relative_directory, test_plan_contains_absolute_path,
};
use crate::{
    AcceptanceStatus, ApprovalDecisionSource, CanonicalHasher, ChangeApplyId, ChangeApplyStatus,
    ChangeId, Digest, PatchId, RoleProfileProvenance, TestPlan, VerificationArtifactId,
    VerificationStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub const MAX_LIFECYCLE_ARTIFACT_REFS: usize = 32;
pub const MAX_LIFECYCLE_TEST_PLANS: usize = crate::MAX_VERIFIER_TEST_PLANS;
pub const MAX_LIFECYCLE_REVIEW_FINDINGS: usize = 32;
pub const MAX_LIFECYCLE_REASON_CODE_BYTES: usize = 64;
pub const MAX_LIFECYCLE_EVIDENCE_ID_BYTES: usize = 128;
pub const MAX_LIFECYCLE_STATE_BYTES: usize = 64 * 1024;
pub const MAX_LIFECYCLE_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_LIFECYCLE_EVENTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LifecycleId(pub Digest);

impl LifecycleId {
    pub fn compute(
        change_id: &ChangeId,
        source_snapshot: Digest,
        profiles: &LifecycleWorkerProfiles,
    ) -> Self {
        let mut hasher = CanonicalHasher::new(b"needle-development-lifecycle");
        hasher.field_str(change_id.as_str());
        hasher.field_digest(source_snapshot);
        profiles.hash_into(&mut hasher);
        Self(hasher.finish())
    }
}

impl std::fmt::Display for LifecycleId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Explore,
    Implement,
    Test,
    Review,
    Verify,
    Apply,
}

impl LifecyclePhase {
    pub const ALL: [Self; 6] =
        [Self::Explore, Self::Implement, Self::Test, Self::Review, Self::Verify, Self::Apply];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Implement => "implement",
            Self::Test => "test",
            Self::Review => "review",
            Self::Verify => "verify",
            Self::Apply => "apply",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Active,
    RepairReserved,
    AwaitingApproval,
    Approved,
    Applying,
    Completed,
    Failed,
    Cancelled,
    Inconclusive,
    RolledBack,
}

impl LifecycleStatus {
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Inconclusive
                | Self::RolledBack
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleTerminalOutcome {
    Applied,
    Failed,
    Cancelled,
    Inconclusive,
    RolledBack,
    RollbackFailed,
    RecoveryConflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleReason {
    pub code: String,
    pub detail_digest: Digest,
}

impl LifecycleReason {
    pub fn new(code: impl Into<String>, detail: impl AsRef<[u8]>) -> Result<Self, LifecycleError> {
        let reason = Self { code: code.into(), detail_digest: Digest::blake3(detail) };
        reason.validate()?;
        Ok(reason)
    }

    pub(super) fn validate(&self) -> Result<(), LifecycleError> {
        if !bounded_identifier(&self.code, MAX_LIFECYCLE_REASON_CODE_BYTES) {
            return Err(LifecycleError::InvalidReason);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleBudget {
    pub max_worker_turns: u32,
    pub max_output_tokens: u64,
    pub max_cost_microusd: u64,
    pub max_concurrent_workers: u8,
}

impl LifecycleBudget {
    fn validate(&self) -> Result<(), LifecycleError> {
        if self.max_worker_turns == 0
            || self.max_output_tokens == 0
            || self.max_cost_microusd == 0
            || self.max_concurrent_workers != 1
        {
            return Err(LifecycleError::InvalidBudget);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleUsage {
    pub worker_turns: u32,
    pub output_tokens: u64,
    pub cost_microusd: u64,
}

impl LifecycleUsage {
    pub(super) fn checked_add(&self, delta: &Self) -> Result<Self, LifecycleError> {
        Ok(Self {
            worker_turns: self
                .worker_turns
                .checked_add(delta.worker_turns)
                .ok_or(LifecycleError::BudgetOverflow)?,
            output_tokens: self
                .output_tokens
                .checked_add(delta.output_tokens)
                .ok_or(LifecycleError::BudgetOverflow)?,
            cost_microusd: self
                .cost_microusd
                .checked_add(delta.cost_microusd)
                .ok_or(LifecycleError::BudgetOverflow)?,
        })
    }

    pub(super) fn within(&self, budget: &LifecycleBudget) -> bool {
        self.worker_turns <= budget.max_worker_turns
            && self.output_tokens <= budget.max_output_tokens
            && self.cost_microusd <= budget.max_cost_microusd
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleWorkerProfiles {
    pub explore: RoleProfileProvenance,
    pub implement: RoleProfileProvenance,
    pub test: RoleProfileProvenance,
    pub review: RoleProfileProvenance,
    pub verify: RoleProfileProvenance,
}

impl LifecycleWorkerProfiles {
    pub fn for_phase(&self, phase: LifecyclePhase) -> Option<&RoleProfileProvenance> {
        match phase {
            LifecyclePhase::Explore => Some(&self.explore),
            LifecyclePhase::Implement => Some(&self.implement),
            LifecyclePhase::Test => Some(&self.test),
            LifecyclePhase::Review => Some(&self.review),
            LifecyclePhase::Verify => Some(&self.verify),
            LifecyclePhase::Apply => None,
        }
    }

    pub(super) fn validate(&self) -> Result<(), LifecycleError> {
        for phase in LifecyclePhase::ALL.into_iter().take(5) {
            self.for_phase(phase)
                .ok_or(LifecycleError::ProfileMismatch)?
                .validate()
                .map_err(|_| LifecycleError::ProfileMismatch)?;
        }
        Ok(())
    }

    fn hash_into(&self, hasher: &mut CanonicalHasher) {
        for phase in LifecyclePhase::ALL.into_iter().take(5) {
            let profile = self.for_phase(phase).expect("worker phase has a profile");
            hasher.field_str(phase.as_str());
            hasher.field_str(profile.profile_id.as_str());
            hasher.field_bytes(&profile.revision.to_le_bytes());
            hasher.field_digest(profile.definition_digest);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleTestPlanBinding {
    pub plan: TestPlan,
    pub certificate_digest: Digest,
}

impl LifecycleTestPlanBinding {
    pub fn plan_digest(&self) -> Digest {
        self.plan.identity_digest()
    }

    fn validate(&self) -> Result<(), LifecycleError> {
        if !self.plan.requires_approval
            || self.plan.execution_evidence_id.is_some()
            || self.plan.test_command().is_err()
            || !safe_relative_directory(&self.plan.cwd_relative)
            || test_plan_contains_absolute_path(&self.plan)
        {
            return Err(LifecycleError::InvalidTestPlan);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSpec {
    pub worker_depth_limit: u8,
    pub profiles: LifecycleWorkerProfiles,
    pub budget: LifecycleBudget,
    pub test_plans: Vec<LifecycleTestPlanBinding>,
}

impl LifecycleSpec {
    pub fn validate(&self) -> Result<(), LifecycleError> {
        if self.worker_depth_limit != 1 {
            return Err(LifecycleError::NestedWorker);
        }
        self.profiles.validate()?;
        self.budget.validate()?;
        if self.test_plans.is_empty() || self.test_plans.len() > MAX_LIFECYCLE_TEST_PLANS {
            return Err(LifecycleError::InvalidTestPlan);
        }
        let mut prior = None;
        for binding in &self.test_plans {
            binding.validate()?;
            let digest = binding.plan_digest();
            if prior.is_some_and(|value| value >= digest) {
                return Err(LifecycleError::InvalidTestPlan);
            }
            prior = Some(digest);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleArtifactKind {
    Exploration,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleArtifactRef {
    pub kind: LifecycleArtifactKind,
    pub id: Digest,
    pub source_snapshot: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecyclePatchRef {
    pub patch_id: PatchId,
    pub revision: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleTestResult {
    pub plan_digest: Digest,
    pub certificate_digest: Digest,
    pub available: bool,
    pub executed: bool,
    pub passed: bool,
    pub evidence_id: Option<String>,
    pub failure_code: Option<String>,
}

impl LifecycleTestResult {
    pub(super) fn validate(&self) -> Result<(), LifecycleError> {
        if self
            .evidence_id
            .as_deref()
            .is_some_and(|value| !bounded_identifier(value, MAX_LIFECYCLE_EVIDENCE_ID_BYTES))
            || self
                .failure_code
                .as_deref()
                .is_some_and(|value| !bounded_identifier(value, MAX_LIFECYCLE_REASON_CODE_BYTES))
        {
            return Err(LifecycleError::InvalidTestEvidence);
        }
        if self.passed {
            if !self.available
                || !self.executed
                || self.evidence_id.is_none()
                || self.failure_code.is_some()
            {
                return Err(LifecycleError::InvalidTestEvidence);
            }
        } else if self.failure_code.is_none() {
            return Err(LifecycleError::InvalidTestEvidence);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleReviewVerdict {
    Approved,
    Rejected,
    Inconclusive,
}

/// Redacted review coverage. Persisted lifecycle events contain only stable
/// digests, never criterion prose, paths, or a reviewer transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleAcceptanceReview {
    pub criterion_digest: Digest,
    pub status: AcceptanceStatus,
    pub evidence_digest: Digest,
}

impl LifecycleAcceptanceReview {
    pub fn new(
        criterion: impl AsRef<[u8]>,
        status: AcceptanceStatus,
        evidence: impl AsRef<[u8]>,
    ) -> Self {
        Self {
            criterion_digest: Digest::blake3(criterion),
            status,
            evidence_digest: Digest::blake3(evidence),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewArtifact {
    pub id: Digest,
    pub change_id: ChangeId,
    pub patch_id: PatchId,
    pub verdict: LifecycleReviewVerdict,
    pub acceptance_coverage: Vec<LifecycleAcceptanceReview>,
    pub findings: Vec<LifecycleReason>,
    pub reviewer_definition: Digest,
    pub created_unix_ms: u64,
}

impl ReviewArtifact {
    pub fn new(
        change_id: ChangeId,
        patch_id: PatchId,
        verdict: LifecycleReviewVerdict,
        mut acceptance_coverage: Vec<LifecycleAcceptanceReview>,
        mut findings: Vec<LifecycleReason>,
        reviewer_definition: Digest,
        created_unix_ms: u64,
    ) -> Result<Self, LifecycleError> {
        acceptance_coverage.sort_by_key(|coverage| coverage.criterion_digest);
        findings.sort_by(|left, right| {
            left.code.cmp(&right.code).then_with(|| left.detail_digest.cmp(&right.detail_digest))
        });
        let mut artifact = Self {
            id: Digest::blake3(b"pending-review-artifact"),
            change_id,
            patch_id,
            verdict,
            acceptance_coverage,
            findings,
            reviewer_definition,
            created_unix_ms,
        };
        artifact.validate_material()?;
        artifact.id = artifact.compute_id();
        Ok(artifact)
    }

    pub fn is_canonical(&self) -> bool {
        self.validate_material().is_ok() && self.id == self.compute_id()
    }

    fn validate_material(&self) -> Result<(), LifecycleError> {
        if self.acceptance_coverage.is_empty()
            || self.acceptance_coverage.len() > MAX_LIFECYCLE_ARTIFACT_REFS
            || self.findings.len() > MAX_LIFECYCLE_REVIEW_FINDINGS
        {
            return Err(LifecycleError::InvalidReview);
        }
        let mut criteria = BTreeSet::new();
        if self
            .acceptance_coverage
            .iter()
            .any(|coverage| !criteria.insert(coverage.criterion_digest))
            || self
                .acceptance_coverage
                .windows(2)
                .any(|pair| pair[0].criterion_digest >= pair[1].criterion_digest)
            || (self.verdict == LifecycleReviewVerdict::Approved
                && self
                    .acceptance_coverage
                    .iter()
                    .any(|coverage| coverage.status != AcceptanceStatus::Addressed))
        {
            return Err(LifecycleError::InvalidReview);
        }
        let mut finding_ids = BTreeSet::new();
        for finding in &self.findings {
            finding.validate()?;
            if !finding_ids.insert((&finding.code, finding.detail_digest)) {
                return Err(LifecycleError::InvalidReview);
            }
        }
        if self.findings.windows(2).any(|pair| {
            (&pair[0].code, pair[0].detail_digest) >= (&pair[1].code, pair[1].detail_digest)
        }) {
            return Err(LifecycleError::InvalidReview);
        }
        Ok(())
    }

    fn compute_id(&self) -> Digest {
        #[derive(Serialize)]
        struct Material<'a> {
            change_id: &'a ChangeId,
            patch_id: PatchId,
            verdict: LifecycleReviewVerdict,
            acceptance_coverage: &'a [LifecycleAcceptanceReview],
            findings: &'a [LifecycleReason],
            reviewer_definition: Digest,
            created_unix_ms: u64,
        }
        let material = Material {
            change_id: &self.change_id,
            patch_id: self.patch_id,
            verdict: self.verdict,
            acceptance_coverage: &self.acceptance_coverage,
            findings: &self.findings,
            reviewer_definition: self.reviewer_definition,
            created_unix_ms: self.created_unix_ms,
        };
        Digest::blake3(serde_json::to_vec(&material).unwrap_or_default())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleVerificationRef {
    pub verification_id: VerificationArtifactId,
    pub patch_id: PatchId,
    pub verdict: VerificationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleApplyApproval {
    pub id: Digest,
    pub approved_state_digest: Digest,
    pub patch_id: PatchId,
    pub verification_id: VerificationArtifactId,
    pub decision_source: ApprovalDecisionSource,
    pub decided_unix_ms: u64,
}

impl LifecycleApplyApproval {
    pub fn new(
        approved_state_digest: Digest,
        patch_id: PatchId,
        verification_id: VerificationArtifactId,
        decision_source: ApprovalDecisionSource,
        decided_unix_ms: u64,
    ) -> Self {
        let id = Self::compute_id(
            approved_state_digest,
            patch_id,
            verification_id,
            decision_source,
            decided_unix_ms,
        );
        Self {
            id,
            approved_state_digest,
            patch_id,
            verification_id,
            decision_source,
            decided_unix_ms,
        }
    }

    pub fn is_canonical(&self) -> bool {
        self.id
            == Self::compute_id(
                self.approved_state_digest,
                self.patch_id,
                self.verification_id,
                self.decision_source,
                self.decided_unix_ms,
            )
    }

    fn compute_id(
        approved_state_digest: Digest,
        patch_id: PatchId,
        verification_id: VerificationArtifactId,
        decision_source: ApprovalDecisionSource,
        decided_unix_ms: u64,
    ) -> Digest {
        let mut hasher = CanonicalHasher::new(b"needle-lifecycle-user-apply-approval");
        hasher.field_digest(approved_state_digest);
        hasher.field_digest(patch_id.0);
        hasher.field_digest(verification_id.0);
        hasher.field_u8(match decision_source {
            ApprovalDecisionSource::AutoPolicy => 0,
            ApprovalDecisionSource::WebUser => 1,
            ApprovalDecisionSource::Timeout => 2,
            ApprovalDecisionSource::Runtime => 3,
        });
        hasher.field_bytes(&decided_unix_ms.to_le_bytes());
        hasher.finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleWorkerCompletion {
    pub profile: RoleProfileProvenance,
    pub worker_depth: u8,
    /// Total logical workers represented by this completion. The single
    /// parent-launched worker counts as one; a value above one proves nested
    /// or duplicate worker creation and is rejected.
    pub logical_worker_spawns: u8,
    pub usage: LifecycleUsage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleTransition {
    CompleteExplore { worker: LifecycleWorkerCompletion, artifacts: Vec<LifecycleArtifactRef> },
    CompleteImplement { worker: LifecycleWorkerCompletion, patch: LifecyclePatchRef },
    CompleteTest { worker: LifecycleWorkerCompletion, results: Vec<LifecycleTestResult> },
    CompleteReview { worker: LifecycleWorkerCompletion, review: ReviewArtifact },
    CompleteVerify { worker: LifecycleWorkerCompletion, verification: LifecycleVerificationRef },
    ConsumeRepair,
    ApproveApply { approval: LifecycleApplyApproval },
    StartApply { apply_id: ChangeApplyId },
    FinishApply { apply_id: ChangeApplyId, status: ChangeApplyStatus },
    Cancel { reason: LifecycleReason },
    Fail { reason: LifecycleReason },
}

impl LifecycleTransition {
    pub fn worker(&self) -> Option<&LifecycleWorkerCompletion> {
        match self {
            Self::CompleteExplore { worker, .. }
            | Self::CompleteImplement { worker, .. }
            | Self::CompleteTest { worker, .. }
            | Self::CompleteReview { worker, .. }
            | Self::CompleteVerify { worker, .. } => Some(worker),
            _ => None,
        }
    }
}
