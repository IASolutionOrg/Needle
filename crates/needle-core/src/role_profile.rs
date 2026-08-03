use crate::{
    CanonicalHasher, Digest, HARD_MAX_NEEDS_PER_TASK, HARD_RESULT_TOKENS, NeedKey, WorkerProfile,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use thiserror::Error;

pub const ROLE_PROFILE_MAX_TURNS: u8 = 8;
pub const ROLE_PROFILE_MAX_COST_MICROUSD: u64 = 1_000_000_000;
pub const ROLE_PROFILE_MAX_TIMEOUT_SECONDS: u64 = 3_600;
pub const ROLE_PROFILE_MAX_MODEL_BYTES: usize = 128;
pub const ROLE_PROFILE_STATE_GENERATION_INITIAL: u64 = 0;

#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RoleProfileId(String);

impl RoleProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, RoleProfileValidationError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty() || bytes.len() > 64 {
            return Err(RoleProfileValidationError::ProfileId(
                "profile id must be between 1 and 64 ASCII bytes".to_owned(),
            ));
        }
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(RoleProfileValidationError::ProfileId(
                "profile id must start with a lowercase ASCII letter or digit".to_owned(),
            ));
        }
        if bytes.iter().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(*byte, b'.' | b'_' | b'-'))
        }) {
            return Err(RoleProfileValidationError::ProfileId(
                "profile id contains an invalid character".to_owned(),
            ));
        }
        if value.contains("..")
            || value.contains('/')
            || value.contains('\\')
            || value.starts_with('~')
            || value.contains(':')
            || credential_prefix(&value)
        {
            return Err(RoleProfileValidationError::ProfileId(
                "profile id contains path or credential-like syntax".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RoleProfileId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for RoleProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RoleProfileId").field(&self.0).finish()
    }
}

impl fmt::Display for RoleProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for RoleProfileId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RoleProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRole {
    Explorer,
    Implementer,
    TestRunner,
    Reviewer,
    Verifier,
    Auditor,
}

impl CodexRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Implementer => "implementer",
            Self::TestRunner => "test_runner",
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
            Self::Auditor => "auditor",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Explorer => 0,
            Self::Implementer => 1,
            Self::TestRunner => 2,
            Self::Reviewer => 3,
            Self::Verifier => 4,
            Self::Auditor => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexHost {
    Codex,
}

impl CodexHost {
    pub const fn as_str(self) -> &'static str {
        "codex"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
        }
    }
}

pub type CodexReasoning = ReasoningLevel;
pub type RoleProfileReasoning = ReasoningLevel;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Default,
    Priority,
}

impl ServiceTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Priority => "priority",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Priority => 1,
        }
    }
}

pub type RoleProfileServiceTier = ServiceTier;
pub type RoleProfileHost = CodexHost;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    ReadOnly,
    IsolatedWrite,
}

impl ToolPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::IsolatedWrite => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPolicy {
    Denied,
    ReadOnly,
    CertifiedTests,
}

impl CommandPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::Denied => 0,
            Self::ReadOnly => 1,
            Self::CertifiedTests => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    ReadOnlyCheckout,
    DisposableCheckout,
}

impl FilesystemPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::ReadOnlyCheckout => 0,
            Self::DisposableCheckout => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Denied,
}

