use crate::{ArtifactId, Digest, Need, NeedId, ObligationId, ReuseSufficiencyCertificateId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const DEFAULT_MAX_NEEDS_PER_TASK: u8 = 3;
pub const DEFAULT_MAX_WORKERS_PER_TASK: u8 = 3;
pub const HARD_MAX_NEEDS_PER_TASK: u8 = 8;
pub const HARD_MAX_WORKERS_PER_TASK: u8 = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NeedCoordination {
    #[default]
    WaitResponse,
    ContinueWorking,
}

impl NeedCoordination {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "wait-response" => Some(Self::WaitResponse),
            "continue-working" => Some(Self::ContinueWorking),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaitResponse => "wait-response",
            Self::ContinueWorking => "continue-working",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiNeedPolicy {
    pub multi_need_enabled: bool,
    pub continue_working_enabled: bool,
    pub max_needs_per_task: u8,
    pub max_workers_per_task: u8,
    pub pending_main_tools: PendingMainTools,
    pub resolver_concurrency: u8,
}

impl Default for MultiNeedPolicy {
    fn default() -> Self {
        Self {
            multi_need_enabled: true,
            continue_working_enabled: true,
            max_needs_per_task: DEFAULT_MAX_NEEDS_PER_TASK,
            max_workers_per_task: DEFAULT_MAX_WORKERS_PER_TASK,
            pending_main_tools: PendingMainTools::AllowAndTaint,
            resolver_concurrency: 1,
        }
    }
}

impl MultiNeedPolicy {
    pub fn validate(&self) -> bool {
        self.max_needs_per_task > 0
            && self.max_needs_per_task <= HARD_MAX_NEEDS_PER_TASK
            && self.max_workers_per_task > 0
            && self.max_workers_per_task <= HARD_MAX_WORKERS_PER_TASK
            && self.resolver_concurrency == 1
    }

    pub fn digest(&self) -> Digest {
        let bytes = serde_json::to_vec(self).expect("MultiNeedPolicy serialization is infallible");
        Digest::blake3(bytes)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingMainTools {
    #[default]
    AllowAndTaint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedStepRelation {
    Repeat,
    Residual,
    Extension,
    Overlap,
    Independent,
    Incompatible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedStepState {
    Requested,
    Queued,
    Resolving,
    Resolved,
    Delivered,
    NativeFallback,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedDelivery {
    TurnStart,
    TurnSteer,
    AlreadySatisfied,
    NativeFallback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MainTurnOutcome {
    Need { step: NeedStep },
    Final { response: String },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedStep {
    pub id: Digest,
    pub ordinal: u8,
    pub turn_id: String,
    pub need_id: NeedId,
    pub coordination: NeedCoordination,
    pub relation: NeedStepRelation,
    pub state: NeedStepState,
    pub required: Vec<ObligationId>,
    pub satisfied: Vec<ObligationId>,
    pub missing: Vec<ObligationId>,
    pub artifacts: Vec<ArtifactId>,
    pub proof: Option<ReuseSufficiencyCertificateId>,
    pub delivery: Option<NeedDelivery>,
    #[serde(default)]
    pub worker_avoided: bool,
    pub main_discovery_tainted: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedSequence {
    pub session_id: String,
    pub steps: Vec<NeedStep>,
}

impl NeedSequence {
    pub fn next_ordinal(&self) -> Option<u8> {
        u8::try_from(self.steps.len()).ok()?.checked_add(1)
    }
}

pub fn classify_need_step(
    previous: &Need,
    current: &Need,
    previously_satisfied: &[ObligationId],
) -> NeedStepRelation {
    if previous.id == current.id {
        return NeedStepRelation::Repeat;
    }
    if previous.world != current.world {
        return NeedStepRelation::Incompatible;
    }
    let previous_subjects = previous.subjects.iter().map(|value| value.id).collect::<BTreeSet<_>>();
    let current_subjects = current.subjects.iter().map(|value| value.id).collect::<BTreeSet<_>>();
    if previous_subjects != current_subjects {
        return NeedStepRelation::Independent;
    }
    if previous
        .required
        .iter()
        .any(|left| current.required.iter().any(|right| obligations_contradict(left, right)))
    {
        return NeedStepRelation::Incompatible;
    }

    let satisfied = previously_satisfied.iter().copied().collect::<BTreeSet<_>>();
    let current_is_covered_by_previous = current.required.iter().all(|current| {
        previous.required.iter().any(|previous| obligations_compatible(previous, current))
    });
    let current_has_unsatisfied = current.required.iter().any(|current| {
        !previous.required.iter().any(|previous| {
            satisfied.contains(&previous.id) && obligations_compatible(previous, current)
        })
    });
    if current_is_covered_by_previous && current_has_unsatisfied {
        return NeedStepRelation::Residual;
    }
    if previous.required.iter().all(|previous| {
        current.required.iter().any(|current| obligations_compatible(previous, current))
    }) {
        return NeedStepRelation::Extension;
    }
    if previous.required.iter().any(|previous| {
        current.required.iter().any(|current| obligations_compatible(previous, current))
    }) {
        return NeedStepRelation::Overlap;
    }
    NeedStepRelation::Independent
}

fn obligations_contradict(left: &crate::Obligation, right: &crate::Obligation) -> bool {
    if left.predicate != right.predicate || left.subject != right.subject {
        return false;
    }
    fn polarity(obligation: &crate::Obligation) -> Option<&str> {
        obligation
            .facets
            .iter()
            .find(|facet| facet.key == "polarity")
            .map(|facet| facet.value.as_str())
    }
    matches!((polarity(left), polarity(right)), (Some(left), Some(right)) if left != right)
}

fn obligations_compatible(left: &crate::Obligation, right: &crate::Obligation) -> bool {
    left.predicate == right.predicate
        && left.subject == right.subject
        && left.facets.iter().all(|left_facet| {
            right
                .facets
                .iter()
                .find(|right_facet| right_facet.key == left_facet.key)
                .is_none_or(|right_facet| right_facet.value == left_facet.value)
        })
        && right.facets.iter().all(|right_facet| {
            left.facets
                .iter()
                .find(|left_facet| left_facet.key == right_facet.key)
                .is_none_or(|left_facet| left_facet.value == right_facet.value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Facet, Obligation, PredicateKind, SemanticWorld, Subject, SubjectKind};

    fn need(required: &[PredicateKind], world: &str) -> Need {
        let lineage = Digest::blake3(b"repository");
        let subject = Subject::exact(lineage, SubjectKind::Symbol, "answer");
        let world = SemanticWorld {
            repository_lineage: lineage,
            source_selector: world.to_owned(),
            platform: "windows".to_owned(),
            features: "default".to_owned(),
            configuration: None,
            toolchain: None,
        };
        let required = required
            .iter()
            .map(|predicate| Obligation::new(*predicate, subject.id, Vec::<Facet>::new()))
            .collect::<Vec<_>>();
        let mut value = Need {
            id: NeedId(Digest::blake3(b"pending")),
            subjects: vec![subject],
            required,
            preferred: Vec::new(),
            semantic_constraints: Vec::new(),
            world,
            input_artifacts: Vec::new(),
            residual: None,
            body_digest: Digest::blake3(b"body"),
            format_revision: 1,
        };
        value.id =
            NeedId(Digest::blake3(serde_json::to_vec(&(&value.required, &value.world)).unwrap()));
        value
    }

    #[test]
    fn relation_distinguishes_repeat_residual_extension_and_world_drift() {
        let location = need(&[PredicateKind::ImplementationLocation], "current");
        let broad =
            need(&[PredicateKind::ImplementationLocation, PredicateKind::FocusedTests], "current");
        let tests = need(&[PredicateKind::FocusedTests], "current");
        let drifted = need(&[PredicateKind::ImplementationLocation], "head");

        assert_eq!(classify_need_step(&location, &location, &[]), NeedStepRelation::Repeat);
        assert_eq!(classify_need_step(&broad, &tests, &[]), NeedStepRelation::Residual);
        assert_eq!(classify_need_step(&location, &broad, &[]), NeedStepRelation::Extension);
        assert_eq!(classify_need_step(&location, &drifted, &[]), NeedStepRelation::Incompatible);
        let mut positive = location.clone();
        positive.required[0] = Obligation::new(
            PredicateKind::ImplementationLocation,
            positive.subjects[0].id,
            vec![Facet { key: "polarity".to_owned(), value: "positive".to_owned() }],
        );
        positive.id = NeedId(Digest::blake3(b"positive"));
        let mut negative = location;
        negative.required[0] = Obligation::new(
            PredicateKind::ImplementationLocation,
            negative.subjects[0].id,
            vec![Facet { key: "polarity".to_owned(), value: "negative".to_owned() }],
        );
        negative.id = NeedId(Digest::blake3(b"negative"));
        assert_eq!(classify_need_step(&positive, &negative, &[]), NeedStepRelation::Incompatible);
    }
}
