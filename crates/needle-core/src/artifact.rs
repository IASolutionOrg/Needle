use crate::{
    ArtifactId, ArtifactRequestId, Digest, NeedFragmentId, NeedKey, ReuseSufficiencyCertificateId,
    SelectedPlanId, normalize_line_endings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const ARTIFACT_RESULT_SCHEMA_ID: &str = "needle.artifact-result/1";
pub const SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID: &str = "needle.artifact-result/2";
pub const MAX_ROUTE_PLAN_NODES: usize = 16;
pub const MAX_TEST_COMMAND_ARGV: usize = 16;
pub const MAX_TEST_COMMAND_ARGUMENT_BYTES: usize = 512;
/// Hard bound for verifier-owned certified focused tests in one turn.
pub const MAX_VERIFIER_TEST_PLANS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactKind(pub String);

impl ArtifactKind {
    pub fn code_location() -> Self {
        Self("code-location".to_owned())
    }

    pub fn behavior_trace() -> Self {
        Self("behavior-trace".to_owned())
    }

    pub fn test_plan() -> Self {
        Self("test-plan".to_owned())
    }

    pub fn evidence_brief() -> Self {
        Self("evidence-brief".to_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    SnapshotExact,
    WorktreeSemantic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactContract {
    pub id: String,
    pub revision: u32,
    pub kind: ArtifactKind,
    pub schema_id: String,
    pub cache_scope: CacheScope,
    pub definition_digest: Digest,
}

impl ArtifactContract {
    pub fn new(
        id: impl Into<String>,
        revision: u32,
        kind: ArtifactKind,
        cache_scope: CacheScope,
    ) -> Self {
        let id = id.into();
        let schema_id = ARTIFACT_RESULT_SCHEMA_ID.to_owned();
        let definition_digest = Digest::blake3(format!(
            "needle-artifact-contract\n{id}\n{revision}\n{}\n{schema_id}\n{cache_scope:?}\n",
            kind.0
        ));
        Self { id, revision, kind, schema_id, cache_scope, definition_digest }
    }

    pub fn semantic(
        id: impl Into<String>,
        revision: u32,
        kind: ArtifactKind,
        cache_scope: CacheScope,
    ) -> Self {
        let id = id.into();
        let schema_id = SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned();
        let mut hasher = crate::CanonicalHasher::new(b"artifact-contract");
        hasher.field_str(&id);
        hasher.field_u32(revision);
        hasher.field_str(&kind.0);
        hasher.field_str(&schema_id);
        Self { id, revision, kind, schema_id, cache_scope, definition_digest: hasher.finish() }
    }
}

/// Semantic request identity. Model, reasoning, prompt and pricing belong to
/// `ExecutionAttempt`, so changing them does not fragment valid artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRequest {
    pub contract_id: String,
    pub contract_revision: u32,
    pub repository_id: Digest,
    pub source_snapshot_digest: Digest,
    pub route_key: NeedKey,
    pub normalized_request: String,
    #[serde(default)]
    pub semantic_fragment_id: Option<NeedFragmentId>,
    pub input_artifact_ids: Vec<Digest>,
}

impl ArtifactRequest {
    pub fn id(&self) -> Digest {
        let mut inputs = self.input_artifact_ids.clone();
        inputs.sort();
        Digest::blake3(format!(
            "needle-artifact-request\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            self.contract_id,
            self.contract_revision,
            self.repository_id,
            self.source_snapshot_digest,
            self.route_key,
            normalize_line_endings(&self.normalized_request),
            inputs.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
        ))
    }

    pub fn logical_id(&self) -> Digest {
        let mut clone = self.clone();
        clone.source_snapshot_digest = Digest::blake3("semantic-scope");
        clone.id()
    }

    pub fn semantic_id(&self) -> ArtifactRequestId {
        self.semantic_id_with_source(self.source_snapshot_digest)
    }

    pub fn semantic_logical_id(&self) -> ArtifactRequestId {
        self.semantic_id_with_source(Digest::blake3(b"semantic-scope"))
    }

    fn semantic_id_with_source(&self, source: Digest) -> ArtifactRequestId {
        let mut inputs = arrayvec::ArrayVec::<Digest, { crate::MAX_NEED_INPUTS }>::new();
        let bounded =
            self.input_artifact_ids.iter().copied().all(|input| inputs.try_push(input).is_ok());
        if bounded {
            inputs.sort();
        }
        let mut hasher = crate::CanonicalHasher::new(b"artifact-request");
        hasher.field_str(&self.contract_id);
        hasher.field_u32(self.contract_revision);
        hasher.field_digest(self.repository_id);
        hasher.field_digest(source);
        hasher.field_normalized_lines(&self.normalized_request);
        match self.semantic_fragment_id {
            Some(fragment) => {
                hasher.field_u8(1);
                hasher.field_digest(fragment.digest());
            }
            None => hasher.field_u8(0),
        }
        hasher.field_u8(u8::from(bounded));
        if bounded {
            for input in inputs {
                hasher.field_digest(input);
            }
        } else {
            for input in &self.input_artifact_ids {
                hasher.field_digest(*input);
            }
        }
        ArtifactRequestId(hasher.finish())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProfile {
    pub platform: String,
    pub model: String,
    pub reasoning: String,
    pub service_tier: Option<String>,
    pub definition_digest: Digest,
}

impl WorkerProfile {
    pub fn new(
        platform: impl Into<String>,
        model: impl Into<String>,
        reasoning: impl Into<String>,
        service_tier: Option<String>,
    ) -> Self {
        let platform = platform.into();
        let model = model.into();
        let reasoning = reasoning.into();
        let definition_digest = Digest::blake3(format!(
            "needle-worker-profile\n{platform}\n{model}\n{reasoning}\n{}\n",
            service_tier.as_deref().unwrap_or_default()
        ));
        Self { platform, model, reasoning, service_tier, definition_digest }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub worker: WorkerProfile,
    pub timeout_seconds: u64,
    pub prompt_profile_digest: Digest,
    pub pricing_snapshot_digest: Option<Digest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAttempt {
    pub id: Digest,
    pub artifact_request_id: Digest,
    pub profile: ExecutionProfile,
    pub started_unix_ms: u64,
    pub completed_unix_ms: Option<u64>,
    pub status: String,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_microusd: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub path: String,
    pub content_digest: Digest,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub claims: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyManifest {
    pub scope: CacheScope,
    pub observed_files_complete: bool,
    pub dependencies: Vec<Dependency>,
    pub gaps: Vec<String>,
}

impl DependencyManifest {
    pub fn supports_worktree_semantic(&self) -> bool {
        self.scope == CacheScope::WorktreeSemantic
            && self.observed_files_complete
            && self.gaps.is_empty()
            && !self.dependencies.is_empty()
            && self.dependencies.iter().all(|dependency| !dependency.claims.is_empty())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerArtifact {
    pub kind: ArtifactKind,
    pub path: String,
    pub symbol: Option<String>,
    pub facts: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerObservationTrace {
    pub observed_files: Vec<String>,
    pub gaps: Vec<String>,
}

impl WorkerObservationTrace {
    pub fn merge(&mut self, other: Self) {
        self.observed_files.extend(other.observed_files);
        self.observed_files.sort();
        self.observed_files.dedup();
        self.gaps.extend(other.gaps);
        self.gaps.sort();
        self.gaps.dedup();
    }

    pub fn is_complete(&self) -> bool {
        self.gaps.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerArtifactResult {
    pub schema_id: String,
    pub artifacts: Vec<WorkerArtifact>,
    pub test_plan: Option<TestPlan>,
    pub observation_trace: WorkerObservationTrace,
    #[serde(default)]
    pub artifact_traces: BTreeMap<ArtifactKind, WorkerObservationTrace>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocationRole {
    Primary,
    Supporting,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticLocation {
    pub role: LocationRole,
    pub path: String,
    pub symbol: Option<String>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowStepRole {
    Producer,
    Carrier,
    Transformation,
    Precedence,
    Consumer,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFlowStep {
    pub role: FlowStepRole,
    pub location: SemanticLocation,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SemanticWorkerArtifact {
    CodeLocation {
        locations: Vec<SemanticLocation>,
        gaps: Vec<String>,
    },
    BehaviorTrace {
        scenario: String,
        steps: Vec<SemanticFlowStep>,
        gaps: Vec<String>,
    },
    TestPlan {
        runner: String,
        argv: Vec<String>,
        cwd_relative: String,
        identifiers: Vec<String>,
        selection: String,
        evidence_paths: Vec<String>,
    },
}

impl SemanticWorkerArtifact {
    pub fn kind(&self) -> ArtifactKind {
        match self {
            Self::CodeLocation { .. } => ArtifactKind::code_location(),
            Self::BehaviorTrace { .. } => ArtifactKind::behavior_trace(),
            Self::TestPlan { .. } => ArtifactKind::test_plan(),
        }
    }

    pub fn canonical_artifact_id(&self, contract_definition: Digest) -> Option<ArtifactId> {
        use arrayvec::ArrayVec;

        let mut hasher = crate::CanonicalHasher::new(b"semantic-artifact");
        hasher.field_digest(contract_definition);
        match self {
            Self::CodeLocation { locations, gaps } => {
                hasher.field_u8(0);
                let mut ordered = ArrayVec::<_, 8>::new();
                for location in locations {
                    ordered.try_push(location).ok()?;
                }
                ordered.sort_by(|left, right| {
                    (&left.path, &left.symbol, left.role, left.byte_start, left.byte_end).cmp(&(
                        &right.path,
                        &right.symbol,
                        right.role,
                        right.byte_start,
                        right.byte_end,
                    ))
                });
                for location in ordered {
                    hash_semantic_location(&mut hasher, location);
                }
                hash_sorted_strings::<8>(&mut hasher, gaps)?;
            }
            Self::BehaviorTrace { scenario, steps, gaps } => {
                hasher.field_u8(1);
                hasher.field_str(scenario);
                if steps.len() > 16 {
                    return None;
                }
                for step in steps {
                    hasher.field_u8(match step.role {
                        FlowStepRole::Producer => 0,
                        FlowStepRole::Carrier => 1,
                        FlowStepRole::Transformation => 2,
                        FlowStepRole::Precedence => 3,
                        FlowStepRole::Consumer => 4,
                    });
                    hash_semantic_location(&mut hasher, &step.location);
                    hasher.field_str(&step.description);
                }
                hash_sorted_strings::<8>(&mut hasher, gaps)?;
            }
            Self::TestPlan {
                runner,
                argv,
                cwd_relative,
                identifiers,
                selection,
                evidence_paths,
            } => {
                hasher.field_u8(2);
                hasher.field_str(runner);
                if argv.len() > 16 {
                    return None;
                }
                for argument in argv {
                    hasher.field_str(argument);
                }
                hasher.field_str(cwd_relative);
                hash_sorted_strings::<8>(&mut hasher, identifiers)?;
                hasher.field_str(selection);
                hash_sorted_strings::<8>(&mut hasher, evidence_paths)?;
            }
        }
        Some(ArtifactId(hasher.finish()))
    }
}

fn hash_semantic_location(hasher: &mut crate::CanonicalHasher, location: &SemanticLocation) {
    hasher.field_u8(match location.role {
        LocationRole::Primary => 0,
        LocationRole::Supporting => 1,
    });
    hasher.field_str(&location.path);
    match &location.symbol {
        Some(symbol) => {
            hasher.field_u8(1);
            hasher.field_str(symbol);
        }
        None => hasher.field_u8(0),
    }
    match location.byte_start {
        Some(value) => {
            hasher.field_u8(1);
            hasher.field_bytes(&value.to_le_bytes());
        }
        None => hasher.field_u8(0),
    }
    match location.byte_end {
        Some(value) => {
            hasher.field_u8(1);
            hasher.field_bytes(&value.to_le_bytes());
        }
        None => hasher.field_u8(0),
    }
}

fn hash_sorted_strings<const N: usize>(
    hasher: &mut crate::CanonicalHasher,
    values: &[String],
) -> Option<()> {
    let mut ordered = arrayvec::ArrayVec::<_, N>::new();
    for value in values {
        ordered.try_push(value).ok()?;
    }
    ordered.sort();
    for value in ordered {
        hasher.field_str(value);
    }
    Some(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticArtifactResult {
    pub schema_id: String,
    pub artifacts: Vec<SemanticWorkerArtifact>,
    pub observation_trace: WorkerObservationTrace,
    #[serde(default)]
    pub artifact_traces: BTreeMap<ArtifactKind, WorkerObservationTrace>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub validator: String,
    pub validator_revision: u32,
    pub status: String,
    pub evidence_digest: Digest,
    pub validated_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: Digest,
    pub request_id: Digest,
    pub contract: ArtifactContract,
    pub payload: Value,
    pub dependency_manifest: DependencyManifest,
    pub validations: Vec<ValidationRecord>,
    pub created_unix_ms: u64,
}

impl Artifact {
    pub fn compute_id(
        request_id: Digest,
        contract: &ArtifactContract,
        payload: &Value,
    ) -> Result<Digest, serde_json::Error> {
        let payload_digest = Digest::blake3(serde_json::to_vec(payload)?);
        Ok(Digest::blake3(format!(
            "needle-artifact\n{request_id}\n{}\n{payload_digest}\n",
            contract.definition_digest
        )))
    }

    /// v0.4 semantic artifact identity. The originating request and all
    /// execution provenance are intentionally excluded.
    pub fn compute_content_id(
        contract: &ArtifactContract,
        payload: &Value,
    ) -> Result<ArtifactId, serde_json::Error> {
        use std::io::Write;

        struct HashWriter(blake3::Hasher);

        impl Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = HashWriter(blake3::Hasher::new());
        writer.0.update(b"needle-semantic-artifact\0");
        writer.0.update(&contract.definition_digest.bytes());
        serde_json::to_writer(&mut writer, payload)?;
        Ok(ArtifactId(Digest(*writer.0.finalize().as_bytes())))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheResolution {
    ExactHit {
        artifact_id: Digest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sufficiency_certificate_id: Option<ReuseSufficiencyCertificateId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_plan_id: Option<SelectedPlanId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution_format_revision: Option<u16>,
    },
    CoverageHit {
        artifact_id: Digest,
        sufficiency_certificate_id: ReuseSufficiencyCertificateId,
        selected_plan_id: SelectedPlanId,
        resolution_format_revision: u16,
    },
    CompositeHit {
        artifact_ids: Vec<Digest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sufficiency_certificate_id: Option<ReuseSufficiencyCertificateId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_plan_id: Option<SelectedPlanId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution_format_revision: Option<u16>,
    },
    ClaimHit {
        artifact_ids: Vec<Digest>,
        claim_ids: Vec<crate::ClaimId>,
        claim_set_certificate_id: crate::ClaimSetCertificateId,
        selected_plan_id: SelectedPlanId,
        resolution_format_revision: u16,
    },
    ClaimCompositeHit {
        artifact_ids: Vec<Digest>,
        claim_ids: Vec<crate::ClaimId>,
        claim_set_certificate_id: crate::ClaimSetCertificateId,
        selected_plan_id: SelectedPlanId,
        resolution_format_revision: u16,
    },
    PartialHit {
        reused: Vec<Digest>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reused_claim_ids: Vec<crate::ClaimId>,
        invalidated_nodes: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_plan_id: Option<SelectedPlanId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution_format_revision: Option<u16>,
    },
    Miss,
    Stale {
        artifact_id: Digest,
        reason: String,
    },
    Rejected {
        reason: String,
    },
    Ambiguous {
        reason: String,
    },
    Contradicted {
        reason: String,
    },
    Bypass {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorDefinition {
    pub id: String,
    pub revision: u32,
    pub input_kinds: Vec<ArtifactKind>,
    pub output_kind: ArtifactKind,
    pub cacheable: bool,
    pub definition_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanNode {
    pub id: String,
    pub operator_id: String,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePlan {
    pub id: String,
    pub revision: u32,
    pub route_key: NeedKey,
    pub nodes: Vec<PlanNode>,
    pub definition_digest: Digest,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum RoutePlanError {
    #[error("route plan must contain between 1 and {MAX_ROUTE_PLAN_NODES} nodes")]
    NodeCount,
    #[error("route plan contains duplicate node `{0}`")]
    DuplicateNode(String),
    #[error("route plan node `{node}` depends on unknown or later node `{dependency}`")]
    InvalidDependency { node: String, dependency: String },
}

impl RoutePlan {
    pub fn new(
        id: impl Into<String>,
        revision: u32,
        route_key: NeedKey,
        nodes: Vec<PlanNode>,
    ) -> Result<Self, RoutePlanError> {
        if nodes.is_empty() || nodes.len() > MAX_ROUTE_PLAN_NODES {
            return Err(RoutePlanError::NodeCount);
        }
        let mut seen = BTreeSet::new();
        for node in &nodes {
            if !seen.insert(node.id.clone()) {
                return Err(RoutePlanError::DuplicateNode(node.id.clone()));
            }
            for dependency in &node.depends_on {
                if !seen.contains(dependency) {
                    return Err(RoutePlanError::InvalidDependency {
                        node: node.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }
        let id = id.into();
        let definition_digest = Digest::blake3(
            serde_json::to_vec(&(&id, revision, &route_key, &nodes)).expect("serializable"),
        );
        Ok(Self { id, revision, route_key, nodes, definition_digest })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicy {
    FixedOrder { profiles: Vec<WorkerProfile>, repair_once: bool, native_fallback: bool },
    CheapestValidatedFirst { promoted_profiles: Vec<WorkerProfile>, native_fallback: bool },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontierItem {
    pub artifact_id: Digest,
    pub kind: ArtifactKind,
    pub summary: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontierView {
    pub route_key: NeedKey,
    pub cache_resolution: CacheResolution,
    pub items: Vec<FrontierItem>,
    pub omitted_items: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeLocation {
    pub path: String,
    pub symbol: Option<String>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub content_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorStep {
    pub ordinal: u32,
    pub location: CodeLocation,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorTrace {
    pub entrypoint: String,
    pub steps: Vec<BehaviorStep>,
    pub uncertainty: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCommandInputForm {
    CompleteArgv,
    RunnerRelativeArgv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCommandViolation {
    RunnerNotCargo,
    ArgvNotDirectCargoTest,
    ArgvNotCanonical,
    ArgvInvalidOrUnsafe,
    ArgvScopeNotFocused,
    ArgvContainsShellSyntax,
    IdentifierInvalidOrUnsafe,
    IdentifierNotInArgv,
}

impl TestCommandViolation {
    pub const fn code(self) -> &'static str {
        match self {
            Self::RunnerNotCargo => "runner_not_cargo",
            Self::ArgvNotDirectCargoTest => "argv_not_direct_cargo_test",
            Self::ArgvNotCanonical => "argv_not_canonical",
            Self::ArgvInvalidOrUnsafe => "argv_invalid_or_unsafe",
            Self::ArgvScopeNotFocused => "argv_scope_not_focused",
            Self::ArgvContainsShellSyntax => "argv_contains_shell_syntax",
            Self::IdentifierInvalidOrUnsafe => "identifier_invalid_or_unsafe",
            Self::IdentifierNotInArgv => "identifier_not_in_argv",
        }
    }
}

/// Canonical, statically safe focused-test command. Worker transports may omit
/// the executable from `argv` because they already carry `runner`; Needle
/// accepts that representation only at ingress and immediately restores the
/// complete process vector used by identity, approval and execution evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCommand {
    runner: String,
    argv: Vec<String>,
    test_identifier: String,
}

impl TestCommand {
    pub fn from_worker_parts(
        runner: &str,
        argv: &[String],
        test_identifier: &str,
    ) -> Result<(Self, TestCommandInputForm), Vec<TestCommandViolation>> {
        let form = if argv.first().map(String::as_str) == Some("cargo")
            && argv.get(1).map(String::as_str) == Some("test")
        {
            Some(TestCommandInputForm::CompleteArgv)
        } else if argv.first().map(String::as_str) == Some("test") {
            Some(TestCommandInputForm::RunnerRelativeArgv)
        } else {
            None
        };
        let mut canonical_argv = Vec::with_capacity(argv.len().saturating_add(usize::from(
            matches!(form, Some(TestCommandInputForm::RunnerRelativeArgv)),
        )));
        if matches!(form, Some(TestCommandInputForm::RunnerRelativeArgv)) {
            canonical_argv.push("cargo".to_owned());
        }
        canonical_argv.extend_from_slice(argv);

        let mut violations = Vec::new();
        if runner != "cargo" {
            violations.push(TestCommandViolation::RunnerNotCargo);
        }
        if form.is_none() {
            violations.push(TestCommandViolation::ArgvNotDirectCargoTest);
        }
        if canonical_argv.len() < 3
            || canonical_argv.len() > MAX_TEST_COMMAND_ARGV
            || canonical_argv.iter().any(|argument| {
                argument.trim().is_empty()
                    || argument.len() > MAX_TEST_COMMAND_ARGUMENT_BYTES
                    || test_command_text_is_unsafe(argument)
            })
        {
            violations.push(TestCommandViolation::ArgvInvalidOrUnsafe);
        }
        if canonical_argv.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--workspace" | "--all" | "--all-targets" | "--manifest-path" | "--config"
            )
        }) {
            violations.push(TestCommandViolation::ArgvScopeNotFocused);
        }
        if canonical_argv
            .iter()
            .any(|argument| argument.contains(['|', ';', '>', '<', '\n', '\r']) || argument == "&&")
        {
            violations.push(TestCommandViolation::ArgvContainsShellSyntax);
        }
        if test_identifier.trim().is_empty()
            || test_identifier.len() > MAX_TEST_COMMAND_ARGUMENT_BYTES
            || test_identifier.starts_with('-')
            || !test_identifier.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'.' | b'-' | b'/')
            })
            || test_command_text_is_unsafe(test_identifier)
        {
            violations.push(TestCommandViolation::IdentifierInvalidOrUnsafe);
        }
        let identifier_filter = test_identifier.rsplit("::").next().unwrap_or(test_identifier);
        if !canonical_argv
            .iter()
            .any(|argument| argument == test_identifier || argument == identifier_filter)
        {
            violations.push(TestCommandViolation::IdentifierNotInArgv);
        }
        violations.sort();
        violations.dedup();
        if !violations.is_empty() {
            return Err(violations);
        }

        Ok((
            Self {
                runner: "cargo".to_owned(),
                argv: canonical_argv,
                test_identifier: test_identifier.to_owned(),
            },
            form.expect("a valid command has a recognized argv form"),
        ))
    }

    pub fn from_canonical_parts(
        runner: &str,
        argv: &[String],
        test_identifier: &str,
    ) -> Result<Self, Vec<TestCommandViolation>> {
        let (command, form) = Self::from_worker_parts(runner, argv, test_identifier)?;
        if form == TestCommandInputForm::RunnerRelativeArgv {
            return Err(vec![TestCommandViolation::ArgvNotCanonical]);
        }
        Ok(command)
    }

    pub fn runner(&self) -> &str {
        &self.runner
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn test_identifier(&self) -> &str {
        &self.test_identifier
    }

    pub fn into_parts(self) -> (String, Vec<String>, String) {
        (self.runner, self.argv, self.test_identifier)
    }
}

fn test_command_text_is_unsafe(value: &str) -> bool {
    value.chars().any(char::is_control)
        || ["@@need", "@@end", "[NEEDLE_CONTEXT]", "[/NEEDLE_CONTEXT]"]
            .iter()
            .any(|marker| value.contains(marker))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestPlan {
    pub runner: String,
    pub argv: Vec<String>,
    pub cwd_relative: String,
    pub test_identifier: String,
    pub requires_approval: bool,
    pub execution_evidence_id: Option<String>,
}

impl TestPlan {
    pub fn test_command(&self) -> Result<TestCommand, Vec<TestCommandViolation>> {
        TestCommand::from_canonical_parts(&self.runner, &self.argv, &self.test_identifier)
    }

    /// Stable identity for a certified focused test.  Execution provenance is
    /// deliberately excluded: the same canonical plan remains the same plan
    /// when a later verifier run captures a different evidence item.
    pub fn identity_digest(&self) -> Digest {
        let canonical = self.test_command().ok();
        let runner = canonical.as_ref().map(TestCommand::runner).unwrap_or(&self.runner);
        let argv = canonical.as_ref().map(TestCommand::argv).unwrap_or(&self.argv);
        let identifier =
            canonical.as_ref().map(TestCommand::test_identifier).unwrap_or(&self.test_identifier);
        let mut hasher = crate::CanonicalHasher::new(b"needle-test-plan-identity-v2");
        hasher.field_str(runner);
        hasher.field_u16(argv.len().try_into().unwrap_or(u16::MAX));
        for argument in argv {
            hasher.field_str(argument);
        }
        hasher.field_str(&self.cwd_relative);
        hasher.field_str(identifier);
        hasher.finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBrief {
    pub summary: String,
    pub locations: Vec<CodeLocation>,
    pub behavior: Option<BehaviorTrace>,
    pub test_plan: Option<TestPlan>,
    pub claims: BTreeMap<String, Vec<String>>,
}

impl EvidenceBrief {
    pub fn deterministic_projection(&self) -> String {
        let mut output = String::from("[NEEDLE_CONTEXT]\n");
        output.push_str(self.summary.trim());
        output.push('\n');
        for location in &self.locations {
            output.push_str("- ");
            output.push_str(&location.path);
            if let Some(symbol) = location.symbol.as_deref() {
                output.push_str(" :: ");
                output.push_str(symbol);
            }
            output.push('\n');
        }
        for (subject, facts) in &self.claims {
            for fact in facts {
                output.push_str("- ");
                output.push_str(subject);
                output.push_str(": ");
                output.push_str(fact.trim());
                output.push('\n');
            }
        }
        if let Some(plan) = &self.test_plan {
            output.push_str("test: ");
            output.push_str(&plan.argv.join(" "));
            output.push('\n');
        }
        output.push_str("[/NEEDLE_CONTEXT]");
        output
    }
}

pub fn built_in_route_plans() -> Vec<RoutePlan> {
    [
        (
            "locate.implementation",
            vec![
                ("location", "code-location", vec![]),
                ("test", "test-plan", vec![]),
                ("brief", "evidence-brief", vec!["location", "test"]),
            ],
        ),
        (
            "trace.state-flow",
            vec![
                ("location", "code-location", vec![]),
                ("behavior", "behavior-trace", vec!["location"]),
                ("test", "test-plan", vec![]),
                ("brief", "evidence-brief", vec!["location", "behavior", "test"]),
            ],
        ),
        (
            "tests.relevant",
            vec![("test", "test-plan", vec![]), ("brief", "evidence-brief", vec!["test"])],
        ),
    ]
    .into_iter()
    .map(|(id, nodes)| {
        RoutePlan::new(
            id,
            1,
            NeedKey::new(id).expect("built-in key"),
            nodes
                .into_iter()
                .map(|(node, operator, dependencies)| PlanNode {
                    id: node.to_owned(),
                    operator_id: operator.to_owned(),
                    depends_on: dependencies.into_iter().map(str::to_owned).collect(),
                })
                .collect(),
        )
        .expect("valid built-in plan")
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ArtifactRequest {
        ArtifactRequest {
            contract_id: "evidence-brief".to_owned(),
            contract_revision: 1,
            repository_id: Digest::blake3("repo"),
            source_snapshot_digest: Digest::blake3("source"),
            route_key: NeedKey::new("locate.implementation").unwrap(),
            normalized_request: "find\r\nimplementation".to_owned(),
            semantic_fragment_id: None,
            input_artifact_ids: vec![Digest::blake3("b"), Digest::blake3("a")],
        }
    }

    #[test]
    fn semantic_request_identity_excludes_execution_profile() {
        let left = request();
        let mut right = request();
        right.input_artifact_ids.reverse();
        right.normalized_request = "find\nimplementation".to_owned();
        assert_eq!(left.id(), right.id());
    }

    #[test]
    fn v04_artifact_request_identity_excludes_route_and_normalizes_lines() {
        let left = request();
        let mut right = request();
        right.route_key = NeedKey::new("trace.state-flow").unwrap();
        right.normalized_request = "find\nimplementation".to_owned();
        right.input_artifact_ids.reverse();
        assert_eq!(left.semantic_id(), right.semantic_id());
        assert_ne!(left.id(), right.id());

        let mut different_fragment = left.clone();
        different_fragment.semantic_fragment_id =
            Some(NeedFragmentId(Digest::blake3(b"different-fragment")));
        assert_ne!(left.semantic_id(), different_fragment.semantic_id());
    }

    #[test]
    fn semantic_artifact_identity_excludes_origin_and_cache_scope() {
        let payload = SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: None,
                byte_start: Some(0),
                byte_end: Some(1),
            }],
            gaps: Vec::new(),
        };
        let exact = ArtifactContract::semantic(
            "needle.semantic.code-location",
            2,
            ArtifactKind::code_location(),
            CacheScope::SnapshotExact,
        );
        let worktree = ArtifactContract::semantic(
            "needle.semantic.code-location",
            2,
            ArtifactKind::code_location(),
            CacheScope::WorktreeSemantic,
        );
        assert_eq!(exact.definition_digest, worktree.definition_digest);
        assert_eq!(
            payload.canonical_artifact_id(exact.definition_digest).unwrap(),
            payload.canonical_artifact_id(worktree.definition_digest).unwrap()
        );
    }

    #[test]
    fn built_in_plans_are_bounded_and_acyclic() {
        let plans = built_in_route_plans();
        assert_eq!(plans.len(), 3);
        assert!(plans.iter().all(|plan| plan.nodes.len() <= MAX_ROUTE_PLAN_NODES));
        assert_eq!(plans[1].nodes.last().unwrap().depends_on.len(), 3);
    }

    #[test]
    fn later_or_unknown_dependency_is_rejected() {
        let error = RoutePlan::new(
            "bad",
            1,
            NeedKey::new("bad").unwrap(),
            vec![PlanNode {
                id: "first".to_owned(),
                operator_id: "operator".to_owned(),
                depends_on: vec!["later".to_owned()],
            }],
        )
        .unwrap_err();
        assert!(matches!(error, RoutePlanError::InvalidDependency { .. }));
    }

    #[test]
    fn worktree_semantic_requires_closed_observed_dependencies() {
        let manifest = DependencyManifest {
            scope: CacheScope::WorktreeSemantic,
            observed_files_complete: true,
            dependencies: vec![Dependency {
                path: "src/lib.rs".to_owned(),
                content_digest: Digest::blake3("file"),
                byte_start: None,
                byte_end: None,
                claims: vec!["claim".to_owned()],
            }],
            gaps: Vec::new(),
        };
        assert!(manifest.supports_worktree_semantic());
    }

    #[test]
    fn focused_test_command_converges_complete_and_runner_relative_argv() {
        let complete = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--test".to_owned(),
            "integration".to_owned(),
            "f416_crlf".to_owned(),
        ];
        let relative = complete[1..].to_vec();
        let (complete, complete_form) =
            TestCommand::from_worker_parts("cargo", &complete, "feature::f416_crlf").unwrap();
        let (relative, relative_form) =
            TestCommand::from_worker_parts("cargo", &relative, "feature::f416_crlf").unwrap();

        assert_eq!(complete_form, TestCommandInputForm::CompleteArgv);
        assert_eq!(relative_form, TestCommandInputForm::RunnerRelativeArgv);
        assert_eq!(complete, relative);
        assert_eq!(relative.argv().first().map(String::as_str), Some("cargo"));
        assert!(
            TestCommand::from_canonical_parts(
                relative.runner(),
                relative.argv(),
                relative.test_identifier()
            )
            .is_ok()
        );
    }

    #[test]
    fn focused_test_command_rejects_unsafe_broad_or_unbound_inputs() {
        let violations = TestCommand::from_worker_parts(
            "cargo",
            &["test".to_owned(), "--workspace".to_owned(), ";".to_owned()],
            "--workspace",
        )
        .unwrap_err();

        assert!(violations.contains(&TestCommandViolation::ArgvScopeNotFocused));
        assert!(violations.contains(&TestCommandViolation::ArgvContainsShellSyntax));
        assert!(violations.contains(&TestCommandViolation::IdentifierInvalidOrUnsafe));
    }

    #[test]
    fn test_plan_identity_is_stable_and_excludes_execution_provenance() {
        let mut plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let first = plan.identity_digest();
        plan.execution_evidence_id = Some("evidence-1".to_owned());
        assert_eq!(first, plan.identity_digest());
        plan.argv.push("--".to_owned());
        assert_ne!(first, plan.identity_digest());
    }
}
