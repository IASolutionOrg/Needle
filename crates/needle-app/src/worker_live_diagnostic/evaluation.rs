use needle_bench::{QualityOracleResult, QualityOracleSpec};
use needle_core::{
    CacheScope, Digest, LocationRole, NeedFragment, SemanticArtifactResult, SemanticWorkerArtifact,
};
use needle_runtime::validate_semantic_artifact_with_trace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SemanticReuseAssessment {
    pub(super) ready: bool,
    trace_closure_complete: bool,
    primary_implementation_present: bool,
    certified_artifacts: usize,
    certified_primary_artifacts: usize,
    rejected_artifacts: usize,
    cache_scope: Option<CacheScope>,
    diagnostics: Vec<String>,
}

pub(super) fn evaluate_structured_worker_quality(
    oracle: &QualityOracleSpec,
    result: Option<&SemanticArtifactResult>,
    repository_root: &Path,
    implementation_paths: &BTreeSet<String>,
    focused_test_identifier: Option<&str>,
    focused_test_evidence_valid: bool,
) -> QualityOracleResult {
    let Some(result) = result else {
        return unavailable_quality();
    };
    let mut paths = BTreeSet::new();
    for artifact in &result.artifacts {
        match artifact {
            SemanticWorkerArtifact::CodeLocation { locations, .. } => {
                for location in locations {
                    paths.insert(location.path.replace('\\', "/"));
                }
            }
            SemanticWorkerArtifact::BehaviorTrace { steps, .. } => {
                for step in steps {
                    paths.insert(step.location.path.replace('\\', "/"));
                }
            }
            SemanticWorkerArtifact::TestPlan { evidence_paths, .. } => {
                paths.extend(evidence_paths.iter().map(|path| path.replace('\\', "/")));
            }
        }
    }

    let required_files_present = paths.iter().any(|path| implementation_paths.contains(path));
    let required_symbols_present = oracle.required_symbols.is_empty()
        || result.artifacts.iter().any(|artifact| {
            let SemanticWorkerArtifact::CodeLocation { locations, .. } = artifact else {
                return false;
            };
            locations.iter().any(|location| {
                implementation_paths.contains(&location.path.replace('\\', "/"))
                    && location.symbol.as_ref().is_some_and(|symbol| {
                        oracle.required_symbols.iter().any(|required| symbol.contains(required))
                    })
            })
        });
    let required_claims_present = oracle.required_claims.iter().all(|required| {
        paths
            .iter()
            .filter(|path| implementation_paths.contains(*path))
            .any(|path| evidence_file_contains(repository_root, path, required.as_bytes()))
    });
    let serialized = serde_json::to_string(result).unwrap_or_default();
    let forbidden_claims_absent =
        oracle.forbidden_claims.iter().all(|forbidden| !serialized.contains(forbidden));
    let focused_test_suggested = focused_test_evidence_valid
        && focused_test_identifier
            .is_some_and(|identifier| oracle.accepts_focused_test_identifier(identifier));
    let mut failures = Vec::new();
    for (passed, failure) in [
        (required_files_present, "required_files"),
        (required_symbols_present, "required_symbols"),
        (required_claims_present, "required_claims"),
        (forbidden_claims_absent, "forbidden_claims"),
        (focused_test_suggested, "focused_test"),
        (focused_test_evidence_valid, "evaluator_test"),
    ] {
        if !passed {
            failures.push(failure.to_owned());
        }
    }
    QualityOracleResult {
        passed: failures.is_empty(),
        required_files_present,
        required_symbols_present,
        required_claims_present,
        forbidden_claims_absent,
        focused_test_suggested,
        evaluator_test_passed: Some(focused_test_evidence_valid),
        failures,
    }
}

