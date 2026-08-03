use needle_core::{
    CanonicalHasher, Digest, Facet, NEED_IR_FORMAT_REVISION, NeedIr, NeedKey, NeedRequest,
    ObligationExpression, PredicateKind, SubjectExpression, SubjectKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) const MAX_MCP_REQUEST_BYTES: usize = 16 * 1024;
const MAX_SUBJECT_BYTES: usize = 512;
const MAX_TASK_BYTES: usize = 8 * 1024;
const MAX_CAPABILITIES: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpNeedContextRequest {
    pub route: String,
    pub subject: McpSubject,
    #[serde(default)]
    pub required: Vec<McpCapability>,
    #[serde(default)]
    pub preferred: Vec<McpCapability>,
    #[serde(default)]
    pub world: McpWorld,
    pub task: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpSubject {
    pub kind: McpSubjectKind,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpSubjectKind {
    Symbol,
    CliOption,
    ConfigurationKey,
    Test,
    File,
    Module,
    Behavior,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpCapability {
    ImplementationLocation {
        #[serde(default)]
        polarity: Option<Positive>,
        #[serde(default)]
        selection: Option<Primary>,
        #[serde(default)]
        granularity: Option<ExactLocation>,
    },
    RuntimeFlow {
        #[serde(default)]
        scenario: Option<DefaultScenario>,
        #[serde(default)]
        completeness: Option<ContractComplete>,
        #[serde(default)]
        granularity: Option<Stepwise>,
    },
    FocusedTests {
        #[serde(default)]
        polarity: Option<Positive>,
        #[serde(default)]
        selection: Option<Representative>,
        #[serde(default)]
        completeness: Option<OpenWorld>,
    },
}

macro_rules! singleton_enum {
    ($name:ident, $variant:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub(crate) enum $name {
            $variant,
        }
    };
}

singleton_enum!(Positive, Positive);
singleton_enum!(Primary, Primary);
singleton_enum!(ExactLocation, ExactLocation);
singleton_enum!(DefaultScenario, Default);
singleton_enum!(ContractComplete, ContractComplete);
singleton_enum!(Stepwise, Stepwise);
singleton_enum!(Representative, Representative);
singleton_enum!(OpenWorld, OpenWorld);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpWorld {
    #[serde(default = "current")]
    pub source: String,
    #[serde(default = "current")]
    pub platform: String,
    #[serde(default = "default_features")]
    pub features: String,
    #[serde(default)]
    pub configuration_digest: Option<String>,
    #[serde(default)]
    pub toolchain_digest: Option<String>,
}

impl Default for McpWorld {
    fn default() -> Self {
        Self {
            source: current(),
            platform: current(),
            features: default_features(),
            configuration_digest: None,
            toolchain_digest: None,
        }
    }
}

fn current() -> String {
    "current".to_owned()
}

fn default_features() -> String {
    "default".to_owned()
}

#[derive(Clone, Debug)]
pub(crate) struct MappedNeedContext {
    pub request: McpNeedContextRequest,
    pub need_ir: NeedIr,
    pub compatibility_request: NeedRequest,
    pub request_digest: Digest,
    pub canonical_json: String,
}

impl McpNeedContextRequest {
    pub(crate) fn validate_and_map(
        mut self,
        enabled_routes: &[String],
        encoded_bytes: usize,
    ) -> Result<MappedNeedContext, String> {
        if encoded_bytes > MAX_MCP_REQUEST_BYTES {
            return Err("arguments exceed the 16 KiB request bound".to_owned());
        }
        if !enabled_routes.iter().any(|route| route == &self.route) {
            return Err("route is not enabled in this session".to_owned());
        }
        if self.subject.name.is_empty() || self.subject.name.len() > MAX_SUBJECT_BYTES {
            return Err("subject name must contain 1 to 512 UTF-8 bytes".to_owned());
        }
        if self.task.trim().is_empty() || self.task.len() > MAX_TASK_BYTES {
            return Err("task must contain 1 to 8192 UTF-8 bytes".to_owned());
        }
        if self.required.len() > MAX_CAPABILITIES || self.preferred.len() > MAX_CAPABILITIES {
            return Err("required and preferred each allow at most three capabilities".to_owned());
        }
        validate_capabilities(&self.required, &self.preferred)?;
        validate_world(&self.world)?;
        self.required.sort_by_key(McpCapability::predicate);
        self.preferred.sort_by_key(McpCapability::predicate);

        let route = NeedKey::new(self.route.clone()).map_err(|error| error.to_string())?;
        let mut world = vec![
            Facet { key: "features".to_owned(), value: self.world.features.clone() },
            Facet { key: "platform".to_owned(), value: self.world.platform.clone() },
            Facet { key: "source".to_owned(), value: self.world.source.clone() },
        ];
        if let Some(value) = &self.world.configuration_digest {
            world.push(Facet { key: "configuration".to_owned(), value: value.clone() });
        }
        if let Some(value) = &self.world.toolchain_digest {
            world.push(Facet { key: "toolchain".to_owned(), value: value.clone() });
        }
        world.sort();
        let need_ir = NeedIr {
            route_hint: Some(route.clone()),
            subjects: vec![SubjectExpression {
                kind: self.subject.kind.into(),
                canonical_name: self.subject.name.clone(),
            }],
            required: self.required.iter().map(McpCapability::obligation).collect(),
            preferred: self.preferred.iter().map(McpCapability::obligation).collect(),
            semantic_constraints: Vec::new(),
            world,
            input_artifacts: Vec::new(),
            projection: Vec::new(),
            body: self.task.clone(),
            format_revision: NEED_IR_FORMAT_REVISION,
        };
        let request_digest = self.canonical_digest();
        let canonical_json = serde_json::to_string(&self).map_err(|error| error.to_string())?;
        let compatibility_request = NeedRequest { key: route, body: self.task.clone() };
        Ok(MappedNeedContext {
            request: self,
            need_ir,
            compatibility_request,
            request_digest,
            canonical_json,
        })
    }

    fn canonical_digest(&self) -> Digest {
        let mut hasher = CanonicalHasher::new(b"mcp-need-context-request");
        hasher.field_str(&self.route);
        hasher.field_str(self.subject.kind.as_str());
        hasher.field_str(&self.subject.name);
        hash_capabilities(&mut hasher, &self.required);
        hash_capabilities(&mut hasher, &self.preferred);
        hasher.field_str(&self.world.source);
        hasher.field_str(&self.world.platform);
        hasher.field_str(&self.world.features);
        hash_optional_digest(&mut hasher, self.world.configuration_digest.as_deref());
        hash_optional_digest(&mut hasher, self.world.toolchain_digest.as_deref());
        hasher.field_normalized_lines(&self.task);
        hasher.finish()
    }
}

impl McpSubjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::CliOption => "cli_option",
            Self::ConfigurationKey => "configuration_key",
            Self::Test => "test",
            Self::File => "file",
            Self::Module => "module",
            Self::Behavior => "behavior",
        }
    }
}

