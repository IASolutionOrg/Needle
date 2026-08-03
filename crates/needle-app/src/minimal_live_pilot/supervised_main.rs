use super::{CachePilotResolveOutcome, Observe};
use needle_bench::ProcessExecutionStatus;
use needle_core::{
    ArtifactId, CacheResolution, CanonicalHasher, EvidenceFailurePolicy, MainTurnOutcome, Need,
    NeedDelivery, NeedId, NeedStep, NeedStepRelation, NeedStepState, WorkerConfig,
    built_in_route_contracts, classify_need_step, compile_need,
};
use needle_platform_codex::{
    CodexMainSession, CodexWorker, MainContinuationDiagnostics, MainNeedDiagnostics,
    MainSessionConfig, MainTurnResult, MainUsage,
};
use needle_runtime::{MainTurnObservationRecord, ResolveRequest, RuntimeEngine, RuntimeStore};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct SupervisedMain {
    pub(super) provider_observation_started: bool,
    pub(super) transport_started: bool,
    pub(super) final_response: String,
    pub(super) usage: MainUsage,
    pub(super) need_diagnostics: Option<MainNeedDiagnostics>,
    pub(super) continuation_diagnostics: Option<MainContinuationDiagnostics>,
    pub(super) resolve: Option<CachePilotResolveOutcome>,
    pub(super) resolves: Vec<CachePilotResolveOutcome>,
    pub(super) need_steps: Vec<NeedStep>,
    pub(super) tool_items_started: u32,
}

struct ResolvedNeed {
    need: Need,
    step: NeedStep,
    rendered: String,
}

