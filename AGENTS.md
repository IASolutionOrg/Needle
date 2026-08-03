# Needle repository instructions for coding agents

These rules apply to the complete workspace.

## Authority

Use current source, tests, migrations, fixtures, and configuration as truth.
`PROJECT_STATUS.md` owns maturity claims; `docs/` explains public behavior.
Plans and old reports are navigation only. Keep implementation, offline
validation, live calibration, and statistical evidence separate.

## Architecture

- Preserve the five crates and dependency direction:
  `needle-core` → `needle-runtime` → platform/application boundaries, with
  `needle-bench` consuming product APIs for evidence.
- Add a crate only for a demonstrated architectural boundary.
- Keep public domain names singular and unversioned. Use migrations, format
  revisions, and definition digests for compatibility.
- SQLite is the local source of truth.
- Keep semantic identity separate from execution, model, prompt, usage, and
  pricing provenance.
- Coverage and claims come from trusted validators, never worker assertions,
  similarity, or embeddings.
- Require validation and replayable sufficiency proof before authoritative
  reuse. Select validity before economics.
- Cache lookup precedes compute. Unknown validity means scope downgrade or
  `BYPASS`. Never serve stale or contradicted evidence.
- Route plans are bounded, acyclic, parent-orchestrated, and dynamically
  unexpandable.
- The main receives bounded projections, not worker transcripts or raw logs.

## Worker and write boundaries

- Evidence workers and verifiers are read-only.
- Patch workers may write only in a disposable checkout and only within paths
  declared by the parent.
- The active worktree is never a worker's writable root.
- Workers receive no network, credentials, project instructions, hooks,
  plugins, external MCP, multi-agent execution, or telemetry.
- Test execution requires a trusted repository, a validated plan, a supported
  command representation, and disposable paths.
- Use a native Codex executable and validate App Server fixtures per supported
  Codex version.

## Working rules

- Inspect `git status` before editing and preserve unrelated user changes.
- When `.codegraph/` exists, use CodeGraph before text search for code paths.
- Prefer targeted reads and focused tests; use RTK when available.
- Use `apply_patch` for file edits and avoid dependency or formatting churn.
- Run deterministic simulation and preflight before any provider experiment.
- Never start or retry a paid run without an estimate and explicit approval.
- Update docs and `PROJECT_STATUS.md` when public behavior, compatibility, or
  evidence changes.

## Human-only publication

Agents must not:

- create commits;
- push branches or tags;
- open, publish, or merge pull requests;
- create releases;
- publish packages or benchmark claims.

AI may assist code and drafts, but a human must understand the complete diff,
finalize commit and pull-request text, verify claims, and execute every
publication action.

Stop with a handoff containing changed files, behavior, design rationale,
validation, unverified boundaries, residual risks, and AI-assistance scope.

## Validation

Start with the smallest proving test. At an integration or publication boundary
run once:

```text
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test -p needle-platform-codex --test offline_n1
cargo test -p needle-platform-codex --test main_interrupt
cargo run --release -p needle-bench --bin proof-microbench

cd crates/needle-app/web
npm test
npm run typecheck
npm run lint
npm run build
npm run test:e2e:local

cargo run -p needle-app -- plugin validate
cargo run -p needle-app -- plugin validate --benchmark
git diff --check
```

No provider call is part of this matrix.

## Definition of done

- Requested behavior is complete and invariants hold.
- Affected callers, persistence, fallbacks, security, and performance were
  checked proportionally.
- Focused validation passed; broader validation ran once when warranted.
- Public docs and project status match current source.
- No unrelated, private, generated, or live-run files changed.
- Remaining limitations and AI assistance are disclosed in the handoff.