impl From<McpSubjectKind> for SubjectKind {
    fn from(value: McpSubjectKind) -> Self {
        match value {
            McpSubjectKind::Symbol => Self::Symbol,
            McpSubjectKind::CliOption => Self::CliOption,
            McpSubjectKind::ConfigurationKey => Self::ConfigurationKey,
            McpSubjectKind::Test => Self::Test,
            McpSubjectKind::File => Self::File,
            McpSubjectKind::Module => Self::Module,
            McpSubjectKind::Behavior => Self::Behavior,
        }
    }
}

impl McpCapability {
    pub(crate) fn predicate(&self) -> PredicateKind {
        match self {
            Self::ImplementationLocation { .. } => PredicateKind::ImplementationLocation,
            Self::RuntimeFlow { .. } => PredicateKind::RuntimeFlow,
            Self::FocusedTests { .. } => PredicateKind::FocusedTests,
        }
    }

    fn obligation(&self) -> ObligationExpression {
        let mut facets = Vec::with_capacity(3);
        match self {
            Self::ImplementationLocation { polarity, selection, granularity } => {
                push_present(&mut facets, "polarity", polarity.map(|_| "positive"));
                push_present(&mut facets, "selection", selection.map(|_| "primary"));
                push_present(&mut facets, "granularity", granularity.map(|_| "exact-location"));
            }
            Self::RuntimeFlow { scenario, completeness, granularity } => {
                push_present(&mut facets, "scenario", scenario.map(|_| "default"));
                push_present(
                    &mut facets,
                    "completeness",
                    completeness.map(|_| "contract-complete"),
                );
                push_present(&mut facets, "granularity", granularity.map(|_| "stepwise"));
            }
            Self::FocusedTests { polarity, selection, completeness } => {
                push_present(&mut facets, "polarity", polarity.map(|_| "positive"));
                push_present(&mut facets, "selection", selection.map(|_| "representative"));
                push_present(&mut facets, "completeness", completeness.map(|_| "open-world"));
            }
        }
        facets.sort();
        ObligationExpression { predicate: self.predicate(), facets }
    }
}

