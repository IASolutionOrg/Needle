# Install and use Needle

Needle is an opt-in companion for Codex. It runs locally, stays inactive until
you enable it, and does not require an MCP server for its default workflow.
The install and CLI are currently a pre-alpha interface without compatibility
guarantees.

## Install on Windows

Download the `needle-windows-x64` artifact produced by the
`package-windows` GitHub workflow, extract it, and run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The installer copies `needle.exe` and Needle's pinned Codex worker runtime to
`%LOCALAPPDATA%\Programs\Needle`, then adds that directory to the user `PATH`.
Users do not need to install Codex CLI separately. Open a new terminal after
installation.

## Enable Needle

Open PowerShell or Command Prompt inside a Git repository and run:

```text
needle enable
```

On the first enable, Needle verifies its managed Codex runtime, loads the models
available to the current Codex account, and shows an interactive model picker.
Use the arrow keys to move, Enter to confirm, or Escape to cancel.
Non-interactive setup can provide the model explicitly:

```text
needle enable --worker-model gpt-5.6-terra
```

Repository scope is the default. Use `--global` only when every repository
should inherit the activation:

```text
needle enable --global --worker-model gpt-5.6-terra
```

A repository-level enable or disable always overrides the global setting.
By default, `needle enable` prints a concise setup summary. Use `--json` only
when machine-readable output is required by a script or automation.

Needle configures each detected Codex client according to the integration that
client supports:

- **Codex Desktop:** Needle installs a managed personal skill at
  `~/.agents/skills/needle/SKILL.md`. The skill lets Desktop decide when focused
  exploration is useful and invoke Needle on demand. Restart Desktop after the
  first installation or a skill update.
- **Codex CLI:** Needle adds its lifecycle commands to the existing user
  `~/.codex/hooks.json` without replacing unrelated hooks. The CLI requires a
  one-time security review for new or changed non-managed hooks. Run `/hooks`
  inside Codex CLI, inspect the Needle commands, and trust them.

`/hooks` is not a Codex Desktop command. Needle never asks Desktop users to run
it. If Codex CLI is not installed, `needle enable` also removes obsolete Needle
hook entries created by earlier pre-alpha builds while preserving every
unrelated hook.

## Normal use

After activation and the client-specific setup:

1. Open Codex normally in the repository.
2. Write an ordinary request.
3. Codex decides whether repository exploration should be delegated. Desktop
   uses the Needle skill; Codex CLI uses the lifecycle hook protocol.
4. Needle resolves the internal request on demand and returns bounded context.
5. Codex completes the task.

Desktop allows at least 360 seconds for an exploration and waits for the same
Needle process to finish. A first uncached exploration can take more than one
minute. Needle writes an initial progress line and a heartbeat every 15 seconds
to standard error; the bounded context is written to standard output only when
the exploration completes. A heartbeat means the request is still running, not
that Codex should start a second process or fall back early.

The internal `@@need` marker belongs only to the Codex CLI hook protocol. Users
do not write it in normal prompts, and the Desktop skill does not expose it. No
resident resolver or MCP process is required; the resolver starts only when an
enabled task actually requests context.

The same bounded exploration entry point is available directly from a terminal:

```text
needle explore --route locate.implementation --subject-kind symbol --subject "activation_status"
needle explore --route trace.state-flow --subject-kind behavior --subject "enable"
needle explore --route tests.relevant --subject-kind behavior --subject "enable"
```

Run these commands from the target repository or pass
`--repository <absolute-path>`. The command requires Needle to be enabled for
that repository. The subject gives the semantic route one stable source-facing
repository concept to resolve; valid kinds are `symbol`, `cli-option`, `configuration-key`,
`test`, `file`, `module`, and `behavior`. Codex Desktop selects these internal
arguments automatically, so normal Desktop prompts remain unchanged.

When `--query` is omitted, as it is by the managed Desktop skill, Needle derives
a deterministic route-specific request from the structured route, subject kind,
and canonical subject. This makes separate Desktop tasks produce the same exact
cache identity without treating paraphrased free text as semantically
equivalent. A terminal user can still pass `--query <custom-request>`; custom
queries remain isolated by their exact normalized request and typed NeedIR
digests.

Repository search starts from the exact subject and prioritizes source files.
Broad searches skip generated directories, dependency lock files, source maps,
and minified JavaScript; explicit reads of a requested repository file remain
available. This keeps the worker focused without hiding files it deliberately
asks to inspect.

After a successful direct exploration, Needle can reuse the result only when
the canonical or custom request, complete typed NeedIR, repository snapshot,
active worker configuration, role profile, route definitions, and
direct-exploration contract are unchanged. The NeedIR binding includes subject
kind, canonical subject, constraints, obligations, world, inputs, and
projection. This narrow local reuse does not promote a route, does not enable
approximate semantic reuse, and does not weaken Needle's proof and utility
gates. Any relevant intent, source, or configuration change produces a new
worker run.

On completion, the command reports either `exact cache hit` or `cache miss` on
standard error, including elapsed time. The bounded context remains isolated on
standard output.

## Control Needle

```text
needle status
needle debug status
needle disable
needle enable
needle ui
```

`needle ui` opens the local control plane in the default browser. Its toggle
and the CLI update the same audited activation state and Codex Desktop skill
lifecycle. Opening the UI alone does not install or modify Codex hooks.
Disabling Needle removes only the managed personal Desktop skill, while keeping
settings, Codex CLI hooks, and cache data. Any unmanaged skill or sibling file
is preserved. Enabling Needle again reinstalls the managed skill, so no
onboarding data is lost. Restart Codex Desktop after either transition so new
tasks use the updated skill inventory.

The Desktop skill is personal rather than repository-local. Consequently, a
repository or global disable removes the managed Desktop skill for the current
Codex installation. Another repository that remains enabled in SQLite will
report that its Desktop integration is not ready until `needle enable` is run
there again.

### Worker debug logging

Enable bounded local diagnostics before reproducing a worker failure:

```text
needle debug enable
needle debug status
needle debug latest
needle debug disable
```

Debug mode persists across Desktop tasks until disabled. It stores the worker
contract, structured response, accepted and rejected artifacts, normalization
diagnostics, and final failure under the product data directory's `debug-logs`
folder. Logs may contain local repository paths and bounded source evidence,
retain only the latest 20 runs, and never trigger an additional provider call.
`needle debug disable` stops new logging but preserves existing logs.

The MCP server remains available for explicit integrations and experiments,
but it is not the default Codex onboarding path.
