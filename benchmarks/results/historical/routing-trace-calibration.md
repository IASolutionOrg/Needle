# Historical trace-routing calibration

| Field | Value |
|---|---|
| Evidence level | Historical live calibration |
| Date | 27 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Explain how `--glob-case-insensitive` works and where it is implemented |
| Route | `trace.state-flow` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | Two main observations; one logical worker with one repair |
| Automatic retries | None |

## Result

Both arms completed and passed the independent task evaluator.

| Metric | Frontier direct | Needle miss |
|---|---:|---:|
| Main discovery calls | 8 | 0 |
| Wall time | 70,847 ms | 109,515 ms |
| Main cost | 11.158850 credits | 2.841875 credits |
| Worker cost | 0 | 2.026445 credits |
| Total cost | 11.158850 credits | 4.868320 credits |

The routed arm used one logical worker and one bounded repair. Its focused
Cargo command executed one matching test successfully in the disposable
checkout. Main continuation produced the final answer and both source
checkouts remained clean.

Observed total reduction was **56.37%**; wall time increased **54.58%**.

## Limits and non-claims

This is evidence that the historical routing path reduced calculated cost for
one trace task while increasing latency. It predates current validation and
proof certificates and cannot establish current cache authority, mutation
safety, generalization, or statistical savings.
