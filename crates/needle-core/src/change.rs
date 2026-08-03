use crate::{ArtifactId, CanonicalHasher, ClaimId, Digest, TestPlan};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_VERIFICATION_FAILURE_REASON_BYTES: usize = 2 * 1024;

pub const MAX_CHANGE_TASK_BYTES: usize = 8 * 1024;
pub const MAX_ACCEPTANCE_CRITERIA: usize = 8;
pub const MAX_ALLOWED_PATHS: usize = 16;
pub const MAX_CHANGE_ARTIFACTS: usize = 16;
pub const MAX_CHANGE_CLAIMS: usize = 32;
pub const MAX_PATCH_FILES: usize = 16;
pub const MAX_PATCH_DIFF_BYTES: usize = 512 * 1024;
pub const MAX_PATCH_FINAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ChangeId(String);

impl ChangeId {
    pub fn from_digest(digest: Digest) -> Self {
        Self(format!("chg_{}", &digest.to_hex()[..24]))
    }

    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let Some(hex) = value.strip_prefix("chg_") else {
            return Err("change id must start with `chg_`");
        };
        if hex.len() != 24 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("change id must contain exactly 24 hexadecimal characters");
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ChangeId").field(&self.0).finish()
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ChangeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ChangeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatchId(pub Digest);

impl fmt::Display for PatchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedPathScope {
    Exact,
    Subtree,
}

impl AllowedPathScope {
    fn discriminant(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Subtree => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedPath {
    pub path: String,
    pub scope: AllowedPathScope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeRequest {
    pub task: String,
    pub acceptance_criteria: Vec<String>,
    pub allowed_paths: Vec<AllowedPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_ids: Vec<ClaimId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
}

impl ChangeRequest {
    pub fn digest(&self, source_snapshot: Digest) -> Digest {
        let mut hasher = CanonicalHasher::new(b"needle-change-request");
        hasher.field_digest(source_snapshot);
        hasher.field_normalized_lines(&self.task);

        let mut acceptance = self.acceptance_criteria.iter().collect::<Vec<_>>();
        acceptance.sort_unstable();
        hasher.field_u16(acceptance.len().try_into().unwrap_or(u16::MAX));
        for criterion in acceptance {
            hasher.field_normalized_lines(criterion);
        }

        let mut paths = self.allowed_paths.iter().collect::<Vec<_>>();
        paths.sort_unstable_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.scope.discriminant().cmp(&right.scope.discriminant()))
        });
        hasher.field_u16(paths.len().try_into().unwrap_or(u16::MAX));
        for path in paths {
            hasher.field_str(&path.path);
            hasher.field_u8(path.scope.discriminant());
        }

        let mut artifacts = self.artifact_ids.clone();
        artifacts.sort_unstable();
        hasher.field_u16(artifacts.len().try_into().unwrap_or(u16::MAX));
        for artifact in artifacts {
            hasher.field_digest(artifact.0);
        }

        let mut claims = self.claim_ids.clone();
        claims.sort_unstable();
        hasher.field_u16(claims.len().try_into().unwrap_or(u16::MAX));
        for claim in claims {
            hasher.field_digest(claim.0);
        }

        let mut constraints = self.constraints.iter().collect::<Vec<_>>();
        constraints.sort_unstable();
        hasher.field_u16(constraints.len().try_into().unwrap_or(u16::MAX));
        for constraint in constraints {
            hasher.field_normalized_lines(constraint);
        }
        hasher.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOperation {
    Create,
    Update,
    Delete,
}

impl PatchOperation {
    fn discriminant(self) -> u8 {
        match self {
            Self::Create => 0,
            Self::Update => 1,
            Self::Delete => 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchFile {
    pub path: String,
    pub operation: PatchOperation,
    pub before_digest: Option<Digest>,
    pub after_digest: Option<Digest>,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    Addressed,
    Partial,
    Unaddressed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCoverage {
    pub criterion: String,
    pub status: AcceptanceStatus,
    pub evidence: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchArtifact {
    pub id: PatchId,
    pub change_id: ChangeId,
    pub revision: u32,
    pub source_snapshot: Digest,
    pub files: Vec<PatchFile>,
    pub summary: String,
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    pub residual_risks: Vec<String>,
    pub declared_output_digest: Digest,
    pub discrepancies: Vec<String>,
}

impl PatchArtifact {
    pub fn compute_id(source_snapshot: Digest, files: &[PatchFile]) -> PatchId {
        let mut hasher = CanonicalHasher::new(b"needle-patch-artifact");
        hasher.field_digest(source_snapshot);
        let mut files = files.iter().collect::<Vec<_>>();
        files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        hasher.field_u16(files.len().try_into().unwrap_or(u16::MAX));
        for file in files {
            hasher.field_str(&file.path);
            hasher.field_u8(file.operation.discriminant());
            hasher.field_u8(u8::from(file.before_digest.is_some()));
            if let Some(digest) = file.before_digest {
                hasher.field_digest(digest);
            }
            hasher.field_u8(u8::from(file.after_digest.is_some()));
            if let Some(digest) = file.after_digest {
                hasher.field_digest(digest);
            }
            hasher.field_bytes(&file.before_bytes.to_le_bytes());
            hasher.field_bytes(&file.after_bytes.to_le_bytes());
        }
        PatchId(hasher.finish())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    NotRequested,
    Verified,
    Rejected,
    Repairable,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VerificationArtifactId(pub Digest);

impl fmt::Display for VerificationArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationArtifact {
    pub id: VerificationArtifactId,
    pub change_id: ChangeId,
    pub patch_id: PatchId,
    pub verdict: VerificationStatus,
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    pub findings: Vec<String>,
    pub test_evidence_ids: Vec<String>,
    /// Runtime-owned per-plan verification records.  This is additive so
    /// artifacts written before bounded multi-plan verification remain
    /// decodable and visibly contain an empty projection.
    #[serde(default)]
    pub test_plan_results: Vec<VerificationPlanResult>,
    /// True when the complete expected plan set exceeded the hard bound. No
    /// truncated subset is represented in `test_plan_results` in that case.
    #[serde(default)]
    pub test_plans_over_cap: bool,
    pub verifier_definition: Digest,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationPlanResult {
    pub plan_digest: Digest,
    pub runner: String,
    pub argv: Vec<String>,
    pub cwd_relative: String,
    pub test_identifier: String,
    pub expected: bool,
    pub available: bool,
    pub executed: bool,
    pub passed: bool,
    #[serde(default)]
    pub evidence_id: Option<String>,
    #[serde(default)]
    pub failure_reason: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct VerificationTestProjection<'a> {
    pub evidence_ids: &'a [String],
    pub plan_results: &'a [VerificationPlanResult],
    pub plans_over_cap: bool,
}

impl VerificationArtifact {
    pub fn compute_id(
        change_id: &ChangeId,
        patch_id: PatchId,
        verdict: VerificationStatus,
        acceptance_coverage: &[AcceptanceCoverage],
        findings: &[String],
        test_evidence_ids: &[String],
        verifier_definition: Digest,
    ) -> VerificationArtifactId {
        Self::compute_id_legacy(
            change_id,
            patch_id,
            verdict,
            acceptance_coverage,
            findings,
            test_evidence_ids,
            verifier_definition,
        )
    }

    fn compute_id_legacy(
        change_id: &ChangeId,
        patch_id: PatchId,
        verdict: VerificationStatus,
        acceptance_coverage: &[AcceptanceCoverage],
        findings: &[String],
        test_evidence_ids: &[String],
        verifier_definition: Digest,
    ) -> VerificationArtifactId {
        let mut hasher = CanonicalHasher::new(b"needle-verification-artifact");
        hasher.field_str(change_id.as_str());
        hasher.field_digest(patch_id.0);
        hasher.field_u8(match verdict {
            VerificationStatus::NotRequested => 0,
            VerificationStatus::Verified => 1,
            VerificationStatus::Rejected => 2,
            VerificationStatus::Repairable => 3,
            VerificationStatus::Inconclusive => 4,
        });
        hasher.field_u16(acceptance_coverage.len().try_into().unwrap_or(u16::MAX));
        for coverage in acceptance_coverage {
            hasher.field_normalized_lines(&coverage.criterion);
            hasher.field_u8(match coverage.status {
                AcceptanceStatus::Addressed => 0,
                AcceptanceStatus::Partial => 1,
                AcceptanceStatus::Unaddressed => 2,
            });
            hasher.field_normalized_lines(&coverage.evidence);
        }
        let mut findings = findings.iter().collect::<Vec<_>>();
        findings.sort_unstable();
        hasher.field_u16(findings.len().try_into().unwrap_or(u16::MAX));
        for finding in findings {
            hasher.field_normalized_lines(finding);
        }
        let mut evidence = test_evidence_ids.iter().collect::<Vec<_>>();
        evidence.sort_unstable();
        hasher.field_u16(evidence.len().try_into().unwrap_or(u16::MAX));
        for id in evidence {
            hasher.field_str(id);
        }
        hasher.field_digest(verifier_definition);
        VerificationArtifactId(hasher.finish())
    }

    pub fn compute_id_with_plan_results(
        change_id: &ChangeId,
        patch_id: PatchId,
        verdict: VerificationStatus,
        acceptance_coverage: &[AcceptanceCoverage],
        findings: &[String],
        test_projection: VerificationTestProjection<'_>,
        verifier_definition: Digest,
    ) -> VerificationArtifactId {
        let VerificationTestProjection {
            evidence_ids: test_evidence_ids,
            plan_results: test_plan_results,
            plans_over_cap: test_plans_over_cap,
        } = test_projection;
        if test_plan_results.is_empty() && !test_plans_over_cap {
            // Preserve the v1 identity calculation for legacy artifacts and
            // for inconclusive records that have no expected plan.
            return Self::compute_id_legacy(
                change_id,
                patch_id,
                verdict,
                acceptance_coverage,
                findings,
                test_evidence_ids,
                verifier_definition,
            );
        }
        let mut hasher = CanonicalHasher::new(b"needle-verification-artifact");
        hasher.field_str(change_id.as_str());
        hasher.field_digest(patch_id.0);
        hasher.field_u8(match verdict {
            VerificationStatus::NotRequested => 0,
            VerificationStatus::Verified => 1,
            VerificationStatus::Rejected => 2,
            VerificationStatus::Repairable => 3,
            VerificationStatus::Inconclusive => 4,
        });
        hasher.field_u16(acceptance_coverage.len().try_into().unwrap_or(u16::MAX));
        for coverage in acceptance_coverage {
            hasher.field_normalized_lines(&coverage.criterion);
            hasher.field_u8(match coverage.status {
                AcceptanceStatus::Addressed => 0,
                AcceptanceStatus::Partial => 1,
                AcceptanceStatus::Unaddressed => 2,
            });
            hasher.field_normalized_lines(&coverage.evidence);
        }
        let mut findings = findings.iter().collect::<Vec<_>>();
        findings.sort_unstable();
        hasher.field_u16(findings.len().try_into().unwrap_or(u16::MAX));
        for finding in findings {
            hasher.field_normalized_lines(finding);
        }
        hasher.field_u16(test_evidence_ids.len().try_into().unwrap_or(u16::MAX));
        for id in test_evidence_ids {
            hasher.field_str(id);
        }
        hasher.field_u16(test_plan_results.len().try_into().unwrap_or(u16::MAX));
        hasher.field_u8(u8::from(test_plans_over_cap));
        for result in test_plan_results {
            hasher.field_digest(result.plan_digest);
            hasher.field_str(&result.runner);
            hasher.field_u16(result.argv.len().try_into().unwrap_or(u16::MAX));
            for argument in &result.argv {
                hasher.field_str(argument);
            }
            hasher.field_str(&result.cwd_relative);
            hasher.field_str(&result.test_identifier);
            hasher.field_u8(u8::from(result.expected));
            hasher.field_u8(u8::from(result.available));
            hasher.field_u8(u8::from(result.executed));
            hasher.field_u8(u8::from(result.passed));
            match result.evidence_id.as_deref() {
                Some(id) => {
                    hasher.field_u8(1);
                    hasher.field_str(id);
                }
                None => hasher.field_u8(0),
            }
            match result.failure_reason.as_deref() {
                Some(reason) => {
                    hasher.field_u8(1);
                    hasher.field_normalized_lines(reason);
                }
                None => hasher.field_u8(0),
            }
        }
        hasher.field_digest(verifier_definition);
        VerificationArtifactId(hasher.finish())
    }

    pub fn is_canonical(&self) -> bool {
        let identity = if self.test_plan_results.is_empty() && !self.test_plans_over_cap {
            Self::compute_id(
                &self.change_id,
                self.patch_id,
                self.verdict,
                &self.acceptance_coverage,
                &self.findings,
                &self.test_evidence_ids,
                self.verifier_definition,
            )
        } else {
            Self::compute_id_with_plan_results(
                &self.change_id,
                self.patch_id,
                self.verdict,
                &self.acceptance_coverage,
                &self.findings,
                VerificationTestProjection {
                    evidence_ids: &self.test_evidence_ids,
                    plan_results: &self.test_plan_results,
                    plans_over_cap: self.test_plans_over_cap,
                },
                self.verifier_definition,
            )
        };
        let ordered =
            self.test_plan_results.windows(2).all(|pair| pair[0].plan_digest < pair[1].plan_digest);
        let record_evidence = self
            .test_plan_results
            .iter()
            .filter_map(|result| result.evidence_id.as_deref())
            .collect::<Vec<_>>();
        let records_valid = self.test_plan_results.iter().all(|result| {
            let plan = TestPlan {
                runner: result.runner.clone(),
                argv: result.argv.clone(),
                cwd_relative: result.cwd_relative.clone(),
                test_identifier: result.test_identifier.clone(),
                requires_approval: true,
                execution_evidence_id: None,
            };
            result.expected
                && plan.test_command().is_ok()
                && plan.identity_digest() == result.plan_digest
                && result.executed == result.evidence_id.is_some()
                && (!result.passed
                    || (result.available && result.executed && result.failure_reason.is_none()))
                && (result.passed || result.failure_reason.is_some())
        });
        let failure_reasons_bounded = self.test_plan_results.iter().all(|result| {
            result
                .failure_reason
                .as_ref()
                .is_none_or(|reason| reason.len() <= MAX_VERIFICATION_FAILURE_REASON_BYTES)
        });
        let evidence_unique = self.test_evidence_ids.windows(2).all(|pair| pair[0] != pair[1])
            && self.test_evidence_ids.iter().collect::<std::collections::BTreeSet<_>>().len()
                == self.test_evidence_ids.len();
        let overflow_signal = !self.test_plans_over_cap
            || (self.test_plan_results.is_empty()
                && self.findings.iter().any(|finding| finding.contains("test-plan bound")));
        let legacy_projection = self.test_plan_results.is_empty() && !self.test_plans_over_cap;
        let verified_projection = legacy_projection
            || (!self.test_plans_over_cap
                && !self.test_plan_results.is_empty()
                && self.test_plan_results.iter().all(|result| {
                    result.expected
                        && result.available
                        && result.executed
                        && result.passed
                        && result.evidence_id.is_some()
                        && result.failure_reason.is_none()
                }));
        (self.test_plan_results.len() <= crate::MAX_VERIFIER_TEST_PLANS || self.test_plans_over_cap)
            && (self.test_plan_results.is_empty() && !self.test_plans_over_cap
                || self.test_evidence_ids.len() <= crate::MAX_VERIFIER_TEST_PLANS)
            && (!self.test_plans_over_cap
                || (self.test_plan_results.is_empty() && self.test_evidence_ids.is_empty()))
            && ordered
            && evidence_unique
            && records_valid
            && failure_reasons_bounded
            && overflow_signal
            && (self.test_plan_results.is_empty()
                || record_evidence
                    == self.test_evidence_ids.iter().map(String::as_str).collect::<Vec<_>>())
            && (self.verdict != VerificationStatus::Verified || verified_projection)
            && self.id == identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeApplyId(pub Digest);

impl fmt::Display for ChangeApplyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeApplyStatus {
    Applying,
    Applied,
    RolledBack,
    RollbackFailed,
    RecoveryConflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeApplyRecord {
    pub id: ChangeApplyId,
    pub change_id: ChangeId,
    pub patch_id: PatchId,
    pub repository_root: String,
    pub pre_snapshot: Digest,
    pub post_snapshot: Option<Digest>,
    pub status: ChangeApplyStatus,
    pub created_unix_ms: u64,
    pub completed_unix_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projected_result(
        identifier: &str,
        available: bool,
        executed: bool,
        passed: bool,
        evidence_id: Option<&str>,
        failure_reason: Option<&str>,
    ) -> VerificationPlanResult {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                identifier.to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            test_identifier: identifier.to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        VerificationPlanResult {
            plan_digest: plan.identity_digest(),
            runner: plan.runner,
            argv: plan.argv,
            cwd_relative: plan.cwd_relative,
            test_identifier: plan.test_identifier,
            expected: true,
            available,
            executed,
            passed,
            evidence_id: evidence_id.map(str::to_owned),
            failure_reason: failure_reason.map(str::to_owned),
        }
    }

    fn projected_artifact(
        verdict: VerificationStatus,
        findings: Vec<String>,
        test_evidence_ids: Vec<String>,
        test_plan_results: Vec<VerificationPlanResult>,
        test_plans_over_cap: bool,
    ) -> VerificationArtifact {
        let change_id = ChangeId::from_digest(Digest::blake3("change-v2-projection"));
        let patch_id = PatchId(Digest::blake3("patch-v2-projection"));
        let verifier_definition = Digest::blake3("definition-v2-projection");
        let id = VerificationArtifact::compute_id_with_plan_results(
            &change_id,
            patch_id,
            verdict,
            &[],
            &findings,
            VerificationTestProjection {
                evidence_ids: &test_evidence_ids,
                plan_results: &test_plan_results,
                plans_over_cap: test_plans_over_cap,
            },
            verifier_definition,
        );
        VerificationArtifact {
            id,
            change_id,
            patch_id,
            verdict,
            acceptance_coverage: Vec::new(),
            findings,
            test_evidence_ids,
            test_plan_results,
            test_plans_over_cap,
            verifier_definition,
            created_unix_ms: 1,
        }
    }

    #[test]
    fn change_id_is_short_and_strict() {
        let id = ChangeId::from_digest(Digest::blake3(b"request"));
        assert_eq!(id.as_str().len(), 28);
        assert_eq!(ChangeId::parse(id.as_str()).unwrap(), id);
        assert!(ChangeId::parse("chg_short").is_err());
    }

    #[test]
    fn patch_identity_uses_observed_filesystem_state() {
        let source = Digest::blake3(b"source");
        let first = PatchFile {
            path: "src/lib.rs".to_owned(),
            operation: PatchOperation::Update,
            before_digest: Some(Digest::blake3(b"before")),
            after_digest: Some(Digest::blake3(b"after")),
            before_bytes: 6,
            after_bytes: 5,
        };
        let mut changed = first.clone();
        changed.after_digest = Some(Digest::blake3(b"different"));
        assert_ne!(
            PatchArtifact::compute_id(source, &[first]),
            PatchArtifact::compute_id(source, &[changed])
        );
    }

    #[test]
    fn legacy_verification_artifact_decodes_without_multi_plan_inference() {
        let change_id = ChangeId::from_digest(Digest::blake3("change"));
        let patch_id = PatchId(Digest::blake3("patch"));
        let definition = Digest::blake3("definition");
        let id = VerificationArtifact::compute_id(
            &change_id,
            patch_id,
            VerificationStatus::Rejected,
            &[],
            &[],
            &["evidence".to_owned()],
            definition,
        );
        let value = serde_json::json!({
            "id": id,
            "change_id": change_id,
            "patch_id": patch_id,
            "verdict": "rejected",
            "acceptance_coverage": [],
            "findings": [],
            "test_evidence_ids": ["evidence"],
            "verifier_definition": definition,
            "created_unix_ms": 1
        });
        let artifact: VerificationArtifact = serde_json::from_value(value).unwrap();
        assert!(artifact.test_plan_results.is_empty());
        assert!(artifact.is_canonical());
    }

    #[test]
    fn over_cap_artifact_rejects_evidence() {
        let change_id = ChangeId::from_digest(Digest::blake3("change-over-cap"));
        let patch_id = PatchId(Digest::blake3("patch-over-cap"));
        let verifier_definition = Digest::blake3("definition-over-cap");
        let verdict = VerificationStatus::Inconclusive;
        let acceptance_coverage = Vec::new();
        let findings = vec!["the verifier test-plan bound was exceeded".to_owned()];
        let test_evidence_ids = vec!["evidence-that-must-not-be-retained".to_owned()];
        let id = VerificationArtifact::compute_id_with_plan_results(
            &change_id,
            patch_id,
            verdict,
            &acceptance_coverage,
            &findings,
            VerificationTestProjection {
                evidence_ids: &test_evidence_ids,
                plan_results: &[],
                plans_over_cap: true,
            },
            verifier_definition,
        );
        let artifact = VerificationArtifact {
            id,
            change_id,
            patch_id,
            verdict,
            acceptance_coverage,
            findings,
            test_evidence_ids,
            test_plan_results: Vec::new(),
            test_plans_over_cap: true,
            verifier_definition,
            created_unix_ms: 1,
        };
        assert!(!artifact.is_canonical());
    }

    #[test]
    fn verified_with_unavailable_plan_is_not_canonical() {
        let artifact = projected_artifact(
            VerificationStatus::Verified,
            Vec::new(),
            Vec::new(),
            vec![projected_result(
                "suite::unavailable",
                false,
                false,
                false,
                None,
                Some("certificate is stale"),
            )],
            false,
        );
        assert!(!artifact.is_canonical());
    }

    #[test]
    fn verified_over_cap_artifact_is_not_canonical() {
        let artifact = projected_artifact(
            VerificationStatus::Verified,
            vec!["the verifier test-plan bound was exceeded".to_owned()],
            Vec::new(),
            Vec::new(),
            true,
        );
        assert!(!artifact.is_canonical());
    }

    #[test]
    fn non_verified_failed_or_over_cap_artifact_remains_canonical() {
        let failed = projected_artifact(
            VerificationStatus::Inconclusive,
            Vec::new(),
            Vec::new(),
            vec![projected_result(
                "suite::failed",
                false,
                false,
                false,
                None,
                Some("focused test was unavailable"),
            )],
            false,
        );
        assert!(failed.is_canonical());

        let over_cap = projected_artifact(
            VerificationStatus::Rejected,
            vec!["the verifier test-plan bound was exceeded".to_owned()],
            Vec::new(),
            Vec::new(),
            true,
        );
        assert!(over_cap.is_canonical());
    }

    #[test]
    fn verified_with_all_passed_plans_is_canonical() {
        let artifact = projected_artifact(
            VerificationStatus::Verified,
            Vec::new(),
            vec!["evidence-passed".to_owned()],
            vec![projected_result(
                "suite::passed",
                true,
                true,
                true,
                Some("evidence-passed"),
                None,
            )],
            false,
        );
        assert!(artifact.is_canonical());
    }

    #[test]
    fn legacy_empty_plan_projection_allows_verified() {
        let artifact = projected_artifact(
            VerificationStatus::Verified,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
        );
        let mut value = serde_json::to_value(&artifact).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("test_plan_results");
        object.remove("test_plans_over_cap");
        let decoded: VerificationArtifact = serde_json::from_value(value).unwrap();
        assert!(decoded.test_plan_results.is_empty());
        assert!(!decoded.test_plans_over_cap);
        assert!(decoded.is_canonical());
    }
}
