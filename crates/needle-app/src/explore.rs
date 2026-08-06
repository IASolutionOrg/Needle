use super::{AppError, product_data_directory};
use needle_core::{
    Digest, NEED_IR_FORMAT_REVISION, NeedIr, NeedKey, NeedRequest, RoleProfileId,
    SubjectExpression, SubjectKind,
};
use needle_platform_codex::HookConfig;
use needle_runtime::{ResolveRequest, RuntimeStore, capture_git_snapshot};
use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Bump this whenever direct exploration changes in a way that must invalidate
// exact local results produced by an earlier CLI or managed-skill contract.
const TRANSPORT_DEFINITION: &[u8] = b"needle.direct-explore/4";
const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

struct ExplorationProgress {
    started: Instant,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ExplorationProgress {
    fn start(route: &str) -> Self {
        eprintln!("needle: exploring `{route}`...");
        let (stop, receiver) = mpsc::channel();
        let route = route.to_owned();
        let started = Instant::now();
        let thread = thread::spawn(move || {
            let mut elapsed = PROGRESS_INTERVAL;
            loop {
                match receiver.recv_timeout(PROGRESS_INTERVAL) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        eprintln!(
                            "needle: still exploring `{route}` ({}s elapsed)...",
                            elapsed.as_secs()
                        );
                        elapsed = elapsed.saturating_add(PROGRESS_INTERVAL);
                    }
                }
            }
        });
        Self { started, stop: Some(stop), thread: Some(thread) }
    }

    fn finish(mut self, success: bool) {
        self.stop_thread();
        let status = if success { "completed" } else { "failed" };
        eprintln!("needle: exploration {status} in {}s", self.started.elapsed().as_secs());
    }

    fn stop_thread(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ExplorationProgress {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

pub(crate) fn run(arguments: Vec<String>) -> Result<(), AppError> {
    if matches!(arguments.as_slice(), [argument] if argument == "--help" || argument == "-h") {
        println!(
            "Usage: needle explore --route <locate.implementation|trace.state-flow|tests.relevant> --subject-kind <symbol|cli-option|configuration-key|test|file|module|behavior> --subject <canonical-name> [--query <custom-request>] [--repository <path>] [--data-dir <path>]"
        );
        return Ok(());
    }
    validate_arguments(&arguments)?;
    let route = option_value(&arguments, "--route")
        .ok_or_else(|| AppError::Usage("explore requires --route".to_owned()))?;
    if !["locate.implementation", "trace.state-flow", "tests.relevant"].contains(&route.as_str()) {
        return Err(AppError::Usage(format!(
            "unsupported exploration route `{route}`; expected locate.implementation, trace.state-flow, or tests.relevant"
        )));
    }
    let subject_kind = option_value(&arguments, "--subject-kind")
        .ok_or_else(|| AppError::Usage("explore requires --subject-kind".to_owned()))
        .and_then(|value| parse_subject_kind(&value))?;
    let subject = option_value(&arguments, "--subject")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Usage("explore requires a non-empty --subject".to_owned()))?;
    let route = NeedKey::new(route)
        .map_err(|error| AppError::Usage(format!("invalid exploration route: {error}")))?;
    let query = match option_value(&arguments, "--query") {
        Some(value) if value.trim().is_empty() => {
            return Err(AppError::Usage("explore requires a non-empty --query".to_owned()));
        }
        Some(value) => value.trim().to_owned(),
        None => canonical_exploration_query(&route, &subject_kind, &subject),
    };
    let (need, need_ir) = exploration_request(route, subject_kind, subject, query.clone());

    let candidate =
        option_value(&arguments, "--repository").map(PathBuf::from).unwrap_or(env::current_dir()?);
    let (repository, _) = capture_git_snapshot(&candidate).map_err(|error| {
        AppError::Runtime(format!(
            "cannot resolve a trusted Git repository from {}: {error}",
            candidate.display()
        ))
    })?;
    let data_directory = product_data_directory(&arguments)?;
    let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
    if !store.path().is_file() {
        return Err(AppError::Runtime(
            "Needle is not initialized; run `needle enable` in this repository first".to_owned(),
        ));
    }
    let activation = store
        .activation_status(&repository)
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    if !activation.enabled {
        return Err(AppError::Runtime(
            "Needle is disabled for this repository; run `needle enable` first".to_owned(),
        ));
    }
    let profile_id = activation.role_profile_id.ok_or_else(|| {
        AppError::Runtime("the active Needle configuration has no explorer profile".to_owned())
    })?;
    resolve(&store, data_directory, repository, profile_id, need, need_ir, query)
}

fn canonical_exploration_query(
    route: &NeedKey,
    subject_kind: &SubjectKind,
    subject: &str,
) -> String {
    let kind = subject_kind_name(subject_kind);
    match route.as_str() {
        "locate.implementation" => format!(
            "Locate the primary implementation of the {kind} `{subject}` and identify its important callers and dependencies."
        ),
        "trace.state-flow" => format!(
            "Trace the runtime and state flow of the {kind} `{subject}` from entry points through decisions, state changes, persistence, and relevant tests."
        ),
        "tests.relevant" => format!(
            "Identify the smallest relevant tests for the {kind} `{subject}` and explain what each covers."
        ),
        _ => unreachable!("the route is validated before canonical query generation"),
    }
}

fn subject_kind_name(subject_kind: &SubjectKind) -> &'static str {
    match subject_kind {
        SubjectKind::Symbol => "symbol",
        SubjectKind::CliOption => "CLI option",
        SubjectKind::ConfigurationKey => "configuration key",
        SubjectKind::Test => "test",
        SubjectKind::File => "file",
        SubjectKind::Module => "module",
        SubjectKind::Behavior => "behavior",
    }
}

