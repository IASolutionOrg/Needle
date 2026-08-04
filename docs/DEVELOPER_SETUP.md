# Developer setup

This guide builds and inspects Needle locally without starting a paid provider
run. It is a development workflow, not a supported installation path.

## Requirements

- Rust `1.90.0` with `rustfmt` and `clippy`;
- Codex `0.144.0` for the currently validated App Server adapter;
- Node.js `22.22.0` or newer and npm for building the embedded frontend assets.
  Node.js is not needed at runtime.

```text
rustup toolchain install 1.90.0 --component rustfmt --component clippy
```

Use the native Codex executable. On Windows this means `codex.exe`, not
`codex.cmd`, `cmd.exe`, PowerShell, or another launcher script. Unsupported
versions or missing App Server capabilities fail preflight.

## Build

```text
cd crates/needle-app/web
npm ci
npm run build
cd ../../..
cargo build --locked --workspace
cargo test --locked --workspace
```

The package is `needle-app`; the binary is `needle`. Workspace packages remain
`publish = false`. The generated `web/dist` assets are embedded in the binary;
Node.js is used for this build step, not when the binary runs.

## Create a development profile

Use an explicit data directory so the profile is isolated and disposable:

```text
cargo run --locked -p needle-app -- init \
  --data-dir .needle-data \
  --codex <path-to-native-codex> \
  --worker-model <worker-model> \
  --worker-reasoning medium \
  --evidence-failure-policy discard_invalid_fact
```

Initialization records the native launcher, worker profile, timeout, evidence
failure policy, trusted-test setting, default routes, plans, and model policy in
SQLite. `repair_once` permits at most one bounded repair in the same temporary
worker thread.

Trusted test execution is opt-in:

```text
cargo run --locked -p needle-app -- init \
  --data-dir .needle-data \
  --codex <path-to-native-codex> \
  --worker-model <worker-model> \
  --worker-reasoning medium \
  --trust-test-execution
```

Trust does not authorize arbitrary commands. The approval broker still checks
the exact plan, command representation, repository, cwd, network, writes, and
execution budget.

## Inspect the profile

```text
cargo run --locked -p needle-app -- doctor --data-dir .needle-data
cargo run --locked -p needle-app -- route list --data-dir .needle-data
cargo run --locked -p needle-app -- route show trace.state-flow --data-dir .needle-data
```

`doctor` checks database readiness, Codex compatibility, configuration,
repository state, and pending cleanup. It does not start a paid run.

## Start the resident runtime

```text
cargo run --locked -p needle-app -- serve \
  --data-dir .needle-data \
  --repository <repository-root>
```

The process acquires one profile lock, starts local IPC, binds a random
loopback HTTP port, and prints a URL with a one-use launch token. The web assets
are embedded in the binary; Node.js is not used at runtime.

The supplied repository becomes the active source boundary for review and
explicit patch application. A worker never receives it as a writable root.

## Start the MCP server

Begin with cache-only behavior:

```text
cargo run --locked -p needle-app -- mcp serve \
  --data-dir <profile-data-directory> \
  --repository <repository-root> \
  --main-model <main-model> \
  --role-profile <active-role-profile-id> \
  --cache-only
```

The `--cache-only` option disables worker fallback and change tools. Omit it only in a
deliberately configured development profile. See [MCP transport](MCP_TRANSPORT.md).
The role-profile selector is mandatory and freezes the selected active revision
for the MCP session.

## Validate plugins

```text
cargo run --locked -p needle-app -- plugin validate
cargo run --locked -p needle-app -- plugin validate --benchmark
```

Packaging is a development operation:

```text
cargo run --locked -p needle-app -- plugin package --output dist/needle-plugin
```

The product plugin provides lifecycle hooks. MCP is configured separately by
the client and never accepts hook marker text as tool input.

## Safe offline verification

```text
cargo test --locked -p needle-platform-codex --test offline_n1
cargo test --locked -p needle-platform-codex --test main_interrupt
cargo test --locked -p needle-app mcp::
cargo test --locked -p needle-app --test mcp_stdio
```

These tests use deterministic simulators and do not authorize provider calls.

## Development-only semantic marker

The lifecycle hook uses an exact unversioned marker:

```text
@@need
@route locate.implementation
@subject symbol:"example"
@require implementation-location selection=primary granularity=exact-location
@world source=current features=default

Locate the implementation.
@@end
```

The parser assigns its revision and the session freezes the grammar digest. Do
not add a visible protocol version.

## Next reading

- [Configuration](CONFIGURATION.md)
- [Architecture](ARCHITECTURE.md)
- [Security and approvals](SECURITY_AND_APPROVALS.md)
- [Development and troubleshooting](DEVELOPMENT_AND_TROUBLESHOOTING.md)
