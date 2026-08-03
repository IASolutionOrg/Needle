# MCP transport

Needle exposes three structured MCP tools: `need_context`, `prepare_change`,
and `verify_change`. The interface is pre-alpha and may change without
compatibility guarantees.

MCP and the lifecycle hook share only the compiled semantic domain model. MCP
never parses `@@need`, and hook marker text is not a valid tool argument.

## Start the server

```text
needle mcp serve \
  --data-dir <profile-data-directory> \
  --repository <repository-root> \
  --main-model <main-model> \
  --role-profile <active-role-profile-id> \
  --cache-only
```

The connection freezes repository, profile, enabled routes, semantic-definition
digest, main model, selected role-profile revision, and worker policy. The
role-profile selector is mandatory. `--cache-only` disables worker fallback
and change tools.

The stdio server accepts bounded JSON-RPC lines and requires `initialize`
before tool operations. It supports MCP `2025-06-18` and `2024-11-05`, rejects
duplicate request IDs, serializes stdout, and resolves one request at a time.

## `need_context`

Example:

```json
{
  "route": "trace.state-flow",
  "subject": {
    "kind": "cli_option",
    "name": "--crlf"
  },
  "required": [
    {
      "kind": "implementation_location",
      "polarity": "positive",
      "selection": "primary"
    },
    {
      "kind": "runtime_flow",
      "scenario": "default",
      "completeness": "contract_complete",
      "granularity": "stepwise"
    }
  ],
  "preferred": [
    {
      "kind": "focused_tests",
      "polarity": "positive",
      "selection": "representative",
      "completeness": "open_world"
    }
  ],
  "world": {
    "source": "current",
    "platform": "current",
    "features": "default"
  },
  "task": "Trace how --crlf changes matching and search line terminators."
}
```

Subject kinds are `symbol`, `cli_option`, `configuration_key`, `test`, `file`,
`module`, and `behavior`. The exact enabled-route enum is frozen at
initialization.

The input is closed at every level. Unknown fields, duplicate capability kinds,
required/preferred conflicts, incompatible facets, disabled routes, malformed
digests, empty subjects, and exceeded bounds return `-32602` before resolution.

The task wording does not define semantic identity. Route minimums are merged
by the compiler; omitted facets are not invented by the transport.

## Result

Modern MCP returns schema-conforming `structuredContent`:

```json
{
  "status": "hit",
  "route": "trace.state-flow",
  "subject": { "kind": "cli_option", "name": "--crlf" },
  "need_id": "b3:...",
  "step": { "ordinal": 1, "relation": "independent" },
  "satisfied": ["implementation_location", "runtime_flow"],
  "missing": [],
  "resolution": {
    "kind": "composite_hit",
    "artifact_ids": ["b3:..."],
    "certificate_id": "b3:...",
    "plan_id": "b3:..."
  },
  "reuse_unit": "artifact",
  "claim_ids": [],
  "cache_hit": true,
  "worker_spawned": false,
  "result_digest": "b3:...",
  "context": "bounded FrontierView text"
}
```

`content[0].text` equals `structuredContent.context`. The legacy negotiated
protocol uses the same input and text fallback. Session and interface metadata
stay in `_meta` and audit storage; transcripts, credentials, raw output, and
test logs are not returned.

Resolution is a tagged union for exact, coverage, composite, claim, partial,
miss, stale, rejected, ambiguous, contradicted, and bypass outcomes. Unsafe
states do not return reusable context.

## `prepare_change`

Input contains:

- task;
- one to eight acceptance criteria;
- one to sixteen exact or subtree path scopes;
- optional certified artifact and claim IDs;
- optional bounded constraints.

The patcher runs inside a disposable checkout and may write only declared UTF-8
paths. The response contains an opaque `change_id`, content-addressed
`patch_id`, changed operations, acceptance coverage, bounded accounting,
residual risks, and `verification_status=not_requested`. Full file blobs and
raw model output remain in SQLite.

## `verify_change`

Input is only:

```json
{"change_id":"chg_<24 hex>"}
```

Needle loads the immutable request and current patch from SQLite, materializes
a fresh checkout, and starts an independent read-only verifier. Verdicts are
`verified`, `rejected`, `repairable`, or `inconclusive`.

A first `repairable` result may consume one repair revision and one new
verification. There is no second repair, automatic escalation, or implicit
apply.

## Multiple calls

Calls on one connection share a bounded need ledger. Later calls are classified
as repeat, residual, extension, overlap, independent, or incompatible.

Defaults allow three needs and three workers, with a hard cap of eight. One
resolution is active; others wait FIFO. Fresh repeats can reproject with zero
worker. Residual calls resolve only missing obligations. Limit exhaustion
returns a normal bypass.

Cancellation is bound to the JSON-RPC request ID and interrupts the active App
Server turn and process tree before recording cleanup.

## Offline validation

```text
cargo test -p needle-app mcp::
cargo test -p needle-app --test mcp_stdio
cargo run -p needle-app -- experiment mcp-contract-microbench \
  --mcp-request-file benchmarks/fixtures/mcp-request.json
```

These commands do not authorize provider execution. See
[Benchmarking](BENCHMARKING.md) and [Project status](../PROJECT_STATUS.md).
