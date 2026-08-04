# Contributing to Needle

Needle is a pre-alpha local-first router and proof-gated cache for coding
agents. Contributions are welcome, but the project is not yet a supported tool
and its interfaces may change without compatibility guarantees.

## Human ownership is mandatory

AI may assist investigation, code, tests, and documentation. It may not replace
the contributor's understanding or ownership.

Before publishing a contribution, you must:

- read and understand the complete diff;
- be able to explain the behavior, design, failure modes, and trade-offs;
- verify test results and every technical or performance claim yourself;
- finalize the commit message and pull-request text in your own words;
- personally execute the commit, push, and pull-request publication.

Do not submit generated changes you cannot review or explain. A contribution
accepted blindly from an AI system does not meet this project's contribution
standard.

## Before starting

Discuss substantial features, new routes, persistence changes, protocol
changes, or benchmark claims before investing in a large patch. Keep each pull
request focused and avoid unrelated refactors.

Read:

- [Project status](PROJECT_STATUS.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Development and troubleshooting](docs/DEVELOPMENT_AND_TROUBLESHOOTING.md)
- [Repository agent instructions](AGENTS.md), when using an agent

## Prerequisites

- Rust `1.90.0` with `rustfmt` and `clippy`;
- Node.js `22.22.0` or newer and npm for building the embedded frontend assets;
- Codex `0.144.0` for adapter and App Server compatibility work.

```text
cd crates/needle-app/web
npm ci
npm run build
cd ../../..
cargo build --locked --workspace
cargo test --locked --workspace
```

The Rust binary embeds the generated `web/dist` assets. Node.js is required to
build them, but not to run the binary.

## Workflow

1. Start from a current checkout and inspect existing changes.
2. Create a descriptive branch.
3. Implement one coherent behavior with focused tests.
4. Update public documentation and `PROJECT_STATUS.md` when behavior,
   configuration, compatibility, or evidence changes.
5. Run the smallest proving tests, then the relevant integration checks.
6. Review the complete diff for generated files, secrets, live artifacts, and
   unrelated changes.
7. Complete the pull-request template and publish it personally.

Never commit `target/`, frontend build output, credentials, live run roots, or
`.codegraph/`.

## Commits

Use one coherent commit per independently reviewable change:

```text
<type>(<scope>): <imperative summary>
```

Accepted types are `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `chore`,
and `ci`. Omit the scope when it adds no useful information. Use the body to
explain why the change exists, important trade-offs, and compatibility impact.
Mark breaking changes explicitly.

The human contributor must finalize and execute every commit. Agents must stop
at a reviewed working-tree handoff.

## Architecture and safety requirements

Contributions must preserve:

- the five-crate architecture unless a new boundary is demonstrated;
- singular, unversioned public domain names;
- SQLite as local source of truth;
- semantic identity independent from model, prompt, usage, and pricing;
- validator-derived coverage and replayable proof before authoritative reuse;
- validity-first and economics-second selection;
- validation before artifact admission and reuse;
- `BYPASS` when dependency validity is unknown;
- bounded, acyclic, parent-orchestrated route plans;
- read-only evidence workers and isolated test execution;
- patch writes confined to a disposable checkout and declared paths;
- no worker credentials, network, external telemetry, or active-worktree
  writes;
- bounded main projections and explicit native fallback.

See [Artifacts and cache](docs/ARTIFACTS_AND_CACHE.md) and
[Security and approvals](docs/SECURITY_AND_APPROVALS.md).

## Adding product concepts

### Routes and plans

A route must define its stable key, typed contract, bounded acyclic plan,
definition digest, cache behavior, failure behavior, tests, UI visibility, and
documentation.

### Artifacts, claims, and predicates

Define semantic identity, payload schema, dependencies, trusted validation,
projection, cache scope, invalidation, hard negatives, certificate drift, and
promotion evidence. Worker assertions, similarity, and embeddings cannot grant
authority.

### Model policies

Preserve user control, bounded repair, explicit native fallback, and promotion
requirements. Recommendations must not silently modify configured policy.

### Persistence

Add a new migration. Never rewrite existing migration text. Include fresh
database, upgrade, failure, and recovery coverage.

## Benchmark contributions

A benchmark task requires:

- a public repository and pinned 40-character commit SHA;
- a short natural English task;
- an independent semantic oracle hidden from the model;
- a verified focused command when the task requires one;
- explicit calibration or holdout classification;
- model, reasoning, Codex, tier, seed, and pricing metadata;
- complete accounting for the path actually executed.

Paid provider experiments require a reviewed protocol, deterministic preflight,
written cost estimate, and explicit human authorization. Infrastructure and
code failures are not quality observations and do not belong in accepted
benchmark evidence.

## Validation

Run the checks proportional to the change. The publication-readiness matrix is:

```text
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --workspace
cargo test --locked -p needle-platform-codex --test offline_n1
cargo test --locked -p needle-platform-codex --test main_interrupt
```

Frontend:

```text
cd crates/needle-app/web
npm test
npm run lint
npm run build
npm run test:e2e:local
```

Plugin:

```text
cargo run --locked -p needle-app -- plugin validate
cargo run --locked -p needle-app -- plugin validate --benchmark
```

Report exact commands and observed outcomes. State clearly what was not run.

## Pull requests

Use the repository pull-request template. A complete pull request summarizes the
change, implementation, validation, risks or limitations, documentation or
evidence, and any unverified boundaries.

The template requires a short AI-assistance disclosure:

```text
AI assistance: none | investigation | code | tests | documentation
Human verification: <what was manually reviewed and how correctness was established>
```

The disclosure does not reduce contributor responsibility. The author remains
accountable for every line and every claim.

## Licensing

Contributions are accepted under the project's [Apache License 2.0](LICENSE.md).
The repository currently requires neither a CLA nor a DCO sign-off.
