use super::{
    SemanticValidationError, contains_bytes, is_safe_relative_path, read_evidence_from_root,
};
use needle_core::{Digest, SemanticWorkerArtifact, TestCommand};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

struct TestPlanProposal<'a> {
    argv: &'a [String],
    cwd_relative: &'a str,
    identifier: &'a str,
    evidence_paths: &'a [String],
}

pub(super) struct TestPlanProofFile {
    pub(super) path: String,
    bytes: Vec<u8>,
    pub(super) content_digest: Digest,
}

pub(super) struct TestPlanProof {
    pub(super) files: Vec<TestPlanProofFile>,
    pub(super) identifier_names_subject: bool,
    pub(super) subject_observed: bool,
    pub(super) identifier_observed: bool,
}

fn validate_test_plan_proposal(
    worker_artifact: &SemanticWorkerArtifact,
) -> Result<Option<TestPlanProposal<'_>>, SemanticValidationError> {
    let SemanticWorkerArtifact::TestPlan {
        runner,
        argv,
        cwd_relative,
        identifiers,
        selection,
        evidence_paths,
    } = worker_artifact
    else {
        return Ok(None);
    };
    let valid_paths = !evidence_paths.is_empty()
        && evidence_paths.len() <= 8
        && evidence_paths
            .iter()
            .all(|value| !value.is_empty() && value.len() <= 512 && is_safe_relative_path(value));
    let identifier = identifiers.first().map_or("", String::as_str);
    let command_violations =
        TestCommand::from_canonical_parts(runner, argv, identifier).err().unwrap_or_default();
    let mut rejection_reasons =
        command_violations.iter().map(|violation| violation.code()).collect::<Vec<_>>();
    if identifiers.len() != 1 {
        rejection_reasons.push("identifier_count_not_one");
    }
    if !valid_paths {
        rejection_reasons.push("bounds_or_paths");
    }
    if selection != "representative" {
        rejection_reasons.push("selection_not_representative");
    }
    if !is_safe_relative_path(cwd_relative) {
        rejection_reasons.push("cwd_not_safe_relative");
    }
    rejection_reasons.sort_unstable();
    rejection_reasons.dedup();
    if !rejection_reasons.is_empty() {
        return Err(SemanticValidationError::Evidence(format!(
            "worker-discovered test plan rejected: {}",
            rejection_reasons.join(",")
        )));
    }
    Ok(Some(TestPlanProposal { argv, cwd_relative, identifier, evidence_paths }))
}

pub(super) fn validate_test_plan_proof(
    worker_artifact: &SemanticWorkerArtifact,
    repository_root: &Path,
    subject: &needle_core::Subject,
    require_declared_target: bool,
) -> Result<Option<TestPlanProof>, SemanticValidationError> {
    let Some(proposal) = validate_test_plan_proposal(worker_artifact)? else {
        return Ok(None);
    };
    let root = fs::canonicalize(repository_root).map_err(|error| {
        SemanticValidationError::Evidence(format!("cannot resolve repository: {error}"))
    })?;
    let mut files = Vec::with_capacity(proposal.evidence_paths.len());
    for path in proposal.evidence_paths.iter().cloned().collect::<BTreeSet<_>>() {
        let bytes = read_evidence_from_root(&root, &path)?;
        files.push(TestPlanProofFile { path, content_digest: Digest::blake3(&bytes), bytes });
    }

    let canonical_subject = subject.canonical_name.trim_start_matches("--");
    let identifier_names_subject = proposal.identifier == subject.canonical_name
        || (!canonical_subject.is_empty() && proposal.identifier.contains(canonical_subject));
    let identifier_leaf =
        proposal.identifier.rsplit("::").next().unwrap_or(proposal.identifier).as_bytes();
    let subject_observed =
        files.iter().any(|file| contains_bytes(&file.bytes, subject.canonical_name.as_bytes()));
    let identifier_observed = files.iter().any(|file| contains_bytes(&file.bytes, identifier_leaf));
    if !identifier_observed {
        return Err(SemanticValidationError::Evidence(
            "worker-discovered test identifier is absent from declared evidence".to_owned(),
        ));
    }
    if !identifier_names_subject && !subject_observed {
        return Err(SemanticValidationError::Evidence(
            "worker-discovered test does not match the requested subject".to_owned(),
        ));
    }
    if require_declared_target {
        close_test_target_from_declared_manifest(
            &root,
            proposal.argv,
            proposal.cwd_relative,
            &mut files,
        )?;
    }
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));

    Ok(Some(TestPlanProof {
        files,
        identifier_names_subject,
        subject_observed,
        identifier_observed,
    }))
}

