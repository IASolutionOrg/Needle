//! Deterministic, offline-first feasibility experiment primitives.

mod artifact_cache_replay;
mod calibration_replay;
mod corpus;
mod final_gate;
mod minimal_pilot;
mod product;
mod shadow_replay;

pub use artifact_cache_replay::*;
pub use calibration_replay::*;
pub use corpus::*;
pub use final_gate::*;
pub use minimal_pilot::*;
pub use product::*;
pub use shadow_replay::*;

use needle_core::{ContinuationEnvelope, Digest, NeedRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ExperimentArm {
    P0,
    P1,
    P2,
    P3,
    P4,
}

impl ExperimentArm {
    pub const ALL: [Self; 5] = [Self::P0, Self::P1, Self::P2, Self::P3, Self::P4];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
            Self::P4 => "P4",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricPrecision {
    Exact,
    Aggregate,
    Partial,
    Unavailable,
}

impl Default for MetricPrecision {
    fn default() -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageObservation {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
    pub input_precision: MetricPrecision,
    pub cached_input_precision: MetricPrecision,
    pub cache_write_precision: MetricPrecision,
    pub output_precision: MetricPrecision,
    pub latency_precision: MetricPrecision,
    pub precision: MetricPrecision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExperimentObservation {
    pub arm: ExperimentArm,
    pub repetition: u32,
    pub task_seed: u64,
    pub usage: UsageObservation,
    pub prompt_profile_digest: Option<Digest>,
    pub profile_payload_bytes: Option<u64>,
    pub mcp_startup_ms: Option<u64>,
    pub compaction_events: u32,
    #[serde(default)]
    pub compaction_precision: MetricPrecision,
    #[serde(default)]
    pub mcp_startup_precision: MetricPrecision,
    pub continuation_success: Option<bool>,
    pub artifact_digest: Option<Digest>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub arm: ExperimentArm,
    pub repetition: u32,
    pub task_seed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExperimentSchedule {
    pub entries: Vec<ScheduleEntry>,
    pub schedule_seed: u64,
}

impl ExperimentSchedule {
    /// Build the Phase 0A transport schedule: exactly one observation per arm.
    ///
    /// The order is fixed so that P4, which repeats P2's payload, always runs
    /// after P2. There is intentionally no cold/warm distinction.
    pub fn single(schedule_seed: u64) -> Self {
        let task_seed = derive_task_seed(schedule_seed);
        let entries = ExperimentArm::ALL
            .into_iter()
            .map(|arm| ScheduleEntry { arm, repetition: 0, task_seed })
            .collect();
        Self { entries, schedule_seed }
    }
}

impl Default for ExperimentSchedule {
    fn default() -> Self {
        Self::single(0)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct JsonlParseResult {
    pub observations: Vec<ExperimentObservation>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskFixture {
    pub id: String,
    pub prompt: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Error)]
pub enum TaskFixtureError {
    #[error("task fixture JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("task fixture must contain a non-empty array")]
    Empty,
}

pub fn parse_task_fixture(input: &str) -> Result<Vec<TaskFixture>, TaskFixtureError> {
    let value: Value = serde_json::from_str(input)?;
    let tasks = value
        .get("tasks")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or(TaskFixtureError::Empty)?;
    if tasks.is_empty() {
        return Err(TaskFixtureError::Empty);
    }
    let mut parsed = Vec::with_capacity(tasks.len());
    for task in tasks {
        parsed.push(serde_json::from_value(task.clone())?);
    }
    Ok(parsed)
}

fn derive_task_seed(schedule_seed: u64) -> u64 {
    splitmix64(schedule_seed ^ 0xBEEF_u64)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Parse event JSONL without allowing one malformed line to erase the rest of
/// an experiment. Unknown fields are retained in `extra`.
pub fn parse_jsonl(input: &str) -> JsonlParseResult {
    let mut result = JsonlParseResult::default();
    for (line_number, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ExperimentObservation>(line) {
            Ok(observation) => result.observations.push(observation),
            Err(error) => result.errors.push(format!("line {}: {error}", line_number + 1)),
        }
    }
    result
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CodexParseResult {
    pub thread_id: Option<String>,
    pub terminal_event: bool,
    pub terminal_success: Option<bool>,
    pub tool_call_success: Option<bool>,
    pub usage: UsageObservation,
    pub compaction_events: u32,
    pub compaction_precision: MetricPrecision,
    pub continuation_success: Option<bool>,
    pub prompt_profile_digest: Option<Digest>,
    pub profile_payload_bytes: Option<u64>,
    pub mcp_startup_ms: Option<u64>,
    pub mcp_startup_precision: MetricPrecision,
    pub final_response: Option<String>,
    pub discovery_before_brief: u32,
    pub discovery_after_brief: u32,
    pub discovery_total: u32,
    pub subagent_spawns: u32,
    pub errors: Vec<String>,
}

/// Extract usage from the documented Codex 0.144 JSONL event shapes. Unknown
/// event types are ignored; success is never inferred from arbitrary text.
pub fn parse_codex_jsonl(input: &str) -> CodexParseResult {
    let mut result = CodexParseResult::default();
    let mut semantic_interrupt_seen = false;
    for (line_number, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                result.errors.push(format!("line {}: {error}", line_number + 1));
                continue;
            }
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            result.errors.push(format!("line {}: missing event type", line_number + 1));
            continue;
        };
        match event_type {
            "thread.started" => {
                result.thread_id = direct_string(&value, "thread_id")
                    .or_else(|| value.get("thread").and_then(|thread| direct_string(thread, "id")));
            }
            "turn.completed" => {
                result.terminal_event = true;
                if result.terminal_success != Some(false) {
                    result.terminal_success = Some(true);
                }
                if let Some(usage) = value.get("usage").and_then(Value::as_object) {
                    parse_usage_object(usage, &mut result.usage);
                }
                result.usage.latency_ms =
                    direct_u64(&value, "duration_ms").or_else(|| direct_u64(&value, "latency_ms"));
                result.usage.latency_precision = if result.usage.latency_ms.is_some() {
                    MetricPrecision::Exact
                } else {
                    MetricPrecision::Unavailable
                };
                result.prompt_profile_digest = result.prompt_profile_digest.or_else(|| {
                    direct_string(&value, "profile_digest")
                        .and_then(|text| Digest::parse(&text).ok())
                });
                result.profile_payload_bytes = result
                    .profile_payload_bytes
                    .or_else(|| direct_u64(&value, "profile_payload_bytes"));
            }
            "turn.failed" | "turn.error" | "error" => {
                result.terminal_event = true;
                result.terminal_success = Some(false);
            }
            "item.completed" => {
                let Some(item) = value.get("item").and_then(Value::as_object) else {
                    continue;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") => {
                        if let Some(text) = item_text(item) {
                            if text.contains("@@need:") {
                                semantic_interrupt_seen = true;
                            }
                            result.final_response = Some(text);
                        }
                    }
                    Some(
                        "command_execution" | "shell_command" | "file_read" | "search"
                        | "tool_call",
                    ) if is_discovery_item(item) => {
                        result.discovery_total = result.discovery_total.saturating_add(1);
                        if semantic_interrupt_seen {
                            result.discovery_after_brief =
                                result.discovery_after_brief.saturating_add(1);
                        } else {
                            result.discovery_before_brief =
                                result.discovery_before_brief.saturating_add(1);
                        }
                    }
                    Some("collaboration_tool_call" | "subagent") => {
                        if item_text(item).is_some_and(|text| {
                            text.contains("spawn_agent") || text.contains("spawn")
                        }) {
                            result.subagent_spawns = result.subagent_spawns.saturating_add(1);
                        }
                    }
                    Some("mcp_tool_call") => {
                        let name = direct_string_value(item, &["name", "tool_name", "tool"]);
                        let has_error = item.get("error").is_some_and(|error| !error.is_null());
                        if name == Some("need_context") && !has_error {
                            result.tool_call_success = Some(true);
                        } else if has_error {
                            result.tool_call_success = Some(false);
                        }
                    }
                    Some("compaction") => {
                        result.compaction_events = result.compaction_events.saturating_add(1);
                        result.compaction_precision = MetricPrecision::Exact;
                    }
                    _ => {}
                }
            }
            "item.error" => {
                if value
                    .get("item")
                    .and_then(Value::as_object)
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("mcp_tool_call")
                {
                    result.tool_call_success = Some(false);
                }
            }
            "turn.compacted" | "compaction.completed" => {
                result.compaction_events = result.compaction_events.saturating_add(1);
                result.compaction_precision = MetricPrecision::Exact;
            }
            "mcp.startup" => {
                result.mcp_startup_ms = direct_u64(&value, "duration_ms");
                result.mcp_startup_precision = if result.mcp_startup_ms.is_some() {
                    MetricPrecision::Exact
                } else {
                    MetricPrecision::Unavailable
                };
            }
            _ => {}
        }
    }
    result.continuation_success = if result.terminal_event {
        if result.terminal_success == Some(false) || result.tool_call_success == Some(false) {
            Some(false)
        } else if result.terminal_success == Some(true) {
            Some(true)
        } else {
            None
        }
    } else {
        None
    };
    result.usage.precision = if [
        result.usage.input_precision,
        result.usage.cached_input_precision,
        result.usage.cache_write_precision,
        result.usage.output_precision,
    ]
    .iter()
    .all(|precision| *precision == MetricPrecision::Exact)
    {
        MetricPrecision::Exact
    } else if [
        result.usage.input_precision,
        result.usage.cached_input_precision,
        result.usage.cache_write_precision,
        result.usage.output_precision,
    ]
    .contains(&MetricPrecision::Aggregate)
    {
        MetricPrecision::Aggregate
    } else if [
        result.usage.input_precision,
        result.usage.cached_input_precision,
        result.usage.cache_write_precision,
        result.usage.output_precision,
    ]
    .iter()
    .any(|precision| *precision != MetricPrecision::Unavailable)
    {
        MetricPrecision::Partial
    } else {
        MetricPrecision::Unavailable
    };
    result
}

fn item_text(item: &Map<String, Value>) -> Option<String> {
    for key in ["text", "message", "command", "name", "tool_name"] {
        if let Some(value) = item.get(key).and_then(Value::as_str) {
            return Some(value.to_owned());
        }
    }
    item.get("content").and_then(|content| match content {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => {
            let text = values
                .iter()
                .filter_map(|value| {
                    value.get("text").and_then(Value::as_str).or_else(|| value.as_str())
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn is_discovery_item(item: &Map<String, Value>) -> bool {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(item_type, "file_read" | "search") {
        return true;
    }
    let text = item_text(item).unwrap_or_default().to_ascii_lowercase();
    [
        "rg ",
        "rg.exe ",
        "git grep",
        "grep ",
        "findstr ",
        "get-content",
        "select-string",
        "cat ",
        "sed ",
        "head ",
        "tail ",
        "ls ",
        "dir ",
        "get-childitem",
        "read_file",
        "search",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn direct_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn direct_string_value<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn direct_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn parse_usage_object(usage: &Map<String, Value>, output: &mut UsageObservation) {
    let aggregate = usage.get("precision").and_then(Value::as_str) == Some("aggregate");
    let (value, precision) = metric(usage.get("input_tokens"), aggregate);
    output.input_tokens = value;
    output.input_precision = precision;
    let (value, precision) = metric(usage.get("cached_input_tokens"), aggregate);
    output.cached_input_tokens = value;
    output.cached_input_precision = precision;
    let (value, precision) = metric(usage.get("cache_write_tokens"), aggregate);
    output.cache_write_tokens = value;
    output.cache_write_precision = precision;
    let (value, precision) = metric(usage.get("output_tokens"), aggregate);
    output.output_tokens = value;
    output.output_precision = precision;
}

fn metric(value: Option<&Value>, aggregate: bool) -> (Option<u64>, MetricPrecision) {
    match value.and_then(Value::as_u64) {
        Some(value) if aggregate => (Some(value), MetricPrecision::Aggregate),
        Some(value) => (Some(value), MetricPrecision::Exact),
        None => (None, MetricPrecision::Unavailable),
    }
}

/// Remove model-visible prompt and content fields recursively.  Structural
/// measurements and digests remain available for offline analysis.
pub fn redact_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if ["prompt", "message", "content", "request", "text"]
                    .iter()
                    .any(|needle| lower.contains(needle))
                {
                    *child = Value::String("<redacted>".to_owned());
                } else {
                    redact_value(child);
                }
            }
        }
        Value::Array(array) => {
            for child in array {
                redact_value(child);
            }
        }
        _ => {}
    }
}

pub fn redact_jsonl(input: &str) -> String {
    let mut lines = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                redact_value(&mut value);
                lines.push(serde_json::to_string(&value).expect("JSON values serialize"));
            }
            Err(_) => lines.push("{\"redacted_invalid_line\":true}".to_owned()),
        }
    }
    if lines.is_empty() { String::new() } else { format!("{}\n", lines.join("\n")) }
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<Digest, ArtifactError> {
        let digest = Digest::blake3(bytes);
        let path = self.path_for(digest);
        if !path.exists() {
            let temporary = path.with_extension("tmp");
            fs::write(&temporary, bytes)?;
            fs::rename(temporary, &path)?;
        }
        Ok(digest)
    }

    pub fn get(&self, digest: Digest) -> Result<Vec<u8>, ArtifactError> {
        Ok(fs::read(self.path_for(digest))?)
    }

    pub fn path_for(&self, digest: Digest) -> PathBuf {
        self.root.join(format!("{}.json", digest.to_hex()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AggregateMetric {
    pub count: usize,
    pub successful: usize,
    pub median_latency_ms: Option<u64>,
    pub sorted_latency_ms: Vec<u64>,
    pub median_profile_payload_bytes: Option<u64>,
    pub median_mcp_startup_ms: Option<u64>,
    pub compaction_precision: MetricPrecision,
    pub mcp_startup_precision: MetricPrecision,
    pub bootstrap_confidence_interval_ms: Option<[f64; 2]>,
    pub confidence_level: f64,
    pub bootstrap_resamples: usize,
    pub precision: MetricPrecision,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExperimentReport {
    pub arms: BTreeMap<ExperimentArm, AggregateMetric>,
    pub observations: usize,
    pub parse_errors: usize,
    pub process_failures: usize,
    pub duplicate_keys: usize,
    pub expected_observations: usize,
    pub cohort_complete: bool,
    pub schedule_consistent: bool,
    pub benchmark_identity_consistent: bool,
    pub preliminary_verdict: String,
    pub verdict_explanation: String,
    pub counterfactual_estimate: Option<String>,
}

impl ExperimentReport {
    pub fn from_observations(observations: &[ExperimentObservation], parse_errors: usize) -> Self {
        let mut arms = BTreeMap::new();
        for arm in ExperimentArm::ALL {
            arms.insert(arm, aggregate(observations, arm));
        }
        let expected = ExperimentSchedule::default();
        let expected_keys = expected
            .entries
            .iter()
            .map(|entry| (entry.repetition, entry.arm))
            .collect::<std::collections::BTreeSet<_>>();
        let mut observed_keys = std::collections::BTreeSet::new();
        let mut duplicate_keys = 0;
        let mut schedule_consistent = true;
        let mut observed_task_seed = None;
        let mut expected_identity = None;
        let mut benchmark_identity_consistent = true;
        for observation in observations {
            if !observed_keys.insert((observation.repetition, observation.arm)) {
                duplicate_keys += 1;
            }
            if expected.entries.iter().any(|entry| {
                entry.repetition == observation.repetition && entry.arm == observation.arm
            }) {
                if let Some(seed) = observed_task_seed {
                    schedule_consistent &= seed == observation.task_seed;
                } else {
                    observed_task_seed = Some(observation.task_seed);
                }
            } else {
                schedule_consistent = false;
            }
            match benchmark_identity(observation) {
                Some(identity) => {
                    if let Some(expected) = &expected_identity {
                        benchmark_identity_consistent &= expected == &identity;
                    } else {
                        expected_identity = Some(identity);
                    }
                }
                None => benchmark_identity_consistent = false,
            }
        }
        let cohort_complete = observations.len() == expected.entries.len()
            && duplicate_keys == 0
            && observed_keys == expected_keys;
        let process_failures = observations
            .iter()
            .filter(|observation| {
                observation.extra.get("process_failure") == Some(&Value::Bool(true))
            })
            .count();
        let complete_evidence = cohort_complete
            && parse_errors == 0
            && process_failures == 0
            && schedule_consistent
            && benchmark_identity_consistent;
        let transport_success =
            [ExperimentArm::P1, ExperimentArm::P2, ExperimentArm::P3, ExperimentArm::P4]
                .iter()
                .all(|arm| {
                    let metric = arms.get(arm).expect("all arms are present");
                    metric.count == 1 && metric.successful == 1
                });
        let (preliminary_verdict, verdict_explanation) = if !complete_evidence {
            (
                "inconclusive".to_owned(),
                "cohort incomplete, duplicated, parse-invalid, process-failed, schedule-inconsistent, or benchmark-identity-inconsistent"
                    .to_owned(),
            )
        } else if transport_success {
            (
                "preliminary-pass".to_owned(),
                "complete single-run cohort with P1-P4 each at 1/1 continuation success".to_owned(),
            )
        } else {
            (
                "preliminary-fail".to_owned(),
                "complete single-run cohort but one or more P1-P4 observations failed".to_owned(),
            )
        };
        Self {
            arms,
            observations: observations.len(),
            parse_errors,
            process_failures,
            duplicate_keys,
            expected_observations: expected.entries.len(),
            cohort_complete,
            schedule_consistent,
            benchmark_identity_consistent,
            preliminary_verdict,
            verdict_explanation,
            counterfactual_estimate: None,
        }
    }

    pub fn with_counterfactual_estimate(mut self, estimate: impl Into<String>) -> Self {
        self.counterfactual_estimate = Some(estimate.into());
        self
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

fn benchmark_identity(
    observation: &ExperimentObservation,
) -> Option<(String, String, String, String)> {
    let text = |key: &str| observation.extra.get(key)?.as_str().map(ToOwned::to_owned);
    Some((
        text("task_id")?,
        text("fixture_digest")?,
        text("repository_sha")?,
        text("request_digest")?,
    ))
}

fn aggregate(observations: &[ExperimentObservation], arm: ExperimentArm) -> AggregateMetric {
    let mut latencies = observations
        .iter()
        .filter(|observation| observation.arm == arm)
        .filter_map(|observation| observation.usage.latency_ms)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let matching =
        observations.iter().filter(|observation| observation.arm == arm).collect::<Vec<_>>();
    let successful = matching
        .iter()
        .filter(|observation| observation.continuation_success == Some(true))
        .count();
    let profile_sizes = matching
        .iter()
        .filter_map(|observation| observation.profile_payload_bytes)
        .collect::<Vec<_>>();
    let mcp_startups =
        matching.iter().filter_map(|observation| observation.mcp_startup_ms).collect::<Vec<_>>();
    let precision = if matching.is_empty()
        || matching
            .iter()
            .all(|observation| observation.usage.precision == MetricPrecision::Unavailable)
    {
        MetricPrecision::Unavailable
    } else if matching
        .iter()
        .all(|observation| observation.usage.precision == MetricPrecision::Exact)
    {
        MetricPrecision::Exact
    } else if matching
        .iter()
        .any(|observation| observation.usage.precision == MetricPrecision::Partial)
    {
        MetricPrecision::Partial
    } else {
        MetricPrecision::Aggregate
    };
    let compaction_precision =
        combine_precision(matching.iter().map(|observation| observation.compaction_precision));
    let mcp_startup_precision =
        combine_precision(matching.iter().map(|observation| observation.mcp_startup_precision));
    AggregateMetric {
        count: matching.len(),
        successful,
        median_latency_ms: latencies.get(latencies.len().saturating_sub(1) / 2).copied(),
        sorted_latency_ms: latencies,
        median_profile_payload_bytes: median(&profile_sizes),
        median_mcp_startup_ms: median(&mcp_startups),
        compaction_precision,
        mcp_startup_precision,
        bootstrap_confidence_interval_ms: None,
        confidence_level: 0.0,
        bootstrap_resamples: 0,
        precision,
    }
}

fn combine_precision(values: impl Iterator<Item = MetricPrecision>) -> MetricPrecision {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() || values.iter().all(|value| *value == MetricPrecision::Unavailable) {
        MetricPrecision::Unavailable
    } else if values.iter().all(|value| *value == MetricPrecision::Exact) {
        MetricPrecision::Exact
    } else if values.contains(&MetricPrecision::Partial) {
        MetricPrecision::Partial
    } else {
        MetricPrecision::Aggregate
    }
}

fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    values.get((values.len() - 1) / 2).copied()
}

/// The exact deterministic payload used for P2 and P3 controls.
pub fn p2_payload(request: &NeedRequest) -> String {
    const RIPGREP_CONTEXT: &str = include_str!("../../../fixtures/ripgrep-14.1.1-context.txt");
    let answer = format!(
        "Deterministic benchmark context for `{}`.\n\nRequest:\n{}\n\n{}\nNo worker or live repository access was used to build this payload.",
        request.key,
        request.body,
        RIPGREP_CONTEXT.trim()
    );
    ContinuationEnvelope::new(request.key.clone(), answer).render()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(arm: ExperimentArm, repetition: u32) -> ExperimentObservation {
        let extra = Map::from_iter([
            ("task_id".to_owned(), Value::String("task".to_owned())),
            ("fixture_digest".to_owned(), Value::String("b3:fixture".to_owned())),
            ("repository_sha".to_owned(), Value::String("sha".to_owned())),
            ("request_digest".to_owned(), Value::String("b3:request".to_owned())),
        ]);
        ExperimentObservation {
            arm,
            repetition,
            task_seed: derive_task_seed(0),
            usage: UsageObservation {
                latency_ms: Some(10 + repetition as u64),
                precision: MetricPrecision::Exact,
                ..UsageObservation::default()
            },
            prompt_profile_digest: None,
            profile_payload_bytes: None,
            mcp_startup_ms: None,
            compaction_events: 0,
            compaction_precision: MetricPrecision::Unavailable,
            mcp_startup_precision: MetricPrecision::Unavailable,
            continuation_success: Some(true),
            artifact_digest: None,
            extra,
        }
    }

    #[test]
    fn schedule_is_balanced_and_repeatable() {
        let schedule = ExperimentSchedule::default();
        assert_eq!(schedule.entries.len(), 5);
        assert_eq!(
            schedule.entries.iter().map(|entry| entry.arm).collect::<Vec<_>>(),
            ExperimentArm::ALL
        );
        assert_eq!(schedule.entries[2].arm, ExperimentArm::P2);
        assert_eq!(schedule.entries[4].arm, ExperimentArm::P4);
        let seeded = ExperimentSchedule::single(42);
        assert_eq!(seeded, ExperimentSchedule::single(42));
        assert!(seeded.entries.iter().all(|entry| entry.task_seed == seeded.entries[0].task_seed));
        assert_ne!(seeded.entries, ExperimentSchedule::single(43).entries);
    }

    #[test]
    fn parser_is_tolerant_and_redaction_is_recursive() {
        let line = serde_json::to_string(&observation(ExperimentArm::P2, 0)).unwrap();
        let parsed = parse_jsonl(&format!("{line}\nnot json\n"));
        assert_eq!(parsed.observations.len(), 1);
        assert_eq!(parsed.errors.len(), 1);
        let redacted = redact_jsonl(r#"{"prompt":"secret","nested":{"content":"x"},"arm":"P2"}"#);
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("<redacted>"));
        let codex = parse_codex_jsonl(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"profile_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000","latency_ms":7}
{"type":"turn.compacted"}"#,
        );
        assert_eq!(codex.usage.precision, MetricPrecision::Partial);
        assert_eq!(codex.compaction_events, 1);
        assert_eq!(codex.continuation_success, Some(true));
        let unavailable = parse_codex_jsonl("{\"status\":\"ok\"}");
        assert_eq!(unavailable.continuation_success, None);
    }

    #[test]
    fn report_keeps_one_metric_per_arm_and_does_not_claim_saved_counterfactuals() {
        let observations = vec![observation(ExperimentArm::P2, 0)];
        let report = ExperimentReport::from_observations(&observations, 0)
            .with_counterfactual_estimate("estimated comparison only");
        assert_eq!(report.arms[&ExperimentArm::P2].count, 1);
        assert!(report.counterfactual_estimate.as_deref().unwrap().contains("estimated"));
        assert_eq!(report.preliminary_verdict, "inconclusive");
        assert_eq!(report.arms[&ExperimentArm::P2].bootstrap_resamples, 0);
        assert_eq!(report.arms[&ExperimentArm::P2].bootstrap_confidence_interval_ms, None);
    }

    #[test]
    fn report_requires_exact_complete_cohort_before_pass_or_fail() {
        let schedule = ExperimentSchedule::single(42);
        let observations = schedule
            .entries
            .iter()
            .map(|entry| ExperimentObservation {
                arm: entry.arm,
                repetition: entry.repetition,
                task_seed: entry.task_seed,
                usage: UsageObservation {
                    latency_ms: Some(10),
                    precision: MetricPrecision::Exact,
                    ..UsageObservation::default()
                },
                prompt_profile_digest: None,
                profile_payload_bytes: None,
                mcp_startup_ms: None,
                compaction_events: 0,
                compaction_precision: MetricPrecision::Unavailable,
                mcp_startup_precision: MetricPrecision::Unavailable,
                continuation_success: Some(true),
                artifact_digest: None,
                extra: observation(entry.arm, entry.repetition).extra,
            })
            .collect::<Vec<_>>();
        let report = ExperimentReport::from_observations(&observations, 0);
        assert!(report.cohort_complete);
        assert!(report.schedule_consistent);
        assert!(report.benchmark_identity_consistent);
        assert_eq!(report.preliminary_verdict, "preliminary-pass");
        let mut duplicate = observations.clone();
        duplicate.push(observations[0].clone());
        let duplicate_report = ExperimentReport::from_observations(&duplicate, 0);
        assert_eq!(duplicate_report.duplicate_keys, 1);
        assert_eq!(duplicate_report.preliminary_verdict, "inconclusive");
        let mut mismatched = observations;
        mismatched[4]
            .extra
            .insert("fixture_digest".to_owned(), Value::String("different".to_owned()));
        let mismatched_report = ExperimentReport::from_observations(&mismatched, 0);
        assert!(!mismatched_report.benchmark_identity_consistent);
        assert_eq!(mismatched_report.preliminary_verdict, "inconclusive");
    }
}
