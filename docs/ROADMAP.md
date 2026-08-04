# Product roadmap

This roadmap describes planned product direction. It is not evidence that a
capability is implemented or supported. [Project status](../PROJECT_STATUS.md)
remains the authority for current behavior, validation, and limitations.

## Direction

Needle will separate four concepts:

- a **role profile**, such as explorer, implementer, test runner, reviewer,
  verifier, or auditor;
- an **agent host**, such as Codex, Claude Code, Cursor, OpenCode, or
  Antigravity;
- a **provider and model** selected through that host;
- a bounded **orchestration policy** owned by the parent runtime.

Execution and orchestration development is Codex-first. Support for other agent
hosts begins as configuration interoperability only. A host appearing in a
configuration screen, import, export, or projection does not mean that Needle
can launch, supervise, or orchestrate it.

Needle-owned role profiles will be the canonical configuration. Host-specific
configuration is derived explicitly and remains reviewable. Needle will not
silently rewrite global host configuration or store provider credentials.

## Milestone 1: Codex role profiles in the control plane

Extend the existing Codex model policy and runtime settings into named,
digest-bound role profiles managed through the local web application.

Planned profile fields include:

- role and immutable definition identity;
- model, reasoning, service tier, timeout, and bounded budget;
- prompt preset and structured output contract;
- tool, command, filesystem, network, and test-execution policy;
- repair, fallback, concurrency, and route assignment;
- draft, activation, revision history, preflight status, and audit metadata.

Credentials remain outside SQLite and configuration exports. Active sessions
retain the profile revision with which they started.

## Milestone 2: Codex development lifecycle orchestration

Implement and validate the complete bounded lifecycle on Codex before making
execution portable to other hosts:

```text
explore -> implement -> test -> review -> verify -> apply
```

The parent owns the plan, transitions, budgets, approvals, and final apply.
Workers do not spawn workers. Evidence, patches, test results, review findings,
and verification are exchanged as bounded typed artifacts rather than raw
transcripts. Write-capable roles remain confined to disposable checkouts, and
active-worktree mutation remains an explicit parent-owned action.

The durable contract and SQLite journal for this sequence are implemented and
offline validated. They freeze active role-profile revisions, certified test
plans, the source snapshot, cumulative budget, one repair allowance, review and
verifier provenance, and approval against the exact verified state digest.
Codex process supervision and the lifecycle read/timeline UI remain separate
pending slices; the contract alone does not launch a lifecycle worker.

## Milestone 3: Other-host subagent configuration

Add configuration-only interoperability before any non-Codex orchestration.

The first targets are:

1. Claude Code;
2. Cursor;
3. OpenCode;
4. Antigravity and later hosts.

This milestone may import, export, validate, or project Needle role profiles
into supported host-specific configuration shapes. It does not launch those
hosts, execute a worker turn, claim sandbox parity, or enable cross-host
fallback. Every projected field must preserve its Needle definition identity,
and unsupported policy fields must fail explicitly rather than being silently
dropped.

## Milestone 4: Agent-host execution contract

After the Codex lifecycle is stable, define and validate the execution boundary
required by a second host. The contract must normalize:

- discovery, version checks, capability negotiation, and preflight;
- launch, structured output, continuation, cancellation, and cleanup;
- tool, command, approval, filesystem, and sandbox events;
- usage, pricing provenance, latency, and bounded error categories;
- adapter version, host capability, role, prompt, tool, sandbox, and output
  schema identity.

The existing runtime `WorkerExecutor` seam is not by itself proof that another
host satisfies this contract.

## Milestone 5: Multi-host orchestration

Enable orchestration through a non-Codex host only after that host passes the
offline conformance suite, isolation preflight, compatibility fixtures, and a
separately approved live validation.

Cross-host fallback and reuse remain disabled by default. They require explicit
policy plus evidence that identity, validation, security, and accounting remain
compatible. Configuration support from Milestone 3 is not sufficient.

## Milestone 6: Open adapter ecosystem

After at least two execution adapters are validated, stabilize an adapter SDK,
capability matrix, conformance suite, compatibility policy, and packaging model
for additional hosts. No stable adapter API is promised before that boundary.

## Relationship to release readiness

The evidence and release-readiness work in
[Project status](../PROJECT_STATUS.md) continues independently: provider-backed
claim calibration, live verified-change validation, a second live operating
system, a powered corpus, stable packaging, compatibility, and security review.
Planned host integrations do not weaken those gates or turn offline evidence
into a provider-backed claim.
