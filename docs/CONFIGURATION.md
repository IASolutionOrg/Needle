# Configuration

Needle stores profile configuration in the same local SQLite database used for
routes, artifacts, proofs, approvals, runs, and changes.

## Data directory

Precedence:

1. `--data-dir <directory>`;
2. `NEEDLE_DATA_DIR`;
3. `%LOCALAPPDATA%\Needle` on Windows;
4. `$XDG_DATA_HOME/needle`;
5. `$HOME/.local/share/needle`.

The database is `<data-dir>/needle.sqlite3`. Use an explicit directory for
tests, experiments, or parallel profiles.

## Initialization

```text
needle init \
  --codex <native-executable> \
  --worker-model <model> \
  --worker-reasoning <low|medium|high|xhigh> \
  [--worker-timeout-seconds <seconds>] \
  [--evidence-failure-policy <discard_invalid_fact|repair_once>] \
  [--trust-test-execution] \
  [--data-dir <directory>]
```

Persistent runtime settings are:

- native Codex executable;
- worker model and reasoning;
- worker timeout;
- evidence failure policy;
- trusted test execution;
- multi-need policy.

Credentials are neither imported nor exported.

## Current agent-host scope

Worker execution and orchestration currently use Codex only. The web control
plane can edit Codex model policy and runtime bounds, but it does not yet manage
named role profiles or configure another agent host.

The planned sequence is Codex-first role configuration and lifecycle
orchestration, followed by configuration-only interoperability for Claude Code
and Cursor, then OpenCode and Antigravity. Configuration interoperability does
not authorize Needle to launch or orchestrate those hosts. Non-Codex execution
is a later milestone with separate compatibility, isolation, and validation
requirements. See the [product roadmap](ROADMAP.md).

## Canonical Codex role profiles

Named Codex role profiles are persisted in the local SQLite database as
canonical, immutable definition revisions. The supported roles are
`explorer`, `implementer`, `test_runner`, `reviewer`, `verifier`, and `auditor`;
the host is always `codex`. Definitions are stored separately from runtime
settings and `ModelPolicy`, and are not automatically selected or activated.

The safe policy vocabulary is deliberately closed:

- tools: `read_only` or `isolated_write`;
- commands: `denied`, `read_only`, or `certified_tests`;
- filesystem: `read_only_checkout` or `disposable_checkout`;
- network: `denied` only;
- tests: `disabled` or `certified`;
- repair: `none` or `once`;
- fallback: `disabled` or `native`.

`isolated_write` is limited to `implementer` in a `disposable_checkout`.
Every other role is read-only. `certified_tests` is paired with `certified`
tests and is limited to `test_runner` or `verifier`. Concurrency is exactly
one. Timeout is 1--3600 seconds; budgets are 1--8 turns, 1--2000 output
tokens, and 1--1,000,000,000 micro-USD. Route assignments are canonicalized
by sort-and-deduplicate and are bounded by the hard per-task limit of eight.
Models are 1--128 ASCII `[A-Za-z0-9._-]` tokens; credential-like prefixes,
paths, credentials, and network settings are rejected.

Each revision stores its complete canonical JSON and definition digest. State
changes use a generation-bound state digest (draft, active, or inactive) and
are applied transactionally. Historical revisions remain readable; activating
an exact historical revision does not rewrite its definition. `Default` tier
projects to the existing `WorkerProfile` representation with `None`, while
`Priority` projects to `Some("priority")`; that compatibility digest is
distinct from the role-profile definition/revision digest. Projection is
explicit and does not read or modify `ModelPolicy`.

The domain and SQLite migration are currently an offline persistence boundary:
there is no HTTP/editor UI, session or worker binding, lifecycle executor, or
automatic activation yet. Role profiles never carry credentials, host paths,
or network access.

## Export and import

```text
needle config export --data-dir .needle-data
needle config export --output needle.toml --data-dir .needle-data
needle config import needle.toml --data-dir .needle-data
```

The TOML snapshot contains a format revision, runtime settings, presets,
routes, and optional model policy. Import rejects unsupported revisions,
invalid values, and definition-digest mismatches.

## Routes

```text
needle route list --data-dir .needle-data
needle route show locate.implementation --data-dir .needle-data
needle route enable locate.implementation --data-dir .needle-data
needle route disable tests.relevant --data-dir .needle-data
```

Each route binds an ID, enabled state, priority, matcher, preset, plan, and
immutable definition digest. State can change; definitions remain digest
addressed.

## Model policies

`FixedOrder` contains explicit worker profiles, one optional repair in the same
thread, and native fallback. Profiles are attempted in configured order.

`CheapestValidatedFirst` accepts only route/profile combinations with recorded
promotion evidence. Recommendations never change policy automatically.

Model policy is edited through the Models page or digest-bound HTTP API.

## Multi-need policy

Default settings:

```toml
[settings.multi_need_policy]
multi_need_enabled = true
continue_working_enabled = true
max_needs_per_task = 3
max_workers_per_task = 3
pending_main_tools = "allow_and_taint"
resolver_concurrency = 1
```

Need and worker limits have an internal hard cap of eight. Resolution
concurrency is one. Main tools used while a continue-working resolution is
pending taint the zero-discovery claim but do not fail the task.

## Capability promotion

Semantic capability modes are `Shadow`, `Advisory`, and `Authoritative`.

Authoritative reuse additionally requires:

- exact canonical repository-scoped subject;
- supported predicate facets and compatible world;
- fresh validation and sufficiency certificates;
- no mandatory residual or contradiction;
- compatible observed fresh and reuse costs;
- positive net reuse value.

Promotion requires the current definition digest, confirmation, and evidence
digest. It is audited in SQLite. Route/profile promotion and semantic
capability promotion are separate decisions.

## Optimistic concurrency

Settings, route state, model policy, capability authority, and apply use:

```text
If-Match: "<current-digest>"
```

- missing digest → `428 Precondition Required`;
- stale digest → `412 Precondition Failed`;
- valid digest → update plus new state/digest.

## Session immutability

A session freezes its prompt profile, transport definition, semantic
definition, route set, main model, repository, and multi-need policy. Later
configuration updates affect future sessions only. A digest mismatch causes
bypass or rejection; it never silently reinterprets historical input.

## Cache operations

```text
needle cache list --data-dir .needle-data
needle cache show <identity-digest> --data-dir .needle-data
needle cache latest-run --data-dir .needle-data
needle cache invalidate <identity-digest> --data-dir .needle-data
needle cache invalidate --all --data-dir .needle-data
```

Invalidation is local and does not change source files or remote state.

## See also

- [Developer setup](DEVELOPER_SETUP.md)
- [Artifacts and cache](ARTIFACTS_AND_CACHE.md)
- [Runtime and web control plane](RUNTIME_AND_WEB_CONTROL_PLANE.md)
