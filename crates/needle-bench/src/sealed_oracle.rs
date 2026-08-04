use crate::{
    CorpusMaterialClass, FocusedTestPolicyRef, FrozenCorpusManifest, QualityOracleResult,
    QualityOracleSpec, read_bounded_file,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path};

pub const SEALED_BUNDLE_INDEX_SCHEMA: &str = "needle.sealed-oracle-index/1";
pub const SEALED_ORACLE_SCHEMA: &str = "needle.sealed-oracle/1";
pub const MAX_SEALED_INDEX_BYTES: usize = 512 * 1024;
pub const MAX_SEALED_DOCUMENT_BYTES: usize = 512 * 1024;
pub const MAX_SEALED_ENTRIES: usize = 512;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 512;
const MAX_POLICY_STRING_BYTES: usize = 4_096;
const MAX_POLICY_VECTOR_ITEMS: usize = 64;
const MAX_POLICY_ARG_BYTES: usize = 256;

/// Evaluator-only index entry.  `relative_path` is accepted only at the
/// evaluator boundary and never appears in a public manifest or ArmLaunch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedOracleIndexEntry {
    pub task_id: String,
    pub material_class: CorpusMaterialClass,
    pub repository_sha: String,
    pub oracle_schema: String,
    pub oracle_digest: String,
    pub focused_test_policy: FocusedTestPolicyRef,
    pub relative_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedOracleIndex {
    pub schema: String,
    pub entries: Vec<SealedOracleIndexEntry>,
}

/// This document is intentionally answer-bearing and is only deserialized by
/// the evaluator process.  It is never a field on a corpus task or launch DTO.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedOracleDocument {
    pub schema: String,
    pub task_id: String,
    pub repository_sha: String,
    pub focused_test_policy: FocusedTestPolicyRef,
    pub focused_test: SealedFocusedTestPolicy,
    pub quality: SealedQualityPolicy,
}

/// Evaluator-only focused-test contract.  The argv is sealed and never
/// projected into a public manifest or launch DTO; only this exact direct
/// offline Cargo integration-test shape is accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedFocusedTestPolicy {
    pub identifier: String,
    pub argv: Vec<String>,
}

