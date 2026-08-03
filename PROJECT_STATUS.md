# Project status

Needle is a pre-alpha developer project. The source is available for review,
experimentation, and contribution, but it is not ready for real workflows,
production deployment, or compatibility-sensitive integration.

This status describes the current default branch and published evidence.

## Evidence language

| Status | Meaning |
|---|---|
| **Implemented** | Behavior exists in the current source. |
| **Offline validated** | Deterministic local tests exercise the behavior without a provider call. |
| **Live calibration** | A bounded provider-backed observation exercised the behavior for one frozen setup. |
| **Statistically validated** | A powered, frozen, multi-task study supports the stated claim. |
| **Pending** | The behavior or required evidence is not available. |

Implementation, offline validation, live calibration, and statistical evidence
are separate claims. A stronger label never follows automatically from a weaker
one.

## Milestones

| Milestone | Implementation | Evidence | Status |
|---|---|---|---|
| Core routing, artifact cache, and sandbox | Typed artifacts, bounded routes, SQLite, read-only workers, disposable checkouts, approvals, exact and partial cache | Workspace tests plus bounded live routing/cache observations | **Implemented; offline validated; live calibration** |
| Proof-gated semantic reuse | Typed needs, exact subjects, validator-derived coverage, validation and sufficiency certificates, exact/coverage/composite/partial resolution | Mutation, freshness, proof replay, cross-route, and live calibration cases | **Implemented; offline validated; live calibration** |
| Multi-need and structured MCP | Sequential and steer delivery, bounded ledger, structured JSON tools, cancellation, shared resolver | App Server simulator and one structured MCP cache-hit observation | **Implemented; offline validated; live calibration** |
| Claim-level reuse | Validator-extracted claims, claim proofs, mixed planning, and bounded authoritative location, runtime-flow, and focused-test claims | Deterministic freshness, mutation, negative, projection, economics, and performance cases | **Implemented; offline validated** |
| Verified changes | Isolated patch preparation, independent verifier, one repair, explicit journaled apply | Simulator and focused persistence, isolation, drift, and recovery tests | **Implemented; offline validated** |
| Codex role-profile control plane | Canonical Codex role definitions, bounded policies, immutable revisions, state-digest CAS, SQLite V14 persistence, audit records, explicit WorkerProfile projection, bounded digest-bound HTTP API, and local editor | Focused deterministic Rust and frontend tests; configuration-only boundary (no worker/session binding or lifecycle execution) | **Implemented; offline validated** |
| Codex development lifecycle orchestration | Evidence, patch, test, verification, approval, and apply primitives exist; the configurable parent-owned role lifecycle is not integrated | Component-level offline evidence only | **Pending** |
| Other-host subagent configuration | Configuration-only interoperability is planned for Claude Code and Cursor, followed by OpenCode and Antigravity | Not available | **Pending** |
| Multi-host orchestration | Execution remains Codex-only; non-Codex execution follows configuration interoperability, a host contract, and conformance evidence | Not available | **Pending** |
| Release readiness | Stable packaging, supported installation, compatibility policy, second live platform, powered corpus | Not available | **Pending** |

## Route and capability matrix

| Route | Required capability | Reuse implemented | Strongest evidence |
|---|---|---|---|
| `locate.implementation` | Primary exact `ImplementationLocation` | Exact, coverage, claim, and partial paths | Live routing/cache calibration for one pinned task |
| `trace.state-flow` | `ImplementationLocation` and `RuntimeFlow`; focused tests when explicitly required | Composite, partial, mixed claim/artifact, and claim-composite planning | One live structured-MCP `CompositeHit`; claim authority is offline validated |
| `tests.relevant` | Representative `FocusedTests` | Coverage, claim, and cross-route reuse | One live partial-to-zero-worker coverage sequence; claim authority is offline validated |

All three built-in claim classes support narrow authoritative reuse after explicit
capability promotion with bounded, validator-derived evidence. Built-in
capability modes remain `Shadow` by default. `RuntimeFlow` and `FocusedTests`
claim authority is covered by deterministic offline validation only; no
provider-backed claim-authority observation exists.

## Runtime and control plane