fn exploration_request(
    route: NeedKey,
    subject_kind: SubjectKind,
    subject: String,
    query: String,
) -> (NeedRequest, NeedIr) {
    let need = NeedRequest { key: route.clone(), body: query.clone() };
    let need_ir = NeedIr {
        route_hint: Some(route),
        subjects: vec![SubjectExpression { kind: subject_kind, canonical_name: subject }],
        required: Vec::new(),
        preferred: Vec::new(),
        semantic_constraints: Vec::new(),
        world: Vec::new(),
        input_artifacts: Vec::new(),
        projection: Vec::new(),
        body: query.clone(),
        format_revision: NEED_IR_FORMAT_REVISION,
    };
    (need, need_ir)
}

fn resolve(
    store: &RuntimeStore,
    data_directory: PathBuf,
    repository: PathBuf,
    profile_id: RoleProfileId,
    need: NeedRequest,
    need_ir: NeedIr,
    root_task: String,
) -> Result<(), AppError> {
    store.settings().map_err(|error| AppError::Runtime(error.to_string()))?;
    let prompt_profile_digest = HookConfig::default().profile()?.definition_digest;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let session_id = format!("needle-explore-{}-{nonce}", std::process::id());
    let turn_id = "turn-1";
    let cwd = repository.to_string_lossy();
    store
        .record_session_start_for_transport_profiled(
            &session_id,
            prompt_profile_digest,
            Some("unknown"),
            Some(&cwd),
            "skill",
            Digest::blake3(TRANSPORT_DEFINITION),
            None,
            &profile_id,
        )
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    store
        .record_user_prompt(&session_id, Some(turn_id), &root_task, Some(&cwd))
        .map_err(|error| AppError::Runtime(error.to_string()))?;

    let request = ResolveRequest {
        session_id: session_id.clone(),
        turn_id: turn_id.to_owned(),
        platform: "codex".to_owned(),
        main_model: "unknown".to_owned(),
        cwd: repository,
        need,
        need_ir: Some(need_ir),
        declared_test_plan: None,
    };
    let worker_policy = if env::var("NEEDLE_RESOLVE_CACHE_ONLY").as_deref() == Ok("1") {
        crate::product_resolver::WorkerPolicy::CacheOnly
    } else {
        crate::product_resolver::WorkerPolicy::Allow
    };
    let resolver = crate::product_resolver::ProductResolver::new(data_directory, worker_policy)
        .map_err(AppError::Runtime)?;
    let progress = ExplorationProgress::start(request.need.key.as_str());
    let outcome = resolver.resolve_direct_explore(&request).map_err(AppError::Runtime);
    let cleanup =
        store.end_session(&session_id).map_err(|error| AppError::Runtime(error.to_string()));
    let elapsed = progress.started.elapsed();
    progress.finish(outcome.is_ok() && cleanup.is_ok());
    if let Ok(resolved) = outcome.as_ref() {
        let elapsed = format_elapsed(elapsed);
        if resolved.cache_hit {
            eprintln!("needle: exact cache hit; context restored in {elapsed}");
        } else if resolved.worker_spawned {
            eprintln!("needle: cache miss; worker exploration completed in {elapsed}");
        }
    }
    let outcome = outcome?;
    cleanup?;
    print!("{}", outcome.rendered);
    Ok(())
}

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn validate_arguments(arguments: &[String]) -> Result<(), AppError> {
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--route" | "--subject-kind" | "--subject" | "--query" | "--repository"
            | "--data-dir" => {
                if arguments.get(index + 1).is_none() {
                    return Err(AppError::Usage(format!("{} requires a value", arguments[index])));
                }
                index += 2;
            }
            argument => {
                return Err(AppError::Usage(format!("unknown explore argument `{argument}`")));
            }
        }
    }
    Ok(())
}

