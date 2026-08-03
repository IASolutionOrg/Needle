# Claim reuse offline authority and performance

| Field | Value |
|---|---|
| Evidence level | Offline validated |
| Date | 2 August 2026 |
| Repository / commit | Deterministic fixtures plus ripgrep `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Validate location-claim authority, freshness, mixed planning, and warm latency |
| Route | `locate.implementation` plus mixed resolver fixtures |
| Models | None; deterministic offline execution |
| Codex / tier | Not applicable |
| Pricing digest | Not applicable |
| Provider calls | Zero |
| Automatic retries | None |
| Host / build | Windows AMD64, 24 logical processors, Rust `1.90.0` release |

## Result

### Scope

The fixture validates authoritative exact-primary `ImplementationLocation`
claim reuse, mixed artifact/claim composition, and claim-aware partial planning.
It makes the origin artifact stale through a supporting-file mutation while
keeping the selected primary claim dependency fresh. Stale supporting facts do
not enter the final projection.

### Warm measurements

Two already-compiled verification passes retained 50 samples per resolver
measurement.

| Measurement | Median range | p95 range |
|---|---:|---:|
| Coverage hit | 2.285–2.400 ms | 3.336–3.388 ms |
| Exact hit | 2.320–2.397 ms | 3.653–3.777 ms |
| Partial scheduling | 2.935–3.066 ms | 4.073–4.080 ms |
| Composite scheduling | 2.878–3.053 ms | 4.158–4.584 ms |
| Claim hit | 4.213–4.266 ms | 5.246–5.360 ms |
| Claim partial scheduling | 4.193–4.462 ms | 6.038–6.315 ms |
| Claim composite scheduling | 4.097–4.299 ms | 4.993–5.877 ms |

Marker parsing, canonical hashing, warm validity planning, and warm proof replay
retained deterministic zero-allocation gates. End-to-end SQLite lookup and
projection are bounded but not zero-allocation claims.

### Verification

- eight focused claim tests passed;
- workspace snapshot: 343 passed, one ignored;
- authoritative claim hit used zero workers;
- stale supporting projection was absent;
- no paid retry or provider call occurred.

## Limits and non-claims

These are local host-specific performance and correctness measurements.
Provider-backed claim reuse, other claim-class authority, and token or cost
savings remain unvalidated.