| Area | Current state |
|---|---|
| Resident runtime and single-instance profile | **Implemented; offline validated** |
| Windows named-pipe IPC | **Implemented; live development evidence** |
| Unix-socket IPC | **Implemented; CI/code support only** |
| SQLite persistence and additive migrations | **Implemented; offline validated** |
| Structured stdio MCP | **Implemented; offline validated; bounded live calibration** |
| Embedded React control plane | **Implemented; frontend and local end-to-end validation** |
| Needs, proofs, claims, changes, runs, models, cache, settings, approvals | **Implemented; development interface** |
| Canonical named Codex role-profile domain and revision store | **Implemented; offline validated** |
| Named role-profile HTTP/editor | **Implemented; offline validated; configuration-only** |
| Role-profile session binding and lifecycle integration | **Pending; Codex-first** |
| Non-Codex subagent configuration | **Pending; configuration only before execution** |
| Non-Codex execution and orchestration | **Pending; later milestone** |
| Stable public API or configuration compatibility | **Pending** |

The web application is an operational development control plane. It is not a
supported hosted service or stable public UI.

## Security boundary

- Evidence workers and verifiers are read-only.
- Patch workers receive workspace-write only inside disposable checkouts.
- File-change approval never grants access to the active worktree.
- Workers have no network, credentials, project instructions, plugins, hooks,
  external MCP, or multi-agent execution.
- Test commands require a trusted repository, a validated plan, bounded paths,
  and an approved command representation.
- Stale, contradicted, ambiguous, or unknown evidence is not served.
- Apply requires a verified revision, unchanged source snapshot, confirmation,
  CSRF, and `If-Match`.

These properties have deterministic coverage. They are not a completed
third-party security audit.

## Performance evidence

The current release-profile microbenchmark on the recorded Windows host
measured:

| Warm path | Recorded p95 |
|---|---:|
| Exact artifact hit | 3.78 ms |
| Coverage hit | 3.39 ms |
| Composite scheduling | 4.59 ms |
| Partial scheduling | 4.08 ms |
| Claim hit | 5.36 ms |
| Claim composite scheduling | 5.88 ms |
| Claim partial scheduling | 6.32 ms |

Common marker recognition, canonical hashing, warm proof replay, and warm
validity planning retain deterministic allocation gates. These are local,
host-specific measurements, not end-to-end product latency claims. See the
[claim reuse performance report](benchmarks/results/offline/claim-reuse-performance.md).

## Platform evidence

| Platform | Code and CI | Provider-backed evidence |
|---|---|---|
| Windows | Implemented | Bounded routing, cache, MCP, and partial-reuse calibration |
| Linux | Implemented | None recorded |
| macOS | Implemented | None recorded |

Cross-platform code support must not be described as cross-platform live
validation.

## Current limitations

- The repository is not ready for installation or production use.
- Public APIs, configuration, storage, and tool schemas are unstable.
- Claim authority is deliberately narrow, defaults to `Shadow`, and has no
  provider-backed evidence.
- Verified changes have no provider-backed patcher or verifier observation.
- Canonical role-profile definitions, revision persistence, bounded HTTP/editor
  flows, and request-time preflight are implemented and offline validated.
  They do not provide session or worker binding, a lifecycle executor, or
  automatic profile activation; activation is an explicit configuration change.
- The verifier handles a deterministic serial set of up to four distinct
  associated certified test plans; exact duplicates collapse to one execution,
  while over-cap and unavailable plans fail closed. This behavior is offline
  validated only and has no provider-backed verifier evidence.
- Binary, symlink, submodule, rename, three-way merge, patch reuse, automatic
  commit, and automatic publication are unsupported.
- Worker execution and orchestration are Codex-only. Claude Code, Cursor,
  OpenCode, and Antigravity configuration and execution are unsupported.
- Linux and macOS lack live platform evidence.
- The accepted economic results are calibrations, not a powered corpus or
  general savings claim.
- No public beta, support channel, or compatibility window exists.

## Next milestone

Release readiness requires all of the following:

1. provider-backed calibration of authoritative claim reuse;
2. live end-to-end verified-change validation (`prepare_change` -> `verify_change` / patcher-verifier);
3. a second live platform;
4. a frozen multi-task corpus with independent oracles;
5. a powered paired analysis and confidence interval;
6. stable developer packaging and a compatibility policy;
7. a dedicated security and publication review.

The separately ordered product-expansion milestones for Codex role profiles,
Codex-first lifecycle orchestration, other-host configuration, and later
multi-host orchestration are documented in the [product roadmap](docs/ROADMAP.md).
Configuration interoperability never implies execution support.

Calibration and planned work are not a release declaration or a general
performance claim.

## Evidence index

- [Accepted benchmark evidence](benchmarks/README.md)
- [Benchmark methodology](docs/BENCHMARKING.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Security and approvals](docs/SECURITY_AND_APPROVALS.md)
- [Verified changes](docs/VERIFIED_CHANGES.md)
