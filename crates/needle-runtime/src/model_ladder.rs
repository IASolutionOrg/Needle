use needle_core::{Digest, ModelPolicy, WorkerProfile};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LadderAttempt<T> {
    Validated(T),
    Invalid { reason: String },
    InfrastructureFailure { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LadderAttemptRecord {
    pub profile_digest: Digest,
    pub repair: bool,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelLadderOutcome<T> {
    Validated { value: T, profile: WorkerProfile, repair: bool, attempts: Vec<LadderAttemptRecord> },
    NativeFallback { attempts: Vec<LadderAttemptRecord> },
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ModelLadderError {
    #[error("model policy contains no worker profiles")]
    Empty,
    #[error("CheapestValidatedFirst contains an unpromoted profile")]
    UnpromotedProfile,
    #[error("model ladder exhausted and native fallback is disabled")]
    Exhausted,
}

pub struct ModelLadder {
    policy: ModelPolicy,
}

impl ModelLadder {
    pub fn new(
        policy: ModelPolicy,
        promoted_profiles: &BTreeSet<Digest>,
    ) -> Result<Self, ModelLadderError> {
        let profiles = match &policy {
            ModelPolicy::FixedOrder { profiles, .. } => profiles,
            ModelPolicy::CheapestValidatedFirst { promoted_profiles: profiles, .. } => {
                if profiles
                    .iter()
                    .any(|profile| !promoted_profiles.contains(&profile.definition_digest))
                {
                    return Err(ModelLadderError::UnpromotedProfile);
                }
                profiles
            }
        };
        if profiles.is_empty() {
            return Err(ModelLadderError::Empty);
        }
        Ok(Self { policy })
    }

    pub fn run<T, F>(&self, mut execute: F) -> Result<ModelLadderOutcome<T>, ModelLadderError>
    where
        F: FnMut(&WorkerProfile, bool) -> LadderAttempt<T>,
    {
        let (profiles, repair_once, native_fallback) = match &self.policy {
            ModelPolicy::FixedOrder { profiles, repair_once, native_fallback } => {
                (profiles, *repair_once, *native_fallback)
            }
            ModelPolicy::CheapestValidatedFirst { promoted_profiles, native_fallback } => {
                (promoted_profiles, false, *native_fallback)
            }
        };
        let mut records = Vec::new();
        for profile in profiles {
            match execute(profile, false) {
                LadderAttempt::Validated(value) => {
                    records.push(record(profile, false, "validated"));
                    return Ok(ModelLadderOutcome::Validated {
                        value,
                        profile: profile.clone(),
                        repair: false,
                        attempts: records,
                    });
                }
                LadderAttempt::Invalid { .. } => {
                    records.push(record(profile, false, "invalid"));
                    if repair_once {
                        match execute(profile, true) {
                            LadderAttempt::Validated(value) => {
                                records.push(record(profile, true, "validated"));
                                return Ok(ModelLadderOutcome::Validated {
                                    value,
                                    profile: profile.clone(),
                                    repair: true,
                                    attempts: records,
                                });
                            }
                            LadderAttempt::Invalid { .. } => {
                                records.push(record(profile, true, "invalid"));
                            }
                            LadderAttempt::InfrastructureFailure { .. } => {
                                records.push(record(profile, true, "infrastructure_failure"));
                            }
                        }
                    }
                }
                LadderAttempt::InfrastructureFailure { .. } => {
                    records.push(record(profile, false, "infrastructure_failure"));
                }
            }
        }
        if native_fallback {
            Ok(ModelLadderOutcome::NativeFallback { attempts: records })
        } else {
            Err(ModelLadderError::Exhausted)
        }
    }
}

fn record(profile: &WorkerProfile, repair: bool, status: &str) -> LadderAttemptRecord {
    LadderAttemptRecord {
        profile_digest: profile.definition_digest,
        repair,
        status: status.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(model: &str) -> WorkerProfile {
        WorkerProfile::new("codex", model, "medium", None)
    }

    #[test]
    fn fixed_order_repairs_same_profile_then_escalates() {
        let cheap = profile("cheap");
        let strong = profile("strong");
        let ladder = ModelLadder::new(
            ModelPolicy::FixedOrder {
                profiles: vec![cheap.clone(), strong.clone()],
                repair_once: true,
                native_fallback: true,
            },
            &BTreeSet::new(),
        )
        .unwrap();
        let mut calls = Vec::new();
        let outcome = ladder
            .run(|profile, repair| {
                calls.push((profile.model.clone(), repair));
                if profile.model == "strong" {
                    LadderAttempt::Validated("brief")
                } else {
                    LadderAttempt::Invalid { reason: "semantic validation".to_owned() }
                }
            })
            .unwrap();
        assert_eq!(
            calls,
            vec![
                ("cheap".to_owned(), false),
                ("cheap".to_owned(), true),
                ("strong".to_owned(), false),
            ]
        );
        assert!(matches!(
            outcome,
            ModelLadderOutcome::Validated { profile, repair: false, .. } if profile == strong
        ));
    }

    #[test]
    fn cheapest_first_rejects_any_unpromoted_profile() {
        let cheap = profile("cheap");
        let policy = ModelPolicy::CheapestValidatedFirst {
            promoted_profiles: vec![cheap],
            native_fallback: true,
        };
        assert_eq!(
            ModelLadder::new(policy, &BTreeSet::new()).err(),
            Some(ModelLadderError::UnpromotedProfile)
        );
    }

    #[test]
    fn infrastructure_failure_does_not_consume_a_repair_turn() {
        let cheap = profile("cheap");
        let ladder = ModelLadder::new(
            ModelPolicy::FixedOrder {
                profiles: vec![cheap],
                repair_once: true,
                native_fallback: true,
            },
            &BTreeSet::new(),
        )
        .unwrap();
        let mut calls = 0;
        let outcome = ladder
            .run::<(), _>(|_, _| {
                calls += 1;
                LadderAttempt::InfrastructureFailure { reason: "transport".to_owned() }
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert!(matches!(outcome, ModelLadderOutcome::NativeFallback { .. }));
    }
}
