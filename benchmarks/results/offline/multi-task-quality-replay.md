# Multi-task quality-oracle replay

| Field | Value |
|---|---|
| Evidence level | Offline validated |
| Date | 30 July 2026 |
| Repository / commit | ripgrep `14.1.1` / `4649aa9700619f94cf9c66876e9549d83420e16c` |
| Task | Validate the CRLF semantic oracle and canonical default scenario |
| Route | `trace.state-flow` |
| Models | None; saved-response and deterministic runtime replay |
| Codex / tier | Not applicable |
| Pricing digest | Not applicable |
| Provider calls | Zero |
| Automatic retries | None |

## Purpose

The replay verifies that the task oracle evaluates observable CRLF behavior
rather than requiring one implementation-specific wording. It also verifies a
bounded canonical default scenario and explicit focused-test alternatives.

## Result

Two saved final responses were evaluated without model execution. Both passed
the corrected task-level oracle. The focused `line_terminator_crlf` command had
already been verified against the pinned source; saved-text replay itself did
not execute tests.

Focused runtime coverage confirms:

- a bounded description whose first complete scenario token is `default`
  derives the canonical default facet;
- an unrelated scenario such as error recovery remains insufficient;
- only explicitly declared focused-test alternatives pass.

## Limits and non-claims

This establishes evaluator and canonical-scenario behavior offline. It does not
produce provider economics, retroactive savings, or a statistically registered
trace comparison.