pub(super) fn run_supervised_main(
    context: &Observe<'_>,
    observation: &mut SupervisedMain,
) -> Result<(), String> {
    let target_root = context.output.join("main-target");
    let temp_root = context.output.join("main-temp");
    fs::create_dir_all(&target_root).map_err(|error| error.to_string())?;
    fs::create_dir_all(&temp_root).map_err(|error| error.to_string())?;
    let config = WorkerConfig {
        executable: context.codex.display().to_string(),
        model: context.main_model.to_owned(),
        reasoning: context.main_reasoning.to_owned(),
        service_tier: Some(context.service_tier.to_owned()),
        timeout_seconds: context.timeout.as_secs(),
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let session_store = RuntimeStore::new(context.product_data.join("needle.sqlite3"));
    let mut session = CodexMainSession::start_pilot(MainSessionConfig {
        codex: &config,
        codex_home: context.codex_home,
        instructions: context.main_instructions,
        checkout_root: context.repository,
        target_root: &target_root,
        temp_root: &temp_root,
        snapshot_digest: context.source_snapshot_digest,
        repository_id: context.repository_id,
        route: context.route,
        store: session_store,
    })?;
    observation.transport_started = true;
    let session_id = session.thread_id().to_owned();
    context
        .store
        .record_session_start_profiled(
            &session_id,
            context.prompt_profile_digest,
            Some(context.main_model),
            context.repository.to_str(),
            context.role_profile_id,
        )
        .map_err(|error| error.to_string())?;

    let result = (|| -> Result<(), String> {
        let started = Instant::now();
        let need_result = session.run_until_need(context.prompt, context.timeout);
        observation.need_diagnostics = session.last_need_diagnostics().cloned();
        if let Some(diagnostics) = observation.need_diagnostics.as_ref() {
            observation.usage.merge_snapshot(diagnostics.usage);
            observation.tool_items_started = diagnostics.tool_items_started;
        }
        let mut need_turn = need_result?;
        observation.usage.merge_snapshot(need_turn.usage);
        let policy = context.store.multi_need_policy().map_err(|error| error.to_string())?;
        let mut ledger = Vec::<ResolvedNeed>::new();
        let mut repeat_deliveries = BTreeMap::<NeedId, u8>::new();
        let mut logical_workers = 0_u8;
        let mut continued_outcome = None;
        let mut continue_delivery_attempted = false;
        let mut pending_need_turns = VecDeque::new();
        let mut pending_need_overflowed = false;

        loop {
            let ordinal = u8::try_from(ledger.len())
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| "need sequence ordinal overflow".to_owned())?;
            if !policy.multi_need_enabled || ordinal > policy.max_needs_per_task {
                if need_turn.active_turn {
                    let interrupted = session.interrupt_active_turn(
                        &need_turn.turn_id,
                        remaining(context.timeout, started),
                    )?;
                    observation.usage.merge_snapshot(interrupted.usage);
                    observation.tool_items_started = observation
                        .tool_items_started
                        .saturating_add(interrupted.tool_items_started);
                    need_turn.active_turn = false;
                }
                let semantic_interrupt = &need_turn.semantic_interrupt;
                let need_ir = semantic_interrupt
                    .typed()
                    .cloned()
                    .ok_or_else(|| "multi-need fallback requires typed NeedIR".to_owned())?;
                let route = built_in_route_contracts()
                    .into_iter()
                    .find(|route| route.route == *semantic_interrupt.key())
                    .ok_or_else(|| {
                        format!("missing route contract {}", semantic_interrupt.key())
                    })?;
                let compiled = compile_need(&need_ir, context.repository_id, &route)
                    .map_err(|error| error.to_string())?;
                let step_id = need_step_id(
                    &session_id,
                    ordinal,
                    &need_turn.turn_id,
                    semantic_interrupt.digest(),
                );
                let step = NeedStep {
                    id: step_id,
                    ordinal,
                    turn_id: need_turn.turn_id.clone(),
                    need_id: compiled.id,
                    coordination: semantic_interrupt.coordination(),
                    relation: classify_against_ledger(&ledger, &compiled),
                    state: NeedStepState::NativeFallback,
                    required: compiled.required.iter().map(|item| item.id).collect(),
                    satisfied: Vec::new(),
                    missing: compiled.required.iter().map(|item| item.id).collect(),
                    artifacts: Vec::new(),
                    proof: None,
                    delivery: Some(NeedDelivery::NativeFallback),
                    worker_avoided: false,
                    main_discovery_tainted: false,
                };
                context
                    .store
                    .record_need_step(
                        &session_id,
                        &step,
                        semantic_interrupt,
                        &need_turn.raw_message,
                    )
                    .map_err(|error| error.to_string())?;
                context
                    .store
                    .append_need_step_event(
                        step_id,
                        NeedStepState::NativeFallback,
                        r#"{"reason":"multi_need_limit"}"#,
                    )
                    .map_err(|error| error.to_string())?;
                observation.need_steps.push(step);
                let next = run_native_fallback(
                    &mut session,
                    context.store,
                    &session_id,
                    Some(step_id),
                    "multi-need limit reached; complete the task with native read-only discovery",
                    remaining(context.timeout, started),
                    observation,
                )?;
                match next {
                    MainTurnResult::Final(final_turn) => {
                        observation.usage.merge_snapshot(final_turn.usage);
                        observation.final_response = final_turn.response;
                        break;
                    }
                    MainTurnResult::Need(_) => {
                        return Err("main emitted a need after bounded native fallback".to_owned());
                    }
                }
            }

            let semantic_interrupt = &need_turn.semantic_interrupt;
            let need_ir = semantic_interrupt
                .typed()
                .cloned()
                .ok_or_else(|| "multi-need coordination requires typed NeedIR".to_owned())?;
            let route = built_in_route_contracts()
                .into_iter()
                .find(|route| route.route == *semantic_interrupt.key())
                .ok_or_else(|| format!("missing route contract {}", semantic_interrupt.key()))?;
            let compiled = compile_need(&need_ir, context.repository_id, &route)
                .map_err(|error| error.to_string())?;
            let relation = classify_against_ledger(&ledger, &compiled);
            let step_id =
                need_step_id(&session_id, ordinal, &need_turn.turn_id, semantic_interrupt.digest());
            let mut step = NeedStep {
                id: step_id,
                ordinal,
                turn_id: need_turn.turn_id.clone(),
                need_id: compiled.id,
                coordination: semantic_interrupt.coordination(),
                relation,
                state: NeedStepState::Requested,
                required: compiled.required.iter().map(|item| item.id).collect(),
                satisfied: Vec::new(),
                missing: compiled.required.iter().map(|item| item.id).collect(),
                artifacts: Vec::new(),
                proof: None,
                delivery: None,
                worker_avoided: false,
                main_discovery_tainted: need_turn.main_discovery_tainted,
            };
            context
                .store
                .record_user_prompt(
                    &session_id,
                    Some(&need_turn.turn_id),
                    context.prompt,
                    context.repository.to_str(),
                )
                .map_err(|error| error.to_string())?;
            context
                .store
                .record_need_step(&session_id, &step, semantic_interrupt, &need_turn.raw_message)
                .map_err(|error| error.to_string())?;
            record_need_turn_observation(context.store, &session_id, &need_turn, &step)?;
            if semantic_interrupt.coordination() == needle_core::NeedCoordination::ContinueWorking {
                context
                    .store
                    .append_need_step_event(step_id, NeedStepState::Queued, "{}")
                    .map_err(|error| error.to_string())?;
                step.state = NeedStepState::Queued;
            }
            context
                .store
                .append_need_step_event(step_id, NeedStepState::Resolving, "{}")
                .map_err(|error| error.to_string())?;
            step.state = NeedStepState::Resolving;

            let repeat = fully_satisfied_ledger_entry(&ledger, &compiled);
            let rendered = if let Some(previous) = repeat {
                let deliveries = repeat_deliveries.entry(compiled.id).or_default();
                let rendered = match *deliveries {
                    0 => previous.rendered.clone(),
                    1 => already_satisfied_context(&step),
                    _ => {
                        context
                            .store
                            .append_need_step_event(
                                step_id,
                                NeedStepState::NativeFallback,
                                r#"{"reason":"repeated_after_already_satisfied"}"#,
                            )
                            .map_err(|error| error.to_string())?;
                        step.state = NeedStepState::NativeFallback;
                        step.delivery = Some(NeedDelivery::NativeFallback);
                        observation.need_steps.push(step);
                        let next = run_native_fallback(
                            &mut session,
                            context.store,
                            &session_id,
                            Some(step_id),
                            "the same satisfied need was repeated; complete natively without another worker",
                            remaining(context.timeout, started),
                            observation,
                        )?;
                        match next {
                            MainTurnResult::Final(final_turn) => {
                                observation.usage.merge_snapshot(final_turn.usage);
                                observation.final_response = final_turn.response;
                                break;
                            }
                            MainTurnResult::Need(_) => {
                                return Err(
                                    "main emitted a need after repeat native fallback".to_owned()
                                );
                            }
                        }
                    }
                };
                *deliveries = deliveries.saturating_add(1);
                step.satisfied = step.required.clone();
                step.missing.clear();
                step.artifacts = previous.step.artifacts.clone();
                step.proof = previous.step.proof;
                step.worker_avoided = true;
                step.delivery = Some(if *deliveries == 1 {
                    NeedDelivery::TurnStart
                } else {
                    NeedDelivery::AlreadySatisfied
                });
                rendered
            } else {
                if logical_workers >= policy.max_workers_per_task {
                    if need_turn.active_turn {
                        let interrupted = session.interrupt_active_turn(
                            &need_turn.turn_id,
                            remaining(context.timeout, started),
                        )?;
                        observation.usage.merge_snapshot(interrupted.usage);
                        observation.tool_items_started = observation
                            .tool_items_started
                            .saturating_add(interrupted.tool_items_started);
                        need_turn.active_turn = false;
                    }
                    context
                        .store
                        .append_need_step_event(
                            step_id,
                            NeedStepState::NativeFallback,
                            r#"{"reason":"logical_worker_limit"}"#,
                        )
                        .map_err(|error| error.to_string())?;
                    step.state = NeedStepState::NativeFallback;
                    step.delivery = Some(NeedDelivery::NativeFallback);
                    observation.need_steps.push(step);
                    let next = run_native_fallback(
                        &mut session,
                        context.store,
                        &session_id,
                        Some(step_id),
                        "logical worker limit reached; complete with native read-only discovery",
                        remaining(context.timeout, started),
                        observation,
                    )?;
                    match next {
                        MainTurnResult::Final(final_turn) => {
                            observation.usage.merge_snapshot(final_turn.usage);
                            observation.final_response = final_turn.response;
                            break;
                        }
                        MainTurnResult::Need(_) => {
                            return Err(
                                "main emitted a need after worker-limit fallback".to_owned()
                            );
                        }
                    }
                }

                let resolve_request = ResolveRequest {
                    session_id: session_id.clone(),
                    turn_id: need_turn.turn_id.clone(),
                    platform: "codex".to_owned(),
                    main_model: context.main_model.to_owned(),
                    cwd: context.repository.to_path_buf(),
                    need: semantic_interrupt.compatibility_request(),
                    need_ir: Some(need_ir),
                    declared_test_plan: Some(context.declared_test_plan.clone()),
                };
                if need_turn.active_turn && !policy.continue_working_enabled {
                    let interrupted = session.interrupt_active_turn(
                        &need_turn.turn_id,
                        remaining(context.timeout, started),
                    )?;
                    observation.usage.merge_snapshot(interrupted.usage);
                    observation.tool_items_started = observation
                        .tool_items_started
                        .saturating_add(interrupted.tool_items_started);
                    step.main_discovery_tainted = interrupted.main_discovery_tainted;
                    need_turn.active_turn = false;
                }
                let resolver_cancellation = Arc::new(AtomicBool::new(false));
                let engine = RuntimeEngine::new(
                    RuntimeStore::new(context.product_data.join("needle.sqlite3")),
                    CodexWorker::with_codex_home(context.product_data, context.codex_home)
                        .with_cancellation(Arc::clone(&resolver_cancellation)),
                );
                let resolve_result = if need_turn.active_turn
                    && semantic_interrupt.coordination()
                        == needle_core::NeedCoordination::ContinueWorking
                    && policy.continue_working_enabled
                {
                    continue_delivery_attempted = true;
                    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                    let cache_only = context.cache_only;
                    std::thread::spawn(move || {
                        let result = if cache_only {
                            engine.resolve_cache_only(&resolve_request)
                        } else {
                            engine.resolve(&resolve_request)
                        }
                        .map_err(|error| error.to_string());
                        let _ = sender.send(result);
                    });
                    let preview_store = context.store.clone();
                    let preview_need = compiled.clone();
                    let preview_step = step.clone();
                    let cancelled_usage = Arc::new(Mutex::new(None));
                    let cancellation_signal = Arc::clone(&resolver_cancellation);
                    let cancellation_observation = Arc::clone(&cancelled_usage);
                    match session.await_resolution_and_steer_cancellable(
                        &need_turn.turn_id,
                        &receiver,
                        move |outcome, pending| {
                            let mut preview = preview_step.clone();
                            let _ = apply_outcome_to_step(
                                &preview_store,
                                &preview_need,
                                outcome,
                                &mut preview,
                                false,
                            );
                            coordination_context(&outcome.rendered, &preview, pending)
                        },
                        move |usage, tools, tainted| {
                            cancellation_signal.store(true, Ordering::Release);
                            if let Ok(mut captured) = cancellation_observation.lock() {
                                *captured = Some((usage, tools, tainted));
                            }
                        },
                        remaining(context.timeout, started),
                    ) {
                        Ok(continued) => {
                            observation.usage.merge_snapshot(continued.usage);
                            observation.tool_items_started = observation
                                .tool_items_started
                                .saturating_add(continued.tool_items_started);
                            step.main_discovery_tainted = continued.main_discovery_tainted;
                            step.delivery = Some(continued.delivery);
                            pending_need_overflowed |= continued.queue_overflowed;
                            pending_need_turns.extend(continued.queued_needs);
                            context
                                .store
                                .record_main_turn_observation(&MainTurnObservationRecord {
                                    session_id: session_id.clone(),
                                    turn_id: need_turn.turn_id.clone(),
                                    need_step_id: Some(step_id),
                                    status: "continue_working_delivered".to_owned(),
                                    delivery: Some(
                                        match continued.delivery {
                                            NeedDelivery::TurnSteer => "turn_steer",
                                            _ => "turn_start",
                                        }
                                        .to_owned(),
                                    ),
                                    usage_json: serde_json::to_string(&continued.usage)
                                        .map_err(|error| error.to_string())?,
                                    tools_json: serde_json::json!({
                                        "started": continued.tool_items_started
                                    })
                                    .to_string(),
                                    main_discovery_tainted: continued.main_discovery_tainted,
                                    outcome: None,
                                })
                                .map_err(|error| error.to_string())?;
                            continued_outcome = continued.outcome;
                            Ok(continued.resolved)
                        }
                        Err(error) if error.starts_with("continue-working resolver failed: ") => {
                            if let Ok(captured) = cancelled_usage.lock()
                                && let Some((usage, tools, tainted)) = *captured
                            {
                                observation.usage.merge_snapshot(usage);
                                observation.tool_items_started =
                                    observation.tool_items_started.saturating_add(tools);
                                step.main_discovery_tainted |= tainted;
                            }
                            Err(error
                                .trim_start_matches("continue-working resolver failed: ")
                                .to_owned())
                        }
                        Err(error) if error.starts_with("main task cancelled") => {
                            resolver_cancellation.store(true, Ordering::Release);
                            let cleanup_observed =
                                receiver.recv_timeout(Duration::from_secs(5)).is_ok();
                            if let Ok(captured) = cancelled_usage.lock()
                                && let Some((usage, tools, tainted)) = *captured
                            {
                                observation.usage.merge_snapshot(usage);
                                observation.tool_items_started =
                                    observation.tool_items_started.saturating_add(tools);
                                step.main_discovery_tainted = tainted;
                            }
                            step.state = NeedStepState::Cancelled;
                            context
                                .store
                                .append_need_step_event(
                                    step_id,
                                    NeedStepState::Cancelled,
                                    &serde_json::json!({
                                        "resolver_cleanup_observed": cleanup_observed
                                    })
                                    .to_string(),
                                )
                                .map_err(|store_error| store_error.to_string())?;
                            observation.need_steps.push(step);
                            if !cleanup_observed {
                                return Err(
                                    "main task cancelled; resolver cleanup was not verifiable"
                                        .to_owned(),
                                );
                            }
                            return Err(error);
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    if context.cache_only {
                        engine.resolve_cache_only(&resolve_request)
                    } else {
                        engine.resolve(&resolve_request)
                    }
                    .map_err(|error| error.to_string())
                };
                let outcome = match resolve_result {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if context.cache_only {
                            return Err(format!("cache-only hit rejected: {error}"));
                        }
                        context
                            .store
                            .append_need_step_event(
                                step_id,
                                NeedStepState::NativeFallback,
                                &serde_json::json!({"reason": error}).to_string(),
                            )
                            .map_err(|store_error| store_error.to_string())?;
                        step.state = NeedStepState::NativeFallback;
                        step.delivery = Some(NeedDelivery::NativeFallback);
                        observation.need_steps.push(step);
                        let next = run_native_fallback(
                            &mut session,
                            context.store,
                            &session_id,
                            Some(step_id),
                            "Needle could not resolve the requested evidence; complete natively without retrying the worker",
                            remaining(context.timeout, started),
                            observation,
                        )?;
                        match next {
                            MainTurnResult::Final(final_turn) => {
                                observation.usage.merge_snapshot(final_turn.usage);
                                observation.final_response = final_turn.response;
                                break;
                            }
                            MainTurnResult::Need(_) => {
                                return Err(
                                    "main emitted a need after resolver fallback".to_owned()
                                );
                            }
                        }
                    }
                };
                if outcome.worker_spawned {
                    logical_workers = logical_workers.saturating_add(1);
                }
                step.worker_avoided = !outcome.worker_spawned;
                let resolve = CachePilotResolveOutcome {
                    status: outcome.status.clone(),
                    cache_resolution: outcome.cache_resolution.clone(),
                    cache_hit: outcome.cache_hit,
                    worker_spawned: outcome.worker_spawned,
                    result_digest: outcome.result_digest,
                };
                if observation.resolve.is_none() {
                    observation.resolve = Some(resolve.clone());
                }
                observation.resolves.push(resolve);
                apply_outcome_to_step(context.store, &compiled, &outcome, &mut step, true)?;
                if step.delivery.is_none() {
                    step.delivery = Some(NeedDelivery::TurnStart);
                }
                outcome.rendered
            };

            if need_turn.active_turn && !continue_delivery_attempted {
                let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                sender
                    .send(Ok(rendered.clone()))
                    .map_err(|_| "cannot queue immediate continue-working reuse".to_owned())?;
                let continued = session.await_resolution_and_steer(
                    &need_turn.turn_id,
                    &receiver,
                    |context, pending| coordination_context(context, &step, pending),
                    remaining(context.timeout, started),
                )?;
                observation.usage.merge_snapshot(continued.usage);
                observation.tool_items_started =
                    observation.tool_items_started.saturating_add(continued.tool_items_started);
                step.main_discovery_tainted = continued.main_discovery_tainted;
                step.delivery = Some(continued.delivery);
                pending_need_overflowed |= continued.queue_overflowed;
                pending_need_turns.extend(continued.queued_needs);
                continued_outcome = continued.outcome;
            }
            continue_delivery_attempted = false;

            step.state = NeedStepState::Resolved;
            context
                .store
                .append_need_step_event(
                    step_id,
                    NeedStepState::Resolved,
                    &serde_json::to_string(&step).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            ledger.push(ResolvedNeed {
                need: compiled,
                step: step.clone(),
                rendered: rendered.clone(),
            });
            observation.need_steps.push(step.clone());

            if pending_need_overflowed {
                if let Some(outcome) = continued_outcome.take() {
                    match outcome {
                        MainTurnResult::Need(turn) if turn.active_turn => {
                            let interrupted = session.interrupt_active_turn(
                                &turn.turn_id,
                                remaining(context.timeout, started),
                            )?;
                            observation.usage.merge_snapshot(interrupted.usage);
                            observation.tool_items_started = observation
                                .tool_items_started
                                .saturating_add(interrupted.tool_items_started);
                        }
                        MainTurnResult::Need(_) => {}
                        MainTurnResult::Final(turn) => {
                            observation.usage.merge_snapshot(turn.usage);
                            observation.tool_items_started = observation
                                .tool_items_started
                                .saturating_add(turn.tool_items_started);
                        }
                    }
                }
                if let Some(last) = ledger.last_mut() {
                    last.step.state = NeedStepState::Delivered;
                }
                if let Some(last) = observation.need_steps.last_mut() {
                    last.state = NeedStepState::Delivered;
                }
                context
                    .store
                    .append_need_step_event(
                        step_id,
                        NeedStepState::Delivered,
                        r#"{"pending_need_overflow":true}"#,
                    )
                    .map_err(|error| error.to_string())?;
                pending_need_turns.clear();
                let fallback = run_native_fallback(
                    &mut session,
                    context.store,
                    &session_id,
                    Some(step_id),
                    "pending need queue reached its hard cap; complete natively",
                    remaining(context.timeout, started),
                    observation,
                )?;
                match fallback {
                    MainTurnResult::Final(turn) => {
                        observation.final_response = turn.response;
                        break;
                    }
                    MainTurnResult::Need(_) => {
                        return Err(
                            "main emitted a need after pending-queue native fallback".to_owned()
                        );
                    }
                }
            }

            let observed_next = if let Some(outcome) = continued_outcome.take() {
                outcome
            } else {
                session.run_next(
                    &coordination_context(&rendered, &step, 0),
                    remaining(context.timeout, started),
                    false,
                )?
            };
            let next = if let Some(queued) = pending_need_turns.pop_front() {
                match observed_next {
                    MainTurnResult::Need(need) => pending_need_turns.push_back(need),
                    MainTurnResult::Final(final_turn) => {
                        observation.usage.merge_snapshot(final_turn.usage);
                        observation.tool_items_started = observation
                            .tool_items_started
                            .saturating_add(final_turn.tool_items_started);
                        context
                            .store
                            .record_main_turn_observation(&MainTurnObservationRecord {
                                session_id: session_id.clone(),
                                turn_id: final_turn.turn_id,
                                need_step_id: Some(step_id),
                                status: "completed_with_pending_need".to_owned(),
                                delivery: step.delivery.map(|delivery| match delivery {
                                    NeedDelivery::TurnSteer => "turn_steer".to_owned(),
                                    _ => "turn_start".to_owned(),
                                }),
                                usage_json: serde_json::to_string(&final_turn.usage)
                                    .map_err(|error| error.to_string())?,
                                tools_json: serde_json::json!({
                                    "started": final_turn.tool_items_started
                                })
                                .to_string(),
                                main_discovery_tainted: step.main_discovery_tainted,
                                outcome: Some(MainTurnOutcome::Final {
                                    response: bound_text(&final_turn.response),
                                }),
                            })
                            .map_err(|error| error.to_string())?;
                    }
                }
                MainTurnResult::Need(queued)
            } else {
                observed_next
            };
            if let Some(last) = ledger.last_mut() {
                last.step.state = NeedStepState::Delivered;
            }
            if let Some(last) = observation.need_steps.last_mut() {
                last.state = NeedStepState::Delivered;
            }
            context
                .store
                .append_need_step_event(
                    step_id,
                    NeedStepState::Delivered,
                    &serde_json::to_string(
                        observation.need_steps.last().expect("the delivered step was recorded"),
                    )
                    .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            match next {
                MainTurnResult::Need(next_need) => {
                    observation.usage.merge_snapshot(next_need.usage);
                    observation.tool_items_started =
                        observation.tool_items_started.saturating_add(next_need.tool_items_started);
                    need_turn = *next_need;
                }
                MainTurnResult::Final(final_turn) => {
                    observation.usage.merge_snapshot(final_turn.usage);
                    observation.tool_items_started = observation
                        .tool_items_started
                        .saturating_add(final_turn.tool_items_started);
                    context
                        .store
                        .record_main_turn_observation(&MainTurnObservationRecord {
                            session_id: session_id.clone(),
                            turn_id: final_turn.turn_id.clone(),
                            need_step_id: Some(step_id),
                            status: "completed".to_owned(),
                            delivery: Some("turn_start".to_owned()),
                            usage_json: serde_json::to_string(&final_turn.usage)
                                .map_err(|error| error.to_string())?,
                            tools_json: serde_json::json!({
                                "started": final_turn.tool_items_started
                            })
                            .to_string(),
                            main_discovery_tainted: false,
                            outcome: Some(MainTurnOutcome::Final {
                                response: bound_text(&final_turn.response),
                            }),
                        })
                        .map_err(|error| error.to_string())?;
                    observation.final_response = final_turn.response;
                    break;
                }
            }
        }

        fs::write(
            context.product_data.join("pilot-outcome.json"),
            serde_json::to_vec(&observation.resolve).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        fs::write(
            context.product_data.join("pilot-need-sequence.json"),
            serde_json::to_vec_pretty(&observation.need_steps)
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        fs::write(
            context.product_data.join("pilot-outcomes.json"),
            serde_json::to_vec_pretty(&observation.resolves).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })();
    observation.provider_observation_started = session.provider_turns_started() > 0;
    let diagnostics_write = if let Some(diagnostics) = observation.need_diagnostics.as_ref() {
        serde_json::to_vec_pretty(diagnostics).map_err(|error| error.to_string()).and_then(
            |bytes| {
                fs::write(context.output.join("main-need-diagnostics.json"), bytes)
                    .map_err(|error| error.to_string())
            },
        )
    } else {
        Ok(())
    };
    let continuation_diagnostics_write =
        if let Some(diagnostics) = observation.continuation_diagnostics.as_ref() {
            serde_json::to_vec_pretty(diagnostics).map_err(|error| error.to_string()).and_then(
                |bytes| {
                    fs::write(context.output.join("main-continuation-diagnostics.json"), bytes)
                        .map_err(|error| error.to_string())
                },
            )
        } else {
            Ok(())
        };
    let cleanup = session.cleanup();
    let end_session = context.store.end_session(&session_id).map_err(|error| error.to_string());
    result.and(diagnostics_write).and(continuation_diagnostics_write).and(cleanup).and(end_session)
}

fn classify_against_ledger(ledger: &[ResolvedNeed], current: &Need) -> NeedStepRelation {
    ledger
        .iter()
        .map(|previous| classify_need_step(&previous.need, current, &previous.step.satisfied))
        .min_by_key(|relation| match relation {
            NeedStepRelation::Repeat => 0,
            NeedStepRelation::Residual => 1,
            NeedStepRelation::Extension => 2,
            NeedStepRelation::Overlap => 3,
            NeedStepRelation::Incompatible => 4,
            NeedStepRelation::Independent => 5,
        })
        .unwrap_or(NeedStepRelation::Independent)
}

fn record_need_turn_observation(
    store: &RuntimeStore,
    session_id: &str,
    turn: &needle_platform_codex::MainNeedTurn,
    step: &NeedStep,
) -> Result<(), String> {
    store
        .record_main_turn_observation(&MainTurnObservationRecord {
            session_id: session_id.to_owned(),
            turn_id: turn.turn_id.clone(),
            need_step_id: Some(step.id),
            status: turn.terminal_status.clone(),
            delivery: None,
            usage_json: serde_json::to_string(&turn.usage).map_err(|error| error.to_string())?,
            tools_json: serde_json::json!({"started": turn.tool_items_started}).to_string(),
            main_discovery_tainted: turn.main_discovery_tainted,
            outcome: Some(MainTurnOutcome::Need { step: step.clone() }),
        })
        .map_err(|error| error.to_string())
}

fn bound_text(value: &str) -> String {
    const MAXIMUM: usize = 16 * 1024;
    let mut end = value.len().min(MAXIMUM);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn need_step_id(
    session_id: &str,
    ordinal: u8,
    turn_id: &str,
    request_digest: needle_core::Digest,
) -> needle_core::Digest {
    let mut hasher = CanonicalHasher::new(b"need-step");
    hasher.field_str(session_id);
    hasher.field_u8(ordinal);
    hasher.field_str(turn_id);
    hasher.field_digest(request_digest);
    hasher.finish()
}

fn resolution_evidence(
    outcome: &needle_runtime::ResolveOutcome,
) -> (Vec<ArtifactId>, Option<needle_core::ReuseSufficiencyCertificateId>) {
    let proof = match &outcome.cache_resolution {
        CacheResolution::ExactHit { sufficiency_certificate_id, .. }
        | CacheResolution::CompositeHit { sufficiency_certificate_id, .. } => {
            *sufficiency_certificate_id
        }
        CacheResolution::CoverageHit { sufficiency_certificate_id, .. } => {
            Some(*sufficiency_certificate_id)
        }
        CacheResolution::Miss
        | CacheResolution::ClaimHit { .. }
        | CacheResolution::ClaimCompositeHit { .. }
        | CacheResolution::PartialHit { .. }
        | CacheResolution::Stale { .. }
        | CacheResolution::Rejected { .. }
        | CacheResolution::Ambiguous { .. }
        | CacheResolution::Contradicted { .. }
        | CacheResolution::Bypass { .. } => None,
    };
    if !outcome.semantic_artifact_ids.is_empty() {
        return (outcome.semantic_artifact_ids.clone(), proof);
    }
    match &outcome.cache_resolution {
        CacheResolution::ExactHit { artifact_id, sufficiency_certificate_id, .. } => {
            (vec![ArtifactId(*artifact_id)], *sufficiency_certificate_id)
        }
        CacheResolution::CoverageHit { artifact_id, sufficiency_certificate_id, .. } => {
            (vec![ArtifactId(*artifact_id)], Some(*sufficiency_certificate_id))
        }
        CacheResolution::CompositeHit { artifact_ids, sufficiency_certificate_id, .. } => {
            (artifact_ids.iter().copied().map(ArtifactId).collect(), *sufficiency_certificate_id)
        }
        CacheResolution::ClaimHit { artifact_ids, .. }
        | CacheResolution::ClaimCompositeHit { artifact_ids, .. } => {
            (artifact_ids.iter().copied().map(ArtifactId).collect(), None)
        }
        CacheResolution::PartialHit { reused, .. } => {
            let mut artifacts = reused.iter().copied().map(ArtifactId).collect::<Vec<_>>();
            let generated = ArtifactId(outcome.result_digest);
            if !artifacts.contains(&generated) {
                artifacts.push(generated);
            }
            (artifacts, None)
        }
        CacheResolution::Miss => (vec![ArtifactId(outcome.result_digest)], None),
        CacheResolution::Stale { .. }
        | CacheResolution::Rejected { .. }
        | CacheResolution::Ambiguous { .. }
        | CacheResolution::Contradicted { .. }
        | CacheResolution::Bypass { .. } => (Vec::new(), None),
    }
}

fn fully_satisfied_ledger_entry<'a>(
    ledger: &'a [ResolvedNeed],
    compiled: &Need,
) -> Option<&'a ResolvedNeed> {
    ledger.iter().rev().find(|previous| {
        previous.step.missing.is_empty()
            && compiled.residual.as_ref().is_none_or(|residual| !residual.mandatory)
            && (previous.need.id == compiled.id
                || (previous.need.world == compiled.world
                    && previous.need.semantic_constraints == compiled.semantic_constraints
                    && previous
                        .need
                        .subjects
                        .iter()
                        .map(|subject| subject.id)
                        .eq(compiled.subjects.iter().map(|subject| subject.id))
                    && compiled
                        .required
                        .iter()
                        .all(|obligation| previous.step.satisfied.contains(&obligation.id))))
    })
}

fn apply_outcome_to_step(
    store: &RuntimeStore,
    need: &Need,
    outcome: &needle_runtime::ResolveOutcome,
    step: &mut NeedStep,
    persist_associations: bool,
) -> Result<(), String> {
    let (artifacts, proof) = resolution_evidence(outcome);
    step.artifacts = artifacts;
    step.proof = proof;
    step.satisfied.clear();
    for artifact in &step.artifacts {
        step.satisfied.extend(
            store
                .covered_obligations_for_artifact(*artifact, need)
                .map_err(|error| error.to_string())?,
        );
        if persist_associations {
            store
                .attach_need_step_artifact(step.id, *artifact, step.proof, "selected")
                .map_err(|error| error.to_string())?;
        }
    }
    step.satisfied.sort();
    step.satisfied.dedup();
    step.missing =
        step.required.iter().filter(|id| !step.satisfied.contains(id)).copied().collect();
    Ok(())
}

fn already_satisfied_context(step: &NeedStep) -> String {
    format!(
        "[NEEDLE_CONTEXT]\n{{\"step\":{},\"need_id\":\"{}\",\"status\":\"already_satisfied\",\"worker_spawned\":false}}\n[/NEEDLE_CONTEXT]\n\nContinue the original task without repeating covered discovery.",
        step.ordinal, step.need_id
    )
}

fn coordination_context(rendered: &str, step: &NeedStep, pending: usize) -> String {
    let satisfied = step.satisfied.iter().map(ToString::to_string).collect::<Vec<_>>();
    let missing = step.missing.iter().map(ToString::to_string).collect::<Vec<_>>();
    let artifacts = step.artifacts.iter().map(ToString::to_string).collect::<Vec<_>>();
    let coordination = serde_json::json!({
        "step": step.ordinal,
        "need_id": step.need_id,
        "satisfied_obligations": satisfied,
        "missing_obligations": missing,
        "artifact_ids": artifacts,
        "proof_id": step.proof,
        "pending_needs": pending,
        "cache_delivery": step.delivery,
        "instruction": "Do not repeat discovery covered by these certified artifacts."
    });
    format!("{rendered}\n\n[NEEDLE_COORDINATION]\n{coordination}\n[/NEEDLE_COORDINATION]")
}

fn remaining(timeout: Duration, started: Instant) -> Duration {
    timeout.saturating_sub(started.elapsed()).max(Duration::from_secs(1))
}

fn run_native_fallback(
    session: &mut CodexMainSession,
    store: &RuntimeStore,
    session_id: &str,
    step_id: Option<needle_core::Digest>,
    reason: &str,
    timeout: Duration,
    observation: &mut SupervisedMain,
) -> Result<MainTurnResult, String> {
    let input = format!(
        "[NEEDLE_BYPASS]\nreason: {reason}\n[/NEEDLE_BYPASS]\n\nComplete the original task using native read-only discovery. Do not request file changes."
    );
    let result = session.run_next(&input, timeout, true)?;
    match &result {
        MainTurnResult::Need(turn) => {
            observation.usage.merge_snapshot(turn.usage);
            observation.tool_items_started =
                observation.tool_items_started.saturating_add(turn.tool_items_started);
            store
                .record_main_turn_observation(&MainTurnObservationRecord {
                    session_id: session_id.to_owned(),
                    turn_id: turn.turn_id.clone(),
                    need_step_id: step_id,
                    status: "native_fallback_need".to_owned(),
                    delivery: Some("turn_start".to_owned()),
                    usage_json: serde_json::to_string(&turn.usage)
                        .map_err(|error| error.to_string())?,
                    tools_json: serde_json::json!({"started": turn.tool_items_started}).to_string(),
                    main_discovery_tainted: turn.tool_items_started > 0,
                    outcome: None,
                })
                .map_err(|error| error.to_string())?;
        }
        MainTurnResult::Final(turn) => {
            observation.usage.merge_snapshot(turn.usage);
            observation.tool_items_started =
                observation.tool_items_started.saturating_add(turn.tool_items_started);
            store
                .record_main_turn_observation(&MainTurnObservationRecord {
                    session_id: session_id.to_owned(),
                    turn_id: turn.turn_id.clone(),
                    need_step_id: step_id,
                    status: "native_fallback_completed".to_owned(),
                    delivery: Some("turn_start".to_owned()),
                    usage_json: serde_json::to_string(&turn.usage)
                        .map_err(|error| error.to_string())?,
                    tools_json: serde_json::json!({"started": turn.tool_items_started}).to_string(),
                    main_discovery_tainted: turn.tool_items_started > 0,
                    outcome: Some(MainTurnOutcome::Final { response: bound_text(&turn.response) }),
                })
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(result)
}

pub(super) fn process_status(error: Option<&str>) -> ProcessExecutionStatus {
    let Some(error) = error else {
        return ProcessExecutionStatus {
            status: "exit:0".to_owned(),
            exit_code: Some(0),
            ..ProcessExecutionStatus::default()
        };
    };
    if error.starts_with("cannot spawn Codex App Server") {
        return ProcessExecutionStatus {
            status: format!("spawn-error:{error}"),
            spawn_error: Some(error.to_owned()),
            ..ProcessExecutionStatus::default()
        };
    }
    if error.contains("timed out") {
        return ProcessExecutionStatus {
            status: "timeout".to_owned(),
            timed_out: true,
            abort_reason: Some(error.to_owned()),
            ..ProcessExecutionStatus::default()
        };
    }
    ProcessExecutionStatus {
        status: format!("aborted:{error}"),
        abort_reason: Some(error.to_owned()),
        ..ProcessExecutionStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        Digest, Facet, Obligation, PredicateKind, ResidualIntent, ResidualReason, SemanticWorld,
        Subject, SubjectKind,
    };

    fn semantic_need(id_seed: &[u8]) -> Need {
        let repository = Digest::blake3(b"repository");
        let subject = Subject::exact(repository, SubjectKind::CliOption, "--example");
        let obligation =
            Obligation::new(PredicateKind::ImplementationLocation, subject.id, Vec::<Facet>::new());
        Need {
            id: NeedId(Digest::blake3(id_seed)),
            subjects: vec![subject],
            required: vec![obligation],
            preferred: Vec::new(),
            semantic_constraints: Vec::new(),
            world: SemanticWorld {
                repository_lineage: repository,
                source_selector: "current".to_owned(),
                platform: "windows".to_owned(),
                features: "default".to_owned(),
                configuration: None,
                toolchain: None,
            },
            input_artifacts: Vec::new(),
            residual: None,
            body_digest: Digest::blake3(b"body"),
            format_revision: 1,
        }
    }

    fn resolved_need(need: Need) -> ResolvedNeed {
        let obligation = need.required[0].id;
        ResolvedNeed {
            step: NeedStep {
                id: Digest::blake3(b"step"),
                ordinal: 1,
                turn_id: "turn-1".to_owned(),
                need_id: need.id,
                coordination: needle_core::NeedCoordination::WaitResponse,
                relation: NeedStepRelation::Independent,
                state: NeedStepState::Delivered,
                required: vec![obligation],
                satisfied: vec![obligation],
                missing: Vec::new(),
                artifacts: vec![ArtifactId(Digest::blake3(b"artifact"))],
                proof: None,
                delivery: Some(NeedDelivery::TurnStart),
                worker_avoided: false,
                main_discovery_tainted: false,
            },
            need,
            rendered: "certified context".to_owned(),
        }
    }

    #[test]
    fn different_need_id_reuses_fully_satisfied_obligations_in_the_same_world() {
        let previous = resolved_need(semantic_need(b"first"));
        let current = semantic_need(b"second");
        let ledger = [previous];

        let matched = fully_satisfied_ledger_entry(&ledger, &current);

        assert!(matched.is_some());
    }

    #[test]
    fn mandatory_residual_never_reuses_only_the_covered_obligations() {
        let previous = resolved_need(semantic_need(b"first"));
        let mut current = semantic_need(b"first");
        current.residual = Some(ResidualIntent {
            raw_digest: Digest::blake3(b"unstructured exact anchor"),
            reason: ResidualReason::UndeclaredExactAnchor,
            mandatory: true,
        });
        let ledger = [previous];

        let matched = fully_satisfied_ledger_entry(&ledger, &current);

        assert!(matched.is_none());
    }
}
