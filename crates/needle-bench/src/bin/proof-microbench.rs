use needle_core::{
    ArtifactId, ArtifactRequest, ArtifactValidationCertificateId, BorrowedNeedIr, CacheResolution,
    CanonicalHasher, CapabilityMode, Digest, EvidenceFailurePolicy, Facet, FlowStepRole,
    LocationRole, Need, NeedFragment, NeedId, NeedIr, NeedKey, Obligation, PredicateKind,
    ProofBudget, ReuseUnit, SemanticFlowStep, SemanticLocation, SemanticWorkerArtifact,
    SemanticWorld, SubjectId, built_in_route_contracts, compile_need, need_fragment,
};
use needle_runtime::{
    NeedShadowWrite, ProofCandidate, ProofPlanner, RuntimeSettings, RuntimeStore,
    SemanticCostEstimates, SemanticResolver, validate_semantic_artifact,
};
use serde::Serialize;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every operation delegates to the process System allocator with the
// exact same pointer and Layout contract. The atomics only observe counts.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegated with the caller-provided valid Layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegated with the caller-provided valid Layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: delegated with the original pointer and Layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: delegated with the caller-provided pointer, Layout and size.
        unsafe { System.realloc(pointer, old, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Serialize)]
struct Observation {
    name: &'static str,
    iterations: usize,
    input_bytes: usize,
    median_ns: u128,
    p95_ns: u128,
    allocations: u64,
    allocated_bytes: u64,
}

fn main() {
    let iterations = 2_000;
    let small = marker_with_body(1_700);
    let large = marker_with_body(15_400);
    let mut observations = vec![
        marker_observation(&small, iterations),
        parse_observation("parse_2k", &small, iterations),
        parse_observation("parse_16k", &large, iterations),
        hash_observation(iterations),
        request_hash_observation(iterations),
        artifact_hash_observation(iterations),
        planner_observation(25),
        replay_observation(iterations),
    ];
    observations.extend(resolver_observations());
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "needle.proof-microbench/1",
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "observations": observations,
        }))
        .expect("microbenchmark report is serializable")
    );
    let parser_allocations = observations
        .iter()
        .filter(|item| item.name.starts_with("parse_"))
        .map(|item| item.allocations)
        .sum::<u64>();
    let hash_allocations = observations
        .iter()
        .filter(|item| {
            matches!(item.name, "canonical_hash" | "artifact_request_hash" | "artifact_hash")
        })
        .map(|item| item.allocations)
        .sum::<u64>();
    let planner_allocations = observations
        .iter()
        .find(|item| item.name == "planner_validity_warm")
        .map(|item| item.allocations)
        .unwrap_or(u64::MAX);
    let replay_allocations = observations
        .iter()
        .find(|item| item.name == "proof_replay_warm")
        .map(|item| item.allocations)
        .unwrap_or(u64::MAX);
    if parser_allocations != 0
        || hash_allocations != 0
        || planner_allocations != 0
        || replay_allocations != 0
    {
        eprintln!(
            "allocation gate failed: parser={parser_allocations}, canonical_hash={hash_allocations}, planner={planner_allocations}, replay={replay_allocations}"
        );
        std::process::exit(2);
    }
}

fn marker_observation(input: &str, iterations: usize) -> Observation {
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        black_box(black_box(input.as_bytes()).starts_with(b"@@need\n"));
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize("marker_recognition", input.len(), durations, allocations, allocated_bytes)
}

fn marker_with_body(body_bytes: usize) -> String {
    let prefix = "@@need\n\
@route locate.implementation\n\
@subject cli-option:\"--glob-case-insensitive\"\n\
@require implementation-location selection=primary granularity=exact-location\n\
@world source=current features=default\n\
\n";
    let mut marker = String::with_capacity(prefix.len() + body_bytes + 8);
    marker.push_str(prefix);
    marker.extend(std::iter::repeat_n('x', body_bytes));
    marker.push_str("\n@@end");
    assert!(marker.len() <= needle_core::MAX_NEED_IR_BYTES);
    marker
}

