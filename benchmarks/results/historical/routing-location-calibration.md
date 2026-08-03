# Historical implementation-location routing calibration

| Field | Value |
|---|---|
| Evidence level | Historical live calibration |
| Date | 28 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Locate how `--glob-case-insensitive` is implemented |
| Route | `locate.implementation` |
| Models | Main `gpt-5.6-sol`; worker `gpt-5.6-luna`; medium reasoning |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | Two main observations; one logical worker in the Needle arm |
| Automatic retries | None |

## Result

Both arms completed with correct user-facing location answers. The routed arm
used one worker, zero main discovery, no repair, and one approved focused Cargo
test that executed successfully in the isolated checkout.

| Metric | Frontier direct | Needle miss |
|---|---:|---:|
| Wall time | 103,735 ms | 65,486 ms |
| Main cost | 9.966550 credits | 2.460750 credits |
| Worker cost | 0 | 1.086970 credits |
| Total cost | 9.966550 credits | 3.547720 credits |
| Main discovery | 10 | 0 |

Observed total reduction was **64.40%** and wall time reduction was **36.87%**.

## Limits and non-claims

This validates historical miss routing and continuation for one location task.
The route was not admitted for current proof reuse, so the report is not a
cache-hit or current authority observation.
