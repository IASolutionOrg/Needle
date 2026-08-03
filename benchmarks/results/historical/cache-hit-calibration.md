# Historical full-cache calibration

| Field | Value |
|---|---|
| Evidence level | Historical live calibration |
| Date | 27 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Publish and repeat the `--glob-case-insensitive` state-flow request |
| Route | `trace.state-flow` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | Two main observations; one logical worker in publication |
| Automatic retries | None |

## Result

The publication request ran one worker and stored four route-plan artifacts.
The unchanged repeat returned a full `CompositeHit`, the same bounded result
digest, and zero workers. Both main runs used zero repository discovery and the
checkout remained unchanged.

| Metric | Publication miss | Full cache hit |
|---|---:|---:|
| Wall time | 111,021 ms | 20,246 ms |
| Main cost | 3.326200 credits | 2.633875 credits |
| Worker cost | 1.985520 credits | 0 |
| Total cost | 5.311720 credits | 2.633875 credits |

Observed cache reduction was **50.41%** and wall time reduction was **81.76%**.

## Limits and non-claims

The report validates the historical full-route cache behavior for one task.
The artifact model predates the current proof kernel, so this is not evidence
for current validator-derived coverage, certificates, or general savings.
