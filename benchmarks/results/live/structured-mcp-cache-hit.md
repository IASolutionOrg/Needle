# Structured MCP cache-hit live calibration

| Field | Value |
|---|---|
| Evidence level | Live functional calibration |
| Date | 31 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Trace how `--crlf` changes matching and search line terminators |
| Route | `trace.state-flow` |
| Models | Main `gpt-5.6-sol`, medium reasoning; no worker |
| Codex / tier | `0.144.0` / `default` |
| Pricing digest | `b3:eca7f25b4a34dcf0f177601be1675bb97c16f24347d998dc9a2899e10b73cee9` |
| Provider calls | One main observation; zero workers |
| Automatic retries | None |

## Protocol

The structured request in `benchmarks/fixtures/mcp-request.json` was frozen by
canonical digest. The source snapshot and warm certified SQLite store were
fixed. Worker execution was disabled.

## Result

The main called `needle/need_context` before repository discovery. Needle
returned the expected three-artifact authoritative `CompositeHit`, sufficiency
certificate, selected plan, and bounded context.

- request digest matched;
- worker count: `0`;
- main discovery: `0`;
- final answer: present;
- source snapshot: unchanged;
- wall time: `24,466 ms`.

Reported usage was 46,183 input tokens, including 29,952 cached, and 737 output
tokens. The pricing snapshot calculates **2.956025 credits**.

## Limits and non-claims

This validates one live structured-MCP call and proof-backed composite cache
delivery. It has no main-only counterfactual and does not establish MCP
superiority, transport economics, general quality, or statistical savings.