fn parse_observation(name: &'static str, input: &str, iterations: usize) -> Observation {
    BorrowedNeedIr::parse(input).expect("warm parse").expect("typed marker");
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        let parsed = BorrowedNeedIr::parse(black_box(input))
            .expect("benchmark marker is valid")
            .expect("benchmark marker is typed");
        black_box(parsed);
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize(name, input.len(), durations, allocations, allocated_bytes)
}

fn hash_observation(iterations: usize) -> Observation {
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for index in 0..iterations {
        let started = Instant::now();
        let mut hash = CanonicalHasher::new(b"microbench");
        hash.field_str("implementation-location");
        hash.field_str("--glob-case-insensitive");
        hash.field_u16(index as u16);
        black_box(hash.finish());
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize("canonical_hash", 0, durations, allocations, allocated_bytes)
}

fn artifact_hash_observation(iterations: usize) -> Observation {
    let artifact = SemanticWorkerArtifact::CodeLocation {
        locations: vec![
            SemanticLocation {
                role: LocationRole::Supporting,
                path: "src/support.rs".to_owned(),
                symbol: Some("support".to_owned()),
                byte_start: Some(20),
                byte_end: Some(40),
            },
            SemanticLocation {
                role: LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: Some(0),
                byte_end: Some(19),
            },
        ],
        gaps: Vec::new(),
    };
    let contract = Digest::blake3(b"contract");
    let _ = artifact.canonical_artifact_id(contract);
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        black_box(artifact.canonical_artifact_id(contract));
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize("artifact_hash", 2, durations, allocations, allocated_bytes)
}

fn request_hash_observation(iterations: usize) -> Observation {
    let request = ArtifactRequest {
        contract_id: "needle.semantic.code-location".to_owned(),
        contract_revision: 2,
        repository_id: Digest::blake3(b"repository"),
        source_snapshot_digest: Digest::blake3(b"source"),
        route_key: NeedKey::new("locate.implementation").expect("valid route"),
        normalized_request: "Locate the implementation.\r\nUse exact evidence.".to_owned(),
        semantic_fragment_id: None,
        input_artifact_ids: vec![Digest::blake3(b"left"), Digest::blake3(b"right")],
    };
    let _ = request.semantic_id();
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        black_box(request.semantic_id());
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize(
        "artifact_request_hash",
        request.normalized_request.len(),
        durations,
        allocations,
        allocated_bytes,
    )
}

fn planner_observation(iterations: usize) -> Observation {
    let (need, candidates) = proof_fixture();
    let planner = ProofPlanner::new();
    planner.plan_validity(&need, &candidates, &ProofBudget::default()).expect("warm planner");
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        black_box(
            planner
                .plan_validity(black_box(&need), black_box(&candidates), &ProofBudget::default())
                .expect("validity plan"),
        );
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize("planner_validity_warm", candidates.len(), durations, allocations, allocated_bytes)
}

fn replay_observation(iterations: usize) -> Observation {
    let (need, candidates) = proof_fixture();
    let planner = ProofPlanner::new();
    let proof = planner
        .plan(&need, &candidates, 1, Some(10_000), &ProofBudget::default())
        .expect("fixture proof");
    let certificate = proof.certificate.expect("fixture is fully covered");
    assert!(planner.replay(&need, &certificate, &candidates));
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        assert!(black_box(planner.replay(
            black_box(&need),
            black_box(&certificate),
            black_box(&candidates),
        )));
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize("proof_replay_warm", candidates.len(), durations, allocations, allocated_bytes)
}

fn proof_fixture() -> (Need, Vec<ProofCandidate>) {
    let subject = SubjectId(Digest::blake3(b"bench-subject"));
    let obligations = (0_u8..16)
        .map(|index| {
            Obligation::new(
                match index % 3 {
                    0 => PredicateKind::ImplementationLocation,
                    1 => PredicateKind::RuntimeFlow,
                    _ => PredicateKind::FocusedTests,
                },
                subject,
                vec![Facet { key: "slot".to_owned(), value: index.to_string() }],
            )
        })
        .collect::<Vec<_>>();
    let need = Need {
        id: NeedId(Digest::blake3(b"bench-need")),
        subjects: Vec::new(),
        required: obligations.clone(),
        preferred: Vec::new(),
        semantic_constraints: Vec::new(),
        world: SemanticWorld {
            repository_lineage: Digest::blake3(b"bench-repository"),
            source_selector: "current".to_owned(),
            platform: "current".to_owned(),
            features: "default".to_owned(),
            configuration: None,
            toolchain: None,
        },
        input_artifacts: Vec::new(),
        residual: None,
        body_digest: Digest::blake3(b"bench-body"),
        format_revision: 1,
    };
    let candidates = (0_u8..64)
        .map(|index| {
            let left = usize::from(index) % obligations.len();
            let right = (left + 1 + usize::from(index / 16)) % obligations.len();
            ProofCandidate {
                artifact: ArtifactId(Digest::blake3([index])),
                validation_certificate: ArtifactValidationCertificateId(Digest::blake3([index, 1])),
                coverage: vec![obligations[left].clone(), obligations[right].clone()],
                exact_request: false,
                expected_reuse_microusd: 1,
                claim_ids: Vec::new(),
                claim_validation_certificate_ids: Vec::new(),
                claim_set_certificate_id: None,
            }
        })
        .collect();
    (need, candidates)
}

struct ResolverFixture {
    root: PathBuf,
    store: RuntimeStore,
    resolver: SemanticResolver,
    locate: Need,
    trace: Need,
    locate_route: NeedKey,
    trace_route: NeedKey,
    trace_fragment: NeedFragment,
    exact_request: Digest,
    source_snapshot_digest: Digest,
}

impl ResolverFixture {
    fn create() -> Self {
        let root = std::env::temp_dir().join(format!(
            "needle-proof-bench-{}-{}",
            std::process::id(),
            Digest::blake3(format!("{:?}", Instant::now())).to_hex()
        ));
        fs::create_dir_all(root.join("src")).expect("benchmark source directory");
        fs::write(root.join("src/location.rs"), "pub fn answer() -> u32 { 42 }\n")
            .expect("benchmark location");
        fs::write(root.join("src/support.rs"), "pub fn support() {}\n")
            .expect("benchmark supporting location");
        fs::write(root.join("src/flow.rs"), "pub fn flow_answer() {}\n").expect("benchmark flow");

        let store = RuntimeStore::new(root.join("needle.sqlite3"));
        store.initialize().expect("benchmark store");
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: "codex".to_owned(),
                worker_model: "benchmark".to_owned(),
                worker_reasoning: "low".to_owned(),
                worker_timeout_seconds: 1,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: false,
                multi_need_policy: needle_core::MultiNeedPolicy::default(),
            })
            .expect("benchmark defaults");
        let contracts = built_in_route_contracts();
        let locate_contract = contracts
            .iter()
            .find(|contract| contract.route.as_str() == "locate.implementation")
            .expect("locate contract");
        let trace_contract = contracts
            .iter()
            .find(|contract| contract.route.as_str() == "trace.state-flow")
            .expect("trace contract");
        let locate_ir = NeedIr::parse(
            "@@need\n\
@route locate.implementation\n\
@subject symbol:\"answer\"\n\
@require implementation-location selection=primary granularity=exact-location polarity=positive\n\
@world source=current features=default\n\
\n\
Locate the implementation.\n\
@@end",
        )
        .expect("locate parse")
        .expect("locate marker");
        let trace_ir = NeedIr::parse(
            "@@need\n\
@route trace.state-flow\n\
@subject symbol:\"answer\"\n\
@require implementation-location selection=primary granularity=exact-location polarity=positive\n\
@require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
@world source=current features=default\n\
\n\
Trace the runtime flow.\n\
@@end",
        )
        .expect("trace parse")
        .expect("trace marker");
        let repository = Digest::blake3(b"proof-benchmark-repository");
        let locate =
            compile_need(&locate_ir, repository, locate_contract).expect("locate compilation");
        let trace = compile_need(&trace_ir, repository, trace_contract).expect("trace compilation");
        let locate_route = locate_contract.route.clone();
        let trace_route = trace_contract.route.clone();
        let locate_fragment = need_fragment(&locate, locate.required.clone(), Vec::new());
        let trace_fragment = need_fragment(&trace, trace.required.clone(), Vec::new());
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "bench-locate",
                turn_id: "turn",
                transport_digest: Digest::blake3(b"bench-locate-transport"),
                parser_definition_digest: Digest::blake3(b"bench-parser"),
                prompt_profile_digest: Digest::blake3(b"bench-profile"),
                need_ir: &locate_ir,
                need: &locate,
                fragments: std::slice::from_ref(&locate_fragment),
            })
            .expect("persist locate");
        store
            .record_need_shadow(NeedShadowWrite {
                session_id: "bench-trace",
                turn_id: "turn",
                transport_digest: Digest::blake3(b"bench-trace-transport"),
                parser_definition_digest: Digest::blake3(b"bench-parser"),
                prompt_profile_digest: Digest::blake3(b"bench-profile"),
                need_ir: &trace_ir,
                need: &trace,
                fragments: std::slice::from_ref(&trace_fragment),
            })
            .expect("persist trace");

        let location = SemanticWorkerArtifact::CodeLocation {
            locations: vec![
                SemanticLocation {
                    role: LocationRole::Primary,
                    path: "src/location.rs".to_owned(),
                    symbol: Some("answer".to_owned()),
                    byte_start: Some(0),
                    byte_end: Some(28),
                },
                SemanticLocation {
                    role: LocationRole::Supporting,
                    path: "src/support.rs".to_owned(),
                    symbol: Some("support".to_owned()),
                    byte_start: None,
                    byte_end: None,
                },
            ],
            gaps: Vec::new(),
        };
        let source_snapshot_digest = Digest::blake3(b"proof-benchmark-source");
        let request = ArtifactRequest {
            contract_id: "needle.semantic.code-location".to_owned(),
            contract_revision: 2,
            repository_id: repository,
            source_snapshot_digest,
            route_key: locate_contract.route.clone(),
            normalized_request: "Locate the implementation.".to_owned(),
            semantic_fragment_id: Some(locate_fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let exact_request = request.semantic_id().digest();
        let validated =
            validate_semantic_artifact(&locate_fragment, &location, &root, exact_request)
                .expect("validate benchmark location");
        store
            .publish_semantic_artifact(
                &request,
                &locate,
                &validated.artifact,
                &validated.certificate,
            )
            .expect("publish benchmark location");
        store
            .publish_claims_shadow(
                &validated.artifact,
                &validated.certificate,
                &validated.claims.claims,
                &validated.claims.origins,
                &validated.claims.relations,
                &validated.claims.certificates,
            )
            .expect("publish benchmark claims");
        let implementation = store
            .capability_classes()
            .expect("capabilities")
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == PredicateKind::ImplementationLocation
            })
            .expect("implementation capability");
        store
            .set_capability_mode(
                &implementation.id,
                implementation.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"benchmark-evidence")),
            )
            .expect("promote implementation");
        let resolver = SemanticResolver::new(store.clone());
        Self {
            root,
            store,
            resolver,
            locate,
            trace,
            locate_route,
            trace_route,
            trace_fragment,
            exact_request,
            source_snapshot_digest,
        }
    }

    fn promote_claim_and_stale_support(&self) {
        let implementation = self
            .store
            .capability_classes()
            .expect("capabilities")
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Claim
                    && class.predicate == PredicateKind::ImplementationLocation
            })
            .expect("implementation claim capability");
        self.store
            .set_capability_mode(
                &implementation.id,
                implementation.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"benchmark-claim-evidence")),
            )
            .expect("promote implementation claim");
        fs::write(self.root.join("src/support.rs"), "pub fn support() { unreachable!() }\n")
            .expect("mutate only supporting dependency");
    }

    fn publish_behavior(&self) {
        let behavior = SemanticWorkerArtifact::BehaviorTrace {
            scenario: "default".to_owned(),
            steps: [
                FlowStepRole::Producer,
                FlowStepRole::Carrier,
                FlowStepRole::Transformation,
                FlowStepRole::Precedence,
                FlowStepRole::Consumer,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, role)| SemanticFlowStep {
                role,
                location: SemanticLocation {
                    role: LocationRole::Supporting,
                    path: "src/flow.rs".to_owned(),
                    symbol: Some("flow".to_owned()),
                    byte_start: Some(index as u64),
                    byte_end: Some(index as u64 + 1),
                },
                description: format!("{role:?}"),
            })
            .collect(),
            gaps: Vec::new(),
        };
        let request = ArtifactRequest {
            contract_id: "needle.semantic.behavior-trace".to_owned(),
            contract_revision: 2,
            repository_id: self.trace.world.repository_lineage,
            source_snapshot_digest: Digest::blake3(b"proof-benchmark-source"),
            route_key: NeedKey::new("trace.state-flow").expect("valid trace route"),
            normalized_request: "Trace the runtime flow.".to_owned(),
            semantic_fragment_id: Some(self.trace_fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let validated = validate_semantic_artifact(
            &self.trace_fragment,
            &behavior,
            &self.root,
            request.semantic_id().digest(),
        )
        .expect("validate benchmark flow");
        self.store
            .publish_semantic_artifact(
                &request,
                &self.trace,
                &validated.artifact,
                &validated.certificate,
            )
            .expect("publish benchmark flow");
        let runtime = self
            .store
            .capability_classes()
            .expect("capabilities")
            .into_iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == PredicateKind::RuntimeFlow
            })
            .expect("runtime capability");
        self.store
            .set_capability_mode(
                &runtime.id,
                runtime.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"benchmark-runtime-evidence")),
            )
            .expect("promote runtime");
    }
}

