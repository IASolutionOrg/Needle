use needle_core::{
    AcceptanceCoverage, AllowedPath, AllowedPathScope, ArtifactId, CanonicalHasher, ChangeId,
    ChangeRequest, ClaimId, Digest, MAX_ACCEPTANCE_CRITERIA, MAX_ALLOWED_PATHS,
    MAX_CHANGE_ARTIFACTS, MAX_CHANGE_CLAIMS, MAX_CHANGE_TASK_BYTES, MAX_PATCH_FILES,
    PatchOperation, VerificationStatus,
};
use needle_platform_codex::{PrepareChangeOutcome, VerifyChangeOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Component, Path};

const MAX_PREPARE_CHANGE_BYTES: usize = 16 * 1024;
const MAX_CONSTRAINTS: usize = 8;
const MAX_CRITERION_BYTES: usize = 2 * 1024;
const MAX_CONSTRAINT_BYTES: usize = 2 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpPrepareChangeRequest {
    pub task: String,
    pub acceptance_criteria: Vec<String>,
    pub allowed_paths: Vec<McpAllowedPath>,
    #[serde(default)]
    pub artifact_ids: Vec<ArtifactId>,
    #[serde(default)]
    pub claim_ids: Vec<ClaimId>,
    #[serde(default)]
    pub constraints: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpAllowedPath {
    pub path: String,
    pub scope: AllowedPathScope,
}

impl McpPrepareChangeRequest {
    pub(crate) fn validate_and_map(self, encoded_bytes: usize) -> Result<ChangeRequest, String> {
        if encoded_bytes > MAX_PREPARE_CHANGE_BYTES {
            return Err("arguments exceed the 16 KiB request bound".to_owned());
        }
        if self.task.trim().is_empty() || self.task.len() > MAX_CHANGE_TASK_BYTES {
            return Err("task must contain 1 to 8192 UTF-8 bytes".to_owned());
        }
        if self.acceptance_criteria.is_empty()
            || self.acceptance_criteria.len() > MAX_ACCEPTANCE_CRITERIA
            || self.acceptance_criteria.iter().any(|criterion| {
                criterion.trim().is_empty() || criterion.len() > MAX_CRITERION_BYTES
            })
        {
            return Err("acceptance_criteria requires 1 to 8 bounded non-empty items".to_owned());
        }
        if self.allowed_paths.is_empty() || self.allowed_paths.len() > MAX_ALLOWED_PATHS {
            return Err("allowed_paths requires 1 to 16 items".to_owned());
        }
        if self.artifact_ids.len() > MAX_CHANGE_ARTIFACTS
            || self.claim_ids.len() > MAX_CHANGE_CLAIMS
        {
            return Err("artifact_ids or claim_ids exceed their bounds".to_owned());
        }
        if self.constraints.len() > MAX_CONSTRAINTS
            || self.constraints.iter().any(|constraint| {
                constraint.trim().is_empty() || constraint.len() > MAX_CONSTRAINT_BYTES
            })
        {
            return Err("constraints allow at most eight bounded non-empty items".to_owned());
        }
        ensure_unique(&self.acceptance_criteria, "acceptance criterion")?;
        ensure_unique(&self.constraints, "constraint")?;
        ensure_unique(&self.artifact_ids, "artifact id")?;
        ensure_unique(&self.claim_ids, "claim id")?;
        let mut allowed = Vec::with_capacity(self.allowed_paths.len());
        let mut seen_paths = BTreeSet::new();
        for path in self.allowed_paths {
            let normalized = validate_path(&path.path)?;
            if !seen_paths.insert(normalized.clone()) {
                return Err("allowed_paths contains a duplicate path".to_owned());
            }
            allowed.push(AllowedPath { path: normalized, scope: path.scope });
        }
        Ok(ChangeRequest {
            task: self.task,
            acceptance_criteria: self.acceptance_criteria,
            allowed_paths: allowed,
            artifact_ids: self.artifact_ids,
            claim_ids: self.claim_ids,
            constraints: self.constraints,
        })
    }
}

fn ensure_unique<T: Ord>(items: &[T], label: &str) -> Result<(), String> {
    let unique = items.iter().collect::<BTreeSet<_>>();
    if unique.len() != items.len() {
        return Err(format!("{label} values must be unique"));
    }
    Ok(())
}

fn validate_path(value: &str) -> Result<String, String> {
    let normalized = value.replace('\\', "/");
    let path = Path::new(&normalized);
    if normalized.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(format!("unsafe allowed path `{value}`"));
    }
    let mut clean = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(format!("unsafe allowed path `{value}`"));
        };
        let component =
            component.to_str().ok_or_else(|| "allowed path must be UTF-8".to_owned())?;
        if !clean.is_empty() {
            clean.push('/');
        }
        clean.push_str(component);
    }
    if clean.split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | ".needle"
                | ".codegraph"
                | ".cache"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
        )
    }) {
        return Err(format!("protected path is not writable: `{clean}`"));
    }
    Ok(clean)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpPrepareChangeResponse {
    pub status: &'static str,
    pub change_id: String,
    pub patch_id: String,
    pub summary: String,
    pub changed_files: Vec<McpChangedFile>,
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    pub discovery: McpChangeDiscovery,
    pub residual_risks: Vec<String>,
    pub verification_status: VerificationStatus,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpChangedFile {
    pub path: String,
    pub operation: PatchOperation,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpChangeDiscovery {
    pub provided_artifacts: u32,
    pub provided_claims: u32,
    pub cache_only_context_calls: u32,
    pub observed_repository_files: u32,
    pub observation_gaps: u32,
}

impl McpPrepareChangeResponse {
    pub(crate) fn from_outcome(
        outcome: PrepareChangeOutcome,
        provided_artifacts: usize,
        provided_claims: usize,
    ) -> Self {
        Self {
            status: "prepared",
            change_id: outcome.change_id.to_string(),
            patch_id: outcome.patch_id.to_string(),
            summary: outcome.summary,
            changed_files: outcome
                .changed_files
                .into_iter()
                .map(|file| McpChangedFile { path: file.path, operation: file.operation })
                .collect(),
            acceptance_coverage: outcome.acceptance_coverage,
            discovery: McpChangeDiscovery {
                provided_artifacts: provided_artifacts.try_into().unwrap_or(u32::MAX),
                provided_claims: provided_claims.try_into().unwrap_or(u32::MAX),
                cache_only_context_calls: 0,
                observed_repository_files: outcome.observed_repository_files,
                observation_gaps: outcome.observation_gaps,
            },
            residual_risks: outcome.residual_risks,
            verification_status: outcome.verification_status,
        }
    }

    pub(crate) fn context(&self) -> String {
        format!(
            "Prepared isolated change {} as patch {} touching {} file(s). Verification has not been requested. {}",
            self.change_id,
            self.patch_id,
            self.changed_files.len(),
            self.summary.trim()
        )
    }
}

pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task": {"type": "string", "minLength": 1, "maxLength": MAX_CHANGE_TASK_BYTES},
            "acceptance_criteria": {
                "type": "array", "minItems": 1, "maxItems": MAX_ACCEPTANCE_CRITERIA,
                "uniqueItems": true,
                "items": {"type": "string", "minLength": 1, "maxLength": MAX_CRITERION_BYTES}
            },
            "allowed_paths": {
                "type": "array", "minItems": 1, "maxItems": MAX_ALLOWED_PATHS,
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "maxLength": 1024},
                        "scope": {"type": "string", "enum": ["exact", "subtree"]}
                    },
                    "required": ["path", "scope"],
                    "additionalProperties": false
                }
            },
            "artifact_ids": {
                "type": "array", "maxItems": MAX_CHANGE_ARTIFACTS, "uniqueItems": true,
                "items": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"}
            },
            "claim_ids": {
                "type": "array", "maxItems": MAX_CHANGE_CLAIMS, "uniqueItems": true,
                "items": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"}
            },
            "constraints": {
                "type": "array", "maxItems": MAX_CONSTRAINTS, "uniqueItems": true,
                "items": {"type": "string", "minLength": 1, "maxLength": MAX_CONSTRAINT_BYTES}
            }
        },
        "required": ["task", "acceptance_criteria", "allowed_paths"],
        "additionalProperties": false
    })
}

