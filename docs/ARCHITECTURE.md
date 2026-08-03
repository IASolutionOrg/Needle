# Architecture

Needle is a local-first routing and evidence-reuse layer between a frontier
coding model and bounded supervised workers.

## Product boundary

Needle owns:

- compilation of typed semantic needs;
- bounded route selection and planning;
- worker dispatch and continuation;
- trusted artifact and claim validation;
- cache freshness, proof replay, and economic selection;
- local persistence, approvals, accounting, and control-plane state.

Needle does not replace a coding client, model, language server, code graph, or
search engine. Those systems may help discovery; they do not grant reuse
authority.

## Workspace topology

```text
needle-core
  domain types, NeedIR, contracts, identities, routes, plans

needle-runtime
  SQLite, validation, resolver, sandbox, approval, orchestration

needle-platform-codex
  hooks, App Server transport, worker protocol, compatibility fixtures

needle-bench
  frozen evidence, accounting, replay, corpus, performance measurements

needle-app
  CLI, resident runtime, MCP, HTTP/SSE, embedded React application
```

Core has no platform dependency. Runtime consumes core. Platform adapters
translate external protocol events into runtime operations. The app composes
the system. Benchmarks consume product interfaces rather than becoming a
parallel resolver.

## Process and trust boundaries

### Main process

The main model emits an unversioned semantic marker through a lifecycle hook or
calls structured MCP. The transport is untrusted input. Needle compiles it,
resolves enabled routes, and returns a bounded `FrontierView` or a typed bypass.

### Resident runtime

`needle serve` owns one profile lock, SQLite connection boundary, IPC endpoint,
loopback HTTP server, approvals, hot immutable caches, and SSE state. SQLite is
the source of truth; memory caches cannot make a rejected record valid.

### Evidence worker

An evidence worker runs through Codex App Server in a read-only sandbox. It may
inspect an exact disposable source snapshot and propose typed artifacts. Trusted
validators, not the model, determine coverage and authority.

### Patch worker

A patch worker receives workspace-write only inside a disposable checkout and
only for parent-declared paths. The filesystem comparison between base and
final checkout creates the patch artifact. The active worktree remains outside
the worker's writable boundary.

### Verifier

The verifier is a separate read-only worker. It receives the patched checkout,
acceptance criteria, and certified test context, but no patcher transcript.

## Request flow

```text
transport input
  -> compile typed need
  -> freeze subject, world, obligations, route snapshot
  -> exact request lookup
  -> artifact and claim candidate lookup
  -> freshness and contradiction checks
  -> predicate satisfaction and proof replay
  -> valid plan set
  -> economic selection
  -> hit, partial worker request, fresh worker, or bypass
  -> bounded FrontierView
  -> main continuation
```

Validity never depends on price. Economics compares only already-valid plans.
Missing reliable cost evidence keeps reuse advisory.

## Built-in routes

| Route | Minimum required output |
|---|---|
| `locate.implementation` | Primary exact implementation location |
| `trace.state-flow` | Implementation location and default runtime flow |
| `tests.relevant` | Representative focused test plan |

Plans are acyclic, parent-orchestrated, bounded to 16 nodes, and cannot expand
workers dynamically. A partial hit runs only operators already declared for
missing typed obligations.

## Multi-need lifecycle

The default coordination mode resolves a need, delivers context in a new turn
on the same App Server thread, and resumes the main. Explicit
`continue-working` permits the current turn to continue while one cancellable
resolution runs; delivery uses `turn/steer` when possible and falls back to a
new turn when not steerable.

The session ledger classifies later needs as repeat, residual, extension,
overlap, independent, or incompatible. Limits produce a bounded native bypass,
not an unbounded worker tree.

## Persistence

SQLite stores immutable definitions, settings, sessions, needs, steps,
artifacts, claims, dependencies, certificates, plans, attempts, approvals,
usage, economic observations, changes, verification, and apply journals.

Migrations are additive and checksummed. Existing migration text is immutable.
Sessions retain their initial route set, prompt profile, grammar or transport
digest, semantic-definition digest, model, and multi-need policy.

## Failure behavior

- malformed, ambiguous, incompatible, or unbounded input → reject or `BYPASS`;
- unknown dependency validity → scope downgrade or `BYPASS`;
- stale or contradicted evidence → never return context;
- ordinary worker failure → bounded native fallback when configured;
- sandbox escape, checkout corruption, or unverifiable cleanup → fail closed;
- cancellation → interrupt turn, terminate process tree, and record cleanup;
- active source drift during apply → `409`, no merge and no write.

## Further reading

- [Artifacts and cache](ARTIFACTS_AND_CACHE.md)
- [Security and approvals](SECURITY_AND_APPROVALS.md)
- [Runtime and web control plane](RUNTIME_AND_WEB_CONTROL_PLANE.md)
- [Project status](../PROJECT_STATUS.md)