fn resolver_observations() -> Vec<Observation> {
    let fixture = ResolverFixture::create();
    let artifact_costs = SemanticCostEstimates {
        fresh_microusd: Some(100),
        artifact_reuse_microusd: Some(1),
        claim_reuse_microusd: None,
        claim_partial_reuse_microusd: None,
    };
    let mut observations = Vec::new();
    observations.push(measure_resolver("coverage_lookup_cold", 1, || {
        fixture
            .resolver
            .resolve_for_route(
                &fixture.locate,
                &fixture.locate_route,
                &fixture.root,
                fixture.source_snapshot_digest,
                artifact_costs,
                &[],
            )
            .expect("cold coverage resolution")
    }));
    observations.push(measure_resolver("coverage_hit_warm", 50, || {
        fixture
            .resolver
            .resolve_for_route(
                &fixture.locate,
                &fixture.locate_route,
                &fixture.root,
                fixture.source_snapshot_digest,
                artifact_costs,
                &[],
            )
            .expect("warm coverage resolution")
    }));
    observations.push(measure_resolver("exact_hit_warm", 50, || {
        fixture
            .resolver
            .resolve_for_route(
                &fixture.locate,
                &fixture.locate_route,
                &fixture.root,
                fixture.source_snapshot_digest,
                artifact_costs,
                &[fixture.exact_request],
            )
            .expect("warm exact resolution")
    }));
    observations.push(measure_resolver("partial_scheduling_warm", 50, || {
        fixture
            .resolver
            .resolve_for_route(
                &fixture.trace,
                &fixture.trace_route,
                &fixture.root,
                fixture.source_snapshot_digest,
                artifact_costs,
                &[],
            )
            .expect("warm partial resolution")
    }));
    fixture.publish_behavior();
    observations.push(measure_resolver("composite_scheduling_warm", 50, || {
        fixture
            .resolver
            .resolve_for_route(
                &fixture.trace,
                &fixture.trace_route,
                &fixture.root,
                fixture.source_snapshot_digest,
                artifact_costs,
                &[],
            )
            .expect("warm composite resolution")
    }));
    let claim_fixture = ResolverFixture::create();
    claim_fixture.promote_claim_and_stale_support();
    let claim_costs = SemanticCostEstimates {
        fresh_microusd: Some(100),
        artifact_reuse_microusd: Some(1),
        claim_reuse_microusd: Some(1),
        claim_partial_reuse_microusd: None,
    };
    let warmed_claim = claim_fixture
        .resolver
        .resolve_for_route(
            &claim_fixture.locate,
            &claim_fixture.locate_route,
            &claim_fixture.root,
            claim_fixture.source_snapshot_digest,
            claim_costs,
            &[],
        )
        .expect("warm claim hit");
    assert!(matches!(warmed_claim.resolution, CacheResolution::ClaimHit { .. }));
    observations.push(measure_resolver("claim_hit_warm", 50, || {
        claim_fixture
            .resolver
            .resolve_for_route(
                &claim_fixture.locate,
                &claim_fixture.locate_route,
                &claim_fixture.root,
                claim_fixture.source_snapshot_digest,
                claim_costs,
                &[],
            )
            .expect("warm claim resolution")
    }));
    let claim_partial_costs = SemanticCostEstimates {
        fresh_microusd: Some(100),
        artifact_reuse_microusd: Some(1),
        claim_reuse_microusd: Some(1),
        claim_partial_reuse_microusd: Some(20),
    };
    let warmed_claim_partial = claim_fixture
        .resolver
        .resolve_for_route(
            &claim_fixture.trace,
            &claim_fixture.trace_route,
            &claim_fixture.root,
            claim_fixture.source_snapshot_digest,
            claim_partial_costs,
            &[],
        )
        .expect("warm claim partial");
    assert!(matches!(
        warmed_claim_partial.resolution,
        CacheResolution::PartialHit { ref reused_claim_ids, .. } if !reused_claim_ids.is_empty()
    ));
    observations.push(measure_resolver("claim_partial_scheduling_warm", 50, || {
        claim_fixture
            .resolver
            .resolve_for_route(
                &claim_fixture.trace,
                &claim_fixture.trace_route,
                &claim_fixture.root,
                claim_fixture.source_snapshot_digest,
                claim_partial_costs,
                &[],
            )
            .expect("warm claim partial resolution")
    }));
    claim_fixture.publish_behavior();
    let warmed_claim_composite = claim_fixture
        .resolver
        .resolve_for_route(
            &claim_fixture.trace,
            &claim_fixture.trace_route,
            &claim_fixture.root,
            claim_fixture.source_snapshot_digest,
            claim_costs,
            &[],
        )
        .expect("warm claim composite");
    assert!(matches!(warmed_claim_composite.resolution, CacheResolution::ClaimCompositeHit { .. }));
    observations.push(measure_resolver("claim_composite_scheduling_warm", 50, || {
        claim_fixture
            .resolver
            .resolve_for_route(
                &claim_fixture.trace,
                &claim_fixture.trace_route,
                &claim_fixture.root,
                claim_fixture.source_snapshot_digest,
                claim_costs,
                &[],
            )
            .expect("warm claim composite resolution")
    }));
    let claim_root = claim_fixture.root.clone();
    drop(claim_fixture);
    let _ = fs::remove_dir_all(claim_root);
    let root = fixture.root.clone();
    drop(fixture);
    let _ = fs::remove_dir_all(root);
    observations
}

fn measure_resolver(
    name: &'static str,
    iterations: usize,
    mut resolve: impl FnMut() -> needle_runtime::SemanticReuseDecision,
) -> Observation {
    let mut durations = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    for _ in 0..iterations {
        let started = Instant::now();
        black_box(resolve());
        durations.push(started.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    summarize(name, 0, durations, allocations, allocated_bytes)
}

fn summarize(
    name: &'static str,
    input_bytes: usize,
    mut durations: Vec<u128>,
    allocations: u64,
    allocated_bytes: u64,
) -> Observation {
    durations.sort_unstable();
    let iterations = durations.len();
    let median_ns = durations[iterations / 2];
    let p95_index =
        ((iterations as f64 * 0.95).ceil() as usize).saturating_sub(1).min(iterations - 1);
    Observation {
        name,
        iterations,
        input_bytes,
        median_ns,
        p95_ns: durations[p95_index],
        allocations,
        allocated_bytes,
    }
}