fn close_test_target_from_declared_manifest(
    repository_root: &Path,
    argv: &[String],
    cwd_relative: &str,
    files: &mut Vec<TestPlanProofFile>,
) -> Result<(), SemanticValidationError> {
    let Some(target) = argv.windows(2).find_map(|pair| (pair[0] == "--test").then_some(&pair[1]))
    else {
        return Ok(());
    };
    let conventional_paths = [
        join_relative(cwd_relative, &format!("tests/{target}.rs")),
        join_relative(cwd_relative, &format!("tests/{target}/main.rs")),
    ];
    let has_conventional_path = conventional_paths
        .iter()
        .flatten()
        .any(|expected| files.iter().any(|file| normalized_path(&file.path) == *expected));
    let mut target_paths = BTreeSet::new();
    let mut conventional_target_supported = false;
    for manifest in files.iter().filter(|file| file.path.ends_with("Cargo.toml")) {
        let Ok(text) = std::str::from_utf8(&manifest.bytes) else {
            continue;
        };
        let Ok(document) = toml::from_str::<toml::Value>(text) else {
            continue;
        };
        let explicit_target =
            document.get("test").and_then(toml::Value::as_array).and_then(|targets| {
                targets.iter().find(|candidate| {
                    candidate.get("name").and_then(toml::Value::as_str) == Some(target.as_str())
                })
            });
        if let Some(target_definition) = explicit_target {
            let Some(path) = target_definition.get("path").and_then(toml::Value::as_str) else {
                conventional_target_supported |= has_conventional_path;
                continue;
            };
            let manifest_parent =
                Path::new(&manifest.path).parent().unwrap_or_else(|| Path::new(""));
            let Some(expected) = normalize_relative_path(&manifest_parent.join(path)) else {
                return Err(SemanticValidationError::Evidence(
                    "worker-discovered cargo test target escapes the repository".to_owned(),
                ));
            };
            target_paths.insert(expected);
            continue;
        }
        let autotests_enabled = document
            .get("package")
            .and_then(|package| package.get("autotests"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        conventional_target_supported |= autotests_enabled && has_conventional_path;
    }

    if target_paths.is_empty() {
        return conventional_target_supported.then_some(()).ok_or_else(|| {
            SemanticValidationError::Evidence(
                "worker-discovered cargo test target is absent from declared evidence".to_owned(),
            )
        });
    }
    if target_paths.len() != 1 {
        return Err(SemanticValidationError::Evidence(
            "worker-discovered cargo test target is ambiguous across declared manifests".to_owned(),
        ));
    }
    let target_path = target_paths.into_iter().next().expect("one target path checked");
    if files.iter().any(|file| normalized_path(&file.path) == target_path) {
        return Ok(());
    }
    let bytes = read_evidence_from_root(repository_root, &target_path).map_err(|error| {
        SemanticValidationError::Evidence(format!(
            "worker-discovered cargo test target cannot be closed from its declared manifest: {error}"
        ))
    })?;
    files.push(TestPlanProofFile {
        path: target_path,
        content_digest: Digest::blake3(&bytes),
        bytes,
    });
    Ok(())
}

fn join_relative(base: &str, child: &str) -> Option<String> {
    normalize_relative_path(&Path::new(base).join(child))
}

fn normalized_path(path: &str) -> String {
    normalize_relative_path(Path::new(path)).unwrap_or_else(|| path.replace('\\', "/"))
}

fn normalize_relative_path(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_str()?.to_owned()),
            Component::ParentDir => {
                components.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(components.join("/"))
}