impl SealedFocusedTestPolicy {
    pub fn is_well_formed(&self) -> bool {
        let expected = [
            "cargo",
            "test",
            "--offline",
            "--test",
            "integration",
            self.identifier.as_str(),
            "--",
            "--exact",
        ];
        !self.identifier.trim().is_empty()
            && self.identifier.len() <= MAX_POLICY_ARG_BYTES
            && !self.identifier.chars().any(char::is_whitespace)
            && self.argv.len() == expected.len()
            && self
                .argv
                .iter()
                .all(|argument| !argument.is_empty() && argument.len() <= MAX_POLICY_ARG_BYTES)
            && self.argv.iter().map(String::as_str).eq(expected)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedQualityPolicy {
    pub required_files: Vec<String>,
    pub required_symbols: Vec<String>,
    pub required_claims: Vec<String>,
    pub forbidden_claims: Vec<String>,
    pub focused_test_command: String,
    pub accepted_focused_test_identifiers: Vec<String>,
    pub focused_test_required: bool,
}

impl SealedQualityPolicy {
    pub fn is_well_formed(&self) -> bool {
        self.required_files.len() <= MAX_POLICY_VECTOR_ITEMS
            && self.required_symbols.len() <= MAX_POLICY_VECTOR_ITEMS
            && self.required_claims.len() <= MAX_POLICY_VECTOR_ITEMS
            && self.forbidden_claims.len() <= MAX_POLICY_VECTOR_ITEMS
            && self.accepted_focused_test_identifiers.len() <= MAX_POLICY_VECTOR_ITEMS
            && self
                .required_files
                .iter()
                .chain(&self.required_symbols)
                .chain(&self.required_claims)
                .chain(&self.forbidden_claims)
                .chain(&self.accepted_focused_test_identifiers)
                .all(|value| value.len() <= MAX_POLICY_STRING_BYTES)
            && self.focused_test_command.len() <= MAX_POLICY_STRING_BYTES
            && self
                .required_files
                .iter()
                .chain(&self.required_symbols)
                .chain(&self.required_claims)
                .chain(&self.forbidden_claims)
                .chain(&self.accepted_focused_test_identifiers)
                .all(|value| !value.trim().is_empty())
            && !self.focused_test_command.trim().is_empty()
    }

    pub fn as_quality_oracle_spec(&self) -> QualityOracleSpec {
        QualityOracleSpec {
            required_files: self.required_files.clone(),
            required_symbols: self.required_symbols.clone(),
            required_claims: self.required_claims.clone(),
            forbidden_claims: self.forbidden_claims.clone(),
            focused_test_command: self.focused_test_command.clone(),
            accepted_focused_test_identifiers: self.accepted_focused_test_identifiers.clone(),
            focused_test_required: self.focused_test_required,
        }
    }
}

#[derive(Serialize)]
struct FocusedTestPolicyMaterial<'a> {
    identity: &'a str,
    focused_test: &'a SealedFocusedTestPolicy,
    quality: &'a SealedQualityPolicy,
}

/// Commitment used by public task refs and evaluator-owned documents.  It is
/// computed from the complete bounded structured policy, never from an answer
/// string or filesystem location.
pub fn focused_test_policy_commitment(
    identity: &str,
    focused_test: &SealedFocusedTestPolicy,
    quality: &SealedQualityPolicy,
) -> String {
    let material = FocusedTestPolicyMaterial { identity, focused_test, quality };
    format!("b3:{}", blake3::hash(&serde_json::to_vec(&material).unwrap_or_default()).to_hex())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundleTaskValidation {
    pub task_id: String,
    pub bound: bool,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SealedBundleValidationReport {
    pub schema: String,
    pub index_digest: Option<String>,
    pub bundle_root: Option<String>,
    pub production_material: bool,
    pub production_bundle_ready: bool,
    pub tasks: Vec<BundleTaskValidation>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedEvaluationResult {
    pub schema: String,
    pub task_id: String,
    pub response_digest: String,
    pub oracle_digest: Option<String>,
    pub quality_passed: Option<bool>,
    pub quality_failures: Vec<String>,
    pub focused_test_evidence_status: String,
    pub infrastructure_errors: Vec<String>,
    pub evaluator_errors: Vec<String>,
    pub error_codes: Vec<String>,
}

impl SealedBundleValidationReport {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            schema: "needle.sealed-oracle-validation/1".to_owned(),
            index_digest: None,
            bundle_root: None,
            production_material: false,
            production_bundle_ready: false,
            tasks: Vec::new(),
            errors: vec![reason.into()],
        }
    }
}

/// Validate a sealed evaluator index and its bytes under a caller-provided
/// private bundle root.  Diagnostics are bounded so malformed material cannot
/// flood preflight output.
pub fn validate_sealed_bundle(
    manifest: &FrozenCorpusManifest,
    index_path: Option<&Path>,
    bundle_root: Option<&Path>,
) -> SealedBundleValidationReport {
    const MAX_ERRORS: usize = 24;
    let (Some(index_path), Some(bundle_root)) = (index_path, bundle_root) else {
        return SealedBundleValidationReport::unavailable(
            "production sealed evaluator bundle is unavailable",
        );
    };
    let index_bytes = match read_bounded_file(index_path, MAX_SEALED_INDEX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return SealedBundleValidationReport::unavailable(
                "sealed evaluator index exceeds bounded byte limit",
            );
        }
        Err(_) => {
            return SealedBundleValidationReport::unavailable(
                "sealed evaluator index cannot be read",
            );
        }
    };
    let index_digest = format!("b3:{}", blake3::hash(&index_bytes).to_hex());
    if let Some(expected) = manifest.sealed_bundle_digest.as_deref()
        && expected != index_digest
    {
        return SealedBundleValidationReport {
            index_digest: Some(index_digest),
            bundle_root: None,
            ..SealedBundleValidationReport::unavailable(
                "sealed evaluator index digest differs from manifest commitment",
            )
        };
    }
    let index: SealedOracleIndex = match serde_json::from_slice(&index_bytes) {
        Ok(index) => index,
        Err(_) => {
            return SealedBundleValidationReport {
                index_digest: Some(index_digest),
                bundle_root: None,
                ..SealedBundleValidationReport::unavailable(
                    "sealed evaluator index JSON is invalid",
                )
            };
        }
    };
    let root = match bundle_root.canonicalize() {
        Ok(root) => root,
        Err(_) => {
            return SealedBundleValidationReport {
                index_digest: Some(index_digest),
                bundle_root: None,
                ..SealedBundleValidationReport::unavailable(
                    "sealed evaluator bundle root is unavailable",
                )
            };
        }
    };
    let mut errors = Vec::new();
    if index.schema != SEALED_BUNDLE_INDEX_SCHEMA {
        errors.push("sealed evaluator index schema is unsupported".to_owned());
    }
    if index.entries.is_empty() || index.entries.len() > MAX_SEALED_ENTRIES {
        errors.push("sealed evaluator index entry count is out of bounds".to_owned());
    }
    let mut seen = BTreeSet::new();
    let mut task_reports = Vec::new();
    let mut all_bound = index.schema == SEALED_BUNDLE_INDEX_SCHEMA
        && !index.entries.is_empty()
        && index.entries.len() <= MAX_SEALED_ENTRIES;
    for entry in &index.entries {
        if entry.task_id.len() > MAX_IDENTIFIER_BYTES
            || entry.repository_sha.len() > MAX_IDENTIFIER_BYTES
            || entry.oracle_schema.len() > MAX_IDENTIFIER_BYTES
            || entry.oracle_digest.len() > MAX_IDENTIFIER_BYTES
            || entry.focused_test_policy.identity.len() > MAX_IDENTIFIER_BYTES
            || entry.focused_test_policy.commitment.len() > MAX_IDENTIFIER_BYTES
            || entry.relative_path.len() > MAX_PATH_BYTES
        {
            push_error(
                &mut errors,
                MAX_ERRORS,
                "sealed entry exceeds bounded field length".to_owned(),
            );
            all_bound = false;
            continue;
        }
        if !seen.insert(entry.task_id.as_str()) {
            all_bound = false;
            push_error(
                &mut errors,
                MAX_ERRORS,
                format!("duplicate sealed task `{}`", entry.task_id),
            );
            continue;
        }
        let Some(task) = manifest.tasks.iter().find(|task| task.id == entry.task_id) else {
            all_bound = false;
            push_error(
                &mut errors,
                MAX_ERRORS,
                format!("sealed task `{}` is not in manifest", entry.task_id),
            );
            continue;
        };
        let mut task_errors = Vec::new();
        if (entry.material_class != CorpusMaterialClass::ProductionSealed
            || task.material_class != CorpusMaterialClass::ProductionSealed)
            && entry.material_class != task.material_class
        {
            task_errors.push("material class differs".to_owned());
        }
        if entry.repository_sha != task.repository_sha {
            task_errors.push("repository SHA differs".to_owned());
        }
        if entry.oracle_schema != task.oracle_schema || entry.oracle_digest != task.oracle_digest {
            task_errors.push("oracle schema or digest differs".to_owned());
        }
        if entry.focused_test_policy != task.focused_test_policy {
            task_errors.push("focused-test policy commitment differs".to_owned());
        }
        let path = Path::new(&entry.relative_path);
        if !safe_relative_path(path) {
            task_errors.push("sealed oracle path is not safe and relative".to_owned());
        }
        let bytes = if task_errors.is_empty() {
            let candidate = root.join(path);
            match candidate.canonicalize() {
                Ok(canonical) if canonical.starts_with(&root) => {
                    match read_bounded_file(&canonical, MAX_SEALED_DOCUMENT_BYTES) {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                            task_errors.push(
                                "sealed oracle document exceeds bounded byte limit".to_owned(),
                            );
                            None
                        }
                        Err(_) => {
                            task_errors.push("sealed oracle bytes cannot be read".to_owned());
                            None
                        }
                    }
                }
                Ok(_) => {
                    task_errors.push("sealed oracle path escapes bundle root".to_owned());
                    None
                }
                Err(_) => {
                    task_errors.push("sealed oracle path is unavailable".to_owned());
                    None
                }
            }
        } else {
            None
        };
        if let Some(bytes) = bytes {
            let actual = format!("b3:{}", blake3::hash(&bytes).to_hex());
            if actual != entry.oracle_digest {
                task_errors.push("sealed oracle digest differs".to_owned());
            }
            match serde_json::from_slice::<SealedOracleDocument>(&bytes) {
                Ok(document) => {
                    if document.schema != SEALED_ORACLE_SCHEMA
                        || document.task_id != task.id
                        || document.repository_sha != task.repository_sha
                        || document.focused_test_policy != task.focused_test_policy
                        || !document.focused_test.is_well_formed()
                        || document.quality.focused_test_command != document.focused_test.identifier
                        || !document.quality.is_well_formed()
                        || focused_test_policy_commitment(
                            &document.focused_test_policy.identity,
                            &document.focused_test,
                            &document.quality,
                        ) != task.focused_test_policy.commitment
                    {
                        task_errors.push("sealed oracle metadata does not bind to task".to_owned());
                    }
                }
                Err(_) => task_errors.push("sealed oracle JSON is invalid".to_owned()),
            }
        }
        let bound = task_errors.is_empty();
        all_bound &= bound;
        for error in &task_errors {
            push_error(&mut errors, MAX_ERRORS, format!("{}: {error}", entry.task_id));
        }
        task_reports.push(BundleTaskValidation {
            task_id: entry.task_id.clone(),
            bound,
            errors: task_errors,
        });
    }
    let expected = manifest.tasks.iter().map(|task| task.id.as_str()).collect::<BTreeSet<_>>();
    if seen != expected {
        push_error(
            &mut errors,
            MAX_ERRORS,
            "sealed bundle task set differs from manifest".to_owned(),
        );
        all_bound = false;
    }
    let production_material = all_bound
        && !manifest.tasks.is_empty()
        && manifest
            .tasks
            .iter()
            .all(|task| task.material_class == CorpusMaterialClass::ProductionSealed);
    SealedBundleValidationReport {
        schema: "needle.sealed-oracle-validation/1".to_owned(),
        index_digest: Some(index_digest),
        bundle_root: None,
        production_material,
        production_bundle_ready: production_material && errors.is_empty(),
        tasks: task_reports,
        errors,
    }
}