fn push_present(facets: &mut Vec<Facet>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        facets.push(Facet { key: key.to_owned(), value: value.to_owned() });
    }
}

fn validate_capabilities(
    required: &[McpCapability],
    preferred: &[McpCapability],
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for capability in required.iter().chain(preferred) {
        if !seen.insert(capability.predicate()) {
            return Err("capability kinds must be unique across required and preferred".to_owned());
        }
    }
    Ok(())
}

fn validate_world(world: &McpWorld) -> Result<(), String> {
    if world.source.is_empty() || world.platform.is_empty() || world.features.is_empty() {
        return Err("world selectors must be non-empty".to_owned());
    }
    for (label, value) in [
        ("configuration_digest", world.configuration_digest.as_deref()),
        ("toolchain_digest", world.toolchain_digest.as_deref()),
    ] {
        if let Some(value) = value
            && (value.len() != 67
                || !value.starts_with("b3:")
                || !value.as_bytes()[3..]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                || Digest::parse(value).is_err())
        {
            return Err(format!("{label} must be a valid b3: digest"));
        }
    }
    Ok(())
}

fn hash_optional_digest(hasher: &mut CanonicalHasher, value: Option<&str>) {
    if let Some(value) = value {
        hasher.field_u8(1);
        hasher.field_str(value);
    } else {
        hasher.field_u8(0);
    }
}

