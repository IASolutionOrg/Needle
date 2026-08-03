# End-to-end proof and application replay

| Field | Value |
|---|---|
| Evidence level | Offline validated |
| Date | 30 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Replay exact, composite, partial, mutation, and application paths |
| Route | `locate.implementation`, `trace.state-flow`, and `tests.relevant` |
| Models | Scripted App Server simulator; no provider model |
| Codex / tier | Simulated compatibility path / not applicable |
| Pricing digest | Not applicable |
| Provider calls | Zero |
| Automatic retries | None |

## Result

### Proof and mutation cases

| Case | Result |
|---|---|
| Exact location request | `ExactHit` |
| Reworded equivalent location request | `CoverageHit` |
| Complete trace | `CompositeHit` |
| Tests cross-route | `CoverageHit` |
| Irrelevant mutation | `CompositeHit` preserved |
| Relevant mutation | `PartialHit`, one stale candidate |
| Restored source | `CompositeHit` restored |

The replay recorded seven true positives, zero false positives, three
validation certificates, and zero provider calls.

The authoritative runtime fixture separately verified that a locate miss
publishes location evidence, a trace request reuses it and requests only flow,
the next trace is a zero-worker composite hit, relevant mutation recomputes only
flow, and final source digest equals the initial digest.

### Application simulator

The two-task provider-capable simulator completed location and trace tasks with:

- one logical worker per miss;
- equal bounded oracle quality;
- three validator-derived certificates;
- zero main discovery in Needle arms;
- final responses present;
- complete synthetic accounting;
- clean disposable checkout;
- zero provider observations and zero retries.

### Integration snapshot

The recorded milestone snapshot passed workspace formatting, tests, Clippy,
build, Codex adapter simulations, frontend tests/typecheck/lint/build/local
end-to-end, plugin validation, and proof microbenchmark gates.

## Limits and non-claims

This is deterministic integration evidence for proof, mutation, and application
paths. Synthetic usage is not live economics and supports no confidence
interval or general savings claim.