fn parse_subject_kind(value: &str) -> Result<SubjectKind, AppError> {
    match value {
        "symbol" => Ok(SubjectKind::Symbol),
        "cli-option" => Ok(SubjectKind::CliOption),
        "configuration-key" => Ok(SubjectKind::ConfigurationKey),
        "test" => Ok(SubjectKind::Test),
        "file" => Ok(SubjectKind::File),
        "module" => Ok(SubjectKind::Module),
        "behavior" => Ok(SubjectKind::Behavior),
        _ => Err(AppError::Usage(format!(
            "unsupported subject kind `{value}`; expected symbol, cli-option, configuration-key, test, file, module, or behavior"
        ))),
    }
}

fn option_value(arguments: &[String], name: &str) -> Option<String> {
    arguments.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_surface_is_closed() {
        assert!(
            validate_arguments(&[
                "--route".to_owned(),
                "trace.state-flow".to_owned(),
                "--subject-kind".to_owned(),
                "behavior".to_owned(),
                "--subject".to_owned(),
                "activation precedence".to_owned(),
                "--query".to_owned(),
                "Trace it".to_owned(),
            ])
            .is_ok()
        );
        assert!(validate_arguments(&["--json".to_owned()]).is_err());
        assert!(validate_arguments(&["--route".to_owned()]).is_err());
    }

    #[test]
    fn subject_kinds_are_explicit_and_closed() {
        assert_eq!(parse_subject_kind("behavior").unwrap(), SubjectKind::Behavior);
        assert!(parse_subject_kind("free-form").is_err());
    }

    #[test]
    fn canonical_queries_are_deterministic_and_subject_bound() {
        let route = NeedKey::new("trace.state-flow").unwrap();
        let first = canonical_exploration_query(&route, &SubjectKind::Behavior, "activation");
        let repeated = canonical_exploration_query(&route, &SubjectKind::Behavior, "activation");
        let different = canonical_exploration_query(&route, &SubjectKind::Behavior, "persistence");

        assert_eq!(first, repeated);
        assert_ne!(first, different);
        assert!(first.contains("behavior `activation`"));
        assert!(first.contains("relevant tests"));
    }

    #[test]
    fn elapsed_time_is_human_readable() {
        assert_eq!(format_elapsed(Duration::from_millis(420)), "420ms");
        assert_eq!(format_elapsed(Duration::from_millis(1_250)), "1.2s");
    }

    #[test]
    fn trace_exploration_preserves_required_and_preferred_obligations() {
        let route = NeedKey::new("trace.state-flow").unwrap();
        let (_, need_ir) = exploration_request(
            route.clone(),
            SubjectKind::Behavior,
            "activation precedence".to_owned(),
            "Trace activation precedence".to_owned(),
        );
        let contract = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route == route)
            .unwrap();
        let compiled =
            needle_core::compile_need(&need_ir, Digest::blake3(b"repository"), &contract).unwrap();

        assert_eq!(compiled.required.len(), 2);
        assert!(compiled.required.iter().any(|obligation| {
            obligation.predicate == needle_core::PredicateKind::ImplementationLocation
        }));
        assert!(
            compiled.required.iter().any(|obligation| {
                obligation.predicate == needle_core::PredicateKind::RuntimeFlow
            })
        );
        assert_eq!(compiled.preferred.len(), 1);
        assert_eq!(compiled.preferred[0].predicate, needle_core::PredicateKind::FocusedTests);
    }
}
