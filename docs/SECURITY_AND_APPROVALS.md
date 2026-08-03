# Security and approvals

Needle treats model output, source repositories, command requests, cached
evidence, patches, and the active worktree as separate trust domains.

## Worker roles

### Evidence worker

- Codex App Server with read-only sandbox;
- approval policy `on-request`;
- no network, web search, hooks, plugins, project instructions, external MCP,
  multi-agent execution, or credentials;
- bounded timeout, output, command count, and process tree;
- temporary thread and disposable checkout.

### Patch worker

- workspace-write only inside a disposable checkout;
- exact or subtree path scope declared by the parent;
- one file-change approval in that checkout;
- no active-worktree access;
- final patch derived from base/final filesystem comparison, not model claims.

### Verifier

- separate read-only checkout;
- no patcher transcript;
- immutable task, criteria, patch, and certified test context loaded from
  SQLite.

## Disposable source snapshot

Clean repositories use a detached worktree at the requested HEAD. Dirty
repositories start from the same HEAD, apply a binary-safe tracked/index
overlay, and copy non-ignored untracked files. The final digest must equal the
requested snapshot.

Failure to reproduce the snapshot yields `BYPASS`. The active worktree and
`.git` control data remain outside worker writes. Toolchain cache, target, temp,
and command output stay under the run root.

## Environment

Worker environments remove credentials, user home pointers, proxies as
authority, and unrelated process state. Only bounded toolchain variables are
projected. Network permission remains false.

On Windows, trusted Cargo execution may receive a discovered MSVC and Windows
SDK environment. If the exact optional toolchain is unavailable, test execution
is disabled for the session while read-only evidence work continues.

Needle emits no external telemetry.

## Approval binding

Every request records protocol identity, thread, turn, item, normalized argv,
cwd, reason, permissions, route, repository, expiry, classification, payload
digest, decision, and decision source.

The payload digest binds effective argv, cwd, and permissions. Changing any of
them invalidates the decision.

Supported decisions:

- `accept`: one execution;
- `decline`;
- `cancel`.

Session-wide approval and execution-policy amendment are unsupported.

## Test execution

A test is eligible for automatic approval only when:

- the profile trusts test execution for the repository;
- a validated `TestPlan` declares the exact command;
- the command normalizes to a supported direct or single-command wrapper;
- cwd and writes remain inside disposable locations;
- no network, chaining, redirection, glob expansion, or environment mutation
  is requested;
- execution stays within the worker budget.

The current test adapter recognizes direct Cargo tests and equivalent
runner-relative worker output after canonicalization. It does not use keyword
heuristics.

Declaring a test plan permits execution; it does not require execution.
Located and executed evidence remain distinct.

## Read-only inspection commands

Read-only sandboxing prevents filesystem mutation; command admission determines
whether an observed command can become trusted evidence.

The bounded inspection policy accepts only supported command families after
normalizing App Server action, displayed command, cwd, paths, shell wrapper,
and permissions. It rejects chaining, redirection, environment changes,
network-capable tools, path escape, and unbounded output.

Unrecognized read-only commands may still be shown in Approval Inbox. They do
not automatically become semantic evidence merely because the sandbox is
read-only.

## Change admission

Patch admission supports regular UTF-8 create, update, and delete operations
within declared scope. It rejects:

- more than 16 files;
- projected diff above 512 KiB;
- final content above 1 MiB;
- binary files, symlinks, submodules, and rename-like pairs;
- protected `.git`, `.needle`, `.codegraph`, build, cache, data, and run paths;
- model output inconsistent with the observed filesystem.

Patch reuse and automatic publication are disabled.

## Apply boundary

Only the local control plane can apply the latest verified patch. Apply
requires confirmation, session cookie, CSRF, exact `If-Match`, current revision,
and an active source snapshot identical to the base. Writes are journaled with
before blobs and rollback.

Source drift, replay, stale verification, path conflict, or recovery ambiguity
rejects the operation. Needle does not merge, stage, commit, push, or publish.

## Fail-closed behavior

- stale, contradicted, ambiguous, or unknown evidence is not served;
- checkout corruption or snapshot mismatch bypasses reuse;
- sandbox escape or unverifiable cleanup fails the task boundary;
- infrastructure test failure produces inconclusive evidence;
- cancellation interrupts App Server, terminates the process tree, and records
  cleanup;
- timed-out manual approval becomes decline.

## Current evidence

Isolation, approval binding, cancellation, cleanup, snapshot integrity,
command normalization, patch scope, verifier independence, apply drift, and
rollback have deterministic test coverage. This is not a third-party security
audit. See [Project status](../PROJECT_STATUS.md).
