# Historical mutation and partial-cache calibration

| Field | Value |
|---|---|
| Evidence level | Historical live calibration |
| Date | 28 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Reuse the `--glob-case-insensitive` flow across irrelevant and relevant mutations |
| Route | `trace.state-flow` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | Three main observations; two logical workers total |
| Automatic retries | None |

## Result

- Publication used one worker.
- An irrelevant untracked-file mutation preserved a zero-worker
  `CompositeHit` and the same result digest.
- A relevant source mutation produced `PartialHit`, reused the independent
  location artifact, and recomputed invalidated behavior, test, and projection
  nodes with one worker.
- No stale hit occurred; all main arms used zero repository discovery; the
  source was restored.

| Metric | Publication | Irrelevant mutation | Relevant mutation |
|---|---:|---:|---:|
| Total cost | 4.680630 | 2.813000 | 3.866985 credits |
| Wall time | 86,478 ms | 19,674 ms | 328,801 ms |
| Workers | 1 | 0 | 1 |

The irrelevant hit reduced cost **39.90%** and wall time **77.25%**. The
relevant partial path reduced cost **17.38%** but was slower than publication.

## Limits and non-claims

This is historical evidence for mutation classification and bounded
recomputation on one task. It is not proof of current claim-level invalidation,
latency improvement, or general cache economics.
