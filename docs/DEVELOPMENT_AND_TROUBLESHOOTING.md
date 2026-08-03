# Development and troubleshooting

This guide covers current development workflows, reproducible checks, and
failure diagnosis.

## Frontend assets

The Rust binary embeds the frontend build. On a clean clone, build those
assets before the workspace commands:

```text
cd crates/needle-app/web
npm ci
npm run build
cd ../../..
```

## Workspace checks

Start focused, then expand once at the relevant boundary:

```text
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --workspace
```

Focused Codex adapter gates:

```text
cargo test --locked -p needle-platform-codex --test offline_n1
cargo test --locked -p needle-platform-codex --test main_interrupt
```

Proof performance:

```text
cargo run --locked --release -p needle-bench --bin proof-microbench
```

## Frontend

```text
cd crates/needle-app/web
npm ci
npm test
npm run lint
npm run build
npm run test:e2e:local
```

The production binary embeds the built frontend. Do not commit `dist`,
`node_modules`, or coverage output.

## Plugin and MCP

```text
cargo run --locked -p needle-app -- plugin validate
cargo run --locked -p needle-app -- plugin validate --benchmark
cargo test --locked -p needle-app mcp::
cargo test --locked -p needle-app --test mcp_stdio
```

## Codex compatibility

Use a native executable and generate the experimental App Server schema from
the exact supported Codex version:

```text
codex app-server generate-json-schema --experimental
```

Do not assume schemas are compatible across releases. Update the checked
fixture and focused compatibility tests together.

## Troubleshooting

### Native launcher rejected

**Symptom:** initialization rejects `.cmd`, `.bat`, `.ps1`, or a shell.

**Action:** point `--codex` to the native platform executable. Shell support in
command approval is a separate worker-test boundary and does not make a shell a
valid Codex launcher.

### Unsupported App Server schema

**Symptom:** preflight reports a missing method, field, or approval shape.

**Action:** verify the exact Codex version, regenerate its experimental schema,
and compare the compatibility fixture. Do not weaken validation globally.

### Approval remains pending

**Symptom:** a worker waits for a test or read command.

**Action:** inspect Approval Inbox. Confirm trusted repository, exact payload,
cwd, permissions, command family, and expiry. Unknown commands require a manual
decision and time out as decline.

### Safe read is declined

**Symptom:** a read-only sandbox command is not auto-approved.

**Action:** distinguish sandbox permission from evidence admission. The command
must normalize to a supported bounded read with identical displayed/action
argv, confined paths, no chaining, no redirection, and no environment mutation.

### Cargo test is unavailable

**Symptom:** worker starts but the planned test cannot run.

**Action:** inspect toolchain preflight, target directory, runner availability,
and Windows MSVC/SDK discovery. Optional test execution should downgrade to
located evidence or decline the command, not block unrelated read-only work.

### Windows linker or `mt.exe` failure

**Symptom:** Cargo reaches linking but cannot find MSVC tools.

**Action:** verify the discovered Visual Studio toolchain and Windows SDK from
the exact process. Restart the process after installing tools. Do not inherit
the full user environment or weaken containment.

### Route returns `BYPASS`

Inspect:

- enabled route and immutable session snapshot;
- exact subject and semantic world;
- mandatory residual;
- source-snapshot reproduction;
- dependency freshness and closure;
- contradiction or ambiguity;
- cache-only worker policy;
- need/worker limits.

`BYPASS` is the safe result when authority cannot be established.

### Valid proof remains advisory

Confirm capability mode, evidence digest, fresh and reuse cost observations,
pricing compatibility, proof overhead, and positive net reuse value. Do not
promote merely to make a demonstration report a hit.

### Marker is rejected

The opening line is exactly `@@need`. Verify required headers, bounds, quoting,
subject kind, route, and terminal `@@end`. A session grammar digest mismatch
must bypass rather than reinterpret the marker.

### Main continues discovery during pending work

Explicit continue-working permits main tools but records discovery taint. The
task can complete, but the run cannot claim zero main discovery for an
unprovable perimeter.

### App Server turn fails

Inspect the structured turn error, partial usage, last item, approval state,
process exit, cancellation, and cleanup. Do not collapse a transport failure
into a quality failure or automatically retry a paid run.

### Disposable checkout is invalid

Verify canonical source path, Git worktree spelling, active HEAD, tracked/index
overlay, untracked copy, final snapshot digest, and cleanup journal. Snapshot
mismatch must fail closed.

### Runtime refuses to start

Check profile lock, stale descriptor, health handshake, database migration
checksum, pending cleanup, loopback bind, and repository canonicalization.

### HTTP returns 401, 403, 412, or 428

- `401`: missing/invalid launch session;
- `403`: Host, Origin, or CSRF failure;
- `412`: stale `If-Match` digest;
- `428`: required `If-Match` missing.

Refetch authoritative state rather than replaying a stale mutation.

## Documentation maintenance

When public behavior changes:

1. update the owning guide;
2. update `PROJECT_STATUS.md` only when maturity or evidence changes;
3. keep README as a summary;
4. keep accepted benchmark reports focused on accepted observations and
   explicit boundaries;
5. audit relative links and documented commands;
6. state unverified boundaries exactly.

## See also

- [Developer setup](DEVELOPER_SETUP.md)
- [Architecture](ARCHITECTURE.md)
- [Security and approvals](SECURITY_AND_APPROVALS.md)
- [Project status](../PROJECT_STATUS.md)
