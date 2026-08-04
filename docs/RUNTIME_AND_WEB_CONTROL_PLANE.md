# Runtime and web control plane

`needle serve` keeps routing state, SQLite, approvals, proof caches, IPC, HTTP,
and the embedded web application resident in one profile-scoped process.

## Startup

```text
needle serve --data-dir <profile> --repository <repository-root>
```

Startup:

1. resolves and validates the profile and repository;
2. acquires a single-instance profile lock;
3. initializes and checks SQLite migrations;
4. recovers pending cleanup and apply journals;
5. starts named-pipe or Unix-socket IPC;
6. binds a random loopback HTTP port;
7. prints a URL containing a one-use launch token.

A second process cannot own the same profile.

## IPC

- Windows: profile-specific named pipe.
- Linux and macOS: profile-specific Unix socket.

The descriptor binds profile identity, process, endpoint, and health nonce.
Clients perform a health handshake before sending bounded requests.

## MCP process

`needle mcp serve` is a separate stdio transport over the same runtime domain,
SQLite schema, resolver, freshness rules, and worker boundary. It fixes the
repository, profile, route snapshot, semantic-definition digest, model, and
worker policy for the connection.

See [MCP transport](MCP_TRANSPORT.md).

## HTTP security

- loopback-only random port;
- validated `Host` and `Origin`;
- one-use launch token exchanged for an HttpOnly, SameSite=Strict cookie;
- CSRF token for mutations;
- restrictive CSP;
- no wildcard CORS;
- no credentials in frontend state, API payloads, logs, or export;
- digest-bound optimistic concurrency.

This is a local development control plane, not a remotely hosted service.

## Pages

- Overview;
- Platforms;
- Agents;
- Routes and Plans;
- Needs and Subjects;
- Contracts, Proofs, and Capability Promotion;
- Artifacts, Claims, and Cache;
- Runs and Approval Inbox;
- Changes;
- Models;
- Codex Role Profiles (under Models);
- Experiments;
- Settings.

The frontend uses React, TypeScript, Vite, React Router, TanStack Query,
Tailwind, shadcn/ui patterns, and Recharts. Node.js 22.22.0 or newer and npm are
required to build the embedded assets; the running Rust binary does not invoke
Node.js.

## API groups

### State and configuration

- `GET /api/v1/control-plane`;
- route, settings, and model-policy mutations with `If-Match`;
- bounded role-profile list/detail/history/audit routes:
  `GET /api/v1/role-profiles`,
  `GET /api/v1/role-profiles/{id}`,
  `GET /api/v1/role-profiles/{id}/revisions`, and
  `GET /api/v1/role-profiles/{id}/audit`;
- request-time role-profile preflight and digest-bound draft/activation lifecycle:
  `POST /api/v1/role-profiles/{id}/preflight`, `/draft`, `/activate`, and
  `/deactivate`;
- cache inspection and invalidation;
- run, artifact, usage, and promotion views.

Role-profile responses use `needle.role-profiles/1` and hard list/history/audit
limits of 100 (smaller defaults). Mutation errors use
`needle.role-profile-error/1` with machine-readable `code` and `message`.
Preflight projects the canonical definition to a `WorkerProfile` digest without
persisting preflight state. Draft, activation, and deactivation require the
same-session CSRF boundary and an exact quoted `If-Match` state digest; 428
means missing and 412 means stale. Activation additionally requires explicit
confirmation of the selected revision and definition digest. The envelope is
Codex configuration-only; non-Codex hosts are reported unavailable.

### Approvals

- `GET /api/v1/approvals?status=pending`;
- `POST /api/v1/approvals/{id}/decision`;
- `GET /api/v1/approvals/events`.

Decisions are bound to the exact approval ID and payload digest.

### Semantic state

- `GET /api/v1/needs` and `/needs/{id}`;
- `GET /api/v1/need-steps` and `/need-steps/{id}`;
- `GET /api/v1/subjects`;
- `GET /api/v1/contracts`;
- `GET /api/v1/plans/{id}`;
- `GET /api/v1/proofs/{id}` and proof replay;
- `GET /api/v1/capabilities` and digest-bound mode mutation;
- claim and claim-proof inspection.

### Changes

- `GET /api/v1/changes`;
- `GET /api/v1/changes/{id}`;
- `GET /api/v1/changes/{id}/diff`;
- `POST /api/v1/changes/{id}/apply`.

Full worker transcripts and persisted file blobs are never returned as general
control-plane state.

## Events

Approval SSE reports new, resolved, and timed-out requests. Need-step SSE
reports requested, queued, resolving, resolved, delivered, fallback, and
cancelled transitions. SQLite remains authoritative; clients refetch state
after an event.

## Optimistic concurrency

Mutable definitions and apply require:

```text
If-Match: "<current-digest>"
```

Missing headers return `428`; stale digests return `412`. Active sessions retain
their original route, prompt, transport, semantic definition, and policy
snapshots.

## Operational data

The UI exposes bounded definitions, identities, dependencies, proof decisions,
worker counts, avoided workers, usage, cost, timing, allocation samples,
approvals, change attempts, verification, and apply state. It does not expose
credentials, unbounded logs, or raw worker transcripts.

## Shutdown and recovery

Shutdown closes HTTP and IPC, removes the runtime descriptor, releases the
profile lock, and cancels active tasks. The next startup rechecks migrations,
pending worker cleanup, and interrupted apply journals before accepting work.

## See also

- [Developer setup](DEVELOPER_SETUP.md)
- [Configuration](CONFIGURATION.md)
- [Security and approvals](SECURITY_AND_APPROVALS.md)
- [Verified changes](VERIFIED_CHANGES.md)