pub(super) fn assess_semantic_reuse(
    fragment: Option<&NeedFragment>,
    result: Option<&SemanticArtifactResult>,
    repository_root: &Path,
    origin_request_id: Digest,
    implementation_paths: &BTreeSet<String>,
) -> SemanticReuseAssessment {
    let (Some(fragment), Some(result)) = (fragment, result) else {
        return unavailable_semantic_reuse();
    };
    let mut trace_gaps = result.observation_trace.gaps.iter().cloned().collect::<BTreeSet<_>>();
    for trace in result.artifact_traces.values() {
        trace_gaps.extend(trace.gaps.iter().cloned());
    }
    let trace_closure_complete = trace_gaps.is_empty();
    let primary_implementation_present = result.artifacts.iter().any(|artifact| {
        let SemanticWorkerArtifact::CodeLocation { locations, .. } = artifact else {
            return false;
        };
        locations.iter().any(|location| {
            location.role == LocationRole::Primary
                && implementation_paths.contains(&location.path.replace('\\', "/"))
        })
    });

    let mut certified_artifacts = 0;
    let mut certified_primary_artifacts = 0;
    let mut rejected_artifacts = 0;
    let mut cache_scope = None;
    let mut diagnostics =
        trace_gaps.into_iter().map(|gap| format!("observation_trace:{gap}")).collect::<Vec<_>>();
    for (index, artifact) in result.artifacts.iter().enumerate() {
        let kind = artifact.kind();
        let trace = result.artifact_traces.get(&kind).unwrap_or(&result.observation_trace);
        match validate_semantic_artifact_with_trace(
            fragment,
            artifact,
            repository_root,
            origin_request_id,
            Some(trace),
        ) {
            Ok(validated) => {
                certified_artifacts += 1;
                cache_scope = Some(match (cache_scope, validated.artifact.contract.cache_scope) {
                    (Some(CacheScope::SnapshotExact), _) | (_, CacheScope::SnapshotExact) => {
                        CacheScope::SnapshotExact
                    }
                    _ => CacheScope::WorktreeSemantic,
                });
                if artifact_has_primary_implementation(artifact, implementation_paths) {
                    certified_primary_artifacts += 1;
                }
            }
            Err(error) => {
                rejected_artifacts += 1;
                diagnostics.push(format!("artifact[{index}]:{error}"));
            }
        }
    }
    if !primary_implementation_present {
        diagnostics.push("primary_implementation_missing".to_owned());
    }
    if certified_primary_artifacts == 0 {
        diagnostics.push("no_certified_primary_artifact".to_owned());
    }
    SemanticReuseAssessment {
        ready: primary_implementation_present && certified_primary_artifacts > 0,
        trace_closure_complete,
        primary_implementation_present,
        certified_artifacts,
        certified_primary_artifacts,
        rejected_artifacts,
        cache_scope,
        diagnostics,
    }
}

pub(super) fn unavailable_quality() -> QualityOracleResult {
    QualityOracleResult {
        passed: false,
        required_files_present: false,
        required_symbols_present: false,
        required_claims_present: false,
        forbidden_claims_absent: false,
        focused_test_suggested: false,
        evaluator_test_passed: None,
        failures: vec!["not_executed".to_owned()],
    }
}

pub(super) fn unavailable_semantic_reuse() -> SemanticReuseAssessment {
    SemanticReuseAssessment {
        ready: false,
        trace_closure_complete: false,
        primary_implementation_present: false,
        certified_artifacts: 0,
        certified_primary_artifacts: 0,
        rejected_artifacts: 0,
        cache_scope: None,
        diagnostics: vec!["not_executed".to_owned()],
    }
}

fn artifact_has_primary_implementation(
    artifact: &SemanticWorkerArtifact,
    implementation_paths: &BTreeSet<String>,
) -> bool {
    let SemanticWorkerArtifact::CodeLocation { locations, .. } = artifact else {
        return false;
    };
    locations.iter().any(|location| {
        location.role == LocationRole::Primary
            && implementation_paths.contains(&location.path.replace('\\', "/"))
    })
}

