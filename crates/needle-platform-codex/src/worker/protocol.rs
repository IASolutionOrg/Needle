use needle_core::{
    ArtifactKind, Claim, Digest, EvidenceFailurePolicy, EvidenceReference, LocationRole,
    NeedResult, SemanticLocation, SemanticWorkerArtifact, TestCommand, TestCommandViolation,
    TestPlan, WorkerArtifact,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};
use std::process::Command;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompactWorkerResponse {
    schema: String,
    artifacts: Vec<CompactGroup>,
    test_plan: Option<CompactTestPlan>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SemanticCompactWorkerResponse {
    schema: String,
    artifacts: Vec<SemanticWorkerArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactGroup {
    kind: ArtifactOutputKind,
    #[serde(alias = "p")]
    path: String,
    #[serde(alias = "s")]
    symbol: Option<String>,
    #[serde(alias = "f")]
    facts: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "kebab-case")]
enum ArtifactOutputKind {
    CodeLocation,
    BehaviorTrace,
    TestPlan,
}

impl ArtifactOutputKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::CodeLocation => "code-location",
            Self::BehaviorTrace => "behavior-trace",
            Self::TestPlan => "test-plan",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactTestPlan {
    runner: String,
    argv: Vec<String>,
    identifier: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyCompactWorkerResponse {
    g: Vec<LegacyCompactGroup>,
    c: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCompactGroup {
    p: String,
    s: Option<String>,
    f: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct GroupKey {
    kind: ArtifactOutputKind,
    path: String,
    symbol: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GroupDiagnostic {
    pub(super) index: Option<usize>,
    pub(super) code: String,
}

#[derive(Default)]
pub(super) struct NormalizedResponse {
    groups: BTreeMap<GroupKey, BTreeSet<String>>,
    commands: BTreeSet<String>,
    source_schema: Option<String>,
    test_plan: Option<TestPlan>,
    semantic_artifacts: Vec<SemanticWorkerArtifact>,
    pub(super) diagnostics: Vec<GroupDiagnostic>,
    pub(super) discarded_facts: u32,
}

impl NormalizedResponse {
    pub(super) fn schema_failure(error: String) -> Self {
        Self {
            diagnostics: vec![GroupDiagnostic {
                index: None,
                code: format!("schema_invalid:{}", compact_code(&error)),
            }],
            ..Self::default()
        }
    }

    pub(super) fn has_facts(&self) -> bool {
        self.groups.values().any(|facts| !facts.is_empty()) || self.test_plan.is_some()
    }

    pub(super) fn record_missing_requested_kinds(&mut self, requested: &[ArtifactKind]) {
        for kind in requested {
            if self.has_requested_kind(kind) {
                continue;
            }
            let code = format!("missing_requested_kind_{}", kind.0.replace('-', "_"));
            if !self.diagnostics.iter().any(|diagnostic| diagnostic.code == code) {
                self.diagnostics.push(GroupDiagnostic { index: None, code });
            }
        }
    }

    pub(super) fn discard_unrequested_kinds(&mut self, requested: &[ArtifactKind]) {
        let requested_names = requested.iter().map(|kind| kind.0.as_str()).collect::<BTreeSet<_>>();
        let mut discarded = self
            .semantic_artifacts
            .iter()
            .map(SemanticWorkerArtifact::kind)
            .filter(|kind| !requested_names.contains(kind.0.as_str()))
            .map(|kind| kind.0)
            .collect::<BTreeSet<_>>();
        for key in self.groups.keys() {
            if !requested_names.contains(key.kind.as_str()) {
                discarded.insert(key.kind.as_str().to_owned());
            }
        }
        if self.test_plan.is_some()
            && !requested_names.contains(ArtifactKind::test_plan().0.as_str())
        {
            discarded.insert(ArtifactKind::test_plan().0);
        }

        self.semantic_artifacts
            .retain(|artifact| requested_names.contains(artifact.kind().0.as_str()));
        self.groups.retain(|key, _| requested_names.contains(key.kind.as_str()));
        if !requested_names.contains(ArtifactKind::test_plan().0.as_str()) {
            self.test_plan = None;
        }
        for kind in discarded {
            self.record_unrequested_kind(&kind);
        }
    }

    pub(super) fn record_unrequested_kind(&mut self, kind: &str) {
        let code = format!("unrequested_artifact_discarded:{}", diagnostic_atom(kind, 64));
        if !self.diagnostics.iter().any(|diagnostic| diagnostic.code == code) {
            self.diagnostics.push(GroupDiagnostic { index: None, code });
        }
    }

    pub(super) fn missing_requested_kinds(&self, requested: &[ArtifactKind]) -> Vec<String> {
        requested
            .iter()
            .filter(|kind| !self.has_requested_kind(kind))
            .map(|kind| kind.0.clone())
            .collect()
    }

    fn has_requested_kind(&self, kind: &ArtifactKind) -> bool {
        (kind == &ArtifactKind::test_plan() && self.test_plan.is_some())
            || self.groups.keys().any(|key| key.kind.as_str() == kind.0.as_str())
    }

    pub(super) fn merge(&mut self, other: Self) {
        let merged_test_plan = match (&self.test_plan, &other.test_plan) {
            (None, Some(value)) => Some(value.clone()),
            (Some(left), Some(right)) if left != right => None,
            _ => self.test_plan.clone(),
        };
        let merged_source_schema = match (&self.source_schema, &other.source_schema) {
            (None, Some(value)) => Some(value.clone()),
            (Some(left), Some(right)) if left != right => None,
            _ => self.source_schema.clone(),
        };
        for (key, facts) in other.groups {
            self.groups.entry(key).or_default().extend(facts);
        }
        self.commands.extend(other.commands);
        self.test_plan = merged_test_plan;
        self.source_schema = merged_source_schema;
        if self.semantic_artifacts.is_empty() {
            self.semantic_artifacts = other.semantic_artifacts;
        } else if !other.semantic_artifacts.is_empty() {
            self.semantic_artifacts.extend(other.semantic_artifacts);
        }
        self.diagnostics.extend(other.diagnostics);
        self.discarded_facts = self.discarded_facts.saturating_add(other.discarded_facts);
    }

    pub(super) fn artifact_result(
        &self,
    ) -> Option<(String, Vec<WorkerArtifact>, Option<TestPlan>)> {
        let schema_id = self.source_schema.clone()?;
        let artifacts = self
            .groups
            .iter()
            .map(|(key, facts)| WorkerArtifact {
                kind: ArtifactKind(key.kind.as_str().to_owned()),
                path: key.path.clone(),
                symbol: key.symbol.clone(),
                facts: facts.iter().cloned().collect(),
            })
            .collect();
        Some((schema_id, artifacts, self.test_plan.clone()))
    }

    pub(super) fn semantic_artifacts(&self) -> &[SemanticWorkerArtifact] {
        &self.semantic_artifacts
    }

    pub(super) fn debug_snapshot(&self) -> Value {
        json!({
            "accepted_artifacts": self.groups.iter().map(|(key, facts)| json!({
                "kind": key.kind.as_str(),
                "path": &key.path,
                "symbol": &key.symbol,
                "fact_count": facts.len(),
            })).collect::<Vec<_>>(),
            "semantic_artifact_kinds": self.semantic_artifacts
                .iter()
                .map(|artifact| artifact.kind().0)
                .collect::<Vec<_>>(),
            "test_plan_accepted": self.test_plan.is_some(),
            "diagnostics": self.diagnostics.iter().map(|diagnostic| json!({
                "index": diagnostic.index,
                "code": &diagnostic.code,
            })).collect::<Vec<_>>(),
            "discarded_facts": self.discarded_facts,
        })
    }

    pub(super) fn into_need_result(self, repository_root: &Path) -> Result<NeedResult, String> {
        let mut claims = Vec::new();
        let mut evidence = Vec::new();
        for (group_index, (key, facts)) in self.groups.into_iter().enumerate() {
            let bytes = fs::read(repository_root.join(&key.path))
                .map_err(|error| format!("cannot bind {}: {error}", key.path))?;
            let evidence_id = format!("evidence-{}", group_index + 1);
            evidence.push(EvidenceReference {
                id: evidence_id.clone(),
                path: key.path.clone(),
                symbol: key.symbol.clone(),
                content_digest: Digest::blake3(&bytes),
                byte_start: (!bytes.is_empty()).then_some(0),
                byte_end: (!bytes.is_empty()).then_some(bytes.len().try_into().unwrap_or(u64::MAX)),
            });
            for fact in facts {
                claims.push(Claim {
                    id: format!("claim-{}", claims.len() + 1),
                    kind: key.kind.as_str().to_owned(),
                    subject: key.symbol.clone().unwrap_or_else(|| key.path.clone()),
                    statement: fact,
                    evidence_ids: vec![evidence_id.clone()],
                });
            }
        }
        Ok(NeedResult {
            complete: true,
            summary: format!("{} snapshot-bound facts.", claims.len()),
            claims,
            evidence,
            suggested_reads: Vec::new(),
            suggested_commands: self.commands.into_iter().collect(),
            uncertainty: Vec::new(),
        })
    }
}

pub(super) fn normalize_response(
    response: CompactWorkerResponse,
    repository_root: &Path,
) -> NormalizedResponse {
    if response.schema != needle_core::ARTIFACT_RESULT_SCHEMA_ID {
        return NormalizedResponse::schema_failure("artifact_schema_id".to_owned());
    }
    let mut output = NormalizedResponse {
        source_schema: Some(needle_core::ARTIFACT_RESULT_SCHEMA_ID.to_owned()),
        ..NormalizedResponse::default()
    };
    for (index, group) in response.artifacts.into_iter().enumerate() {
        let original_fact_count = group.facts.len().try_into().unwrap_or(u32::MAX);
        let path = match normalize_path(&group.path, repository_root) {
            Ok(path) => path,
            Err(code) => {
                output.discarded_facts = output.discarded_facts.saturating_add(original_fact_count);
                output
                    .diagnostics
                    .push(GroupDiagnostic { index: Some(index), code: code.to_owned() });
                continue;
            }
        };
        let symbol =
            group.symbol.map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
        if symbol.as_deref().is_some_and(contains_unsafe_text) {
            output.discarded_facts = output.discarded_facts.saturating_add(original_fact_count);
            output
                .diagnostics
                .push(GroupDiagnostic { index: Some(index), code: "unsafe_symbol".to_owned() });
            continue;
        }
        let key = GroupKey { kind: group.kind, path, symbol };
        let facts = output.groups.entry(key.clone()).or_default();
        let mut invalid_fact = false;
        for fact in group.facts {
            let fact = fact.trim();
            if fact.is_empty() || contains_unsafe_text(fact) {
                output.discarded_facts = output.discarded_facts.saturating_add(1);
                invalid_fact = true;
            } else {
                facts.insert(fact.to_owned());
            }
        }
        if invalid_fact {
            output
                .diagnostics
                .push(GroupDiagnostic { index: Some(index), code: "invalid_fact".to_owned() });
        }
        if facts.is_empty() {
            output.groups.remove(&key);
            if !invalid_fact {
                output
                    .diagnostics
                    .push(GroupDiagnostic { index: Some(index), code: "empty_facts".to_owned() });
            }
        }
    }
    if let Some(plan) = response.test_plan {
        if let Ok((command, _)) =
            TestCommand::from_worker_parts(&plan.runner, &plan.argv, &plan.identifier)
        {
            let (runner, argv, test_identifier) = command.into_parts();
            output.commands.insert(argv.join(" "));
            output.test_plan = Some(TestPlan {
                runner,
                argv,
                cwd_relative: ".".to_owned(),
                test_identifier,
                requires_approval: true,
                execution_evidence_id: None,
            });
        } else {
            output
                .diagnostics
                .push(GroupDiagnostic { index: None, code: "invalid_test_plan".to_owned() });
        }
    }
    output
}

pub(super) fn normalize_semantic_response(
    response: SemanticCompactWorkerResponse,
    repository_root: &Path,
) -> NormalizedResponse {
    if response.schema != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID {
        return NormalizedResponse::schema_failure("semantic_artifact_schema_id".to_owned());
    }
    let mut output = NormalizedResponse {
        source_schema: Some(needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned()),
        ..NormalizedResponse::default()
    };
    for (index, artifact) in response.artifacts.into_iter().enumerate() {
        match normalize_semantic_artifact(artifact, repository_root) {
            Ok(artifact) => {
                adapt_semantic_artifact(&artifact, &mut output);
                output.semantic_artifacts.push(artifact);
            }
            Err(code) => output
                .diagnostics
                .push(GroupDiagnostic { index: Some(index), code: code.to_owned() }),
        }
    }
    output
}

fn normalize_semantic_artifact(
    artifact: SemanticWorkerArtifact,
    repository_root: &Path,
) -> Result<SemanticWorkerArtifact, String> {
    match artifact {
        SemanticWorkerArtifact::CodeLocation { mut locations, gaps } => {
            if locations.is_empty()
                || !locations.iter().any(|location| location.role == LocationRole::Primary)
                || !safe_texts(&gaps)
            {
                return Err("invalid_code_location".to_owned());
            }
            for location in &mut locations {
                normalize_semantic_location(location, repository_root)?;
            }
            Ok(SemanticWorkerArtifact::CodeLocation { locations, gaps })
        }
        SemanticWorkerArtifact::BehaviorTrace { scenario, mut steps, gaps } => {
            if scenario.trim().is_empty()
                || contains_unsafe_text(&scenario)
                || steps.is_empty()
                || !safe_texts(&gaps)
            {
                return Err("invalid_behavior_trace".to_owned());
            }
            for step in &mut steps {
                if step.description.trim().is_empty() || contains_unsafe_text(&step.description) {
                    return Err("invalid_behavior_trace".to_owned());
                }
                step.description = step.description.trim().to_owned();
                normalize_semantic_location(&mut step.location, repository_root)?;
            }
            Ok(SemanticWorkerArtifact::BehaviorTrace {
                scenario: scenario.trim().to_owned(),
                steps,
                gaps,
            })
        }
        SemanticWorkerArtifact::TestPlan {
            runner,
            argv,
            cwd_relative,
            identifiers,
            selection,
            mut evidence_paths,
        } => {
            let identifier = identifiers.first().map_or("", String::as_str);
            let (command, command_violations) =
                match TestCommand::from_worker_parts(&runner, &argv, identifier) {
                    Ok(command) => (Some(command), Vec::new()),
                    Err(violations) => (None, violations),
                };
            if let Some(diagnostic) = invalid_test_plan_diagnostic(
                &runner,
                &argv,
                &cwd_relative,
                &identifiers,
                &selection,
                &evidence_paths,
                &command_violations,
            ) {
                return Err(diagnostic);
            }
            let (command, _) = command.expect("a valid test plan has one valid command");
            let (runner, argv, identifier) = command.into_parts();
            for path in &mut evidence_paths {
                *path = normalize_path(path, repository_root)
                    .map_err(|code| format!("invalid_test_plan:evidence_path_{code}"))?;
            }
            Ok(SemanticWorkerArtifact::TestPlan {
                runner,
                argv,
                cwd_relative,
                identifiers: vec![identifier],
                selection: selection.trim().to_owned(),
                evidence_paths,
            })
        }
    }
}

fn invalid_test_plan_diagnostic(
    runner: &str,
    argv: &[String],
    cwd_relative: &str,
    identifiers: &[String],
    selection: &str,
    evidence_paths: &[String],
    command_violations: &[TestCommandViolation],
) -> Option<String> {
    let mut reasons =
        command_violations.iter().map(|violation| violation.code()).collect::<Vec<_>>();
    if identifiers.len() != 1 {
        reasons.push("identifier_count_not_one");
    }
    if cwd_relative != "." {
        reasons.push("cwd_not_repository_root");
    }
    if selection != "representative" {
        reasons.push("selection_not_representative");
    }
    if evidence_paths.is_empty()
        || evidence_paths.len() > 8
        || evidence_paths.iter().any(|path| path.len() > 512)
    {
        reasons.push("evidence_path_count_invalid");
    }
    reasons.sort_unstable();
    reasons.dedup();
    if reasons.is_empty() {
        return None;
    }

    let summary = RejectedTestPlanSummary {
        reasons,
        runner: diagnostic_atom(runner, 16),
        argv: diagnostic_values(argv, 3, 24),
        argv_count: argv.len(),
        cwd_relative: diagnostic_atom(cwd_relative, 32),
        identifiers: diagnostic_values(identifiers, 1, 32),
        identifier_count: identifiers.len(),
        selection: diagnostic_atom(selection, 20),
        evidence_paths: diagnostic_values(evidence_paths, 2, 32),
        evidence_path_count: evidence_paths.len(),
    };
    let encoded = serde_json::to_string(&summary)
        .unwrap_or_else(|_| r#"{"reasons":["diagnostic_serialization_failed"]}"#.to_owned());
    Some(format!("invalid_test_plan:{encoded}"))
}

#[derive(Serialize)]
struct RejectedTestPlanSummary {
    reasons: Vec<&'static str>,
    runner: String,
    argv: Vec<String>,
    argv_count: usize,
    cwd_relative: String,
    identifiers: Vec<String>,
    identifier_count: usize,
    selection: String,
    evidence_paths: Vec<String>,
    evidence_path_count: usize,
}

fn diagnostic_values(values: &[String], maximum_items: usize, maximum_bytes: usize) -> Vec<String> {
    values.iter().take(maximum_items).map(|value| diagnostic_atom(value, maximum_bytes)).collect()
}

fn diagnostic_atom(value: &str, maximum_bytes: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let next = if character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_' | '.' | '/' | ':' | '=')
        {
            character
        } else {
            '_'
        };
        if output.len().saturating_add(next.len_utf8()) > maximum_bytes {
            break;
        }
        output.push(next);
    }
    if output.is_empty() { "_empty_".to_owned() } else { output }
}

fn normalize_semantic_location(
    location: &mut SemanticLocation,
    repository_root: &Path,
) -> Result<(), &'static str> {
    location.path = normalize_path(&location.path, repository_root)?;
    location.symbol =
        location.symbol.take().map(|symbol| symbol.trim().to_owned()).filter(|s| !s.is_empty());
    if location.symbol.as_deref().is_some_and(contains_unsafe_text) {
        return Err("unsafe_symbol");
    }
    if matches!(
        (location.byte_start, location.byte_end),
        (Some(start), Some(end)) if start >= end
    ) {
        return Err("invalid_byte_range");
    }
    Ok(())
}

fn safe_texts(values: &[String]) -> bool {
    values.iter().all(|value| !value.trim().is_empty() && !contains_unsafe_text(value))
}

fn adapt_semantic_artifact(artifact: &SemanticWorkerArtifact, output: &mut NormalizedResponse) {
    match artifact {
        SemanticWorkerArtifact::CodeLocation { locations, .. } => {
            for location in locations {
                let fact = match location.role {
                    LocationRole::Primary => "primary implementation location",
                    LocationRole::Supporting => "supporting implementation location",
                };
                output
                    .groups
                    .entry(GroupKey {
                        kind: ArtifactOutputKind::CodeLocation,
                        path: location.path.clone(),
                        symbol: location.symbol.clone(),
                    })
                    .or_default()
                    .insert(fact.to_owned());
            }
        }
        SemanticWorkerArtifact::BehaviorTrace { steps, .. } => {
            for step in steps {
                output
                    .groups
                    .entry(GroupKey {
                        kind: ArtifactOutputKind::BehaviorTrace,
                        path: step.location.path.clone(),
                        symbol: step.location.symbol.clone(),
                    })
                    .or_default()
                    .insert(step.description.clone());
            }
        }
        SemanticWorkerArtifact::TestPlan {
            runner,
            argv,
            cwd_relative,
            identifiers,
            evidence_paths,
            ..
        } => {
            if let Some(identifier) = identifiers.first() {
                output.commands.insert(argv.join(" "));
                output.test_plan = Some(TestPlan {
                    runner: runner.clone(),
                    argv: argv.clone(),
                    cwd_relative: cwd_relative.clone(),
                    test_identifier: identifier.clone(),
                    requires_approval: true,
                    execution_evidence_id: None,
                });
                for path in evidence_paths {
                    output
                        .groups
                        .entry(GroupKey {
                            kind: ArtifactOutputKind::TestPlan,
                            path: path.clone(),
                            symbol: Some(identifier.clone()),
                        })
                        .or_default()
                        .insert(format!("representative focused test `{identifier}`"));
                }
            }
        }
    }
}

pub(super) fn normalize_legacy_response(
    response: LegacyCompactWorkerResponse,
    repository_root: &Path,
) -> NormalizedResponse {
    let commands = response.c;
    let mut normalized = normalize_response(
        CompactWorkerResponse {
            schema: needle_core::ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
            artifacts: response
                .g
                .into_iter()
                .map(|group| CompactGroup {
                    kind: ArtifactOutputKind::CodeLocation,
                    path: group.p,
                    symbol: group.s,
                    facts: group.f,
                })
                .collect(),
            test_plan: None,
        },
        repository_root,
    );
    normalized.commands.extend(
        commands
            .into_iter()
            .map(|command| command.trim().to_owned())
            .filter(|command| !command.is_empty() && !contains_unsafe_text(command)),
    );
    normalized.source_schema = Some(needle_core::NEED_RESULT_SCHEMA_ID.to_owned());
    normalized
}

fn normalize_path(value: &str, repository_root: &Path) -> Result<String, &'static str> {
    let mut value = value.trim().replace('\\', "/");
    while let Some(stripped) = value.strip_prefix("./") {
        value = stripped.to_owned();
    }
    if value.is_empty() || contains_unsafe_text(&value) {
        return Err("unsafe_path");
    }
    if value.starts_with('/') || value.starts_with("//") || value.as_bytes().get(1) == Some(&b':') {
        return Err("absolute_path");
    }
    let relative = Path::new(&value);
    if relative.components().any(|component| {
        matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    }) {
        return Err("path_escape");
    }
    let repository_root =
        fs::canonicalize(repository_root).map_err(|_| "repository_unavailable")?;
    let canonical = fs::canonicalize(repository_root.join(relative)).map_err(|_| "file_missing")?;
    if !canonical.starts_with(&repository_root) || !canonical.is_file() {
        return Err("outside_repository");
    }
    if !is_snapshot_member(&repository_root, &value) {
        return Err("outside_snapshot");
    }
    Ok(value)
}

fn is_snapshot_member(repository_root: &Path, relative: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "--"])
        .arg(relative)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.replace('\\', "/") == relative)
        })
}

