use crate::{RIPGREP_CALIBRATION_SHA, RIPGREP_CALIBRATION_SUBJECT};
use needle_core::{
    ArtifactRequest, CacheResolution, CacheScope, CapabilityMode, Digest, EvidenceFailurePolicy,
    Need, NeedFragment, NeedIr, NeedKey, PredicateKind, ReuseUnit, SemanticArtifactResult,
    built_in_route_contracts, compile_need, need_fragment,
};
use needle_runtime::{
    NeedShadowWrite, RouteCostObservation, RuntimeSettings, RuntimeStore, SemanticCostEstimates,
    SemanticResolver, SemanticReuseDecision, capture_git_snapshot,
    validate_semantic_artifact_with_trace,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const ARTIFACT_CACHE_REPLAY_SCHEMA_ID: &str = "needle.artifact-cache-replay/2";
pub const RECORDED_SOURCE_DIGEST: &str =
    "b3:994f4829c87bf118d2686ae8eaf316d513588acd03348d3d0a40da19f98177c5";

const RECORDED_RESULT: &str =
    include_str!("../../../benchmarks/fixtures/worker-code-location-artifact-result.json");
const RECORDED_EVIDENCE: [(&str, &str); 3] = [
    (
        "crates/core/flags/defs.rs",
        "b3:a3f606bb330073d209e08a8109f35ab80bdff3d41880318226eb6082fd6db345",
    ),
    (
        "crates/core/flags/hiargs.rs",
        "b3:26622141076625b0d6bf6119a0a429141f62d5042864532ad7cd76f3fe284441",
    ),
    ("tests/misc.rs", "b3:23db27d82d5076a892c42b644ef5a878d05e11ae2aa07e4418b0b884ca2c19b8"),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactCacheReplayCase {
    pub name: String,
    pub expected_resolution: String,
    pub observed_resolution: String,
    pub authoritative: bool,
    pub selected_artifacts: Vec<String>,
    pub sufficiency_certificate: Option<String>,
    pub stale_candidates: u32,
    pub logical_workers: u32,
    pub passed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactCacheReplayReport {
    pub schema_id: String,
    pub mode: String,
    pub provider_calls: u32,
    pub logical_workers: u32,
    pub repository_sha: String,
    pub snapshot_identity_revision: u16,
    pub recorded_snapshot_identity_revision: u16,
    pub source_snapshot_digest: String,
    pub recorded_source_snapshot_digest: String,
    pub recorded_source_digest_matches: bool,
    pub recorded_evidence_digests_match: bool,
    pub snapshot_identity_note: String,
    pub worker_schema: String,
    pub worker_result_fixture_digest: String,
    pub validator_revision: u32,
    pub validated_artifacts: u32,
    pub rejected_artifacts: u32,
    pub admitted_scope: CacheScope,
    pub artifact_id: String,
    pub validation_certificate_id: String,
    pub cases: Vec<ArtifactCacheReplayCase>,
    pub authoritative_hits: u32,
    pub workers_avoided: u32,
    pub passed: bool,
}

#[derive(Debug, Error)]
pub enum ArtifactCacheReplayError {
    #[error("artifact cache replay I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact cache replay snapshot failed: {0}")]
    Snapshot(#[from] needle_runtime::SnapshotError),
    #[error("artifact cache replay store failed: {0}")]
    Store(#[from] needle_runtime::StoreError),
    #[error("artifact cache replay validation failed: {0}")]
    Validation(#[from] needle_runtime::SemanticValidationError),
    #[error("artifact cache replay resolution failed: {0}")]
    Resolver(#[from] needle_runtime::SemanticResolverError),
    #[error("artifact cache replay JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("artifact cache replay input is invalid: {0}")]
    Invalid(String),
}

pub fn run_artifact_cache_replay(
    source_repository: &Path,
    artifact_root: &Path,
) -> Result<ArtifactCacheReplayReport, ArtifactCacheReplayError> {
    let (repository_root, snapshot) = capture_git_snapshot(source_repository)?;
    if snapshot.head_sha != RIPGREP_CALIBRATION_SHA {
        return Err(ArtifactCacheReplayError::Invalid(format!(
            "source HEAD is {}, expected {RIPGREP_CALIBRATION_SHA}",
            snapshot.head_sha
        )));
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&repository_root)
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()?;
    if !status.status.success() {
        return Err(ArtifactCacheReplayError::Invalid(
            "cannot inspect source repository status".to_owned(),
        ));
    }
    if !status.stdout.is_empty() {
        return Err(ArtifactCacheReplayError::Invalid("source repository is dirty".to_owned()));
    }
    if artifact_root.join("needle.sqlite3").exists() {
        return Err(ArtifactCacheReplayError::Invalid(
            "artifact root already contains a replay database".to_owned(),
        ));
    }
    let recorded_evidence_digests_match = RECORDED_EVIDENCE.iter().try_fold(
        true,
        |matches, (relative, expected)| -> Result<bool, std::io::Error> {
            let observed = Digest::blake3(fs::read(repository_root.join(relative))?).to_string();
            Ok(matches && observed == *expected)
        },
    )?;
    if !recorded_evidence_digests_match {
        return Err(ArtifactCacheReplayError::Invalid(
            "pinned checkout evidence differs from the recorded fixture bytes".to_owned(),
        ));
    }
    fs::create_dir_all(artifact_root)?;

    let ir = locate_need("Locate the option implementation.");
    let route = built_in_route_contracts()
        .into_iter()
        .find(|route| route.route.as_str() == "locate.implementation")
        .ok_or_else(|| ArtifactCacheReplayError::Invalid("missing locate route".to_owned()))?;
    let need = compile_need(&ir, snapshot.repository_id, &route)
        .map_err(|error| ArtifactCacheReplayError::Invalid(error.to_string()))?;
    let fragment = need_fragment(&need, need.required.clone(), Vec::new());

    let store = RuntimeStore::new(artifact_root.join("needle.sqlite3"));
    store.initialize()?;
    store.initialize_defaults(&RuntimeSettings {
        codex_executable: "codex".to_owned(),
        worker_model: "recorded-r35-fixture".to_owned(),
        worker_reasoning: "none".to_owned(),
        worker_timeout_seconds: 1,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        trusted_test_execution: false,
        multi_need_policy: needle_core::MultiNeedPolicy::default(),
    })?;
    record_need(&store, "r35-exact", &ir, &need, &fragment)?;

    let result: SemanticArtifactResult = serde_json::from_str(RECORDED_RESULT)?;
    if result.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID {
        return Err(ArtifactCacheReplayError::Invalid(
            "recorded fixture does not use artifact-result/2".to_owned(),
        ));
    }

    let request = semantic_request(
        &need,
        &fragment,
        snapshot.source_digest,
        "Locate the option implementation.",
    );
    let mut validated = Vec::new();
    let mut rejected_artifacts = 0_u32;
    for artifact in &result.artifacts {
        let kind = artifact.kind();
        let trace = result.artifact_traces.get(&kind).unwrap_or(&result.observation_trace);
        match validate_semantic_artifact_with_trace(
            &fragment,
            artifact,
            &repository_root,
            request.semantic_id().digest(),
            Some(trace),
        ) {
            Ok(artifact) => validated.push(artifact),
            Err(_) => rejected_artifacts = rejected_artifacts.saturating_add(1),
        }
    }
    if validated.len() != 1 {
        return Err(ArtifactCacheReplayError::Invalid(format!(
            "expected one certifiable implementation artifact, observed {}",
            validated.len()
        )));
    }
    let validated = validated.remove(0);
    if validated.artifact.contract.cache_scope != CacheScope::SnapshotExact {
        return Err(ArtifactCacheReplayError::Invalid(format!(
            "expected SnapshotExact, observed {:?}",
            validated.artifact.contract.cache_scope
        )));
    }
    store.publish_semantic_artifact(
        &request,
        &need,
        &validated.artifact,
        &validated.certificate,
    )?;

    let capability = store
        .capability_classes()?
        .into_iter()
        .find(|capability| {
            capability.reuse_unit == ReuseUnit::Artifact
                && capability.predicate == PredicateKind::ImplementationLocation
        })
        .ok_or_else(|| {
            ArtifactCacheReplayError::Invalid(
                "missing ImplementationLocation capability".to_owned(),
            )
        })?;
    store.set_capability_mode(
        &capability.id,
        capability.definition_digest,
        CapabilityMode::Authoritative,
        Some(Digest::blake3(b"r35-offline-cache-replay-evidence")),
    )?;
    for (source, cost) in [("fresh", 5_000_000), ("reuse", 100_000)] {
        store.record_route_cost_observation(&RouteCostObservation {
            route_key: "locate.implementation".to_owned(),
            cost_microusd: cost,
            source: source.to_owned(),
            evidence_digest: Digest::blake3(format!("r35-cache-replay-{source}")),
            observed_unix_ms: now_ms(),
        })?;
    }

    let resolver = SemanticResolver::new(store);
    let costs = SemanticCostEstimates {
        fresh_microusd: Some(5_000_000),
        artifact_reuse_microusd: Some(100_000),
        claim_reuse_microusd: None,
        claim_partial_reuse_microusd: None,
    };
    let exact = observe(
        "same-wording",
        "ExactHit",
        resolver.resolve_for_route(
            &need,
            &route.route,
            &repository_root,
            snapshot.source_digest,
            costs,
            &[request.semantic_id().digest()],
        )?,
    );

    let reworded_ir = locate_need("Find the code that implements this CLI option.");
    let reworded = compile_need(&reworded_ir, snapshot.repository_id, &route)
        .map_err(|error| ArtifactCacheReplayError::Invalid(error.to_string()))?;
    if reworded.id != need.id {
        return Err(ArtifactCacheReplayError::Invalid(
            "semantically equivalent wording changed the NeedId".to_owned(),
        ));
    }
    let coverage = observe(
        "reworded-same-need",
        "CoverageHit",
        resolver.resolve_for_route(
            &reworded,
            &route.route,
            &repository_root,
            snapshot.source_digest,
            costs,
            &[],
        )?,
    );
    let stale = observe(
        "different-source",
        "Stale",
        resolver.resolve_for_route(
            &need,
            &route.route,
            &repository_root,
            Digest::blake3(b"r35-different-source"),
            costs,
            &[],
        )?,
    );
    let cases = vec![exact, coverage, stale];
    let authoritative_hits =
        u32::try_from(cases.iter().filter(|case| case.authoritative).count()).unwrap_or(u32::MAX);
    let workers_avoided = authoritative_hits;
    let recorded_source_digest_matches =
        snapshot.source_digest.to_string() == RECORDED_SOURCE_DIGEST;
    let passed = recorded_evidence_digests_match
        && rejected_artifacts == 1
        && cases.iter().all(|case| case.passed)
        && authoritative_hits == 2
        && workers_avoided == 2;

    Ok(ArtifactCacheReplayReport {
        schema_id: ARTIFACT_CACHE_REPLAY_SCHEMA_ID.to_owned(),
        mode: "deterministic-offline-isolated-authority".to_owned(),
        provider_calls: 0,
        logical_workers: 0,
        repository_sha: RIPGREP_CALIBRATION_SHA.to_owned(),
        snapshot_identity_revision: snapshot.identity_revision,
        recorded_snapshot_identity_revision:
            needle_core::LEGACY_REPOSITORY_SNAPSHOT_IDENTITY_REVISION,
        source_snapshot_digest: snapshot.source_digest.to_string(),
        recorded_source_snapshot_digest: RECORDED_SOURCE_DIGEST.to_owned(),
        recorded_source_digest_matches,
        recorded_evidence_digests_match,
        snapshot_identity_note: "the recorded fixture digest uses legacy path-scoped identity revision 1 and is not comparable with lineage-based revision 2; new revision-2 snapshots are stable across equivalent clones and worktrees"
            .to_owned(),
        worker_schema: result.schema_id,
        worker_result_fixture_digest: Digest::blake3(RECORDED_RESULT.as_bytes()).to_string(),
        validator_revision: needle_runtime::SEMANTIC_VALIDATOR_REVISION,
        validated_artifacts: 1,
        rejected_artifacts,
        admitted_scope: validated.artifact.contract.cache_scope,
        artifact_id: validated.semantic_id.to_string(),
        validation_certificate_id: validated.certificate.id.to_string(),
        cases,
        authoritative_hits,
        workers_avoided,
        passed,
    })
}

fn locate_need(body: &str) -> NeedIr {
    NeedIr::parse(&format!(
        "@@need\n\
         @route locate.implementation\n\
         @subject cli-option:\"{RIPGREP_CALIBRATION_SUBJECT}\"\n\
         @require implementation-location granularity=exact-location polarity=positive selection=primary\n\
         @world source=current features=default\n\
         \n\
         {body}\n\
         @@end"
    ))
    .expect("static cache replay marker is valid")
    .expect("static cache replay marker is present")
}

fn semantic_request(
    need: &Need,
    fragment: &NeedFragment,
    source_snapshot_digest: Digest,
    wording: &str,
) -> ArtifactRequest {
    ArtifactRequest {
        contract_id: "needle.semantic.code-location".to_owned(),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest,
        route_key: NeedKey::new("locate.implementation").expect("static route is valid"),
        normalized_request: wording.to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    }
}

fn record_need(
    store: &RuntimeStore,
    prefix: &str,
    ir: &NeedIr,
    need: &Need,
    fragment: &NeedFragment,
) -> Result<(), needle_runtime::StoreError> {
    store.record_need_shadow(NeedShadowWrite {
        session_id: prefix,
        turn_id: "turn",
        transport_digest: Digest::blake3(format!("{prefix}-transport")),
        parser_definition_digest: Digest::blake3(b"needle.need-ir-parser/allocation-free/1"),
        prompt_profile_digest: Digest::blake3(b"r35-cache-replay-profile"),
        need_ir: ir,
        need,
        fragments: std::slice::from_ref(fragment),
    })
}

fn observe(
    name: &str,
    expected_resolution: &str,
    decision: SemanticReuseDecision,
) -> ArtifactCacheReplayCase {
    let observed_resolution = resolution_name(&decision.resolution).to_owned();
    let selected_artifacts =
        decision.artifacts.iter().map(|artifact| artifact.id.to_string()).collect::<Vec<_>>();
    let stale_candidates = u32::try_from(decision.stale_candidates).unwrap_or(u32::MAX);
    let logical_workers = 0;
    let passed = observed_resolution == expected_resolution
        && logical_workers == 0
        && match expected_resolution {
            "ExactHit" | "CoverageHit" => {
                decision.authoritative
                    && selected_artifacts.len() == 1
                    && decision.certificate.is_some()
                    && stale_candidates == 0
            }
            "Stale" => {
                !decision.authoritative
                    && selected_artifacts.is_empty()
                    && decision.certificate.is_none()
                    && stale_candidates == 1
            }
            _ => false,
        };
    ArtifactCacheReplayCase {
        name: name.to_owned(),
        expected_resolution: expected_resolution.to_owned(),
        observed_resolution,
        authoritative: decision.authoritative,
        selected_artifacts,
        sufficiency_certificate: decision
            .certificate
            .as_ref()
            .map(|certificate| certificate.id.to_string()),
        stale_candidates,
        logical_workers,
        passed,
    }
}

fn resolution_name(resolution: &CacheResolution) -> &'static str {
    match resolution {
        CacheResolution::ExactHit { .. } => "ExactHit",
        CacheResolution::CoverageHit { .. } => "CoverageHit",
        CacheResolution::CompositeHit { .. } => "CompositeHit",
        CacheResolution::ClaimHit { .. } => "ClaimHit",
        CacheResolution::ClaimCompositeHit { .. } => "ClaimCompositeHit",
        CacheResolution::PartialHit { .. } => "PartialHit",
        CacheResolution::Miss => "Miss",
        CacheResolution::Stale { .. } => "Stale",
        CacheResolution::Rejected { .. } => "Rejected",
        CacheResolution::Bypass { .. } => "Bypass",
        CacheResolution::Ambiguous { .. } => "Ambiguous",
        CacheResolution::Contradicted { .. } => "Contradicted",
    }
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
    use needle_core::LocationRole;

    #[test]
    fn recorded_fixture_preserves_the_live_worker_shape() {
        let result: SemanticArtifactResult = serde_json::from_str(RECORDED_RESULT).unwrap();
        assert_eq!(result.schema_id, needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID);
        assert_eq!(result.artifacts.len(), 2);
        assert_eq!(result.observation_trace.gaps, vec!["unknown_command_action"]);
        let needle_core::SemanticWorkerArtifact::CodeLocation { locations, gaps } =
            &result.artifacts[0]
        else {
            panic!("expected code-location");
        };
        assert!(gaps.is_empty());
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].role, LocationRole::Primary);
        assert_eq!(locations[0].path, "crates/core/flags/hiargs.rs");
        assert_eq!(locations[0].symbol.as_deref(), Some("globs"));
        assert!(locations[0].byte_start.is_none());
        assert!(locations[0].byte_end.is_none());
    }
}
