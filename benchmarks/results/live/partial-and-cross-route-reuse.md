# Partial and cross-route reuse live calibration

| Field | Value |
|---|---|
| Evidence level | Live functional calibration |
| Date | 1 August 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Resolve the missing focused test for `--crlf`, then reuse it cross-route |
| Route | `trace.state-flow`, then `tests.relevant` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | One main sequence plus one logical worker |
| Automatic retries | None |

## Protocol

The store began with certified implementation-location and runtime-flow
artifacts. The first request additionally required focused tests. The second
request asked only for relevant tests for the same exact subject.

Test execution was not mandatory. A worker-discovered focused plan had to pass
static command, subject, identifier, target, evidence-path, and dependency
validation before publication.

## Result

- first request: operational `PartialHit`;
- seeded artifacts reused: `2`;
- missing capability: `FocusedTests` only;
- logical workers: `1`;
- second request: authoritative `CoverageHit`;
- second-request workers: `0`;
- main repository discovery: `0`;
- final answer: present;
- source snapshot: unchanged.

Observed main cost was **2.895175 credits** and worker cost was **0.987680
credits**, for **3.882855 credits** total.

## Limits and non-claims

This validates live obligation-level partial reuse, worker publication of a
statically located test plan, and zero-worker cross-route reuse for one subject.
It is not a powered benchmark, a main-only comparison, or a general savings
claim.