fn contains_unsafe_text(value: &str) -> bool {
    value.chars().any(char::is_control)
        || ["@@need", "@@end", "[NEEDLE_CONTEXT]", "[/NEEDLE_CONTEXT]"]
            .iter()
            .any(|marker| value.contains(marker))
}

pub(super) fn worker_output_schema(requested: &[ArtifactKind]) -> Value {
    let kinds = if requested.is_empty() {
        vec!["code-location".to_owned(), "behavior-trace".to_owned(), "test-plan".to_owned()]
    } else {
        requested.iter().map(|kind| kind.0.clone()).collect()
    };
    let include_test_plan = kinds.iter().any(|kind| kind == "test-plan");
    let claim_kinds =
        kinds.iter().filter(|kind| kind.as_str() != "test-plan").cloned().collect::<Vec<_>>();
    let claim_kinds = if claim_kinds.is_empty() { kinds.clone() } else { claim_kinds };
    let mut schema = json!({
        "$id": "needle.artifact-result/1",
        "type": "object",
        "additionalProperties": false,
        "required": ["schema", "artifacts"],
        "properties": {
            "schema": {"type": "string", "const": "needle.artifact-result/1"},
            "artifacts": {
                "type": "array", "minItems": 1, "maxItems": 8,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "path", "symbol", "facts"],
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": claim_kinds
                        },
                        "path": {"type": "string", "minLength": 1, "maxLength": 1024},
                        "symbol": {"type": ["string", "null"], "minLength": 1, "maxLength": 512},
                        "facts": {
                            "type": "array", "minItems": 1, "maxItems": 3,
                            "items": {"type": "string", "minLength": 1, "maxLength": 1000}
                        }
                    }
                }
            }
        }
    });
    if include_test_plan {
        schema["required"].as_array_mut().unwrap().push(json!("test_plan"));
        schema["properties"]["test_plan"] = json!({
            "anyOf": [
                {"type": "null"},
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["runner", "argv", "identifier"],
                    "properties": {
                        "runner": {"type": "string", "const": "cargo"},
                        "argv": {
                            "type": "array", "minItems": 2, "maxItems": 16,
                            "items": {"type": "string", "minLength": 1, "maxLength": 512}
                        },
                        "identifier": {"type": "string", "minLength": 1, "maxLength": 512}
                    }
                }
            ]
        });
    }
    schema
}

