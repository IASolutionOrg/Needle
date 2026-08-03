# Routing and cache live calibration

| Field | Value |
|---|---|
| Evidence level | Live calibration |
| Date | 30 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Locate the implementation of `--glob-case-insensitive` |
| Route | `locate.implementation` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | Three main observations; one logical worker in the miss arm |
| Automatic retries | None |

## Protocol

The frozen natural task and quality oracle were identical across frontier
direct, routed miss, and proof-backed hit arms. The miss could use at most one
worker; the hit had to remain cache-only.

## Result

All final responses completed and passed the quality oracle.

| Arm | Main cost | Worker cost | Total cost | Workers | Main discovery |
|---|---:|---:|---:|---:|---:|
| `MainOnly` | 3.267775 | 0 | 3.267775 credits | 0 | 3 |
| `NeedleMiss` | 0.349300 | 0.486690 | 0.835990 credits | 1 | 0 |
| `NeedleHit` | 0.357425 | 0 | 0.357425 credits | 0 | 0 |

The miss published one validated implementation artifact. The hit reused the
same artifact through an authoritative `CoverageHit`, with validation and
sufficiency certificates, positive observed reuse economics, zero stale or
contradicted evidence, and `worker_avoided=true`.

Observed reductions for this sample:

- routing: **74.41%**;
- cache relative to miss: **57.24%**;
- end-to-end relative to direct: **89.06%**.

The source remained clean. Focused-test execution was not required by the task.

## Limits and non-claims

This validates the current live route → worker → validation → publication →
zero-worker reuse path for one pinned location task. It does not establish a
confidence interval, multi-task behavior, other routes, or general savings.