pub(crate) fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"const": "prepared"},
            "change_id": {"type": "string", "pattern": "^chg_[0-9a-f]{24}$"},
            "patch_id": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
            "summary": {"type": "string", "minLength": 1, "maxLength": 4096},
            "changed_files": {
                "type": "array", "minItems": 1, "maxItems": MAX_PATCH_FILES,
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "operation": {"type": "string", "enum": ["create", "update", "delete"]}
                    },
                    "required": ["path", "operation"],
                    "additionalProperties": false
                }
            },
            "acceptance_coverage": {
                "type": "array", "minItems": 1, "maxItems": MAX_ACCEPTANCE_CRITERIA,
                "items": {
                    "type": "object",
                    "properties": {
                        "criterion": {"type": "string"},
                        "status": {"type": "string", "enum": ["addressed", "partial", "unaddressed"]},
                        "evidence": {"type": "string"}
                    },
                    "required": ["criterion", "status", "evidence"],
                    "additionalProperties": false
                }
            },
            "discovery": {
                "type": "object",
                "properties": {
                    "provided_artifacts": {"type": "integer", "minimum": 0, "maximum": MAX_CHANGE_ARTIFACTS},
                    "provided_claims": {"type": "integer", "minimum": 0, "maximum": MAX_CHANGE_CLAIMS},
                    "cache_only_context_calls": {"const": 0},
                    "observed_repository_files": {"type": "integer", "minimum": 0},
                    "observation_gaps": {"type": "integer", "minimum": 0}
                },
                "required": [
                    "provided_artifacts", "provided_claims", "cache_only_context_calls",
                    "observed_repository_files", "observation_gaps"
                ],
                "additionalProperties": false
            },
            "residual_risks": {"type": "array", "maxItems": 8, "items": {"type": "string"}},
            "verification_status": {"const": "not_requested"}
        },
        "required": [
            "status", "change_id", "patch_id", "summary", "changed_files",
            "acceptance_coverage", "discovery", "residual_risks", "verification_status"
        ],
        "additionalProperties": false
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpVerifyChangeRequest {
    pub change_id: ChangeId,
}