pub(super) fn semantic_worker_output_schema_for_scenario(
    requested: &[ArtifactKind],
    runtime_scenario: Option<&str>,
) -> Value {
    let requested = if requested.is_empty() {
        vec![
            ArtifactKind::code_location(),
            ArtifactKind::behavior_trace(),
            ArtifactKind::test_plan(),
        ]
    } else {
        requested.to_vec()
    };
    let runtime_scenario_schema = runtime_scenario.map_or_else(
        || json!({"type": "string", "minLength": 1, "maxLength": 512}),
        |scenario| json!({"type": "string", "const": scenario}),
    );
    let mut variants = Vec::new();
    if requested.contains(&ArtifactKind::code_location()) {
        variants.push(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["kind", "locations", "gaps"],
            "properties": {
                "kind": {"type": "string", "const": "code-location"},
                "locations": {
                    "type": "array", "minItems": 1, "maxItems": 8,
                    "items": semantic_location_schema()
                },
                "gaps": bounded_strings(0, 8, 512)
            }
        }));
    }
    if requested.contains(&ArtifactKind::behavior_trace()) {
        variants.push(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["kind", "scenario", "steps", "gaps"],
            "properties": {
                "kind": {"type": "string", "const": "behavior-trace"},
                "scenario": runtime_scenario_schema,
                "steps": {
                    "type": "array", "minItems": 1, "maxItems": 16,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["role", "location", "description"],
                        "properties": {
                            "role": {
                                "type": "string",
                                "enum": [
                                    "producer", "carrier", "transformation",
                                    "precedence", "consumer"
                                ]
                            },
                            "location": semantic_location_schema(),
                            "description": {
                                "type": "string", "minLength": 1, "maxLength": 1000
                            }
                        }
                    }
                },
                "gaps": bounded_strings(0, 8, 512)
            }
        }));
    }
    if requested.contains(&ArtifactKind::test_plan()) {
        variants.push(json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "kind", "runner", "argv", "cwd_relative", "identifiers",
                "selection", "evidence_paths"
            ],
            "properties": {
                "kind": {"type": "string", "const": "test-plan"},
                "runner": {
                    "type": "string",
                    "const": "cargo",
                    "description": "Logical test runner. Needle stores this as cargo."
                },
                "argv": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 16,
                    "items": {"type": "string", "minLength": 1, "maxLength": 512},
                    "description": "Complete process argv. It must include cargo as argv[0] and test as argv[1], even though runner is also present."
                },
                "cwd_relative": {"type": "string", "const": "."},
                "identifiers": bounded_strings(1, 1, 512),
                "selection": {"type": "string", "const": "representative"},
                "evidence_paths": bounded_strings(1, 8, 512)
            }
        }));
    }
    let items = if variants.len() == 1 {
        variants.pop().expect("one requested semantic artifact variant")
    } else {
        json!({"anyOf": variants})
    };
    json!({
        "$id": needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID,
        "type": "object",
        "additionalProperties": false,
        "required": ["schema", "artifacts"],
        "properties": {
            "schema": {
                "type": "string",
                "const": needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
            },
            "artifacts": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "items": items
            }
        }
    })
}

