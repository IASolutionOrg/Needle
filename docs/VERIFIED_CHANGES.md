# Verified changes

Needle separates evidence reuse, patch preparation, verification, and active
worktree mutation. The workflow is implemented and offline validated, but has
no provider-backed patcher or verifier evidence.

## Workflow

```text
prepare_change
  -> disposable checkout
  -> isolated workspace-write patcher
  -> filesystem-derived PatchArtifact in SQLite

verify_change(change_id)
  -> fresh checkout from the same base snapshot
  -> deterministic patch materialization
  -> independent read-only verifier
  -> optional one-shot repair and new verification

Changes page
  -> human diff review
  -> explicit apply
  -> source recheck, journaled writes, post-snapshot record
```

## Preparation

The parent provides a task, acceptance criteria, allowed paths, optional
certified artifact/claim context, and constraints. Exact paths and subtrees are
supported.

The patcher has no network, credentials, project instructions, hooks, plugins,
external MCP, or multi-agent access. It may inspect its checkout and make
bounded changes only within declared scope.

Needle derives authority from the observed filesystem, not the model response.
It admits regular UTF-8 create, update, and delete operations and rejects path
escape, protected paths, binary data, symlinks, submodules, rename-like pairs,
and size/count overflow.

The patch artifact stores source snapshot, ordered manifest, before/after
digests and blobs, patch digest, declared output, and discrepancies. Patch reuse
is disabled.

## Verification

`verify_change` accepts only the opaque change ID. The task, criteria, patch,
context, and certified test plans are loaded from SQLite.

The verifier:

- receives a fresh checkout with the patch materialized;
- is read-only;
- does not receive the patcher transcript;
- may execute only already associated, trusted, certified tests;
- must cover every acceptance criterion;
- reports findings, evidence, and gaps.

Infrastructure failure or missing required evidence is `inconclusive`, never
`verified`.

## One repair

The first `repairable` verdict may reserve one repair transactionally. A new
patcher starts from the original base with the first patch and verifier findings.
The resulting second revision receives an independent verification.

No second repair, third revision, automatic model escalation, or concurrent
double-consumption of the repair allowance is permitted.

## Explicit apply

MCP deliberately has no apply tool. The local control plane requires:

1. review of the latest bounded diff;
2. latest revision verified;
3. explicit confirmation;
4. valid session cookie and CSRF;
5. exact `If-Match` change digest;
6. active source snapshot equal to the preparation base.

Apply operations are serialized and journaled before the first write. Failure
restores persisted before blobs and verifies the pre-apply snapshot. Drift,
replay, stale verification, symlink, path conflict, or recovery ambiguity
rejects the operation.

Needle does not merge, stage, commit, create a branch, push, open a pull request,
or publish.

## Limits

- at most 16 changed files;
- projected before/after data at most 512 KiB;
- final content at most 1 MiB;
- UTF-8 regular files only;
- no binary, symlink, submodule, or rename support;
- verifier automation supports a deterministic, serial set of up to four
  distinct associated certified test plans; exact duplicates collapse to one
  execution and over-cap or unavailable plans fail closed;
- no patch reuse or three-way merge.

## Evidence boundary

Deterministic tests cover active-worktree isolation, exact blobs, independent
verification, one repair, stale apply rejection, recovery, apply, and replay
rejection. This is offline evidence only. See
[Project status](../PROJECT_STATUS.md).
