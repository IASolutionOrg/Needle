# Benchmark evidence

This directory contains frozen inputs and published evidence reports for Needle.
Reports are organized by current-path and historical calibration boundaries.

Needle is a pre-alpha project. Every retained live result is a bounded
calibration for one recorded setup, not a general or statistically supported
savings claim.

## Layout

```text
benchmarks/
  corpus/router-cache/   frozen tasks, oracles, campaign, and cost model
  fixtures/              structured protocol and worker fixtures
  results/historical/    accepted observations from an earlier product boundary
  results/live/          accepted provider-backed current-path calibration
  results/offline/       deterministic replay and local performance evidence
```

The experimental method, cost formulas, quality boundary, and paid-run policy
are documented in [Benchmarking](../docs/BENCHMARKING.md).

## Current live calibration

| Report | What it establishes | What it does not establish |
|---|---|---|
| [Routing and cache](results/live/routing-and-cache-calibration.md) | One routed miss plus authoritative zero-worker location hit with complete cost accounting | General savings or route coverage |
| [Structured MCP cache hit](results/live/structured-mcp-cache-hit.md) | One typed MCP call, authoritative composite hit, final response, zero worker, zero main discovery | MCP superiority or paired economics |
| [Partial and cross-route reuse](results/live/partial-and-cross-route-reuse.md) | Only the missing focused-test capability ran; the next route reused it with zero worker | Powered savings or other subjects |

## Offline current-path evidence

| Report | Scope |
|---|---|
| [Claim reuse performance](results/offline/claim-reuse-performance.md) | Claim authority, freshness isolation, mixed planning, and host-specific warm timings |
| [Multi-task quality replay](results/offline/multi-task-quality-replay.md) | Bounded semantic oracle and canonical scenario behavior |
| [End-to-end proof replay](results/offline/end-to-end-proof-replay.md) | Exact, coverage, composite, partial, mutation, and application-simulator closure |

## Historical calibration

These reports predate the current proof kernel. They remain valid records of
the observed routing/cache path at the time, but cannot validate current proof
authority.

- [Trace routing](results/historical/routing-trace-calibration.md)
- [Full cache hit](results/historical/cache-hit-calibration.md)
- [Mutation and partial cache](results/historical/cache-mutation-calibration.md)
- [Implementation-location routing](results/historical/routing-location-calibration.md)

## Evidence policy

A published report records:

- evidence level and date;
- repository and exact SHA;
- natural task and route;
- models, reasoning, Codex version, service tier, and pricing digest when live;
- provider observation and retry count;
- complete observed result and accounting;
- reported interpretation and limitations.

Infrastructure failures, code defects, aborted runs, and preflight-only reports
are excluded from published benchmark evidence and do not support benchmark
claims.

The evidence index records observations; it does not approve or schedule
provider runs.
