use needle_core::{SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID, SemanticWorkerArtifact};
use needle_runtime::RuntimeStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const SHADOW_REPLAY_SCHEMA_ID: &str = "needle.proof-shadow-replay/1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowReplaySource {
    pub run_id: String,
    pub route: String,
    pub product_data: PathBuf,
    pub repository_root: PathBuf,
    pub report_path: PathBuf,
    pub recorded_cases: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShadowReplayRun {
    pub run_id: String,
    pub route: String,
    pub report_digest: String,
    pub database_digest: String,
    pub recorded_cases: u32,
    pub cache_entries: u32,
    pub persisted_artifacts: u32,
    pub legacy_result_v1_count: u32,
    pub semantic_result_v2_count: u32,
    pub directly_decodable_semantic_artifacts: u32,
    pub certifiable_artifacts: u32,
    pub proof_candidates: u32,
    pub selected_proofs: u32,
    pub false_positives: u32,
    pub observation_gaps: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoricalCostEvidence {
    pub main_microcredits: u64,
    pub worker_microcredits: u64,
    pub repair_microcredits: Option<u64>,
    pub escalation_microcredits: Option<u64>,
    pub total_microcredits: u64,
    pub pricing_snapshot_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProofShadowReplayReport {
    pub schema_id: String,
    pub mode: String,
    pub provider_calls: u32,
    pub runs: Vec<ShadowReplayRun>,
    pub recorded_cases: u32,
    pub selected_proofs: u32,
    pub true_positives: u32,
    pub false_positives: u32,
    pub proof_precision: Option<f64>,
    pub proof_precision_reason: Option<String>,
    pub opportunity_rate: f64,
    pub safety_gate_passed: bool,
    pub usefulness_gate_passed: bool,
    pub live_run_ready: bool,
    pub historical_cost_evidence: HistoricalCostEvidence,
    pub future_live_estimate_microcredits: Option<u64>,
    pub future_live_estimate_reason: String,
}

#[derive(Debug, Error)]
pub enum ShadowReplayError {
    #[error("shadow replay I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("shadow replay store failed: {0}")]
    Store(#[from] needle_runtime::StoreError),
    #[error("shadow replay input is invalid: {0}")]
    Invalid(String),
}

pub fn run_shadow_replay(
    sources: &[ShadowReplaySource],
    scratch_root: &Path,
    historical_cost_evidence: HistoricalCostEvidence,
) -> Result<ProofShadowReplayReport, ShadowReplayError> {
    if sources.is_empty() {
        return Err(ShadowReplayError::Invalid("at least one source is required".to_owned()));
    }
    fs::create_dir_all(scratch_root)?;
    let mut runs = Vec::with_capacity(sources.len());
    for source in sources {
        runs.push(replay_source(source, scratch_root)?);
    }

    let recorded_cases: u32 = runs.iter().map(|run| run.recorded_cases).sum();
    let selected_proofs: u32 = runs.iter().map(|run| run.selected_proofs).sum();
    let false_positives: u32 = runs.iter().map(|run| run.false_positives).sum();
    let true_positives = selected_proofs.saturating_sub(false_positives);
    let proof_precision =
        (selected_proofs > 0).then(|| f64::from(true_positives) / f64::from(selected_proofs));
    let opportunity_rate = if recorded_cases == 0 {
        0.0
    } else {
        f64::from(selected_proofs) / f64::from(recorded_cases)
    };

    Ok(ProofShadowReplayReport {
        schema_id: SHADOW_REPLAY_SCHEMA_ID.to_owned(),
        mode: "shadow".to_owned(),
        provider_calls: 0,
        runs,
        recorded_cases,
        selected_proofs,
        true_positives,
        false_positives,
        proof_precision,
        proof_precision_reason: proof_precision
            .is_none()
            .then(|| "undefined because the proof resolver selected no proof".to_owned()),
        opportunity_rate,
        safety_gate_passed: false_positives == 0,
        usefulness_gate_passed: selected_proofs > 0,
        live_run_ready: false,
        historical_cost_evidence,
        future_live_estimate_microcredits: None,
        future_live_estimate_reason:
            "unavailable until a v0.4 artifact-result/2 calibration fixture passes shadow reuse and the live corpus is frozen"
                .to_owned(),
    })
}

fn replay_source(
    source: &ShadowReplaySource,
    scratch_root: &Path,
) -> Result<ShadowReplayRun, ShadowReplayError> {
    if source.recorded_cases == 0 {
        return Err(ShadowReplayError::Invalid(format!(
            "{} has no recorded replay cases",
            source.run_id
        )));
    }
    if !source.repository_root.is_dir() {
        return Err(ShadowReplayError::Invalid(format!(
            "{} repository root does not exist",
            source.run_id
        )));
    }
    let source_database = source.product_data.join("needle.sqlite3");
    if !source_database.is_file() || !source.report_path.is_file() {
        return Err(ShadowReplayError::Invalid(format!(
            "{} is missing its database or report",
            source.run_id
        )));
    }

    let run_scratch = scratch_root.join(&source.run_id);
    fs::create_dir_all(&run_scratch)?;
    let copied_database = run_scratch.join("needle.sqlite3");
    fs::copy(&source_database, &copied_database)?;
    for suffix in ["-wal", "-shm"] {
        let source_sidecar = PathBuf::from(format!("{}{suffix}", source_database.display()));
        if source_sidecar.is_file() {
            fs::copy(
                source_sidecar,
                PathBuf::from(format!("{}{suffix}", copied_database.display())),
            )?;
        }
    }

    let store = RuntimeStore::new(&copied_database);
    store.initialize()?;
    let cache_records = store.cache_records()?;
    let artifacts = store.artifacts()?;
    let directly_decodable_semantic_artifacts = artifacts
        .iter()
        .filter(|artifact| {
            artifact.contract.schema_id == SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
                && serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone())
                    .is_ok()
        })
        .count();

    let mut legacy_result_v1_count = 0_u32;
    let mut semantic_result_v2_count = 0_u32;
    let mut semantic_artifact_count = 0_u32;
    let mut observation_gaps = BTreeSet::new();
    for record in &cache_records {
        let Some(entry) = store.cache_entry(record.identity_digest)? else {
            return Err(ShadowReplayError::Invalid(format!(
                "{} cache index references a missing entry",
                source.run_id
            )));
        };
        if let Some(result) = entry.worker_outcome.artifact_result {
            legacy_result_v1_count = legacy_result_v1_count.saturating_add(1);
            observation_gaps.extend(result.observation_trace.gaps);
        }
        if let Some(result) = entry.worker_outcome.semantic_artifact_result {
            semantic_result_v2_count = semantic_result_v2_count.saturating_add(1);
            semantic_artifact_count = semantic_artifact_count
                .saturating_add(u32::try_from(result.artifacts.len()).unwrap_or(u32::MAX));
            observation_gaps.extend(result.observation_trace.gaps);
        }
    }

    let certifiable_artifacts = if directly_decodable_semantic_artifacts == 0 {
        0
    } else {
        semantic_artifact_count
            .min(u32::try_from(directly_decodable_semantic_artifacts).unwrap_or(u32::MAX))
    };
    let proof_candidates = 0;
    let selected_proofs = 0;
    let false_positives = 0;
    let mut blockers = Vec::new();
    if legacy_result_v1_count > 0 {
        blockers.push(
            "legacy needle.artifact-result/1 output has no validator-derived v0.4 coverage"
                .to_owned(),
        );
    }
    if semantic_result_v2_count == 0 {
        blockers.push("no needle.artifact-result/2 worker output was recorded".to_owned());
    }
    if artifacts.is_empty() {
        blockers.push("the recorded run published no cache artifact".to_owned());
    } else if directly_decodable_semantic_artifacts == 0 {
        blockers.push(
            "persisted v0.3 payloads cannot be decoded as typed semantic artifacts".to_owned(),
        );
    }
    if observation_gaps.iter().any(|gap| gap == "unknown_command_action") {
        blockers.push(
            "the legacy worker observation manifest contains unknown_command_action".to_owned(),
        );
    }
    blockers.push(
        "the recorded run predates NeedIR fragments and replayable sufficiency certificates"
            .to_owned(),
    );

    Ok(ShadowReplayRun {
        run_id: source.run_id.clone(),
        route: source.route.clone(),
        report_digest: digest_file(&source.report_path)?,
        database_digest: digest_database(&source_database)?,
        recorded_cases: source.recorded_cases,
        cache_entries: u32::try_from(cache_records.len()).unwrap_or(u32::MAX),
        persisted_artifacts: u32::try_from(artifacts.len()).unwrap_or(u32::MAX),
        legacy_result_v1_count,
        semantic_result_v2_count,
        directly_decodable_semantic_artifacts: u32::try_from(directly_decodable_semantic_artifacts)
            .unwrap_or(u32::MAX),
        certifiable_artifacts,
        proof_candidates,
        selected_proofs,
        false_positives,
        observation_gaps: observation_gaps.into_iter().collect(),
        blockers,
    })
}

fn digest_database(path: &Path) -> Result<String, std::io::Error> {
    let mut hasher = blake3::Hasher::new();
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.is_file() {
            let bytes = fs::read(candidate)?;
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
    }
    Ok(format!("b3:{}", hasher.finalize().to_hex()))
}

fn digest_file(path: &Path) -> Result<String, std::io::Error> {
    Ok(format!("b3:{}", blake3::hash(&fs::read(path)?).to_hex()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        Digest, EvidenceFailurePolicy, NeedCacheEntry, NeedCacheIdentity, NeedKey, NeedResult,
        WorkerArtifactResult, WorkerObservationTrace, WorkerOutcome,
    };
    use needle_runtime::RuntimeSettings;
    use std::collections::BTreeMap;

    #[test]
    fn legacy_inputs_fail_closed_without_becoming_false_positive_proofs() {
        let root = std::env::temp_dir().join(format!(
            "needle-shadow-replay-{}",
            Digest::blake3(format!("{:?}", std::time::Instant::now())).to_hex()
        ));
        let product_data = root.join("source-data");
        let repository = root.join("repo");
        fs::create_dir_all(&product_data).unwrap();
        fs::create_dir_all(&repository).unwrap();
        fs::write(repository.join("lib.rs"), "fn answer() {}\n").unwrap();
        let store = RuntimeStore::new(product_data.join("needle.sqlite3"));
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
        let identity = NeedCacheIdentity {
            repository_id: Digest::blake3(b"repo"),
            source_snapshot_digest: Digest::blake3(b"source"),
            prompt_profile_digest: Digest::blake3(b"profile"),
            route_definition_digest: Digest::blake3(b"route"),
            preset_definition_digest: Digest::blake3(b"preset"),
            need_key: NeedKey::new("locate.implementation").unwrap(),
            normalized_request_digest: Digest::blake3(b"request"),
            worker_configuration_digest: Digest::blake3(b"worker"),
            output_schema_digest: Digest::blake3(b"schema"),
        };
        let result = NeedResult {
            complete: true,
            summary: "legacy".to_owned(),
            claims: Vec::new(),
            evidence: Vec::new(),
            suggested_reads: Vec::new(),
            suggested_commands: Vec::new(),
            uncertainty: Vec::new(),
        };
        let entry = NeedCacheEntry {
            identity: identity.clone(),
            result: result.clone(),
            worker_outcome: WorkerOutcome {
                result,
                artifact_result: Some(WorkerArtifactResult {
                    schema_id: "needle.artifact-result/1".to_owned(),
                    artifacts: Vec::new(),
                    test_plan: None,
                    observation_trace: WorkerObservationTrace {
                        observed_files: Vec::new(),
                        gaps: vec!["unknown_command_action".to_owned()],
                    },
                    artifact_traces: BTreeMap::new(),
                }),
                semantic_artifact_result: None,
                worker_model: "test".to_owned(),
                worker_reasoning: "low".to_owned(),
                codex_version: "0.144.0".to_owned(),
                input_tokens: None,
                cached_input_tokens: None,
                output_tokens: None,
                duration_ms: 1,
                process_status: "success".to_owned(),
                logical_worker_spawns: 1,
                worker_turns: 1,
                repair_performed: false,
                discarded_facts: 0,
                worker_session_id: None,
                session_cleanup_success: Some(true),
            },
            created_unix_ms: 1,
            hit_count: 0,
        };
        store.publish(&entry).unwrap();
        fs::write(root.join("report.json"), "{}").unwrap();

        let report = run_shadow_replay(
            &[ShadowReplaySource {
                run_id: "legacy".to_owned(),
                route: "locate.implementation".to_owned(),
                product_data,
                repository_root: repository,
                report_path: root.join("report.json"),
                recorded_cases: 1,
            }],
            &root.join("scratch"),
            HistoricalCostEvidence {
                main_microcredits: 1,
                worker_microcredits: 1,
                repair_microcredits: None,
                escalation_microcredits: None,
                total_microcredits: 2,
                pricing_snapshot_digest: Digest::blake3(b"pricing").to_string(),
            },
        )
        .unwrap();
        assert_eq!(report.selected_proofs, 0);
        assert_eq!(report.false_positives, 0);
        assert!(report.proof_precision.is_none());
        assert_eq!(report.opportunity_rate, 0.0);
        assert!(report.safety_gate_passed);
        assert!(!report.usefulness_gate_passed);
        assert!(!report.live_run_ready);
        let _ = fs::remove_dir_all(root);
    }
}
