use crate::{
    ChangeApplyId, ChangeApplyStatus, ChangeId, Digest, PatchId, VerificationArtifactId,
    VerificationStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

mod model;
pub use model::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentLifecycle {
    pub id: LifecycleId,
    pub change_id: ChangeId,
    pub source_snapshot: Digest,
    pub spec: LifecycleSpec,
    pub phase: LifecyclePhase,
    pub status: LifecycleStatus,
    pub terminal_outcome: Option<LifecycleTerminalOutcome>,
    pub terminal_reason: Option<LifecycleReason>,
    pub generation: u64,
    pub usage: LifecycleUsage,
    pub exploration_artifacts: Vec<LifecycleArtifactRef>,
    pub patch: Option<LifecyclePatchRef>,
    pub test_results: Vec<LifecycleTestResult>,
    pub review: Option<ReviewArtifact>,
    pub verification: Option<LifecycleVerificationRef>,
    pub repair_reserved: bool,
    pub repair_consumed: bool,
    pub approval: Option<LifecycleApplyApproval>,
    pub apply_id: Option<ChangeApplyId>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

impl DevelopmentLifecycle {
    pub fn new(
        change_id: ChangeId,
        source_snapshot: Digest,
        spec: LifecycleSpec,
        created_unix_ms: u64,
    ) -> Result<Self, LifecycleError> {
        spec.validate()?;
        let state = Self {
            id: LifecycleId::compute(&change_id, source_snapshot, &spec.profiles),
            change_id,
            source_snapshot,
            spec,
            phase: LifecyclePhase::Explore,
            status: LifecycleStatus::Active,
            terminal_outcome: None,
            terminal_reason: None,
            generation: 0,
            usage: LifecycleUsage::default(),
            exploration_artifacts: Vec::new(),
            patch: None,
            test_results: Vec::new(),
            review: None,
            verification: None,
            repair_reserved: false,
            repair_consumed: false,
            approval: None,
            apply_id: None,
            created_unix_ms,
            updated_unix_ms: created_unix_ms,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn state_digest(&self) -> Digest {
        Digest::blake3(serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn validate(&self) -> Result<(), LifecycleError> {
        self.spec.validate()?;
        if self.id
            != LifecycleId::compute(&self.change_id, self.source_snapshot, &self.spec.profiles)
            || self.updated_unix_ms < self.created_unix_ms
            || !self.usage.within(&self.spec.budget)
            || self.generation >= MAX_LIFECYCLE_EVENTS as u64
        {
            return Err(LifecycleError::InvalidState);
        }
        if self.status.terminal() != self.terminal_outcome.is_some()
            || self.status.terminal()
                != (self.terminal_reason.is_some()
                    || self.terminal_outcome == Some(LifecycleTerminalOutcome::Applied)
                    || self.terminal_outcome == Some(LifecycleTerminalOutcome::RolledBack))
        {
            return Err(LifecycleError::InvalidState);
        }
        if self.repair_reserved && self.repair_consumed {
            return Err(LifecycleError::InvalidState);
        }
        self.validate_artifact_chain()?;
        self.validate_status_shape()?;
        if serde_json::to_vec(self).map_err(|_| LifecycleError::InvalidState)?.len()
            > MAX_LIFECYCLE_STATE_BYTES
        {
            return Err(LifecycleError::StateTooLarge);
        }
        Ok(())
    }

    pub fn transition(
        &self,
        transition: LifecycleTransition,
        created_unix_ms: u64,
    ) -> Result<(Self, LifecycleEvent), LifecycleError> {
        self.validate()?;
        if self.status.terminal() {
            return Err(LifecycleError::Terminal);
        }
        if created_unix_ms < self.updated_unix_ms {
            return Err(LifecycleError::TimeRegression);
        }
        match &transition {
            LifecycleTransition::CompleteReview { review, .. }
                if review.created_unix_ms < self.created_unix_ms
                    || review.created_unix_ms > created_unix_ms =>
            {
                return Err(LifecycleError::TimeRegression);
            }
            LifecycleTransition::ApproveApply { approval }
                if approval.decided_unix_ms < self.created_unix_ms
                    || approval.decided_unix_ms > created_unix_ms =>
            {
                return Err(LifecycleError::TimeRegression);
            }
            _ => {}
        }
        let prior_digest = self.state_digest();
        let mut next = self.clone();
        next.apply_transition(&transition)?;
        next.generation =
            next.generation.checked_add(1).ok_or(LifecycleError::GenerationOverflow)?;
        next.updated_unix_ms = created_unix_ms;
        next.validate()?;
        let event = LifecycleEvent::transitioned(&next, prior_digest, transition, created_unix_ms)?;
        Ok((next, event))
    }

    fn apply_transition(&mut self, transition: &LifecycleTransition) -> Result<(), LifecycleError> {
        match transition {
            LifecycleTransition::Cancel { reason } => {
                if self.status == LifecycleStatus::Applying {
                    return Err(LifecycleError::InvalidTransition);
                }
                reason.validate()?;
                self.finish_terminal(
                    LifecycleStatus::Cancelled,
                    LifecycleTerminalOutcome::Cancelled,
                    Some(reason.clone()),
                );
                return Ok(());
            }
            LifecycleTransition::Fail { reason } => {
                if self.status == LifecycleStatus::Applying {
                    return Err(LifecycleError::InvalidTransition);
                }
                reason.validate()?;
                self.finish_terminal(
                    LifecycleStatus::Failed,
                    LifecycleTerminalOutcome::Failed,
                    Some(reason.clone()),
                );
                return Ok(());
            }
            _ => {}
        }

        match transition {
            LifecycleTransition::CompleteExplore { worker, artifacts } => {
                self.require_active_phase(LifecyclePhase::Explore)?;
                self.accept_worker(LifecyclePhase::Explore, worker)?;
                if artifacts.is_empty() || artifacts.len() > MAX_LIFECYCLE_ARTIFACT_REFS {
                    return Err(LifecycleError::MissingArtifact);
                }
                let mut ids = BTreeSet::new();
                for artifact in artifacts {
                    if artifact.source_snapshot != self.source_snapshot || !ids.insert(artifact.id)
                    {
                        return Err(LifecycleError::InvalidArtifact);
                    }
                }
                self.exploration_artifacts = artifacts.clone();
                self.phase = LifecyclePhase::Implement;
            }
            LifecycleTransition::CompleteImplement { worker, patch } => {
                self.require_active_phase(LifecyclePhase::Implement)?;
                self.accept_worker(LifecyclePhase::Implement, worker)?;
                let expected_revision = if self.repair_consumed { 2 } else { 1 };
                if patch.revision != expected_revision {
                    return Err(LifecycleError::InvalidArtifact);
                }
                self.patch = Some(patch.clone());
                self.test_results.clear();
                self.review = None;
                self.verification = None;
                self.approval = None;
                self.apply_id = None;
                self.phase = LifecyclePhase::Test;
            }
            LifecycleTransition::CompleteTest { worker, results } => {
                self.require_active_phase(LifecyclePhase::Test)?;
                self.accept_worker(LifecyclePhase::Test, worker)?;
                if results.len() != self.spec.test_plans.len() {
                    return Err(LifecycleError::MissingTestEvidence);
                }
                for (result, binding) in results.iter().zip(&self.spec.test_plans) {
                    result.validate()?;
                    if result.plan_digest != binding.plan_digest()
                        || result.certificate_digest != binding.certificate_digest
                    {
                        return Err(LifecycleError::InvalidTestEvidence);
                    }
                }
                self.test_results = results.clone();
                if results.iter().any(|result| !result.available || !result.executed) {
                    self.finish_terminal(
                        LifecycleStatus::Inconclusive,
                        LifecycleTerminalOutcome::Inconclusive,
                        Some(LifecycleReason::new(
                            "test_evidence_unavailable",
                            b"trusted test evidence unavailable",
                        )?),
                    );
                } else if results.iter().any(|result| !result.passed) {
                    self.finish_terminal(
                        LifecycleStatus::Failed,
                        LifecycleTerminalOutcome::Failed,
                        Some(LifecycleReason::new("test_failed", b"trusted test failed")?),
                    );
                } else {
                    self.phase = LifecyclePhase::Review;
                }
            }
            LifecycleTransition::CompleteReview { worker, review } => {
                self.require_active_phase(LifecyclePhase::Review)?;
                self.accept_worker(LifecyclePhase::Review, worker)?;
                let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
                if !review.is_canonical()
                    || review.change_id != self.change_id
                    || review.patch_id != patch.patch_id
                    || review.reviewer_definition != worker.profile.definition_digest
                {
                    return Err(LifecycleError::InvalidReview);
                }
                self.review = Some(review.clone());
                match review.verdict {
                    LifecycleReviewVerdict::Approved => self.phase = LifecyclePhase::Verify,
                    LifecycleReviewVerdict::Rejected => self.finish_terminal(
                        LifecycleStatus::Failed,
                        LifecycleTerminalOutcome::Failed,
                        Some(LifecycleReason::new("review_rejected", b"review rejected patch")?),
                    ),
                    LifecycleReviewVerdict::Inconclusive => self.finish_terminal(
                        LifecycleStatus::Inconclusive,
                        LifecycleTerminalOutcome::Inconclusive,
                        Some(LifecycleReason::new(
                            "review_inconclusive",
                            b"review was inconclusive",
                        )?),
                    ),
                }
            }
            LifecycleTransition::CompleteVerify { worker, verification } => {
                self.require_active_phase(LifecyclePhase::Verify)?;
                self.accept_worker(LifecyclePhase::Verify, worker)?;
                let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
                if verification.patch_id != patch.patch_id
                    || verification.verdict == VerificationStatus::NotRequested
                {
                    return Err(LifecycleError::InvalidVerification);
                }
                self.verification = Some(verification.clone());
                match verification.verdict {
                    VerificationStatus::Verified => {
                        self.phase = LifecyclePhase::Apply;
                        self.status = LifecycleStatus::AwaitingApproval;
                    }
                    VerificationStatus::Repairable => {
                        if self.repair_reserved || self.repair_consumed {
                            self.finish_terminal(
                                LifecycleStatus::Failed,
                                LifecycleTerminalOutcome::Failed,
                                Some(LifecycleReason::new(
                                    "repair_limit_exhausted",
                                    b"verification requested a second repair",
                                )?),
                            );
                        } else {
                            self.status = LifecycleStatus::RepairReserved;
                            self.repair_reserved = true;
                        }
                    }
                    VerificationStatus::Rejected => self.finish_terminal(
                        LifecycleStatus::Failed,
                        LifecycleTerminalOutcome::Failed,
                        Some(LifecycleReason::new(
                            "verification_rejected",
                            b"verification rejected patch",
                        )?),
                    ),
                    VerificationStatus::Inconclusive => self.finish_terminal(
                        LifecycleStatus::Inconclusive,
                        LifecycleTerminalOutcome::Inconclusive,
                        Some(LifecycleReason::new(
                            "verification_inconclusive",
                            b"verification was inconclusive",
                        )?),
                    ),
                    VerificationStatus::NotRequested => unreachable!(),
                }
            }
            LifecycleTransition::ConsumeRepair => {
                if self.phase != LifecyclePhase::Verify
                    || self.status != LifecycleStatus::RepairReserved
                    || !self.repair_reserved
                    || self.repair_consumed
                {
                    return Err(LifecycleError::InvalidTransition);
                }
                self.repair_reserved = false;
                self.repair_consumed = true;
                self.phase = LifecyclePhase::Implement;
                self.status = LifecycleStatus::Active;
                self.patch = None;
                self.test_results.clear();
                self.review = None;
                self.verification = None;
                self.approval = None;
                self.apply_id = None;
            }
            LifecycleTransition::ApproveApply { approval } => {
                if self.phase != LifecyclePhase::Apply
                    || self.status != LifecycleStatus::AwaitingApproval
                {
                    return Err(LifecycleError::InvalidTransition);
                }
                let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
                let verification =
                    self.verification.as_ref().ok_or(LifecycleError::MissingArtifact)?;
                if approval.approved_state_digest != self.state_digest()
                    || approval.patch_id != patch.patch_id
                    || approval.verification_id != verification.verification_id
                    || approval.decision_source != crate::ApprovalDecisionSource::WebUser
                {
                    return Err(LifecycleError::StaleApproval);
                }
                self.approval = Some(approval.clone());
                self.status = LifecycleStatus::Approved;
            }
            LifecycleTransition::StartApply { apply_id } => {
                if self.phase != LifecyclePhase::Apply || self.status != LifecycleStatus::Approved {
                    return Err(LifecycleError::InvalidTransition);
                }
                self.apply_id = Some(*apply_id);
                self.status = LifecycleStatus::Applying;
            }
            LifecycleTransition::FinishApply { apply_id, status } => {
                if self.phase != LifecyclePhase::Apply
                    || self.status != LifecycleStatus::Applying
                    || self.apply_id != Some(*apply_id)
                    || *status == ChangeApplyStatus::Applying
                {
                    return Err(LifecycleError::InvalidTransition);
                }
                match status {
                    ChangeApplyStatus::Applied => self.finish_terminal(
                        LifecycleStatus::Completed,
                        LifecycleTerminalOutcome::Applied,
                        None,
                    ),
                    ChangeApplyStatus::RolledBack => self.finish_terminal(
                        LifecycleStatus::RolledBack,
                        LifecycleTerminalOutcome::RolledBack,
                        None,
                    ),
                    ChangeApplyStatus::RollbackFailed => self.finish_terminal(
                        LifecycleStatus::Failed,
                        LifecycleTerminalOutcome::RollbackFailed,
                        Some(LifecycleReason::new(
                            "rollback_failed",
                            b"active apply rollback failed",
                        )?),
                    ),
                    ChangeApplyStatus::RecoveryConflict => self.finish_terminal(
                        LifecycleStatus::Failed,
                        LifecycleTerminalOutcome::RecoveryConflict,
                        Some(LifecycleReason::new(
                            "recovery_conflict",
                            b"pending apply recovery conflicted",
                        )?),
                    ),
                    ChangeApplyStatus::Applying => unreachable!(),
                }
            }
            LifecycleTransition::Cancel { .. } | LifecycleTransition::Fail { .. } => unreachable!(),
        }
        Ok(())
    }

    fn require_active_phase(&self, expected: LifecyclePhase) -> Result<(), LifecycleError> {
        if self.phase != expected || self.status != LifecycleStatus::Active {
            return Err(LifecycleError::InvalidTransition);
        }
        Ok(())
    }

    fn accept_worker(
        &mut self,
        phase: LifecyclePhase,
        worker: &LifecycleWorkerCompletion,
    ) -> Result<(), LifecycleError> {
        if worker.worker_depth != 1 || worker.logical_worker_spawns != 1 {
            return Err(LifecycleError::NestedWorker);
        }
        if worker.usage.worker_turns == 0 {
            return Err(LifecycleError::InvalidWorkerCompletion);
        }
        if self.spec.profiles.for_phase(phase) != Some(&worker.profile) {
            return Err(LifecycleError::ProfileMismatch);
        }
        let usage = self.usage.checked_add(&worker.usage)?;
        if !usage.within(&self.spec.budget) {
            return Err(LifecycleError::BudgetExceeded);
        }
        self.usage = usage;
        Ok(())
    }

    fn finish_terminal(
        &mut self,
        status: LifecycleStatus,
        outcome: LifecycleTerminalOutcome,
        reason: Option<LifecycleReason>,
    ) {
        self.repair_reserved = false;
        self.status = status;
        self.terminal_outcome = Some(outcome);
        self.terminal_reason = reason;
    }

    fn validate_artifact_chain(&self) -> Result<(), LifecycleError> {
        if self.exploration_artifacts.len() > MAX_LIFECYCLE_ARTIFACT_REFS
            || self.exploration_artifacts.iter().any(|artifact| {
                artifact.source_snapshot != self.source_snapshot
                    || artifact.kind != LifecycleArtifactKind::Exploration
            })
            || self.exploration_artifacts.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(LifecycleError::InvalidArtifact);
        }
        if let Some(patch) = &self.patch {
            let expected_revision = if self.repair_consumed { 2 } else { 1 };
            if self.exploration_artifacts.is_empty() || patch.revision != expected_revision {
                return Err(LifecycleError::InvalidArtifact);
            }
        } else if !self.test_results.is_empty()
            || self.review.is_some()
            || self.verification.is_some()
            || self.approval.is_some()
            || self.apply_id.is_some()
        {
            return Err(LifecycleError::InvalidState);
        }
        if !self.test_results.is_empty() {
            if self.test_results.len() != self.spec.test_plans.len() {
                return Err(LifecycleError::MissingTestEvidence);
            }
            for (result, binding) in self.test_results.iter().zip(&self.spec.test_plans) {
                result.validate()?;
                if result.plan_digest != binding.plan_digest()
                    || result.certificate_digest != binding.certificate_digest
                {
                    return Err(LifecycleError::InvalidTestEvidence);
                }
            }
        }
        if let Some(review) = &self.review {
            let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
            if self.test_results.iter().any(|result| !result.passed)
                || self.test_results.len() != self.spec.test_plans.len()
                || !review.is_canonical()
                || review.change_id != self.change_id
                || review.patch_id != patch.patch_id
                || review.reviewer_definition != self.spec.profiles.review.definition_digest
            {
                return Err(LifecycleError::InvalidReview);
            }
        }
        if let Some(verification) = &self.verification {
            let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
            if self.review.as_ref().map(|review| review.verdict)
                != Some(LifecycleReviewVerdict::Approved)
                || verification.patch_id != patch.patch_id
                || verification.verdict == VerificationStatus::NotRequested
            {
                return Err(LifecycleError::InvalidVerification);
            }
        }
        if let Some(approval) = &self.approval {
            let patch = self.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
            let verification = self.verification.as_ref().ok_or(LifecycleError::MissingArtifact)?;
            if !approval.is_canonical()
                || verification.verdict != VerificationStatus::Verified
                || approval.patch_id != patch.patch_id
                || approval.verification_id != verification.verification_id
            {
                return Err(LifecycleError::StaleApproval);
            }
        }
        if self.apply_id.is_some() && self.approval.is_none() {
            return Err(LifecycleError::InvalidState);
        }
        Ok(())
    }

    fn validate_status_shape(&self) -> Result<(), LifecycleError> {
        if self.status.terminal() {
            let valid_terminal = matches!(
                (self.status, self.terminal_outcome),
                (LifecycleStatus::Completed, Some(LifecycleTerminalOutcome::Applied))
                    | (LifecycleStatus::Failed, Some(LifecycleTerminalOutcome::Failed))
                    | (LifecycleStatus::Failed, Some(LifecycleTerminalOutcome::RollbackFailed))
                    | (LifecycleStatus::Failed, Some(LifecycleTerminalOutcome::RecoveryConflict))
                    | (LifecycleStatus::Cancelled, Some(LifecycleTerminalOutcome::Cancelled))
                    | (LifecycleStatus::Inconclusive, Some(LifecycleTerminalOutcome::Inconclusive))
                    | (LifecycleStatus::RolledBack, Some(LifecycleTerminalOutcome::RolledBack))
            );
            if !valid_terminal
                || self.repair_reserved
                || matches!(
                    self.terminal_outcome,
                    Some(
                        LifecycleTerminalOutcome::Applied
                            | LifecycleTerminalOutcome::RolledBack
                            | LifecycleTerminalOutcome::RollbackFailed
                            | LifecycleTerminalOutcome::RecoveryConflict
                    )
                ) && (self.phase != LifecyclePhase::Apply || self.apply_id.is_none())
            {
                return Err(LifecycleError::InvalidState);
            }
            return Ok(());
        }
        let valid = match (self.phase, self.status) {
            (LifecyclePhase::Explore, LifecycleStatus::Active) => {
                self.exploration_artifacts.is_empty()
                    && self.patch.is_none()
                    && !self.repair_consumed
            }
            (LifecyclePhase::Implement, LifecycleStatus::Active) => {
                !self.exploration_artifacts.is_empty()
                    && self.patch.is_none()
                    && self.test_results.is_empty()
                    && self.review.is_none()
                    && self.verification.is_none()
            }
            (LifecyclePhase::Test, LifecycleStatus::Active) => {
                self.patch.is_some()
                    && self.test_results.is_empty()
                    && self.review.is_none()
                    && self.verification.is_none()
            }
            (LifecyclePhase::Review, LifecycleStatus::Active) => {
                self.test_results.len() == self.spec.test_plans.len()
                    && self.test_results.iter().all(|result| result.passed)
                    && self.review.is_none()
                    && self.verification.is_none()
            }
            (LifecyclePhase::Verify, LifecycleStatus::Active) => {
                self.review.as_ref().map(|review| review.verdict)
                    == Some(LifecycleReviewVerdict::Approved)
                    && self.verification.is_none()
                    && !self.repair_reserved
            }
            (LifecyclePhase::Verify, LifecycleStatus::RepairReserved) => {
                self.verification.as_ref().map(|verification| verification.verdict)
                    == Some(VerificationStatus::Repairable)
                    && self.repair_reserved
                    && !self.repair_consumed
            }
            (LifecyclePhase::Apply, LifecycleStatus::AwaitingApproval) => {
                self.verification.as_ref().map(|verification| verification.verdict)
                    == Some(VerificationStatus::Verified)
                    && self.approval.is_none()
                    && self.apply_id.is_none()
            }
            (LifecyclePhase::Apply, LifecycleStatus::Approved) => {
                self.approval.is_some() && self.apply_id.is_none()
            }
            (LifecyclePhase::Apply, LifecycleStatus::Applying) => {
                self.approval.is_some() && self.apply_id.is_some()
            }
            _ => false,
        };
        if !valid || self.terminal_outcome.is_some() || self.terminal_reason.is_some() {
            return Err(LifecycleError::InvalidState);
        }
        Ok(())
    }

    pub fn replay(events: &[LifecycleEvent]) -> Result<Self, LifecycleError> {
        let Some(first) = events.first() else {
            return Err(LifecycleError::EventReplay);
        };
        let LifecycleEventKind::Created { state } = &first.kind else {
            return Err(LifecycleError::EventReplay);
        };
        state.validate()?;
        if *first != LifecycleEvent::created(state)? {
            return Err(LifecycleError::EventReplay);
        }
        let mut current = state.as_ref().clone();
        for event in &events[1..] {
            let LifecycleEventKind::Transitioned { transition } = &event.kind else {
                return Err(LifecycleError::EventReplay);
            };
            if event.sequence != current.generation + 1
                || event.prior_state_digest != Some(current.state_digest())
            {
                return Err(LifecycleError::EventReplay);
            }
            let (next, reproduced) =
                current.transition(transition.as_ref().clone(), event.created_unix_ms)?;
            if reproduced != *event || next.state_digest() != event.resulting_state_digest {
                return Err(LifecycleError::EventReplay);
            }
            current = next;
        }
        Ok(current)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleEventKind {
    Created { state: Box<DevelopmentLifecycle> },
    Transitioned { transition: Box<LifecycleTransition> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEvent {
    pub lifecycle_id: LifecycleId,
    pub change_id: ChangeId,
    pub sequence: u64,
    pub phase: LifecyclePhase,
    pub status: LifecycleStatus,
    pub source_snapshot: Digest,
    pub profile_revision_digest: Option<Digest>,
    pub patch_id: Option<PatchId>,
    pub verification_id: Option<VerificationArtifactId>,
    pub prior_state_digest: Option<Digest>,
    pub resulting_state_digest: Digest,
    pub kind: LifecycleEventKind,
    pub created_unix_ms: u64,
}

impl LifecycleEvent {
    pub fn created(state: &DevelopmentLifecycle) -> Result<Self, LifecycleError> {
        let event = Self {
            lifecycle_id: state.id,
            change_id: state.change_id.clone(),
            sequence: 0,
            phase: state.phase,
            status: state.status,
            source_snapshot: state.source_snapshot,
            profile_revision_digest: None,
            patch_id: None,
            verification_id: None,
            prior_state_digest: None,
            resulting_state_digest: state.state_digest(),
            kind: LifecycleEventKind::Created { state: Box::new(state.clone()) },
            created_unix_ms: state.created_unix_ms,
        };
        event.validate_size()?;
        Ok(event)
    }

    fn transitioned(
        state: &DevelopmentLifecycle,
        prior_state_digest: Digest,
        transition: LifecycleTransition,
        created_unix_ms: u64,
    ) -> Result<Self, LifecycleError> {
        let profile_revision_digest =
            transition.worker().map(|worker| worker.profile.definition_digest);
        let event = Self {
            lifecycle_id: state.id,
            change_id: state.change_id.clone(),
            sequence: state.generation,
            phase: state.phase,
            status: state.status,
            source_snapshot: state.source_snapshot,
            profile_revision_digest,
            patch_id: state.patch.as_ref().map(|patch| patch.patch_id),
            verification_id: state
                .verification
                .as_ref()
                .map(|verification| verification.verification_id),
            prior_state_digest: Some(prior_state_digest),
            resulting_state_digest: state.state_digest(),
            kind: LifecycleEventKind::Transitioned { transition: Box::new(transition) },
            created_unix_ms,
        };
        event.validate_size()?;
        Ok(event)
    }

    fn validate_size(&self) -> Result<(), LifecycleError> {
        if serde_json::to_vec(self).map_err(|_| LifecycleError::EventReplay)?.len()
            > MAX_LIFECYCLE_EVENT_BYTES
        {
            return Err(LifecycleError::EventTooLarge);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LifecycleError {
    #[error("lifecycle budget is invalid")]
    InvalidBudget,
    #[error("lifecycle budget arithmetic overflowed")]
    BudgetOverflow,
    #[error("lifecycle budget was exceeded")]
    BudgetExceeded,
    #[error("worker depth or spawn count exceeds the depth-one contract")]
    NestedWorker,
    #[error("worker profile does not match the frozen phase binding")]
    ProfileMismatch,
    #[error("worker completion accounting is invalid")]
    InvalidWorkerCompletion,
    #[error("lifecycle transition is illegal from the current phase/state")]
    InvalidTransition,
    #[error("lifecycle is terminal")]
    Terminal,
    #[error("lifecycle artifact is missing")]
    MissingArtifact,
    #[error("lifecycle artifact is invalid")]
    InvalidArtifact,
    #[error("lifecycle test plan is invalid")]
    InvalidTestPlan,
    #[error("lifecycle test evidence is missing")]
    MissingTestEvidence,
    #[error("lifecycle test evidence is invalid")]
    InvalidTestEvidence,
    #[error("review artifact is invalid")]
    InvalidReview,
    #[error("verification artifact reference is invalid")]
    InvalidVerification,
    #[error("apply approval is stale or references different artifacts")]
    StaleApproval,
    #[error("lifecycle reason code is invalid")]
    InvalidReason,
    #[error("lifecycle timestamp regressed")]
    TimeRegression,
    #[error("lifecycle generation overflowed")]
    GenerationOverflow,
    #[error("lifecycle state is invalid")]
    InvalidState,
    #[error("lifecycle state exceeds the byte bound")]
    StateTooLarge,
    #[error("lifecycle event exceeds the byte bound")]
    EventTooLarge,
    #[error("lifecycle event replay failed")]
    EventReplay,
}

fn bounded_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn safe_relative_directory(value: &str) -> bool {
    let path = std::path::Path::new(value);
    path.is_relative()
        && !value.is_empty()
        && value.len() <= 512
        && path.components().all(|component| {
            matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
        })
}

fn test_plan_contains_absolute_path(plan: &crate::TestPlan) -> bool {
    plan.argv.iter().any(|argument| {
        std::path::Path::new(argument).components().any(|component| {
            matches!(component, std::path::Component::RootDir | std::path::Component::Prefix(_))
        })
    })
}

#[cfg(test)]
#[path = "lifecycle/tests.rs"]
mod tests;
