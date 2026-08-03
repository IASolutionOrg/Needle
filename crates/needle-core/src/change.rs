use crate::{ArtifactId, CanonicalHasher, ClaimId, Digest};
use serde::{Deserialize, Serialize};
use std::fmt;

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
    pub verifier_definition: Digest,
    pub created_unix_ms: u64,
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

    pub fn is_canonical(&self) -> bool {
        self.id
            == Self::compute_id(
                &self.change_id,
                self.patch_id,
                self.verdict,
                &self.acceptance_coverage,
                &self.findings,
                &self.test_evidence_ids,
                self.verifier_definition,
            )
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
}