fn safe_relative_path(path: &Path) -> bool {
    path.is_relative()
        && !path.as_os_str().is_empty()
        && path.components().all(|component| matches!(component, Component::Normal(_)))
}

fn push_error(errors: &mut Vec<String>, limit: usize, error: String) {
    if errors.len() < limit {
        errors.push(error);
    }
}

/// Keep a direct helper for callers that need to commit an index's raw bytes.
pub fn sealed_index_digest(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

/// Evaluator-only response boundary.  The caller supplies the private bundle
/// and completed response; the returned projection contains only digests,
/// quality booleans/failure codes, and bounded infrastructure diagnostics.
pub fn evaluate_sealed_response(
    manifest: &FrozenCorpusManifest,
    index_path: &Path,
    bundle_root: &Path,
    task_id: &str,
    response: &str,
    focused_test_passed: Option<bool>,
) -> SealedEvaluationResult {
    const MAX_ERRORS: usize = 24;
    let response_digest = format!("b3:{}", blake3::hash(response.as_bytes()).to_hex());
    let mut result = SealedEvaluationResult {
        schema: "needle.sealed-evaluation/1".to_owned(),
        task_id: task_id.chars().take(MAX_IDENTIFIER_BYTES).collect(),
        response_digest,
        oracle_digest: manifest
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.oracle_digest.clone()),
        quality_passed: None,
        quality_failures: Vec::new(),
        focused_test_evidence_status: match focused_test_passed {
            Some(true) => "passed".to_owned(),
            Some(false) => "failed".to_owned(),
            None => "not_run".to_owned(),
        },
        infrastructure_errors: Vec::new(),
        evaluator_errors: Vec::new(),
        error_codes: Vec::new(),
    };
    if task_id.is_empty()
        || task_id.len() > MAX_IDENTIFIER_BYTES
        || response.len() > MAX_SEALED_DOCUMENT_BYTES
    {
        result.evaluator_errors.push("evaluation input exceeds bounded limits".to_owned());
        result.error_codes.push("input_bounds".to_owned());
        return result;
    }
    let bundle = validate_sealed_bundle(manifest, Some(index_path), Some(bundle_root));
    if !bundle.errors.is_empty() {
        result.infrastructure_errors.push("sealed bundle validation failed".to_owned());
        result.error_codes.push("bundle_invalid".to_owned());
        return result;
    }
    let index_bytes = match read_bounded_file(index_path, MAX_SEALED_INDEX_BYTES) {
        Ok(bytes) => bytes,
        _ => {
            result.infrastructure_errors.push("sealed evaluator index cannot be read".to_owned());
            result.error_codes.push("index_unavailable".to_owned());
            return result;
        }
    };
    let index = match serde_json::from_slice::<SealedOracleIndex>(&index_bytes) {
        Ok(index) => index,
        Err(_) => {
            result.infrastructure_errors.push("sealed evaluator index is invalid".to_owned());
            result.error_codes.push("index_invalid".to_owned());
            return result;
        }
    };
    let Some(entry) = index.entries.iter().find(|entry| entry.task_id == task_id) else {
        result.evaluator_errors.push("evaluation task is absent from sealed index".to_owned());
        result.error_codes.push("task_missing".to_owned());
        return result;
    };
    let root = match bundle_root.canonicalize() {
        Ok(root) => root,
        Err(_) => {
            result.infrastructure_errors.push("sealed bundle root is unavailable".to_owned());
            result.error_codes.push("bundle_unavailable".to_owned());
            return result;
        }
    };
    let Some(path) = root.join(&entry.relative_path).canonicalize().ok() else {
        result.infrastructure_errors.push("sealed oracle document is unavailable".to_owned());
        result.error_codes.push("oracle_unavailable".to_owned());
        return result;
    };
    let candidate = root.join(&entry.relative_path);
    if !path.starts_with(&root) || !candidate.exists() {
        result.infrastructure_errors.push("sealed oracle path is outside bundle".to_owned());
        result.error_codes.push("oracle_path".to_owned());
        return result;
    }
    let document = match read_bounded_file(&path, MAX_SEALED_DOCUMENT_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SealedOracleDocument>(&bytes).ok())
    {
        Some(document) => document,
        None => {
            result.infrastructure_errors.push("sealed oracle document is invalid".to_owned());
            result.error_codes.push("oracle_invalid".to_owned());
            return result;
        }
    };
    let quality = QualityOracleResult::evaluate(
        &document.quality.as_quality_oracle_spec(),
        response,
        focused_test_passed,
    );
    result.quality_passed = Some(quality.passed);
    result.quality_failures = quality.failures.into_iter().take(MAX_ERRORS).collect();
    if !quality.passed {
        result.error_codes.push("quality_failed".to_owned());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CorpusTask;
    use std::fs;

    #[test]
    fn missing_bundle_is_fail_closed_and_bounded() {
        let report = validate_sealed_bundle(
            &FrozenCorpusManifest {
                schema: "needle.frozen-corpus/4".to_owned(),
                frozen_unix_ms: 1,
                arms: Vec::new(),
                cost_model_path: String::new(),
                cost_model_digest: String::new(),
                next_pilot_path: String::new(),
                next_pilot_digest: String::new(),
                campaign_path: None,
                campaign_digest: None,
                schedule_path: None,
                schedule_digest: None,
                power_plan_path: None,
                power_plan_digest: None,
                sealed_bundle_schema: None,
                sealed_bundle_digest: None,
                tasks: Vec::new(),
            },
            None,
            None,
        );
        assert!(!report.production_bundle_ready);
        assert!(report.bundle_root.is_none());
        assert!(report.errors.len() <= 24);
    }

    #[test]
    fn unsafe_and_absolute_bundle_paths_fail_closed() {
        assert!(!safe_relative_path(Path::new("../oracle.json")));
        assert!(!safe_relative_path(Path::new("/tmp/oracle.json")));
        assert!(!safe_relative_path(Path::new("")));
        assert!(safe_relative_path(Path::new("task/oracle.json")));
    }

    #[test]
    fn malformed_focused_test_command_is_rejected() {
        let malformed = SealedFocusedTestPolicy {
            identifier: "suite::case".to_owned(),
            argv: ["sh", "-c", "cargo test suite::case"].into_iter().map(str::to_owned).collect(),
        };
        assert!(!malformed.is_well_formed());
    }

    #[test]
    fn bundle_binding_rejects_mutation_swap_and_unknown_fields() {
        let root = std::env::temp_dir().join(format!("needle-sealed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let quality = SealedQualityPolicy {
            required_files: Vec::new(),
            required_symbols: Vec::new(),
            required_claims: Vec::new(),
            forbidden_claims: Vec::new(),
            focused_test_command: "synthetic-test".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: false,
        };
        let focused_test = SealedFocusedTestPolicy {
            identifier: "synthetic-test".to_owned(),
            argv: [
                "cargo",
                "test",
                "--offline",
                "--test",
                "integration",
                "synthetic-test",
                "--",
                "--exact",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        };
        let policy = FocusedTestPolicyRef {
            identity: "synthetic-policy".to_owned(),
            commitment: focused_test_policy_commitment("synthetic-policy", &focused_test, &quality),
        };
        let document = |task_id: &str| SealedOracleDocument {
            schema: SEALED_ORACLE_SCHEMA.to_owned(),
            task_id: task_id.to_owned(),
            repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            focused_test_policy: policy.clone(),
            focused_test: focused_test.clone(),
            quality: quality.clone(),
        };
        let first = serde_json::to_vec(&document("one")).unwrap();
        let second = serde_json::to_vec(&document("two")).unwrap();
        fs::write(root.join("one.json"), &first).unwrap();
        fs::write(root.join("two.json"), &second).unwrap();
        let task = |id: &str, path: &str, bytes: &[u8]| CorpusTask {
            id: id.to_owned(),
            route: crate::BenchmarkRoute::LocateImplementation,
            split: crate::CorpusSplit::Calibration,
            repository_url: "https://example.invalid/repo.git".to_owned(),
            repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            prompt: "A bounded synthetic prompt with no answer-bearing material.".to_owned(),
            material_class: CorpusMaterialClass::ProductionSealed,
            focused_test_policy: policy.clone(),
            oracle_schema: SEALED_ORACLE_SCHEMA.to_owned(),
            oracle_digest: sealed_index_digest(bytes),
            oracle_path: path.to_owned(),
            test_identifier: String::new(),
            focused_command: Vec::new(),
        };
        let manifest = FrozenCorpusManifest {
            schema: "needle.frozen-corpus/4".to_owned(),
            frozen_unix_ms: 1,
            arms: Vec::new(),
            cost_model_path: String::new(),
            cost_model_digest: String::new(),
            next_pilot_path: String::new(),
            next_pilot_digest: String::new(),
            campaign_path: None,
            campaign_digest: None,
            schedule_path: None,
            schedule_digest: None,
            power_plan_path: None,
            power_plan_digest: None,
            sealed_bundle_schema: None,
            sealed_bundle_digest: None,
            tasks: vec![task("one", "one.json", &first), task("two", "two.json", &second)],
        };
        let index = SealedOracleIndex {
            schema: SEALED_BUNDLE_INDEX_SCHEMA.to_owned(),
            entries: vec![
                SealedOracleIndexEntry {
                    task_id: "one".to_owned(),
                    material_class: CorpusMaterialClass::ProductionSealed,
                    repository_sha: manifest.tasks[0].repository_sha.clone(),
                    oracle_schema: SEALED_ORACLE_SCHEMA.to_owned(),
                    oracle_digest: manifest.tasks[0].oracle_digest.clone(),
                    focused_test_policy: policy.clone(),
                    relative_path: "one.json".to_owned(),
                },
                SealedOracleIndexEntry {
                    task_id: "two".to_owned(),
                    material_class: CorpusMaterialClass::ProductionSealed,
                    repository_sha: manifest.tasks[1].repository_sha.clone(),
                    oracle_schema: SEALED_ORACLE_SCHEMA.to_owned(),
                    oracle_digest: manifest.tasks[1].oracle_digest.clone(),
                    focused_test_policy: policy,
                    relative_path: "two.json".to_owned(),
                },
            ],
        };
        let index_path = root.join("index.json");
        fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let valid = validate_sealed_bundle(&manifest, Some(&index_path), Some(&root));
        assert!(valid.production_bundle_ready, "{:?}", valid.errors);
        fs::copy(root.join("one.json"), root.join("two.json")).unwrap();
        let swapped = validate_sealed_bundle(&manifest, Some(&index_path), Some(&root));
        assert!(!swapped.production_bundle_ready);
        assert!(swapped.errors.iter().any(|error| error.contains("two")));
        fs::write(root.join("two.json"), &second).unwrap();
        fs::remove_file(root.join("one.json")).unwrap();
        let missing = validate_sealed_bundle(&manifest, Some(&index_path), Some(&root));
        assert!(!missing.production_bundle_ready);
        assert!(missing.errors.iter().any(|error| error.contains("one")));
        fs::write(root.join("one.json"), &first).unwrap();
        fs::write(root.join("two.json"), b"mutated").unwrap();
        let mutated = validate_sealed_bundle(&manifest, Some(&index_path), Some(&root));
        assert!(!mutated.production_bundle_ready);
        assert!(mutated.errors.iter().any(|error| error.contains("two")));
        assert!(
            serde_json::from_str::<SealedOracleIndex>(
                r#"{"schema":"needle.sealed-oracle-index/1","entries":[],"answer":"leak"}"#
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synthetic_fixture_evaluator_returns_only_bounded_projection() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/corpus/router-cache");
        let manifest: FrozenCorpusManifest =
            serde_json::from_slice(&fs::read(root.join("manifest.json")).expect("manifest"))
                .expect("manifest JSON");
        let bundle_root = root.join("synthetic-sealed");
        let report = validate_sealed_bundle(
            &manifest,
            Some(&bundle_root.join("index.json")),
            Some(&bundle_root),
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.production_bundle_ready);
        let result = evaluate_sealed_response(
            &manifest,
            &bundle_root.join("index.json"),
            &bundle_root,
            "ripgrep-glob-case-insensitive-locate-calibration",
            "The implementation is globs in crates/core/flags/hiargs.rs; the test is misc::glob_always_case_insensitive.",
            Some(true),
        );
        assert_eq!(result.quality_passed, Some(true));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("synthetic-sealed"));
        assert!(!encoded.contains("hiargs.rs"));
        assert!(result.error_codes.is_empty());
    }
}