impl NetworkPolicy {
    const fn tag(self) -> u8 {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestPolicy {
    Disabled,
    Certified,
}

impl TestPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Certified => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairPolicy {
    None,
    Once,
}

impl RepairPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Once => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    Disabled,
    Native,
}

impl FallbackPolicy {
    const fn tag(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Native => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileBudget {
    pub max_turns: u8,
    pub max_output_tokens: u32,
    pub max_cost_microusd: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileDefinitionInput {
    pub profile_id: RoleProfileId,
    pub role: CodexRole,
    pub host: CodexHost,
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub service_tier: ServiceTier,
    pub timeout_seconds: u64,
    pub budget: RoleProfileBudget,
    pub prompt_profile_digest: Digest,
    pub output_contract_digest: Digest,
    pub tool_policy: ToolPolicy,
    pub command_policy: CommandPolicy,
    pub filesystem_policy: FilesystemPolicy,
    pub network_policy: NetworkPolicy,
    pub test_policy: TestPolicy,
    pub repair_policy: RepairPolicy,
    pub fallback_policy: FallbackPolicy,
    pub concurrency: u8,
    pub route_assignments: Vec<NeedKey>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileDefinition {
    pub profile_id: RoleProfileId,
    pub role: CodexRole,
    pub host: CodexHost,
    pub model: String,
    pub reasoning: ReasoningLevel,
    pub service_tier: ServiceTier,
    pub timeout_seconds: u64,
    pub budget: RoleProfileBudget,
    pub prompt_profile_digest: Digest,
    pub output_contract_digest: Digest,
    pub tool_policy: ToolPolicy,
    pub command_policy: CommandPolicy,
    pub filesystem_policy: FilesystemPolicy,
    pub network_policy: NetworkPolicy,
    pub test_policy: TestPolicy,
    pub repair_policy: RepairPolicy,
    pub fallback_policy: FallbackPolicy,
    pub concurrency: u8,
    pub route_assignments: Vec<NeedKey>,
    pub definition_digest: Digest,
}

#[derive(Serialize)]
struct CanonicalDefinitionView<'a> {
    profile_id: &'a RoleProfileId,
    role: CodexRole,
    host: CodexHost,
    model: &'a str,
    reasoning: ReasoningLevel,
    service_tier: ServiceTier,
    timeout_seconds: u64,
    budget: &'a RoleProfileBudget,
    prompt_profile_digest: Digest,
    output_contract_digest: Digest,
    tool_policy: ToolPolicy,
    command_policy: CommandPolicy,
    filesystem_policy: FilesystemPolicy,
    network_policy: NetworkPolicy,
    test_policy: TestPolicy,
    repair_policy: RepairPolicy,
    fallback_policy: FallbackPolicy,
    concurrency: u8,
    route_assignments: &'a [NeedKey],
}

impl RoleProfileDefinition {
    pub fn new(input: RoleProfileDefinitionInput) -> Result<Self, RoleProfileValidationError> {
        Self::canonicalize(input)
    }

    pub fn canonicalize(
        mut input: RoleProfileDefinitionInput,
    ) -> Result<Self, RoleProfileValidationError> {
        if input.route_assignments.len() > HARD_MAX_NEEDS_PER_TASK as usize {
            return Err(RoleProfileValidationError::Routes);
        }
        input.route_assignments.sort();
        input.route_assignments.dedup();
        let definition = Self {
            profile_id: input.profile_id,
            role: input.role,
            host: input.host,
            model: input.model,
            reasoning: input.reasoning,
            service_tier: input.service_tier,
            timeout_seconds: input.timeout_seconds,
            budget: input.budget,
            prompt_profile_digest: input.prompt_profile_digest,
            output_contract_digest: input.output_contract_digest,
            tool_policy: input.tool_policy,
            command_policy: input.command_policy,
            filesystem_policy: input.filesystem_policy,
            network_policy: input.network_policy,
            test_policy: input.test_policy,
            repair_policy: input.repair_policy,
            fallback_policy: input.fallback_policy,
            concurrency: input.concurrency,
            route_assignments: input.route_assignments,
            definition_digest: Digest([0; 32]),
        };
        definition.validate_without_digest()?;
        let digest = definition.compute_digest();
        Ok(Self { definition_digest: digest, ..definition })
    }

    pub fn validate(&self) -> Result<(), RoleProfileValidationError> {
        self.validate_without_digest()?;
        let expected = self.compute_digest();
        if expected != self.definition_digest {
            return Err(RoleProfileValidationError::DigestMismatch {
                expected,
                actual: self.definition_digest,
            });
        }
        Ok(())
    }

    pub fn is_canonical(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        let mut routes = self.route_assignments.clone();
        routes.sort();
        routes.dedup();
        serde_json::to_string(&CanonicalDefinitionView {
            profile_id: &self.profile_id,
            role: self.role,
            host: self.host,
            model: &self.model,
            reasoning: self.reasoning,
            service_tier: self.service_tier,
            timeout_seconds: self.timeout_seconds,
            budget: &self.budget,
            prompt_profile_digest: self.prompt_profile_digest,
            output_contract_digest: self.output_contract_digest,
            tool_policy: self.tool_policy,
            command_policy: self.command_policy,
            filesystem_policy: self.filesystem_policy,
            network_policy: self.network_policy,
            test_policy: self.test_policy,
            repair_policy: self.repair_policy,
            fallback_policy: self.fallback_policy,
            concurrency: self.concurrency,
            route_assignments: &routes,
        })
    }

    pub fn to_worker_profile(&self) -> Result<WorkerProfile, RoleProfileValidationError> {
        self.validate()?;
        Ok(WorkerProfile::new(
            self.host.as_str(),
            self.model.clone(),
            self.reasoning.as_str(),
            match self.service_tier {
                ServiceTier::Default => None,
                ServiceTier::Priority => Some("priority".to_owned()),
            },
        ))
    }

    pub fn profile_id(&self) -> &RoleProfileId {
        &self.profile_id
    }

    fn validate_without_digest(&self) -> Result<(), RoleProfileValidationError> {
        validate_model(&self.model)?;
        if self.host != CodexHost::Codex {
            return Err(RoleProfileValidationError::Host);
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > ROLE_PROFILE_MAX_TIMEOUT_SECONDS {
            return Err(RoleProfileValidationError::Timeout);
        }
        if self.budget.max_turns == 0 || self.budget.max_turns > ROLE_PROFILE_MAX_TURNS {
            return Err(RoleProfileValidationError::Budget("max_turns must be between 1 and 8"));
        }
        if self.budget.max_output_tokens == 0
            || self.budget.max_output_tokens > HARD_RESULT_TOKENS as u32
        {
            return Err(RoleProfileValidationError::Budget(
                "max_output_tokens is outside the hard bound",
            ));
        }
        if self.budget.max_cost_microusd == 0
            || self.budget.max_cost_microusd > ROLE_PROFILE_MAX_COST_MICROUSD
        {
            return Err(RoleProfileValidationError::Budget(
                "max_cost_microusd is outside the hard bound",
            ));
        }
        if self.route_assignments.len() > HARD_MAX_NEEDS_PER_TASK as usize {
            return Err(RoleProfileValidationError::Routes);
        }
        if self.route_assignments.iter().any(|route| credential_prefix(route.as_str())) {
            return Err(RoleProfileValidationError::RouteCredential);
        }
        let mut routes = self.route_assignments.clone();
        routes.sort();
        routes.dedup();
        if routes != self.route_assignments {
            return Err(RoleProfileValidationError::Routes);
        }
        if self.concurrency != 1 {
            return Err(RoleProfileValidationError::Concurrency);
        }
        if self.network_policy != NetworkPolicy::Denied {
            return Err(RoleProfileValidationError::Policy("network must be denied"));
        }
        if self.tool_policy == ToolPolicy::IsolatedWrite
            && (self.filesystem_policy != FilesystemPolicy::DisposableCheckout
                || self.role != CodexRole::Implementer)
        {
            return Err(RoleProfileValidationError::Policy(
                "isolated_write requires disposable_checkout and implementer role",
            ));
        }
        if self.role != CodexRole::Implementer
            && (self.tool_policy != ToolPolicy::ReadOnly
                || self.filesystem_policy != FilesystemPolicy::ReadOnlyCheckout)
        {
            return Err(RoleProfileValidationError::Policy(
                "non-implementer roles require read_only tool and filesystem policies",
            ));
        }
        if (self.command_policy == CommandPolicy::CertifiedTests)
            != (self.test_policy == TestPolicy::Certified)
        {
            return Err(RoleProfileValidationError::Policy(
                "certified_tests command requires certified test policy and vice versa",
            ));
        }
        if self.command_policy == CommandPolicy::CertifiedTests
            && !matches!(self.role, CodexRole::TestRunner | CodexRole::Verifier)
        {
            return Err(RoleProfileValidationError::Policy(
                "certified_tests is allowed only for test_runner or verifier",
            ));
        }
        Ok(())
    }

    fn compute_digest(&self) -> Digest {
        let mut hasher = CanonicalHasher::new(b"needle-codex-role-profile-definition-v1");
        hasher.field_str(self.profile_id.as_str());
        hasher.field_u8(self.role.tag());
        hasher.field_u8(0);
        hasher.field_str(self.model.as_str());
        hasher.field_u8(self.reasoning.tag());
        hasher.field_u8(self.service_tier.tag());
        hasher.field_bytes(&self.timeout_seconds.to_le_bytes());
        hasher.field_u8(self.budget.max_turns);
        hasher.field_bytes(&self.budget.max_output_tokens.to_le_bytes());
        hasher.field_bytes(&self.budget.max_cost_microusd.to_le_bytes());
        hasher.field_digest(self.prompt_profile_digest);
        hasher.field_digest(self.output_contract_digest);
        hasher.field_u8(self.tool_policy.tag());
        hasher.field_u8(self.command_policy.tag());
        hasher.field_u8(self.filesystem_policy.tag());
        hasher.field_u8(NetworkPolicy::Denied.tag());
        hasher.field_u8(self.test_policy.tag());
        hasher.field_u8(self.repair_policy.tag());
        hasher.field_u8(self.fallback_policy.tag());
        hasher.field_u8(self.concurrency);
        hasher.field_bytes(&(self.route_assignments.len() as u32).to_le_bytes());
        for route in &self.route_assignments {
            hasher.field_str(route.as_str());
        }
        hasher.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleProfileState {
    Draft,
    Active,
    Inactive,
}

pub type RoleProfileRevisionState = RoleProfileState;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileRevision {
    pub profile_id: RoleProfileId,
    pub revision: u64,
    pub definition: RoleProfileDefinition,
    pub state: RoleProfileState,
    pub created_unix_ms: u64,
    pub activated_unix_ms: Option<u64>,
}

impl RoleProfileRevision {
    pub fn to_worker_profile(&self) -> Result<WorkerProfile, RoleProfileValidationError> {
        self.validate()?;
        self.definition.to_worker_profile()
    }

    pub fn validate(&self) -> Result<(), RoleProfileValidationError> {
        if self.revision == 0 {
            return Err(RoleProfileValidationError::Revision);
        }
        self.definition.validate()?;
        if self.profile_id != self.definition.profile_id {
            return Err(RoleProfileValidationError::IdentityMismatch);
        }
        if self.state == RoleProfileState::Active && self.activated_unix_ms.is_none() {
            return Err(RoleProfileValidationError::StateMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum RoleProfileValidationError {
    #[error("invalid role-profile id: {0}")]
    ProfileId(String),
    #[error("model token is invalid: {0}")]
    Model(String),
    #[error("role-profile host must be codex")]
    Host,
    #[error("timeout_seconds must be between 1 and 3600")]
    Timeout,
    #[error("budget is invalid: {0}")]
    Budget(&'static str),
    #[error("route assignments exceed the hard bound or are not canonical")]
    Routes,
    #[error("route assignment contains a credential-like prefix")]
    RouteCredential,
    #[error("concurrency must equal one")]
    Concurrency,
    #[error("unsafe role-profile policy combination: {0}")]
    Policy(&'static str),
    #[error("definition digest mismatch (expected {expected}, got {actual})")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("revision must be nonzero")]
    Revision,
    #[error("profile identity does not match definition identity")]
    IdentityMismatch,
    #[error("state metadata is inconsistent")]
    StateMetadata,
}

fn credential_prefix(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "sk-",
        "sk_",
        "rk_",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "bearer ",
        "basic ",
    ]
    .iter()
    .any(|prefix| lowered.starts_with(prefix))
        || google_api_key(value)
        || aws_access_key_id(value)
}

fn google_api_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 39
        && bytes.starts_with(b"AIza")
        && bytes[4..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
}

fn aws_access_key_id(value: &str) -> bool {
    value.len() == 20
        && value.as_bytes().iter().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && (value.starts_with("AKIA") || value.starts_with("ASIA"))
}

fn validate_model(value: &str) -> Result<(), RoleProfileValidationError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > ROLE_PROFILE_MAX_MODEL_BYTES {
        return Err(RoleProfileValidationError::Model(
            "model must be between 1 and 128 ASCII bytes".to_owned(),
        ));
    }
    if bytes
        .iter()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-')))
    {
        return Err(RoleProfileValidationError::Model(
            "model must contain only ASCII letters, digits, '.', '_' or '-'".to_owned(),
        ));
    }
    if credential_prefix(value) {
        return Err(RoleProfileValidationError::Model(
            "model contains a credential-like prefix".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "role_profile/tests.rs"]
mod tests;