fn hash_capabilities(hasher: &mut CanonicalHasher, capabilities: &[McpCapability]) {
    for capability in capabilities {
        let obligation = capability.obligation();
        hasher.field_str(obligation.predicate.as_str());
        for facet in obligation.facets {
            hasher.field_str(&facet.key);
            hasher.field_str(&facet.value);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpNeedContextResponse {
    pub status: String,
    pub route: String,
    pub subject: McpSubject,
    pub need_id: String,
    pub step: McpStep,
    pub satisfied: Vec<String>,
    pub missing: Vec<String>,
    pub resolution: McpResolution,
    pub reuse_unit: String,
    pub claim_ids: Vec<String>,
    pub cache_hit: bool,
    pub worker_spawned: bool,
    pub calibration: bool,
    pub result_digest: String,
    pub context: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpStep {
    pub ordinal: u8,
    pub relation: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum McpResolution {
    ExactHit {
        artifact_ids: Vec<String>,
        certificate_id: Option<String>,
        plan_id: Option<String>,
    },
    CoverageHit {
        artifact_ids: Vec<String>,
        certificate_id: String,
        plan_id: String,
    },
    CompositeHit {
        artifact_ids: Vec<String>,
        certificate_id: Option<String>,
        plan_id: Option<String>,
    },
    ClaimHit {
        artifact_ids: Vec<String>,
        claim_ids: Vec<String>,
        claim_set_certificate_id: String,
        plan_id: String,
    },
    ClaimCompositeHit {
        artifact_ids: Vec<String>,
        claim_ids: Vec<String>,
        claim_set_certificate_id: String,
        plan_id: String,
    },
    PartialHit {
        artifact_ids: Vec<String>,
        claim_ids: Vec<String>,
        invalidated_nodes: Vec<String>,
        plan_id: Option<String>,
    },
    Miss,
    Stale {
        artifact_ids: Vec<String>,
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

pub(crate) fn input_schema(enabled_routes: &[String]) -> Value {
    let capability_variants = json!([
        capability_schema(
            "implementation_location",
            &[
                ("polarity", "positive"),
                ("selection", "primary"),
                ("granularity", "exact_location"),
            ]
        ),
        capability_schema(
            "runtime_flow",
            &[
                ("scenario", "default"),
                ("completeness", "contract_complete"),
                ("granularity", "stepwise"),
            ]
        ),
        capability_schema(
            "focused_tests",
            &[
                ("polarity", "positive"),
                ("selection", "representative"),
                ("completeness", "open_world"),
            ]
        )
    ]);
    json!({
        "type": "object",
        "properties": {
            "route": {"type": "string", "enum": enabled_routes},
            "subject": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": [
                        "symbol", "cli_option", "configuration_key", "test", "file", "module",
                        "behavior"
                    ]},
                    "name": {"type": "string", "minLength": 1, "maxLength": 512}
                },
                "required": ["kind", "name"],
                "additionalProperties": false
            },
            "required": {"type": "array", "maxItems": 3, "items": {"oneOf": capability_variants}},
            "preferred": {"type": "array", "maxItems": 3, "items": {"oneOf": capability_variants}},
            "world": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "default": "current"},
                    "platform": {"type": "string", "default": "current"},
                    "features": {"type": "string", "default": "default"},
                    "configuration_digest": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
                    "toolchain_digest": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"}
                },
                "additionalProperties": false
            },
            "task": {"type": "string", "minLength": 1, "maxLength": 8192}
        },
        "required": ["route", "subject", "task"],
        "additionalProperties": false
    })
}

fn capability_schema(kind: &str, fields: &[(&str, &str)]) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("kind".to_owned(), json!({"const": kind}));
    for (field, value) in fields {
        properties.insert((*field).to_owned(), json!({"type": "string", "enum": [value]}));
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["kind"],
        "additionalProperties": false
    })
}

pub(crate) fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["hit", "generated", "bypass"]},
            "route": {"type": "string"},
            "subject": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string"},
                    "name": {"type": "string"}
                },
                "required": ["kind", "name"],
                "additionalProperties": false
            },
            "need_id": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
            "step": {
                "type": "object",
                "properties": {
                    "ordinal": {"type": "integer", "minimum": 1, "maximum": 8},
                    "relation": {"type": "string", "enum": [
                        "repeat", "residual", "extension", "overlap", "independent", "incompatible"
                    ]}
                },
                "required": ["ordinal", "relation"],
                "additionalProperties": false
            },
            "satisfied": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
            "missing": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
            "resolution": resolution_schema(),
            "reuse_unit": {"type": "string", "enum": ["artifact", "claim", "mixed", "none"]},
            "claim_ids": {
                "type": "array",
                "items": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
                "maxItems": 16
            },
            "cache_hit": {"type": "boolean"},
            "worker_spawned": {"type": "boolean"},
            "calibration": {"type": "boolean"},
            "result_digest": {"type": "string", "pattern": "^b3:[0-9a-f]{64}$"},
            "context": {"type": "string", "maxLength": 65536}
        },
        "required": [
            "status", "route", "subject", "need_id", "step", "satisfied", "missing",
            "resolution", "reuse_unit", "claim_ids", "cache_hit", "worker_spawned",
            "calibration", "result_digest", "context"
        ],
        "additionalProperties": false
    })
}