fn semantic_location_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["role", "path", "symbol", "byte_start", "byte_end"],
        "properties": {
            "role": {"type": "string", "enum": ["primary", "supporting"]},
            "path": {"type": "string", "minLength": 1, "maxLength": 1024},
            "symbol": {"type": ["string", "null"], "minLength": 1, "maxLength": 512},
            "byte_start": {
                "type": "null",
                "description": "Always null. Needle resolves the exact symbol anchor."
            },
            "byte_end": {
                "type": "null",
                "description": "Always null. Line numbers are not byte offsets."
            }
        }
    })
}

fn bounded_strings(min: usize, max: usize, item_max: usize) -> Value {
    json!({
        "type": "array",
        "minItems": min,
        "maxItems": max,
        "items": {"type": "string", "minLength": 1, "maxLength": item_max}
    })
}

pub(super) fn repair_prompt(diagnostics: &[GroupDiagnostic]) -> String {
    let codes = diagnostics
        .iter()
        .map(|item| match item.index {
            Some(index) => format!("{}:{}", index + 1, item.code),
            None => item.code.clone(),
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("repair_artifact invalid={codes}; return the full corrected artifact JSON only")
}

pub(super) fn should_repair(
    policy: EvidenceFailurePolicy,
    normalized: &NormalizedResponse,
) -> bool {
    !normalized.has_facts()
        || (policy == EvidenceFailurePolicy::RepairOnce
            && normalized
                .diagnostics
                .iter()
                .any(|diagnostic| !diagnostic.code.starts_with("unrequested_artifact_discarded:")))
}

fn compact_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static REPOSITORY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn repository() -> PathBuf {
        let root = env::temp_dir().join(format!(
            "needle-worker-v5-{}-{}",
            std::process::id(),
            unique_id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "needle@example.invalid"],
            vec!["config", "user.name", "Needle Test"],
            vec!["add", "src/lib.rs"],
            vec!["commit", "--quiet", "-m", "fixture"],
        ] {
            assert!(Command::new("git").args(args).current_dir(&root).status().unwrap().success());
        }
        root
    }

    fn unique_id() -> String {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let counter = REPOSITORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{timestamp}-{counter}")
    }

    #[test]
    fn output_schema_is_typed_artifact_v1_and_bounded() {
        let schema = worker_output_schema(&[]);
        assert_eq!(schema["$id"], "needle.artifact-result/1");
        assert_eq!(schema["properties"]["artifacts"]["maxItems"], 8);
        assert_eq!(
            schema["properties"]["artifacts"]["items"]["properties"]["facts"]["minItems"],
            1
        );
        assert_eq!(
            schema["properties"]["artifacts"]["items"]["properties"]["facts"]["maxItems"],
            3
        );
        assert!(schema["properties"]["artifacts"]["items"]["properties"].get("p").is_none());
        assert!(schema["properties"].get("facts").is_none());
        assert!(schema["properties"].get("sources").is_none());
        assert!(schema["properties"].get("refs").is_none());
    }

    #[test]
    fn partial_output_schema_allows_only_requested_node_kinds() {
        let schema = worker_output_schema(&[ArtifactKind::behavior_trace()]);
        assert_eq!(
            schema["properties"]["artifacts"]["items"]["properties"]["kind"]["enum"],
            json!(["behavior-trace"])
        );
        assert!(schema["properties"].get("test_plan").is_none());
        assert!(!schema["required"].as_array().unwrap().contains(&json!("test_plan")));
    }

    #[test]
    fn semantic_schema_uses_any_of_for_typed_union_without_duplicate_test_plan() {
        let schema = semantic_worker_output_schema_for_scenario(
            &[ArtifactKind::code_location(), ArtifactKind::test_plan()],
            None,
        );
        assert_eq!(schema["$id"], needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID);
        assert!(schema["properties"].get("test_plan").is_none());
        assert!(!schema.to_string().contains("\"oneOf\""));
        let variants = schema["properties"]["artifacts"]["items"]["anyOf"].as_array().unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0]["properties"]["kind"]["const"], json!("code-location"));
        assert_eq!(variants[1]["properties"]["kind"]["const"], json!("test-plan"));
    }

    #[test]
    fn semantic_test_plan_schema_requires_one_representative_identifier() {
        let schema = semantic_worker_output_schema_for_scenario(&[ArtifactKind::test_plan()], None);
        let properties = &schema["properties"]["artifacts"]["items"]["properties"];

        assert_eq!(properties["identifiers"]["minItems"], json!(1));
        assert_eq!(properties["identifiers"]["maxItems"], json!(1));
        assert_eq!(properties["selection"]["const"], json!("representative"));
    }

    #[test]
    fn semantic_schema_inlines_a_single_requested_artifact() {
        let schema =
            semantic_worker_output_schema_for_scenario(&[ArtifactKind::code_location()], None);
        let items = &schema["properties"]["artifacts"]["items"];
        assert_eq!(items["properties"]["kind"]["const"], json!("code-location"));
        assert_eq!(
            items["properties"]["locations"]["items"]["properties"]["byte_start"]["type"],
            json!("null")
        );
        assert_eq!(
            items["properties"]["locations"]["items"]["properties"]["byte_end"]["type"],
            json!("null")
        );
        assert!(items.get("oneOf").is_none());
        assert!(items.get("anyOf").is_none());
    }

    #[test]
    fn semantic_behavior_trace_schema_binds_the_parent_owned_scenario() {
        let schema = semantic_worker_output_schema_for_scenario(
            &[ArtifactKind::behavior_trace()],
            Some("default"),
        );
        let scenario = &schema["properties"]["artifacts"]["items"]["properties"]["scenario"];
        assert_eq!(scenario["const"], json!("default"));
        assert!(scenario.get("minLength").is_none());
    }

    #[test]
    fn semantic_response_normalizes_and_adapts_for_v03_continuation() {
        let root = repository();
        let response: SemanticCompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/2",
            "artifacts": [
                {
                    "kind": "code-location",
                    "locations": [{
                        "role": "primary",
                        "path": "./src\\lib.rs",
                        "symbol": " answer ",
                        "byte_start": 0,
                        "byte_end": 31
                    }],
                    "gaps": []
                },
                {
                    "kind": "test-plan",
                    "runner": "cargo",
                    "argv": ["cargo", "test", "answer"],
                    "cwd_relative": ".",
                    "identifiers": ["answer"],
                    "selection": "representative",
                    "evidence_paths": ["src/lib.rs"]
                }
            ]
        }))
        .unwrap();
        let normalized = normalize_semantic_response(response, &root);
        assert!(normalized.diagnostics.is_empty());
        assert_eq!(normalized.semantic_artifacts().len(), 2);
        assert_eq!(normalized.test_plan.as_ref().unwrap().test_identifier, "answer");
        assert!(normalized.has_requested_kind(&ArtifactKind::code_location()));
        assert!(normalized.has_requested_kind(&ArtifactKind::test_plan()));
        let (schema, artifacts, _) = normalized.artifact_result().unwrap();
        assert_eq!(schema, needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID);
        assert_eq!(artifacts.len(), 2);
        let result = normalized.into_need_result(&root).unwrap();
        assert_eq!(result.claims.len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn semantic_test_plan_accepts_a_fully_qualified_identifier_with_suffix_filter() {
        let root = repository();
        let response: SemanticCompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/2",
            "artifacts": [{
                "kind": "test-plan",
                "runner": "cargo",
                "argv": ["cargo", "test", "answer"],
                "cwd_relative": ".",
                "identifiers": ["module::answer"],
                "selection": "representative",
                "evidence_paths": ["src/lib.rs"]
            }]
        }))
        .unwrap();

        let normalized = normalize_semantic_response(response, &root);
        assert!(normalized.diagnostics.is_empty());
        assert_eq!(
            normalized.test_plan.as_ref().map(|plan| plan.test_identifier.as_str()),
            Some("module::answer")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn semantic_test_plan_canonicalizes_runner_relative_argv_from_r112() {
        let root = repository();
        let response: SemanticCompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/2",
            "artifacts": [{
                "kind": "test-plan",
                "runner": "cargo",
                "argv": ["test", "--test", "integration", "f416_crlf"],
                "cwd_relative": ".",
                "identifiers": ["f416_crlf"],
                "selection": "representative",
                "evidence_paths": ["src/lib.rs"]
            }]
        }))
        .unwrap();

        let normalized = normalize_semantic_response(response, &root);
        assert!(normalized.diagnostics.is_empty());
        let plan = normalized.test_plan.as_ref().expect("normalized test plan");
        assert_eq!(plan.argv, ["cargo", "test", "--test", "integration", "f416_crlf"]);
        assert!(plan.test_command().is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_test_plan_reports_exact_bounded_repairable_reasons() {
        let root = repository();
        let response: SemanticCompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/2",
            "artifacts": [{
                "kind": "test-plan",
                "runner": "cargo",
                "argv": ["cargo", "check", "answer"],
                "cwd_relative": ".",
                "identifiers": ["module::missing@@need"],
                "selection": "representative",
                "evidence_paths": ["src/lib.rs"]
            }]
        }))
        .unwrap();

        let normalized = normalize_semantic_response(response, &root);
        let diagnostic = &normalized.diagnostics[0].code;
        assert!(diagnostic.contains("argv_not_direct_cargo_test"));
        assert!(diagnostic.contains("identifier_invalid_or_unsafe"));
        assert!(diagnostic.contains("identifier_not_in_argv"));
        assert!(diagnostic.contains(r#""argv":["cargo","check","answer"]"#));
        assert!(!diagnostic.contains("@@need"));
        assert!(diagnostic.len() <= 900);
        let repair = repair_prompt(&normalized.diagnostics);
        assert!(repair.contains("argv_not_direct_cargo_test"));
        assert!(repair.contains("identifier_not_in_argv"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_test_plan_summary_remains_complete_at_the_failure_boundary() {
        let mut argv = vec![
            "powershell".to_owned(),
            "-Command".to_owned(),
            "&&".to_owned(),
            "--workspace".to_owned(),
        ];
        argv.extend((0..32).map(|index| format!("argument-{index:02}")));
        let command_violations = TestCommand::from_worker_parts("not-cargo", &argv, "--bad")
            .expect_err("the command violates the canonical contract");
        let diagnostic = invalid_test_plan_diagnostic(
            "not-cargo",
            &argv,
            "..",
            &["--bad".to_owned(), String::new()],
            "exhaustive",
            &[],
            &command_violations,
        )
        .expect("the plan violates every static predicate");

        assert!(diagnostic.len() <= 900);
        let summary: Value = serde_json::from_str(
            diagnostic.strip_prefix("invalid_test_plan:").expect("diagnostic prefix"),
        )
        .expect("bounded diagnostic must retain complete structured JSON");
        assert_eq!(summary["argv_count"], json!(36));
        assert_eq!(summary["identifier_count"], json!(2));
        assert!(summary["reasons"].as_array().is_some_and(|reasons| reasons.len() >= 9));
    }

    #[test]
    fn unrequested_covered_artifacts_are_discarded_without_forcing_repair() {
        let root = repository();
        let response: SemanticCompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/2",
            "artifacts": [
                {
                    "kind": "code-location",
                    "locations": [{
                        "role": "primary",
                        "path": "src/lib.rs",
                        "symbol": "answer",
                        "byte_start": null,
                        "byte_end": null
                    }],
                    "gaps": []
                },
                {
                    "kind": "test-plan",
                    "runner": "cargo",
                    "argv": ["cargo", "test", "answer"],
                    "cwd_relative": ".",
                    "identifiers": ["module::answer"],
                    "selection": "representative",
                    "evidence_paths": ["src/lib.rs"]
                }
            ]
        }))
        .unwrap();

        let mut normalized = normalize_semantic_response(response, &root);
        normalized.discard_unrequested_kinds(&[ArtifactKind::test_plan()]);
        normalized.record_missing_requested_kinds(&[ArtifactKind::test_plan()]);

        assert_eq!(normalized.semantic_artifacts().len(), 1);
        assert!(normalized.has_requested_kind(&ArtifactKind::test_plan()));
        assert!(!normalized.has_requested_kind(&ArtifactKind::code_location()));
        assert!(
            normalized
                .diagnostics
                .iter()
                .any(|item| item.code == "unrequested_artifact_discarded:code-location")
        );
        assert!(!should_repair(EvidenceFailurePolicy::RepairOnce, &normalized));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_v4_payload_is_rejected() {
        let legacy = json!({
            "complete": true,
            "summary": "legacy",
            "facts": [{"statement":"fact","refs":[0]}],
            "sources": [{"path":"src/lib.rs","symbol":null,"range":null}],
            "commands": [],
            "missing": []
        });
        assert!(serde_json::from_value::<CompactWorkerResponse>(legacy).is_err());
    }

    #[test]
    fn legacy_need_result_v5_is_supported_only_through_the_adapter() {
        let root = repository();
        let legacy: LegacyCompactWorkerResponse = serde_json::from_value(json!({
            "g": [{"p":"src/lib.rs","s":"answer","f":["Legacy fact."]}],
            "c":["cargo test answer"]
        }))
        .unwrap();
        let normalized = normalize_legacy_response(legacy, &root);
        let result = normalized.into_need_result(&root).unwrap();
        assert_eq!(result.claims[0].kind, "code-location");
        assert_eq!(result.suggested_commands, vec!["cargo test answer"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn normalization_merges_slashes_prefixes_and_duplicates() {
        let root = repository();
        let response: CompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/1",
            "artifacts": [
                {
                    "kind":"code-location",
                    "path":"./src\\lib.rs",
                    "symbol":" answer ",
                    "facts":[" Fact. ","Fact."]
                },
                {
                    "kind":"code-location",
                    "path":"src/lib.rs",
                    "symbol":"answer",
                    "facts":["Second."]
                }
            ],
            "test_plan": {
                "runner": "cargo",
                "argv": ["cargo", "test", "focused"],
                "identifier": "focused"
            }
        }))
        .unwrap();
        let normalized = normalize_response(response, &root);
        assert_eq!(normalized.groups.len(), 1);
        assert_eq!(normalized.groups.values().next().unwrap().len(), 2);
        assert_eq!(normalized.commands.len(), 1);
        let (schema, artifacts, test_plan) = normalized.artifact_result().unwrap();
        assert_eq!(schema, needle_core::ARTIFACT_RESULT_SCHEMA_ID);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, ArtifactKind::code_location());
        assert_eq!(artifacts[0].facts, vec!["Fact.", "Second."]);
        assert_eq!(test_plan.unwrap().test_identifier, "focused");
        let result = normalized.into_need_result(&root).unwrap();
        assert_eq!(result.claims.len(), 2);
        assert_eq!(result.evidence.len(), 1);
        assert_eq!(result.evidence[0].byte_start, Some(0));
        assert_eq!(
            result.evidence[0].byte_end,
            Some(fs::metadata(root.join("src/lib.rs")).unwrap().len())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn structured_test_plan_satisfies_the_requested_test_plan_kind() {
        let root = repository();
        let response: CompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/1",
            "artifacts": [
                {"kind":"code-location","p":"src/lib.rs","s":"answer","f":["location"]},
                {"kind":"behavior-trace","p":"src/lib.rs","s":"answer","f":["behavior"]}
            ],
            "test_plan": {
                "runner": "cargo",
                "argv": ["cargo", "test", "focused"],
                "identifier": "focused"
            }
        }))
        .unwrap();
        let mut normalized = normalize_response(response, &root);
        let requested = vec![
            ArtifactKind::code_location(),
            ArtifactKind::behavior_trace(),
            ArtifactKind::test_plan(),
        ];
        normalized.record_missing_requested_kinds(&requested);
        assert!(normalized.missing_requested_kinds(&requested).is_empty());
        assert!(normalized.diagnostics.is_empty());
        assert!(!should_repair(EvidenceFailurePolicy::RepairOnce, &normalized));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_group_is_discarded_without_losing_valid_group() {
        let root = repository();
        let response: CompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/1",
            "artifacts": [
                {"kind":"code-location","p":"../escape.rs","s":null,"f":["bad"]},
                {"kind":"behavior-trace","p":"src/lib.rs","s":"answer","f":["good"]}
            ],
            "test_plan": null
        }))
        .unwrap();
        let normalized = normalize_response(response, &root);
        assert!(normalized.has_facts());
        assert_eq!(normalized.discarded_facts, 1);
        assert_eq!(normalized.diagnostics[0].code, "path_escape");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repair_merge_adopts_the_first_valid_typed_schema() {
        let root = repository();
        let mut first = NormalizedResponse::schema_failure("broken".to_owned());
        let repaired = normalize_response(
            serde_json::from_value(json!({
                "schema": "needle.artifact-result/1",
                "artifacts": [
                    {"kind":"code-location","p":"src/lib.rs","s":"answer","f":["valid"]}
                ],
                "test_plan": null
            }))
            .unwrap(),
            &root,
        );
        first.merge(repaired);
        assert_eq!(first.artifact_result().unwrap().0, needle_core::ARTIFACT_RESULT_SCHEMA_ID);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nested_markers_and_missing_files_are_discarded() {
        let root = repository();
        let response: CompactWorkerResponse = serde_json::from_value(json!({
            "schema": "needle.artifact-result/1",
            "artifacts": [
                {"kind":"code-location","p":"src/lib.rs","s":null,"f":["@@need nested"]},
                {"kind":"test-plan","p":"src/missing.rs","s":null,"f":["missing"]}
            ],
            "test_plan": null
        }))
        .unwrap();
        let normalized = normalize_response(response, &root);
        assert!(!normalized.has_facts());
        assert_eq!(normalized.discarded_facts, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repair_prompt_contains_only_bounded_diagnostics() {
        let prompt = repair_prompt(&[
            GroupDiagnostic { index: Some(0), code: "unsafe_path".to_owned() },
            GroupDiagnostic { index: Some(2), code: "invalid_fact".to_owned() },
        ]);
        assert_eq!(
            prompt,
            "repair_artifact invalid=1:unsafe_path,3:invalid_fact; return the full corrected artifact JSON only"
        );
        assert!(!prompt.contains("@@need"));
    }

    #[test]
    fn policy_repairs_only_when_configured_or_no_facts_survive() {
        let mut partial = NormalizedResponse::default();
        partial.groups.insert(
            GroupKey {
                kind: ArtifactOutputKind::CodeLocation,
                path: "src/lib.rs".to_owned(),
                symbol: None,
            },
            ["valid".to_owned()].into_iter().collect(),
        );
        partial
            .diagnostics
            .push(GroupDiagnostic { index: Some(1), code: "file_missing".to_owned() });
        assert!(!should_repair(EvidenceFailurePolicy::DiscardInvalidFact, &partial));
        assert!(should_repair(EvidenceFailurePolicy::RepairOnce, &partial));
        assert!(should_repair(
            EvidenceFailurePolicy::DiscardInvalidFact,
            &NormalizedResponse::default()
        ));
    }
}