impl McpVerifyChangeRequest {
    pub(crate) fn digest(&self) -> Digest {
        let mut hasher = CanonicalHasher::new(b"mcp-verify-change-request");
        hasher.field_str(self.change_id.as_str());
        hasher.finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpVerifyChangeResponse {
    pub status: VerificationStatus,
    pub change_id: String,
    pub patch_id: String,
    pub verification_id: String,
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    pub findings: Vec<String>,
    pub test_evidence_ids: Vec<String>,
    pub verifier_started: bool,
    pub repair_attempted: bool,
    pub repair_performed: bool,
    pub verification_attempts: u8,
}

impl McpVerifyChangeResponse {
    pub(crate) fn from_outcome(
        outcome: VerifyChangeOutcome,
        repair_attempted: bool,
        repair_performed: bool,
        verification_attempts: u8,
    ) -> Self {
        Self {
            status: outcome.artifact.verdict,
            change_id: outcome.artifact.change_id.to_string(),
            patch_id: outcome.artifact.patch_id.to_string(),
            verification_id: outcome.artifact.id.to_string(),
            acceptance_coverage: outcome.artifact.acceptance_coverage,
            findings: outcome.artifact.findings,
            test_evidence_ids: outcome.artifact.test_evidence_ids,
            verifier_started: outcome.verifier_started,
            repair_attempted,
            repair_performed,
            verification_attempts,
        }
    }

    pub(crate) fn context(&self) -> String {
        format!(
            "Verification for change {} completed with status {:?}. Patch {} remains unapplied.{}{}",
            self.change_id,
            self.status,
            self.patch_id,
            if self.findings.is_empty() {
                String::new()
            } else {
                format!(" Findings: {}", self.findings.join("; "))
            },
            if self.repair_performed {
                " One bounded repair and independent re-verification were performed."
            } else {
                ""
            },
        )
    }
}

pub(crate) fn verify_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "change_id": {"type": "string", "pattern": "^chg_[0-9a-f]{24}$"}
        },
        "required": ["change_id"],
        "additionalProperties": false
    })
}

pub(crate) fn verify_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["verified", "rejected", "repairable", "inconclusive"]},
            "change_id": {"type": "string", "pattern": "^chg_[0-9a-f]{24}$"},
            "patch_id": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
            "verification_id": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
            "acceptance_coverage": {
                "type": "array", "maxItems": MAX_ACCEPTANCE_CRITERIA,
                "items": {
                    "type": "object",
                    "properties": {
                        "criterion": {"type": "string"},
                        "status": {"type": "string", "enum": ["addressed", "partial", "unaddressed"]},
                        "evidence": {"type": "string"}
                    },
                    "required": ["criterion", "status", "evidence"],
                    "additionalProperties": false
                }
            },
            "findings": {"type": "array", "maxItems": 16, "items": {"type": "string"}},
            "test_evidence_ids": {"type": "array", "maxItems": 2, "items": {"type": "string"}},
            "verifier_started": {"type": "boolean"},
            "repair_attempted": {"type": "boolean"},
            "repair_performed": {"type": "boolean"},
            "verification_attempts": {"type": "integer", "minimum": 1, "maximum": 2}
        },
        "required": [
            "status", "change_id", "patch_id", "verification_id", "acceptance_coverage",
            "findings", "test_evidence_ids", "verifier_started", "repair_attempted",
            "repair_performed", "verification_attempts"
        ],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_closed_at_every_object_boundary() {
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["allowed_paths"]["items"]["additionalProperties"], false);
        let output = output_schema();
        assert_eq!(output["additionalProperties"], false);
        assert_eq!(output["properties"]["discovery"]["additionalProperties"], false);
        assert_eq!(verify_input_schema()["additionalProperties"], false);
        assert_eq!(verify_output_schema()["additionalProperties"], false);
    }

    #[test]
    fn request_rejects_unknown_and_unsafe_paths() {
        let unknown = serde_json::from_value::<McpPrepareChangeRequest>(json!({
            "task": "change",
            "acceptance_criteria": ["works"],
            "allowed_paths": [{"path": "src/lib.rs", "scope": "exact"}],
            "extra": true
        }));
        assert!(unknown.is_err());
        let unsafe_request: McpPrepareChangeRequest = serde_json::from_value(json!({
            "task": "change",
            "acceptance_criteria": ["works"],
            "allowed_paths": [{"path": "../outside", "scope": "exact"}]
        }))
        .unwrap();
        assert!(unsafe_request.validate_and_map(128).is_err());
    }
}