fn resolution_schema() -> Value {
    let hit = |kind: &str, certificate_required: bool| {
        let mut required = vec!["kind", "artifact_ids"];
        if certificate_required {
            required.extend(["certificate_id", "plan_id"]);
        }
        json!({
            "type": "object",
            "properties": {
                "kind": {"const": kind},
                "artifact_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                "certificate_id": {"type": ["string", "null"]},
                "plan_id": {"type": ["string", "null"]}
            },
            "required": required,
            "additionalProperties": false
        })
    };
    let reason = |kind: &str| {
        json!({
            "type": "object",
            "properties": {"kind": {"const": kind}, "reason": {"type": "string"}},
            "required": ["kind", "reason"],
            "additionalProperties": false
        })
    };
    json!({"oneOf": [
        hit("exact_hit", false),
        hit("coverage_hit", true),
        hit("composite_hit", false),
        {
            "type": "object",
            "properties": {
                "kind": {"const": "claim_hit"},
                "artifact_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                "claim_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                "claim_set_certificate_id": {"type": "string"},
                "plan_id": {"type": "string"}
            },
            "required": [
                "kind", "artifact_ids", "claim_ids", "claim_set_certificate_id", "plan_id"
            ],
            "additionalProperties": false
        },
        {
            "type": "object",
            "properties": {
                "kind": {"const": "claim_composite_hit"},
                "artifact_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                "claim_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                "claim_set_certificate_id": {"type": "string"},
                "plan_id": {"type": "string"}
            },
            "required": [
                "kind", "artifact_ids", "claim_ids", "claim_set_certificate_id", "plan_id"
            ],
            "additionalProperties": false
        },
        {
            "type": "object",
            "properties": {
                "kind": {"const": "partial_hit"},
                "artifact_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                "claim_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                "invalidated_nodes": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                "plan_id": {"type": ["string", "null"]}
            },
            "required": ["kind", "artifact_ids", "claim_ids", "invalidated_nodes", "plan_id"],
            "additionalProperties": false
        },
        {"type": "object", "properties": {"kind": {"const": "miss"}}, "required": ["kind"], "additionalProperties": false},
        {
            "type": "object",
            "properties": {
                "kind": {"const": "stale"},
                "artifact_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 1},
                "reason": {"type": "string"}
            },
            "required": ["kind", "artifact_ids", "reason"],
            "additionalProperties": false
        },
        reason("rejected"), reason("ambiguous"), reason("contradicted"), reason("bypass")
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> McpNeedContextRequest {
        serde_json::from_value(json!({
            "route": "trace.state-flow",
            "subject": {"kind": "cli_option", "name": "--crlf"},
            "required": [
                {"kind": "implementation_location", "selection": "primary"},
                {"kind": "runtime_flow", "scenario": "default"}
            ],
            "preferred": [{"kind": "focused_tests", "selection": "representative"}],
            "task": "Trace how the option changes matching."
        }))
        .unwrap()
    }

    #[test]
    fn maps_json_directly_to_need_ir_without_transport_text() {
        let mapped = request().validate_and_map(&["trace.state-flow".to_owned()], 512).unwrap();
        assert_eq!(mapped.need_ir.route_hint.unwrap().as_str(), "trace.state-flow");
        assert_eq!(mapped.need_ir.subjects[0].canonical_name, "--crlf");
        assert_eq!(mapped.need_ir.required.len(), 2);
        assert_eq!(mapped.need_ir.preferred.len(), 1);
        assert!(!mapped.canonical_json.contains("@@need"));
    }

    #[test]
    fn rejects_unknown_fields_duplicates_and_malformed_digests() {
        assert!(
            serde_json::from_value::<McpNeedContextRequest>(json!({
                "route": "trace.state-flow",
                "subject": {"kind": "cli_option", "name": "--crlf", "extra": true},
                "task": "trace"
            }))
            .is_err()
        );
        let mut duplicate = request();
        duplicate.preferred = duplicate.required.clone();
        assert!(duplicate.validate_and_map(&["trace.state-flow".to_owned()], 512).is_err());
        let mut digest = request();
        digest.world.configuration_digest = Some("not-a-digest".to_owned());
        assert!(digest.validate_and_map(&["trace.state-flow".to_owned()], 512).is_err());
    }

    #[test]
    fn schemas_are_closed_and_never_describe_hook_syntax() {
        let input = input_schema(&["locate.implementation".to_owned()]);
        let output = output_schema();
        assert_eq!(input["additionalProperties"], false);
        assert_eq!(input["properties"]["subject"]["additionalProperties"], false);
        assert_eq!(output["properties"]["reuse_unit"]["enum"][1], "claim");
        assert!(output["required"].as_array().unwrap().iter().any(|field| field == "claim_ids"));
        assert!(output["properties"]["resolution"]["oneOf"].as_array().unwrap().iter().any(
            |variant| {
                variant["properties"]["kind"]["const"] == "claim_hit"
                    && variant["additionalProperties"] == false
            }
        ));
        assert!(output["properties"]["resolution"]["oneOf"].as_array().unwrap().iter().any(
            |variant| {
                variant["properties"]["kind"]["const"] == "claim_composite_hit"
                    && variant["additionalProperties"] == false
            }
        ));
        assert!(output["properties"]["resolution"]["oneOf"].as_array().unwrap().iter().any(
            |variant| {
                variant["properties"]["kind"]["const"] == "partial_hit"
                    && variant["properties"]["claim_ids"]["maxItems"] == 16
                    && variant["additionalProperties"] == false
            }
        ));
        assert!(!input.to_string().contains("@@need"));
        assert!(!output.to_string().contains("@@need"));
    }

    #[test]
    fn hook_and_json_transport_compile_to_the_same_semantic_identity() {
        let lineage = Digest::blake3(b"repository-lineage");
        let contract = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == "trace.state-flow")
            .unwrap();
        let mapped = request().validate_and_map(&["trace.state-flow".to_owned()], 512).unwrap();
        let hook = NeedIr::parse(
            "@@need\n@route trace.state-flow\n@subject cli-option:\"--crlf\"\n\
             @require implementation-location selection=primary\n\
             @require runtime-flow scenario=default\n\
             @prefer focused-tests selection=representative\n\
             @world source=current platform=current features=default\n\
             \nTrace how the option changes matching.\n@@end",
        )
        .unwrap()
        .unwrap();
        let json_need = needle_core::compile_need(&mapped.need_ir, lineage, &contract).unwrap();
        let hook_need = needle_core::compile_need(&hook, lineage, &contract).unwrap();
        assert_eq!(json_need.id, hook_need.id);

        let mut reworded = request();
        reworded.task = "Explain the matching effect using the same evidence.".to_owned();
        let reworded = reworded.validate_and_map(&["trace.state-flow".to_owned()], 512).unwrap();
        let reworded = needle_core::compile_need(&reworded.need_ir, lineage, &contract).unwrap();
        assert_eq!(json_need.id, reworded.id);

        let mut changed_world = request();
        changed_world.world.features = "all".to_owned();
        let changed_world =
            changed_world.validate_and_map(&["trace.state-flow".to_owned()], 512).unwrap();
        let changed_world =
            needle_core::compile_need(&changed_world.need_ir, lineage, &contract).unwrap();
        assert_ne!(json_need.id, changed_world.id);

        let mut changed_obligations = request();
        let focused = changed_obligations.preferred.pop().unwrap();
        changed_obligations.required.push(focused);
        let changed_obligations =
            changed_obligations.validate_and_map(&["trace.state-flow".to_owned()], 512).unwrap();
        let changed_obligations =
            needle_core::compile_need(&changed_obligations.need_ir, lineage, &contract).unwrap();
        assert_ne!(json_need.id, changed_obligations.id);
    }
}
