use needle_core::{CacheResolution, Digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

const TOKENS_PER_MILLION: u128 = 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingSnapshot {
    pub format_revision: u32,
    pub revision: String,
    pub platform: String,
    pub unit: String,
    pub source_url: String,
    pub retrieved_at: String,
    pub models: Vec<ModelPricing>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub model: String,
    pub service_tier: String,
    pub uncached_input_microcredits_per_million: u64,
    pub cached_input_microcredits_per_million: u64,
    pub output_microcredits_per_million: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenCost {
    pub pricing_snapshot_digest: Digest,
    pub pricing_revision: String,
    pub model: String,
    pub service_tier: String,
    pub unit: String,
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub uncached_input_microcredits_per_million: u64,
    pub cached_input_microcredits_per_million: u64,
    pub output_microcredits_per_million: u64,
    pub uncached_input_microcredits: u64,
    pub cached_input_microcredits: u64,
    pub output_microcredits: u64,
    pub total_microcredits: u64,
    pub rounding: String,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PricingError {
    #[error("unsupported pricing snapshot format revision {0}")]
    UnsupportedFormat(u32),
    #[error("pricing snapshot field `{0}` must not be empty")]
    EmptyField(&'static str),
    #[error("pricing snapshot unit must be `credits`")]
    UnsupportedUnit,
    #[error("pricing snapshot must contain at least one model")]
    EmptyModels,
    #[error("pricing entry model and service tier must not be empty")]
    EmptyModelIdentity,
    #[error("duplicate pricing entry for model `{model}` and tier `{service_tier}`")]
    DuplicateEntry { model: String, service_tier: String },
    #[error("pricing rates must be positive for model `{model}` and tier `{service_tier}`")]
    ZeroRate { model: String, service_tier: String },
    #[error("pricing is unavailable for model `{model}` and tier `{service_tier}`")]
    MissingModel { model: String, service_tier: String },
    #[error("cached input tokens exceed total input tokens")]
    InvalidCachedInput,
    #[error("pricing arithmetic overflowed")]
    Overflow,
}

impl PricingSnapshot {
    pub fn validate(&self) -> Result<(), PricingError> {
        if self.format_revision != 1 {
            return Err(PricingError::UnsupportedFormat(self.format_revision));
        }
        for (value, field) in [
            (&self.revision, "revision"),
            (&self.platform, "platform"),
            (&self.source_url, "source_url"),
            (&self.retrieved_at, "retrieved_at"),
        ] {
            if value.trim().is_empty() {
                return Err(PricingError::EmptyField(field));
            }
        }
        if self.unit != "credits" {
            return Err(PricingError::UnsupportedUnit);
        }
        if self.models.is_empty() {
            return Err(PricingError::EmptyModels);
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.models {
            if entry.model.trim().is_empty() || entry.service_tier.trim().is_empty() {
                return Err(PricingError::EmptyModelIdentity);
            }
            let key = (entry.model.clone(), entry.service_tier.clone());
            if !seen.insert(key) {
                return Err(PricingError::DuplicateEntry {
                    model: entry.model.clone(),
                    service_tier: entry.service_tier.clone(),
                });
            }
            if entry.uncached_input_microcredits_per_million == 0
                || entry.cached_input_microcredits_per_million == 0
                || entry.output_microcredits_per_million == 0
            {
                return Err(PricingError::ZeroRate {
                    model: entry.model.clone(),
                    service_tier: entry.service_tier.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<Digest, serde_json::Error> {
        serde_json::to_vec(self).map(Digest::blake3)
    }

    pub fn price_usage(
        &self,
        model: &str,
        service_tier: &str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    ) -> Result<TokenCost, PricingError> {
        self.validate()?;
        if cached_input_tokens > input_tokens {
            return Err(PricingError::InvalidCachedInput);
        }
        let entry = self.entry(model, service_tier)?;
        let uncached_input_tokens = input_tokens - cached_input_tokens;
        let uncached_input_microcredits = priced_microcredits(
            uncached_input_tokens,
            entry.uncached_input_microcredits_per_million,
        )?;
        let cached_input_microcredits =
            priced_microcredits(cached_input_tokens, entry.cached_input_microcredits_per_million)?;
        let output_microcredits =
            priced_microcredits(output_tokens, entry.output_microcredits_per_million)?;
        let total_microcredits = uncached_input_microcredits
            .checked_add(cached_input_microcredits)
            .and_then(|value| value.checked_add(output_microcredits))
            .ok_or(PricingError::Overflow)?;
        let pricing_snapshot_digest = self.digest().map_err(|_| PricingError::Overflow)?;
        Ok(TokenCost {
            pricing_snapshot_digest,
            pricing_revision: self.revision.clone(),
            model: model.to_owned(),
            service_tier: service_tier.to_owned(),
            unit: self.unit.clone(),
            uncached_input_tokens,
            cached_input_tokens,
            output_tokens,
            uncached_input_microcredits_per_million: entry.uncached_input_microcredits_per_million,
            cached_input_microcredits_per_million: entry.cached_input_microcredits_per_million,
            output_microcredits_per_million: entry.output_microcredits_per_million,
            uncached_input_microcredits,
            cached_input_microcredits,
            output_microcredits,
            total_microcredits,
            rounding: "ceiling_to_microcredit_per_component".to_owned(),
        })
    }

    pub fn worker_is_strictly_cheaper(
        &self,
        main_model: &str,
        worker_model: &str,
        service_tier: &str,
    ) -> Result<bool, PricingError> {
        self.validate()?;
        if main_model == worker_model {
            return Ok(false);
        }
        let main = self.entry(main_model, service_tier)?;
        let worker = self.entry(worker_model, service_tier)?;
        Ok(worker.uncached_input_microcredits_per_million
            < main.uncached_input_microcredits_per_million
            && worker.cached_input_microcredits_per_million
                < main.cached_input_microcredits_per_million
            && worker.output_microcredits_per_million < main.output_microcredits_per_million)
    }

    fn entry(&self, model: &str, service_tier: &str) -> Result<&ModelPricing, PricingError> {
        self.models
            .iter()
            .find(|entry| entry.model == model && entry.service_tier == service_tier)
            .ok_or_else(|| PricingError::MissingModel {
                model: model.to_owned(),
                service_tier: service_tier.to_owned(),
            })
    }
}

fn priced_microcredits(tokens: u64, rate_per_million: u64) -> Result<u64, PricingError> {
    let numerator = u128::from(tokens)
        .checked_mul(u128::from(rate_per_million))
        .ok_or(PricingError::Overflow)?;
    let rounded = numerator.checked_add(TOKENS_PER_MILLION - 1).ok_or(PricingError::Overflow)?
        / TOKENS_PER_MILLION;
    rounded.try_into().map_err(|_| PricingError::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductArm {
    P0,
    NativeSubagent,
    NeedleMiss,
    NeedleHit,
    StaleMutation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityOracleSpec {
    pub required_files: Vec<String>,
    pub required_symbols: Vec<String>,
    pub required_claims: Vec<String>,
    pub forbidden_claims: Vec<String>,
    pub focused_test_command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_focused_test_identifiers: Vec<String>,
    #[serde(default = "default_true")]
    pub focused_test_required: bool,
}

const fn default_true() -> bool {
    true
}

impl QualityOracleSpec {
    pub fn accepts_focused_test_identifier(&self, identifier: &str) -> bool {
        identifier == self.focused_test_command
            || self.accepted_focused_test_identifiers.iter().any(|accepted| identifier == accepted)
    }

    fn response_references_focused_test(&self, response: &str) -> bool {
        std::iter::once(&self.focused_test_command)
            .chain(self.accepted_focused_test_identifiers.iter())
            .any(|identifier| {
                response.contains(identifier)
                    || identifier.rsplit("::").next().is_some_and(|leaf| response.contains(leaf))
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QualityOracleResult {
    pub passed: bool,
    pub required_files_present: bool,
    pub required_symbols_present: bool,
    pub required_claims_present: bool,
    pub forbidden_claims_absent: bool,
    pub focused_test_suggested: bool,
    pub evaluator_test_passed: Option<bool>,
    pub failures: Vec<String>,
}

impl QualityOracleResult {
    pub fn evaluate(
        spec: &QualityOracleSpec,
        response: &str,
        evaluator_test_passed: Option<bool>,
    ) -> Self {
        let required_files_present =
            spec.required_files.iter().all(|value| response.contains(value));
        let required_symbols_present =
            spec.required_symbols.iter().all(|value| response.contains(value));
        let required_claims_present =
            spec.required_claims.iter().all(|value| response.contains(value));
        let forbidden_claims_absent =
            spec.forbidden_claims.iter().all(|value| !response.contains(value));
        let focused_test_suggested = spec.response_references_focused_test(response);
        let mut failures = Vec::new();
        if !required_files_present {
            failures.push("required_files".to_owned());
        }
        if !required_symbols_present {
            failures.push("required_symbols".to_owned());
        }
        if !required_claims_present {
            failures.push("required_claims".to_owned());
        }
        if !forbidden_claims_absent {
            failures.push("forbidden_claims".to_owned());
        }
        if spec.focused_test_required && !focused_test_suggested {
            failures.push("focused_test_command".to_owned());
        }
        if evaluator_test_passed == Some(false) {
            failures.push("evaluator_test".to_owned());
        }
        let passed = failures.is_empty() && evaluator_test_passed != Some(false);
        Self {
            passed,
            required_files_present,
            required_symbols_present,
            required_claims_present,
            forbidden_claims_absent,
            focused_test_suggested,
            evaluator_test_passed,
            failures,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProductRunManifest {
    pub format_revision: u32,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_prompt_digest: Option<Digest>,
    pub arm: ProductArm,
    pub main_model: String,
    pub main_reasoning: String,
    pub worker_model: Option<String>,
    pub worker_reasoning: Option<String>,
    pub service_tier: String,
    pub codex_version: String,
    pub operating_system: String,
    pub repository_sha: String,
    pub repository_snapshot_digest: Digest,
    pub prompt_profile_digest: Option<Digest>,
    pub route_definition_digest: Option<Digest>,
    pub preset_definition_digest: Option<Digest>,
    pub worker_configuration_digest: Option<Digest>,
    pub output_schema_digest: Option<Digest>,
    pub schedule_seed: u64,
    pub pricing_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_snapshot_digest: Option<Digest>,
}

impl ProductRunManifest {
    pub fn digest(&self) -> Result<Digest, serde_json::Error> {
        serde_json::to_vec(self).map(Digest::blake3)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessExecutionStatus {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
}

impl Default for ProcessExecutionStatus {
    fn default() -> Self {
        Self {
            status: "unavailable".to_owned(),
            spawn_error: None,
            exit_code: None,
            timed_out: false,
            abort_reason: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProductObservation {
    pub manifest_digest: Digest,
    pub arm: ProductArm,
    pub task_id: String,
    pub transport_success: bool,
    pub process_success: bool,
    pub quality: QualityOracleResult,
    pub cache_lookup: Option<String>,
    pub cache_lookup_latency_ms: Option<u64>,
    pub worker_spawns: u32,
    pub duplicate_worker_spawns: u32,
    #[serde(default)]
    pub logical_worker_spawns: u32,
    #[serde(default)]
    pub worker_turns: u32,
    #[serde(default)]
    pub repair_performed: bool,
    #[serde(default)]
    pub discarded_facts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pilot_abort_reason: Option<String>,
    #[serde(default)]
    pub process: ProcessExecutionStatus,
    pub main_discovery_before_brief: u32,
    pub main_discovery_after_brief: u32,
    pub main_discovery_total: u32,
    pub wall_time_ms: u64,
    pub main_input_tokens: Option<u64>,
    pub main_cached_input_tokens: Option<u64>,
    pub main_output_tokens: Option<u64>,
    pub worker_input_tokens: Option<u64>,
    pub worker_cached_input_tokens: Option<u64>,
    pub worker_output_tokens: Option<u64>,
    #[serde(default)]
    pub worker_pricing_verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_cost: Option<TokenCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_cost: Option<TokenCost>,
    pub result_digest: Option<Digest>,
    pub stale_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePilotResolveOutcome {
    pub status: String,
    pub cache_resolution: CacheResolution,
    pub cache_hit: bool,
    pub worker_spawned: bool,
    pub result_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePilotArmObservation {
    pub arm: ProductArm,
    pub transport_success: bool,
    pub process_success: bool,
    pub process: ProcessExecutionStatus,
    pub resolve: Option<CachePilotResolveOutcome>,
    pub worker_runs_before: u64,
    pub worker_runs_after: u64,
    pub worker_run_delta: u64,
    pub main_discovery_total: u32,
    pub wall_time_ms: u64,
    pub main_input_tokens: Option<u64>,
    pub main_cached_input_tokens: Option<u64>,
    pub main_output_tokens: Option<u64>,
    pub main_cost: Option<TokenCost>,
    pub worker_cost: Option<TokenCost>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePilotGateResult {
    pub passed: bool,
    pub publication_miss_valid: bool,
    pub full_cache_hit_valid: bool,
    pub same_artifact: bool,
    pub exact_zero_worker: bool,
    pub artifacts_after_publication: u64,
    pub cache_entries_after_publication: u64,
    pub checkout_clean: bool,
    pub publication: CachePilotArmObservation,
    pub exact: CachePilotArmObservation,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationPilotGateResult {
    pub passed: bool,
    pub publication_miss_valid: bool,
    pub irrelevant_full_hit_valid: bool,
    pub relevant_partial_hit_valid: bool,
    pub no_stale_hit: bool,
    pub semantic_artifacts_after_publication: u64,
    pub artifacts_after_publication: u64,
    pub checkout_restored: bool,
    pub irrelevant_path: String,
    pub relevant_path: String,
    pub publication: CachePilotArmObservation,
    pub irrelevant: CachePilotArmObservation,
    pub relevant: CachePilotArmObservation,
    pub failures: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_mutation_pilot(
    publication: CachePilotArmObservation,
    irrelevant: CachePilotArmObservation,
    relevant: CachePilotArmObservation,
    semantic_artifacts_after_publication: u64,
    artifacts_after_publication: u64,
    checkout_restored: bool,
    irrelevant_path: String,
    relevant_path: String,
) -> MutationPilotGateResult {
    let publication_miss_valid = publication.transport_success
        && publication.process_success
        && publication.worker_run_delta == 1
        && publication.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "generated"
                && matches!(outcome.cache_resolution, CacheResolution::Miss)
                && !outcome.cache_hit
                && outcome.worker_spawned
        });
    let irrelevant_full_hit_valid = irrelevant.transport_success
        && irrelevant.process_success
        && irrelevant.worker_run_delta == 0
        && publication.resolve.as_ref().zip(irrelevant.resolve.as_ref()).is_some_and(
            |(seed, outcome)| {
                outcome.status == "hit"
                    && matches!(
                        outcome.cache_resolution,
                        CacheResolution::ExactHit { .. } | CacheResolution::CompositeHit { .. }
                    )
                    && outcome.cache_hit
                    && !outcome.worker_spawned
                    && outcome.result_digest == seed.result_digest
            },
        );
    let relevant_partial_hit_valid = relevant.transport_success
        && relevant.process_success
        && relevant.worker_run_delta == 1
        && relevant.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "generated-partial"
                && matches!(
                    &outcome.cache_resolution,
                    CacheResolution::PartialHit { reused, invalidated_nodes, .. }
                        if !reused.is_empty() && !invalidated_nodes.is_empty()
                )
                && outcome.cache_hit
                && outcome.worker_spawned
        });
    let no_stale_hit = [&publication, &irrelevant, &relevant].into_iter().all(|observation| {
        observation.resolve.as_ref().is_none_or(|outcome| {
            !matches!(outcome.cache_resolution, CacheResolution::Stale { .. })
        })
    });
    let all_artifacts_semantic = artifacts_after_publication > 0
        && semantic_artifacts_after_publication == artifacts_after_publication;
    let mut failures = Vec::new();
    if !publication_miss_valid {
        failures.push("publication_miss".to_owned());
    }
    if !all_artifacts_semantic {
        failures.push("semantic_artifact_scope".to_owned());
    }
    if !irrelevant_full_hit_valid {
        failures.push("irrelevant_mutation".to_owned());
    }
    if !relevant_partial_hit_valid {
        failures.push("relevant_mutation".to_owned());
    }
    if !no_stale_hit {
        failures.push("stale_hit".to_owned());
    }
    if !checkout_restored {
        failures.push("checkout_restore".to_owned());
    }
    MutationPilotGateResult {
        passed: failures.is_empty(),
        publication_miss_valid,
        irrelevant_full_hit_valid,
        relevant_partial_hit_valid,
        no_stale_hit,
        semantic_artifacts_after_publication,
        artifacts_after_publication,
        checkout_restored,
        irrelevant_path,
        relevant_path,
        publication,
        irrelevant,
        relevant,
        failures,
    }
}

pub fn evaluate_cache_pilot(
    publication: CachePilotArmObservation,
    exact: CachePilotArmObservation,
    artifacts_after_publication: u64,
    cache_entries_after_publication: u64,
    checkout_clean: bool,
) -> CachePilotGateResult {
    let publication_miss_valid = publication.transport_success
        && publication.process_success
        && publication.worker_run_delta == 1
        && publication.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "generated"
                && matches!(outcome.cache_resolution, CacheResolution::Miss)
                && !outcome.cache_hit
                && outcome.worker_spawned
        });
    let exact_zero_worker = exact.worker_run_delta == 0
        && exact.resolve.as_ref().is_some_and(|outcome| !outcome.worker_spawned);
    let full_cache_hit_valid = exact.transport_success
        && exact.process_success
        && exact_zero_worker
        && exact.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "hit"
                && matches!(
                    outcome.cache_resolution,
                    CacheResolution::ExactHit { .. } | CacheResolution::CompositeHit { .. }
                )
                && outcome.cache_hit
        });
    let same_artifact = publication
        .resolve
        .as_ref()
        .zip(exact.resolve.as_ref())
        .is_some_and(|(left, right)| left.result_digest == right.result_digest);
    let mut failures = Vec::new();
    if !publication_miss_valid {
        failures.push("publication_miss".to_owned());
    }
    if artifacts_after_publication == 0 || cache_entries_after_publication == 0 {
        failures.push("publication_storage".to_owned());
    }
    if !full_cache_hit_valid {
        failures.push("full_cache_hit".to_owned());
    }
    if !same_artifact {
        failures.push("artifact_identity".to_owned());
    }
    if !checkout_clean {
        failures.push("checkout_integrity".to_owned());
    }
    CachePilotGateResult {
        passed: failures.is_empty(),
        publication_miss_valid,
        full_cache_hit_valid,
        same_artifact,
        exact_zero_worker,
        artifacts_after_publication,
        cache_entries_after_publication,
        checkout_clean,
        publication,
        exact,
        failures,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisVerdict {
    Pass,
    Fail,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProductVerdict {
    pub transport: AxisVerdict,
    pub quality: AxisVerdict,
    pub utility: AxisVerdict,
    pub cache_safety: AxisVerdict,
    pub economics: AxisVerdict,
    pub explanations: BTreeMap<String, String>,
}

impl ProductVerdict {
    pub fn evaluate(observations: &[ProductObservation]) -> Self {
        let mut explanations = BTreeMap::new();
        let transport = if observations.is_empty() {
            AxisVerdict::Unavailable
        } else if observations.iter().all(|item| item.transport_success && item.process_success) {
            AxisVerdict::Pass
        } else {
            AxisVerdict::Fail
        };
        let quality = if observations.is_empty() {
            AxisVerdict::Unavailable
        } else if observations.iter().all(|item| item.quality.passed) {
            AxisVerdict::Pass
        } else {
            AxisVerdict::Fail
        };
        let baseline_discovery = observations
            .iter()
            .filter(|item| item.arm == ProductArm::P0)
            .map(|item| u64::from(item.main_discovery_total))
            .collect::<Vec<_>>();
        let needle_observations = observations
            .iter()
            .filter(|item| {
                matches!(
                    item.arm,
                    ProductArm::NeedleMiss | ProductArm::NeedleHit | ProductArm::StaleMutation
                )
            })
            .collect::<Vec<_>>();
        let utility = if baseline_discovery.is_empty() || needle_observations.is_empty() {
            AxisVerdict::Unavailable
        } else if needle_observations.iter().all(|item| {
            item.transport_success
                && item.process_success
                && baseline_discovery
                    .iter()
                    .any(|baseline| u64::from(item.main_discovery_total) < *baseline)
        }) {
            AxisVerdict::Pass
        } else {
            AxisVerdict::Fail
        };
        if utility == AxisVerdict::Fail {
            explanations.insert(
                "utility".to_owned(),
                "a Needle run failed to complete or did not reduce main-agent discovery relative to the baseline"
                    .to_owned(),
            );
        }
        let cache_observations = observations
            .iter()
            .filter(|item| matches!(item.arm, ProductArm::NeedleHit | ProductArm::StaleMutation))
            .collect::<Vec<_>>();
        let cache_safety = if cache_observations.is_empty() {
            AxisVerdict::Unavailable
        } else if cache_observations.iter().all(|item| {
            let matching_miss_digest = observations
                .iter()
                .find(|candidate| {
                    candidate.arm == ProductArm::NeedleMiss && candidate.task_id == item.task_id
                })
                .and_then(|candidate| candidate.result_digest);
            !item.stale_hit
                && item.duplicate_worker_spawns == 0
                && (item.arm != ProductArm::NeedleHit
                    || (item.worker_spawns == 0
                        && item.cache_lookup_latency_ms.is_some_and(|latency| latency < 100)
                        && item.result_digest == matching_miss_digest))
        }) {
            AxisVerdict::Pass
        } else {
            AxisVerdict::Fail
        };
        let miss_observations = observations
            .iter()
            .filter(|item| item.arm == ProductArm::NeedleMiss)
            .collect::<Vec<_>>();
        let economics = if miss_observations.is_empty() {
            AxisVerdict::Unavailable
        } else {
            let paired = miss_observations
                .iter()
                .map(|miss| {
                    let baseline = observations.iter().find(|candidate| {
                        candidate.arm == ProductArm::P0 && candidate.task_id == miss.task_id
                    })?;
                    let baseline_cost = baseline.total_cost_microcredits()?;
                    let miss_cost = miss.total_cost_microcredits()?;
                    Some(
                        baseline.cost_identity() == miss.cost_identity()
                            && miss.worker_priced_lower()
                            && miss_cost <= baseline_cost,
                    )
                })
                .collect::<Option<Vec<_>>>();
            match paired {
                Some(results) if results.iter().all(|passed| *passed) => AxisVerdict::Pass,
                Some(_) => AxisVerdict::Fail,
                None => AxisVerdict::Unavailable,
            }
        };
        if economics == AxisVerdict::Fail {
            explanations.insert(
                "economics".to_owned(),
                "a Needle miss used a non-cheaper worker or exceeded its equivalent P0 credit cost"
                    .to_owned(),
            );
        }
        Self { transport, quality, utility, cache_safety, economics, explanations }
    }
}

impl ProductObservation {
    pub fn total_cost_microcredits(&self) -> Option<u64> {
        let main_cost = self.main_cost.as_ref()?;
        let main = main_cost.total_microcredits;
        let worker_executed =
            self.worker_spawns > 0 || self.logical_worker_spawns > 0 || self.worker_turns > 0;
        if !worker_executed {
            return Some(main);
        }
        let worker = self.worker_cost.as_ref()?;
        if main_cost.pricing_snapshot_digest != worker.pricing_snapshot_digest
            || main_cost.unit != worker.unit
        {
            return None;
        }
        main.checked_add(worker.total_microcredits)
    }

    fn cost_identity(&self) -> Option<(Digest, &str)> {
        let cost = self.main_cost.as_ref()?;
        Some((cost.pricing_snapshot_digest, cost.unit.as_str()))
    }

    fn worker_priced_lower(&self) -> bool {
        match (&self.main_cost, &self.worker_cost) {
            (Some(main), Some(worker)) => {
                main.pricing_snapshot_digest == worker.pricing_snapshot_digest
                    && main.unit == worker.unit
                    && main.model != worker.model
                    && worker.uncached_input_microcredits_per_million
                        < main.uncached_input_microcredits_per_million
                    && worker.cached_input_microcredits_per_million
                        < main.cached_input_microcredits_per_million
                    && worker.output_microcredits_per_million < main.output_microcredits_per_million
            }
            _ => self.worker_pricing_verified,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PilotGateResult {
    pub passed: bool,
    #[serde(default)]
    pub needle_process: ProcessExecutionStatus,
    pub comparable_completion: bool,
    pub quality_equal_and_passing: bool,
    pub exactly_one_worker: bool,
    pub zero_main_discovery: bool,
    pub main_discovery_reduced: bool,
    pub wall_time_bound: bool,
    pub worker_priced_lower: bool,
    pub economics_pass: bool,
    pub economic_savings_observed: Option<bool>,
    pub baseline_cost_microcredits: Option<u64>,
    pub needle_cost_microcredits: Option<u64>,
    pub cost_delta_microcredits: Option<i128>,
    pub cost_ratio_basis_points: Option<u64>,
    pub failures: Vec<String>,
}

pub fn evaluate_pilot_pair(
    baseline: &ProductObservation,
    needle: &ProductObservation,
) -> PilotGateResult {
    let comparable_completion = baseline.transport_success
        && baseline.process_success
        && needle.transport_success
        && needle.process_success;
    let quality_equal_and_passing = baseline.quality.passed && needle.quality.passed;
    let exactly_one_worker =
        needle.logical_worker_spawns == 1 && needle.duplicate_worker_spawns == 0;
    let zero_main_discovery =
        needle.main_discovery_before_brief == 0 && needle.main_discovery_after_brief == 0;
    let main_discovery_reduced = needle.main_discovery_total < baseline.main_discovery_total;
    let wall_time_bound = needle.wall_time_ms <= baseline.wall_time_ms.saturating_mul(2);
    let worker_priced_lower = needle.worker_priced_lower();
    let baseline_cost_microcredits = baseline.total_cost_microcredits();
    let needle_cost_microcredits = needle.total_cost_microcredits();
    let economic_savings_observed = baseline_cost_microcredits
        .zip(needle_cost_microcredits)
        .map(|(baseline, needle)| needle < baseline);
    let economics_pass = economic_savings_observed == Some(true);
    let cost_delta_microcredits = baseline_cost_microcredits
        .zip(needle_cost_microcredits)
        .map(|(baseline, needle)| i128::from(needle) - i128::from(baseline));
    let cost_ratio_basis_points =
        baseline_cost_microcredits.zip(needle_cost_microcredits).and_then(|(baseline, needle)| {
            if baseline == 0 {
                None
            } else {
                u128::from(needle)
                    .checked_mul(10_000)
                    .map(|value| value / u128::from(baseline))
                    .and_then(|value| value.try_into().ok())
            }
        });
    let mut failures = Vec::new();
    if !comparable_completion {
        failures.push("completion".to_owned());
    }
    if !economics_pass {
        failures.push("economics".to_owned());
    }
    PilotGateResult {
        passed: failures.is_empty(),
        needle_process: needle.process.clone(),
        comparable_completion,
        quality_equal_and_passing,
        exactly_one_worker,
        zero_main_discovery,
        main_discovery_reduced,
        wall_time_bound,
        worker_priced_lower,
        economics_pass,
        economic_savings_observed,
        baseline_cost_microcredits,
        needle_cost_microcredits,
        cost_delta_microcredits,
        cost_ratio_basis_points,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(passed: bool) -> QualityOracleResult {
        QualityOracleResult {
            passed,
            required_files_present: passed,
            required_symbols_present: passed,
            required_claims_present: passed,
            forbidden_claims_absent: passed,
            focused_test_suggested: passed,
            evaluator_test_passed: Some(passed),
            failures: Vec::new(),
        }
    }

    fn observation(arm: ProductArm, discovery: u32, wall: u64) -> ProductObservation {
        ProductObservation {
            manifest_digest: Digest::blake3("manifest"),
            arm,
            task_id: "task".to_owned(),
            transport_success: true,
            process_success: true,
            quality: quality(true),
            cache_lookup: None,
            cache_lookup_latency_ms: None,
            worker_spawns: u32::from(arm == ProductArm::NeedleMiss),
            duplicate_worker_spawns: 0,
            logical_worker_spawns: u32::from(arm == ProductArm::NeedleMiss),
            worker_turns: u32::from(arm == ProductArm::NeedleMiss),
            repair_performed: false,
            discarded_facts: 0,
            pilot_abort_reason: None,
            process: ProcessExecutionStatus {
                status: "exit:0".to_owned(),
                exit_code: Some(0),
                ..ProcessExecutionStatus::default()
            },
            main_discovery_before_brief: 0,
            main_discovery_after_brief: discovery,
            main_discovery_total: discovery,
            wall_time_ms: wall,
            main_input_tokens: None,
            main_cached_input_tokens: None,
            main_output_tokens: None,
            worker_input_tokens: None,
            worker_cached_input_tokens: None,
            worker_output_tokens: None,
            worker_pricing_verified: arm == ProductArm::NeedleMiss,
            main_cost: None,
            worker_cost: None,
            result_digest: None,
            stale_hit: false,
        }
    }

    fn cache_observation(
        arm: ProductArm,
        resolution: CacheResolution,
        worker_spawned: bool,
        worker_run_delta: u64,
    ) -> CachePilotArmObservation {
        CachePilotArmObservation {
            arm,
            transport_success: true,
            process_success: true,
            process: ProcessExecutionStatus {
                status: "exit:0".to_owned(),
                exit_code: Some(0),
                ..ProcessExecutionStatus::default()
            },
            resolve: Some(CachePilotResolveOutcome {
                status: if worker_spawned { "generated" } else { "hit" }.to_owned(),
                cache_resolution: resolution,
                cache_hit: !worker_spawned,
                worker_spawned,
                result_digest: Digest::blake3("artifact"),
            }),
            worker_runs_before: 0,
            worker_runs_after: worker_run_delta,
            worker_run_delta,
            main_discovery_total: 0,
            wall_time_ms: 1,
            main_input_tokens: Some(1),
            main_cached_input_tokens: Some(0),
            main_output_tokens: Some(1),
            main_cost: None,
            worker_cost: None,
        }
    }

    #[test]
    fn cache_pilot_requires_publication_then_same_exact_artifact_without_worker() {
        let artifact_id = Digest::blake3("artifact");
        let publication = cache_observation(ProductArm::NeedleMiss, CacheResolution::Miss, true, 1);
        let exact = cache_observation(
            ProductArm::NeedleHit,
            CacheResolution::ExactHit {
                artifact_id,
                sufficiency_certificate_id: None,
                selected_plan_id: None,
                resolution_format_revision: None,
            },
            false,
            0,
        );
        let report = evaluate_cache_pilot(publication, exact, 4, 1, true);
        assert!(report.passed);
        assert!(report.same_artifact);
        assert!(report.exact_zero_worker);
    }

    #[test]
    fn cache_pilot_accepts_complete_dag_hit_and_rejects_partial_or_worker_spawn() {
        let artifact_id = Digest::blake3("artifact");
        let publication = cache_observation(ProductArm::NeedleMiss, CacheResolution::Miss, true, 1);
        let mut exact = cache_observation(
            ProductArm::NeedleHit,
            CacheResolution::CompositeHit {
                artifact_ids: vec![artifact_id],
                sufficiency_certificate_id: None,
                selected_plan_id: None,
                resolution_format_revision: None,
            },
            false,
            0,
        );
        assert!(evaluate_cache_pilot(publication.clone(), exact.clone(), 4, 1, true).passed);

        exact.resolve.as_mut().unwrap().cache_resolution = CacheResolution::PartialHit {
            reused: vec![artifact_id],
            reused_claim_ids: Vec::new(),
            invalidated_nodes: vec!["brief".to_owned()],
            selected_plan_id: None,
            resolution_format_revision: None,
        };
        assert_eq!(
            evaluate_cache_pilot(publication.clone(), exact.clone(), 4, 1, true).failures,
            vec!["full_cache_hit"]
        );

        exact.resolve.as_mut().unwrap().cache_resolution = CacheResolution::ExactHit {
            artifact_id,
            sufficiency_certificate_id: None,
            selected_plan_id: None,
            resolution_format_revision: None,
        };
        exact.resolve.as_mut().unwrap().worker_spawned = true;
        exact.worker_run_delta = 1;
        assert_eq!(
            evaluate_cache_pilot(publication, exact, 4, 1, true).failures,
            vec!["full_cache_hit"]
        );
    }

    #[test]
    fn mutation_pilot_requires_semantic_full_hit_then_bounded_partial_hit() {
        let artifact_id = Digest::blake3("artifact");
        let publication = cache_observation(ProductArm::NeedleMiss, CacheResolution::Miss, true, 1);
        let irrelevant = cache_observation(
            ProductArm::NeedleHit,
            CacheResolution::CompositeHit {
                artifact_ids: vec![artifact_id],
                sufficiency_certificate_id: None,
                selected_plan_id: None,
                resolution_format_revision: None,
            },
            false,
            0,
        );
        let mut relevant = cache_observation(
            ProductArm::StaleMutation,
            CacheResolution::PartialHit {
                reused: vec![artifact_id],
                reused_claim_ids: Vec::new(),
                invalidated_nodes: vec!["behavior".to_owned(), "brief".to_owned()],
                selected_plan_id: None,
                resolution_format_revision: None,
            },
            true,
            1,
        );
        let outcome = relevant.resolve.as_mut().unwrap();
        outcome.status = "generated-partial".to_owned();
        outcome.cache_hit = true;

        let report = evaluate_mutation_pilot(
            publication,
            irrelevant,
            relevant,
            4,
            4,
            true,
            "needle-irrelevant.txt".to_owned(),
            "src/behavior.rs".to_owned(),
        );
        assert!(report.passed);
        assert!(report.no_stale_hit);
    }

    #[test]
    fn mutation_pilot_rejects_snapshot_only_publication_and_stale_result() {
        let artifact_id = Digest::blake3("artifact");
        let publication = cache_observation(ProductArm::NeedleMiss, CacheResolution::Miss, true, 1);
        let irrelevant = cache_observation(
            ProductArm::NeedleHit,
            CacheResolution::CompositeHit {
                artifact_ids: vec![artifact_id],
                sufficiency_certificate_id: None,
                selected_plan_id: None,
                resolution_format_revision: None,
            },
            false,
            0,
        );
        let relevant = cache_observation(
            ProductArm::StaleMutation,
            CacheResolution::Stale { artifact_id, reason: "dependency changed".to_owned() },
            false,
            0,
        );
        let report = evaluate_mutation_pilot(
            publication,
            irrelevant,
            relevant,
            3,
            4,
            true,
            "needle-irrelevant.txt".to_owned(),
            "src/behavior.rs".to_owned(),
        );
        assert_eq!(
            report.failures,
            vec!["semantic_artifact_scope", "relevant_mutation", "stale_hit"]
        );
    }

    #[test]
    fn pilot_gate_requires_comparable_completion_and_combined_savings() {
        let baseline = observation(ProductArm::P0, 20, 100);
        let mut needle = observation(ProductArm::NeedleMiss, 0, 200);
        let pricing = pricing_snapshot();
        let baseline_cost = pricing.price_usage("gpt-5.6-sol", "default", 100, 20, 10).unwrap();
        let main_cost = pricing.price_usage("gpt-5.6-sol", "default", 10, 2, 1).unwrap();
        let worker_cost = pricing.price_usage("gpt-5.6-luna", "default", 100, 20, 10).unwrap();
        let mut baseline = baseline;
        baseline.main_cost = Some(baseline_cost);
        needle.main_cost = Some(main_cost);
        needle.worker_cost = Some(worker_cost);
        assert!(evaluate_pilot_pair(&baseline, &needle).passed);

        needle.quality = quality(false);
        needle.main_discovery_after_brief = 1;
        needle.main_discovery_before_brief = 1;
        needle.logical_worker_spawns = 2;
        needle.duplicate_worker_spawns = 1;
        needle.worker_cost = needle.main_cost.clone();
        let diagnostic_mismatches = evaluate_pilot_pair(&baseline, &needle);
        assert!(diagnostic_mismatches.passed);
        assert!(!diagnostic_mismatches.quality_equal_and_passing);
        assert!(!diagnostic_mismatches.exactly_one_worker);
        assert!(!diagnostic_mismatches.zero_main_discovery);

        needle.process_success = false;
        assert_eq!(evaluate_pilot_pair(&baseline, &needle).failures, vec!["completion"]);
    }

    #[test]
    fn pilot_gate_requires_strict_observed_savings() {
        let pricing = pricing_snapshot();
        let mut baseline = observation(ProductArm::P0, 20, 100);
        let mut needle = observation(ProductArm::NeedleMiss, 5, 100);
        let equal_cost = pricing.price_usage("gpt-5.6-sol", "default", 1_000, 0, 100).unwrap();
        baseline.main_cost = Some(equal_cost.clone());
        needle.main_cost = Some(equal_cost);

        let gate = evaluate_pilot_pair(&baseline, &needle);
        assert!(!gate.passed);
        assert_eq!(gate.failures, vec!["economics"]);
    }

    #[test]
    fn pricing_uses_uncached_cached_and_output_rates() {
        let pricing = pricing_snapshot();
        let cost =
            pricing.price_usage("gpt-5.6-luna", "default", 1_100_000, 100_000, 10_000).unwrap();
        assert_eq!(cost.uncached_input_microcredits, 25_000_000);
        assert_eq!(cost.cached_input_microcredits, 250_000);
        assert_eq!(cost.output_microcredits, 1_500_000);
        assert_eq!(cost.total_microcredits, 26_750_000);
        assert!(
            pricing.worker_is_strictly_cheaper("gpt-5.6-sol", "gpt-5.6-luna", "default").unwrap()
        );
        assert!(
            !pricing.worker_is_strictly_cheaper("gpt-5.6-sol", "gpt-5.6-sol", "default").unwrap()
        );
    }

    #[test]
    fn bundled_pricing_snapshot_is_valid_and_prices_luna_below_sol() {
        let pricing: PricingSnapshot = serde_json::from_str(include_str!(
            "../../../fixtures/openai-codex-pricing-2026-07-27.json"
        ))
        .unwrap();
        pricing.validate().unwrap();
        assert!(
            pricing.worker_is_strictly_cheaper("gpt-5.6-sol", "gpt-5.6-luna", "default").unwrap()
        );
    }

    #[test]
    fn economics_compares_combined_credits_not_wall_time() {
        let pricing = pricing_snapshot();
        let mut baseline = observation(ProductArm::P0, 20, 100);
        baseline.main_cost =
            Some(pricing.price_usage("gpt-5.6-sol", "default", 1_000, 0, 100).unwrap());
        let mut needle = observation(ProductArm::NeedleMiss, 0, 10_000);
        needle.main_cost = Some(pricing.price_usage("gpt-5.6-sol", "default", 10, 0, 1).unwrap());
        needle.worker_cost =
            Some(pricing.price_usage("gpt-5.6-luna", "default", 100, 0, 10).unwrap());
        assert_eq!(ProductVerdict::evaluate(&[baseline, needle]).economics, AxisVerdict::Pass);
    }

    #[test]
    fn utility_reports_reduced_discovery_without_requiring_zero_or_exact_oracle_match() {
        let baseline = observation(ProductArm::P0, 20, 100);
        let mut needle = observation(ProductArm::NeedleMiss, 3, 100);
        needle.quality = quality(false);

        let verdict = ProductVerdict::evaluate(&[baseline, needle]);
        assert_eq!(verdict.quality, AxisVerdict::Fail);
        assert_eq!(verdict.utility, AxisVerdict::Pass);
    }

    fn pricing_snapshot() -> PricingSnapshot {
        PricingSnapshot {
            format_revision: 1,
            revision: "test".to_owned(),
            platform: "chatgpt_codex".to_owned(),
            unit: "credits".to_owned(),
            source_url: "https://example.test".to_owned(),
            retrieved_at: "2026-07-27".to_owned(),
            models: vec![
                ModelPricing {
                    model: "gpt-5.6-sol".to_owned(),
                    service_tier: "default".to_owned(),
                    uncached_input_microcredits_per_million: 125_000_000,
                    cached_input_microcredits_per_million: 12_500_000,
                    output_microcredits_per_million: 750_000_000,
                },
                ModelPricing {
                    model: "gpt-5.6-luna".to_owned(),
                    service_tier: "default".to_owned(),
                    uncached_input_microcredits_per_million: 25_000_000,
                    cached_input_microcredits_per_million: 2_500_000,
                    output_microcredits_per_million: 150_000_000,
                },
            ],
        }
    }

    #[test]
    fn quality_oracle_checks_focused_command() {
        let spec = QualityOracleSpec {
            required_files: vec!["src/main.rs".to_owned()],
            required_symbols: vec!["main".to_owned()],
            required_claims: vec!["precedence".to_owned()],
            forbidden_claims: vec!["wrong".to_owned()],
            focused_test_command: "cargo test focused".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: true,
        };
        assert!(
            QualityOracleResult::evaluate(
                &spec,
                "src/main.rs main precedence cargo test focused",
                Some(true),
            )
            .passed
        );
    }

    #[test]
    fn quality_oracle_accepts_a_focused_test_reference_without_execution() {
        let spec = QualityOracleSpec {
            required_files: vec!["src/main.rs".to_owned()],
            required_symbols: vec!["main".to_owned()],
            required_claims: Vec::new(),
            forbidden_claims: Vec::new(),
            focused_test_command: "suite::focused".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: true,
        };
        let result = QualityOracleResult::evaluate(&spec, "src/main.rs main focused", None);
        assert!(result.passed, "{:?}", result.failures);
        assert!(result.focused_test_suggested);
        assert_eq!(result.evaluator_test_passed, None);
    }

    #[test]
    fn quality_oracle_keeps_an_unrequested_focused_test_diagnostic_only() {
        let spec = QualityOracleSpec {
            required_files: vec!["src/main.rs".to_owned()],
            required_symbols: Vec::new(),
            required_claims: Vec::new(),
            forbidden_claims: Vec::new(),
            focused_test_command: "suite::focused".to_owned(),
            accepted_focused_test_identifiers: Vec::new(),
            focused_test_required: false,
        };
        let result = QualityOracleResult::evaluate(&spec, "src/main.rs", None);
        assert!(result.passed, "{:?}", result.failures);
        assert!(!result.focused_test_suggested);
    }

    #[test]
    fn quality_oracle_accepts_only_declared_focused_test_alternatives() {
        let spec = QualityOracleSpec {
            required_files: Vec::new(),
            required_symbols: Vec::new(),
            required_claims: Vec::new(),
            forbidden_claims: Vec::new(),
            focused_test_command: "suite::integration_case".to_owned(),
            accepted_focused_test_identifiers: vec!["unit::direct_case".to_owned()],
            focused_test_required: true,
        };
        assert!(
            QualityOracleResult::evaluate(&spec, "The focused test is direct_case.", None).passed
        );
        let rejected =
            QualityOracleResult::evaluate(&spec, "The focused test is another_case.", None);
        assert_eq!(rejected.failures, vec!["focused_test_command"]);
    }

    #[test]
    fn observation_cost_follows_actual_worker_execution() {
        let pricing = pricing_snapshot();
        let main = pricing.price_usage("gpt-5.6-sol", "default", 100, 0, 10).unwrap();
        let worker = pricing.price_usage("gpt-5.6-luna", "default", 100, 0, 10).unwrap();

        let mut hit = observation(ProductArm::NeedleHit, 0, 1);
        hit.main_cost = Some(main.clone());
        assert_eq!(hit.total_cost_microcredits(), Some(main.total_microcredits));

        hit.worker_spawns = 1;
        hit.logical_worker_spawns = 1;
        hit.worker_turns = 1;
        assert_eq!(hit.total_cost_microcredits(), None);

        hit.worker_cost = Some(worker.clone());
        assert_eq!(
            hit.total_cost_microcredits(),
            main.total_microcredits.checked_add(worker.total_microcredits)
        );
    }
}
