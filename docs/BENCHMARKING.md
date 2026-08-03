# Benchmarking

Needle benchmarks completed tasks and total observed cost. Transport wiring,
prompt length, cache lookup alone, or a worker process alone is not a product
result.

Accepted reports live in the [evidence index](../benchmarks/README.md). Failed
or diagnostic attempts are excluded from that public archive.

## Product comparison

Needle is primarily an economic task router. Proof-gated cache reuse is the
second multiplier.

| Arm | Execution |
|---|---|
| `MainOnly` | Frontier model completes the task and performs discovery |
| `NeedleMiss` | Same frontier model delegates bounded work to lower-cost workers |
| `NeedleHit` | Needle returns validated cached evidence and starts zero workers |

Primary ratios:

```text
routing saving = 1 - NeedleMiss / MainOnly
cache saving   = 1 - NeedleHit / NeedleMiss
total saving   = 1 - NeedleHit / MainOnly
```

Costs follow the path actually executed. `NeedleMiss` includes every main turn,
worker, repair, escalation, and fallback. `NeedleHit` includes every main turn,
continuation, proof overhead, and projection. If an intended hit starts a
worker, it is not a hit observation.

## Paired-task contract

Economic arms must share:

- natural user objective;
- repository and pinned SHA;
- subject, world, obligations, and completeness;
- main model, reasoning, service tier, and pricing snapshot;
- independent hidden quality oracle.

Wording may differ only when the compiled semantic need remains equivalent.
Narrower, wider, residual, or mutation requests are separate cache-behavior
experiments.

## Quality boundary

Quality asks whether the final answer satisfies the natural task without
material error or stale evidence. It does not require details the user did not
request.

Files, symbols, flow steps, and tests become mandatory only when the task or
typed contract requires them. Focused-test identification and test execution
are separate. Test execution is not a universal product gate.

Any quality failure blocks promotion for that observation. A corrected oracle
can validate current evaluator behavior offline, but it cannot retroactively
turn an invalid economic pair into accepted savings evidence.

## Cache correctness

Cache behavior is tested independently from production economics:

- equivalent need resolves from eligible evidence;
- full hit starts zero workers;
- irrelevant mutation preserves independent evidence;
- relevant mutation invalidates only dependent evidence;
- partial hit computes only missing obligations;
- stale, contradicted, ambiguous, or unknown evidence is not served;
- main and worker do not repeat certified discovery.

An offline forced-plan replay can validate correctness. It cannot establish
provider economics. Production mode separately requires positive expected net
reuse value.

## Evidence levels

### Offline validated

Deterministic source, simulator, replay, mutation, or performance evidence with
zero provider calls. Synthetic token or cost values validate accounting only.

### Live calibration

One bounded provider-backed observation for a frozen task, model, tool version,
and pricing snapshot. It validates that path only and is not a general claim.

### Statistically validated

A pre-registered multi-task study with frozen calibration/holdout split,
independent oracles, paired execution, justified sample size, and confidence
interval. Needle has no such result yet.

Transport-only evidence is reported as functional transport calibration, not
product savings.

## Accounting

Record separately:

- main input, cached input, output, and reasoning usage;
- every worker, repair, steer, escalation, and fallback;
- proof lookup, validation, planning, projection, and continuation overhead;
- wall time and failure stage;
- pricing snapshot digest and calculated cost.

Thread usage events may be cumulative snapshots and must not be summed twice.
Interrupted or failed turns retain observed usage. Calculated credits are based
on the repository pricing snapshot and are not provider invoices.

## Accepted report metadata

Every retained report states:

- evidence level and date;
- purpose and task;
- public repository and exact SHA;
- route and semantic request;
- main/worker models, reasoning, Codex version, and tier;
- pricing digest;
- provider observation and retry count;
- exact observed outcome;
- source-integrity and discovery checks;
- interpretation and explicit non-claims.

Reports describe accepted evidence and explicit boundaries. Implementation
failures are excluded from accepted benchmark results.

## Current accepted observations

- One current routing/cache calibration covers a live miss and authoritative
  zero-worker location hit.
- One structured-MCP live calibration covers a three-artifact composite hit,
  final response, and zero main discovery.
- One partial-reuse live calibration computes only focused tests and then
  reuses them across routes with zero second worker.
- Claim reuse, multi-task proof closure, and performance have deterministic
  offline evidence.
- Earlier routing, cache, and mutation reports remain historical calibration
  and do not validate the current proof kernel.

No powered corpus or publishable general savings interval exists.

## Paid-run discipline

Before any provider run:

1. freeze task, SHA, request, oracle, models, pricing, and retry policy;
2. run deterministic simulator and current native preflight;
3. verify source integrity and cleanup;
4. produce a complete cost estimate;
5. obtain explicit human approval for that exact stage;
6. execute once unless the approved protocol explicitly permits repetition.

Preflight and an estimate do not authorize execution. Failures do not authorize
automatic retry.

## Final statistical gate

A future general claim requires:

- calibration and holdout tasks frozen before final runs;
- at least both built-in discovery routes;
- powered one-sided paired design;
- 95% BCa bootstrap interval for paired cost ratio entirely below `1.0`;
- no quality failure or stale hit;
- complete main and worker accounting;
- claim limited to measured workloads, models, versions, and pricing.

See [Project status](../PROJECT_STATUS.md) for remaining release work.