fn evidence_file_contains(repository_root: &Path, relative: &str, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let Ok(repository_root) = fs::canonicalize(repository_root) else {
        return false;
    };
    let Ok(path) = fs::canonicalize(repository_root.join(relative)) else {
        return false;
    };
    if !path.starts_with(&repository_root) {
        return false;
    }
    fs::read(path).is_ok_and(|content| content.windows(needle.len()).any(|window| window == needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        NeedIr, SemanticLocation, WorkerObservationTrace, built_in_route_contracts, compile_need,
        need_fragment,
    };
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    const MARKER: &str = r#"@@need
@route locate.implementation
@subject cli-option:"--glob-case-insensitive"
@require implementation-location granularity=exact-location polarity=positive selection=primary
@world source=current features=default

Locate the implementation.
@@end"#;

    fn r35_semantic_result() -> SemanticArtifactResult {
        SemanticArtifactResult {
            schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: vec![
                SemanticWorkerArtifact::CodeLocation {
                    locations: vec![
                        SemanticLocation {
                            role: LocationRole::Primary,
                            path: "crates/core/flags/hiargs.rs".to_owned(),
                            symbol: Some("globs".to_owned()),
                            byte_start: None,
                            byte_end: None,
                        },
                        SemanticLocation {
                            role: LocationRole::Supporting,
                            path: "crates/core/flags/defs.rs".to_owned(),
                            symbol: Some("GlobCaseInsensitive::update".to_owned()),
                            byte_start: None,
                            byte_end: None,
                        },
                    ],
                    gaps: Vec::new(),
                },
                SemanticWorkerArtifact::CodeLocation {
                    locations: vec![SemanticLocation {
                        role: LocationRole::Primary,
                        path: "tests/misc.rs".to_owned(),
                        symbol: Some("glob_always_case_insensitive (line 358)".to_owned()),
                        byte_start: None,
                        byte_end: None,
                    }],
                    gaps: Vec::new(),
                },
            ],
            observation_trace: WorkerObservationTrace {
                observed_files: Vec::new(),
                gaps: vec!["unknown_command_action".to_owned()],
            },
            artifact_traces: Default::default(),
        }
    }

    fn r35_repository() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "needle-worker-r35-quality-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(root.join("crates/core/flags")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("crates/core/flags/defs.rs"),
            "/// --glob-case-insensitive\nstruct GlobCaseInsensitive;\n\
             fn update() { args.glob_case_insensitive = true; }\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/core/flags/hiargs.rs"),
            "// --glob-case-insensitive\nfn globs() { builder.case_insensitive(true); }\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/misc.rs"),
            "fn glob_always_case_insensitive() { /* --glob-case-insensitive */ }\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn structured_quality_uses_selected_evidence_instead_of_duplicate_claim_text() {
        let root = r35_repository();
        let oracle = QualityOracleSpec {
            required_files: vec![
                "crates/core/flags/defs.rs".to_owned(),
                "crates/core/flags/hiargs.rs".to_owned(),
                "tests/misc.rs".to_owned(),
            ],
            required_symbols: vec![
                "GlobCaseInsensitive".to_owned(),
                "globs".to_owned(),
                "glob_always_case_insensitive".to_owned(),
            ],
            required_claims: vec![
                "--glob-case-insensitive".to_owned(),
                "case_insensitive".to_owned(),
            ],
            forbidden_claims: vec!["wrong-test-command".to_owned()],
            focused_test_command: "misc::glob_always_case_insensitive".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: true,
        };
        let quality = evaluate_structured_worker_quality(
            &oracle,
            Some(&r35_semantic_result()),
            &root,
            &["crates/core/flags/defs.rs".to_owned(), "crates/core/flags/hiargs.rs".to_owned()]
                .into_iter()
                .collect(),
            Some("misc::glob_always_case_insensitive"),
            true,
        );
        assert!(quality.passed, "{:?}", quality.failures);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_quality_does_not_require_the_complete_state_flow() {
        let root = r35_repository();
        let mut result = r35_semantic_result();
        result.artifacts.truncate(1);
        if let SemanticWorkerArtifact::CodeLocation { locations, .. } = &mut result.artifacts[0] {
            locations.retain(|location| location.path == "crates/core/flags/hiargs.rs");
        }
        let oracle = QualityOracleSpec {
            required_files: vec![
                "crates/core/flags/defs.rs".to_owned(),
                "crates/core/flags/hiargs.rs".to_owned(),
                "tests/misc.rs".to_owned(),
            ],
            required_symbols: vec!["GlobCaseInsensitive".to_owned(), "globs".to_owned()],
            required_claims: vec![
                "--glob-case-insensitive".to_owned(),
                "case_insensitive".to_owned(),
            ],
            forbidden_claims: Vec::new(),
            focused_test_command: "misc::glob_always_case_insensitive".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: true,
        };
        let implementation_paths =
            ["crates/core/flags/defs.rs".to_owned(), "crates/core/flags/hiargs.rs".to_owned()]
                .into_iter()
                .collect();
        let quality = evaluate_structured_worker_quality(
            &oracle,
            Some(&result),
            &root,
            &implementation_paths,
            Some("misc::glob_always_case_insensitive"),
            true,
        );
        assert!(quality.passed, "{:?}", quality.failures);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn structured_quality_accepts_an_explicit_test_alternative_with_no_symbol_anchor() {
        let root = r35_repository();
        let oracle = QualityOracleSpec {
            required_files: vec!["crates/core/flags/hiargs.rs".to_owned()],
            required_symbols: Vec::new(),
            required_claims: vec!["case_insensitive".to_owned()],
            forbidden_claims: Vec::new(),
            focused_test_command: "feature::canonical_test".to_owned(),
            accepted_focused_test_identifiers: vec![
                "misc::glob_always_case_insensitive".to_owned(),
            ],
            focused_test_required: true,
        };
        let implementation_paths = ["crates/core/flags/hiargs.rs".to_owned()].into_iter().collect();
        let quality = evaluate_structured_worker_quality(
            &oracle,
            Some(&r35_semantic_result()),
            &root,
            &implementation_paths,
            Some("misc::glob_always_case_insensitive"),
            true,
        );
        assert!(quality.passed, "{:?}", quality.failures);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn r35_shape_is_snapshot_exact_reuse_ready() {
        let root = r35_repository();
        let ir = NeedIr::parse(MARKER).unwrap().unwrap();
        let contract = built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == "locate.implementation")
            .unwrap();
        let repository_id = Digest::blake3(b"r35-repository");
        let need = compile_need(&ir, repository_id, &contract).unwrap();
        let fragment = need_fragment(&need, need.required.clone(), Vec::new());
        let implementation_paths =
            ["crates/core/flags/defs.rs".to_owned(), "crates/core/flags/hiargs.rs".to_owned()]
                .into_iter()
                .collect();
        let assessment = assess_semantic_reuse(
            Some(&fragment),
            Some(&r35_semantic_result()),
            &root,
            Digest::blake3(b"r35-request"),
            &implementation_paths,
        );
        assert!(assessment.primary_implementation_present);
        assert!(!assessment.trace_closure_complete);
        assert_eq!(assessment.certified_artifacts, 1);
        assert_eq!(assessment.certified_primary_artifacts, 1);
        assert_eq!(assessment.rejected_artifacts, 1);
        assert_eq!(assessment.cache_scope, Some(CacheScope::SnapshotExact));
        assert!(assessment.ready);
        assert!(
            assessment
                .diagnostics
                .iter()
                .any(|failure| failure == "observation_trace:unknown_command_action")
        );
        assert!(assessment.diagnostics.iter().any(|failure| failure.starts_with("artifact[1]:")));
        let _ = fs::remove_dir_all(root);
    }
}
