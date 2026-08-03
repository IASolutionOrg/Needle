# Artifacts and cache

Needle reuses typed, validated repository evidence. It does not cache arbitrary
model responses.

## Identity boundaries

### Semantic identity

Semantic identity describes what evidence means:

- exact repository-scoped subject;
- predicate and facets;
- semantic world;
- canonical payload;
- contract and validator definitions;
- input artifact identities when derived.

Model, reasoning, prompt, worker, transport, pricing, usage, and wall time do
not normally belong to semantic identity.

### Execution provenance

An execution attempt records how evidence was produced: worker profile, prompt
profile, App Server version, usage, cost, commands, timing, repair, and
fallback. Different attempts may produce the same semantic artifact.

### Operational cache state

Resolution state records whether evidence was selected, stale, rejected,
contradicted, bypassed, or recomputed. It is not part of artifact identity.

## Contracts

Current semantic outputs are:

- `ImplementationLocation`: primary and supporting exact code locations;
- `RuntimeFlow`: ordered producer, carrier, transformation, precedence, and
  consumer steps;
- `FocusedTests`: representative runner, argv, cwd, identifier, selection,
  and structural evidence.

Workers return typed proposals. Validators derive authoritative coverage from
payload structure and observed evidence. A worker cannot declare that it
satisfied an obligation.

## Validation certificate

An artifact validation certificate binds:

- contract and validator definition;
- canonical subject and semantic world;
- payload and artifact identity;
- input artifacts;
- exact dependency closure;
- validator-derived coverage;
- evidence and freshness requirements.

Changing the validator, contract, dependencies, or world invalidates the
certificate for authoritative reuse.

## Sufficiency certificate

A reuse sufficiency certificate proves that a selected plan satisfies the
current need. It contains required obligations, selected artifacts and claims,
satisfaction steps, freshness, world compatibility, contradiction state,
residual state, and proof-engine definition.

Proof replay invokes no model. `Unknown` is unsatisfied.

## Artifact-level reuse

Resolver order:

```text
exact request
  -> artifact coverage
  -> claim coverage
  -> mixed artifact/claim plan
  -> typed partial plan
  -> fresh worker
  -> native fallback or bypass
```

Primary outcomes:

| Resolution | Meaning |
|---|---|
| `ExactHit` | Exact artifact request identity reused |
| `CoverageHit` | One artifact satisfies an equivalent semantic need |
| `CompositeHit` | Several artifacts jointly satisfy the need |
| `PartialHit` | Valid evidence covers part of the need; only missing obligations run |
| `Miss` | No valid reusable plan exists |
| `Stale` | Candidate dependencies are no longer fresh |
| `Ambiguous` | Exact subject authority cannot be established |
| `Contradicted` | Active evidence conflicts |
| `Rejected` | Candidate failed validation or authority rules |
| `Bypass` | The proof resolver cannot safely decide or policy forbids compute |

Full authoritative hits require positive net reuse value. A valid but
economically unsupported plan remains advisory.

## Claim-level reuse

Validators can extract smaller trusted claims from an artifact:

- one location claim per exact location;
- one runtime-flow claim per typed step plus certified order relations;
- one focused-test claim per representative test identifier and command.

Claim identity is content-addressed from claim kind, contract definition, and
canonical payload. Origin artifact, route, worker, model, prompt, and pricing
remain provenance.

Claim certificates retain subject, world, freshness, origin, dependency
closure, and contradiction membership. A stale origin artifact does not
automatically make an independent claim fresh: only the claim's selected
dependency closure can do that.

Current authority is deliberately narrow: exact canonical positive primary
`ImplementationLocation`, contract-complete `RuntimeFlow`, and representative
open-world `FocusedTests` claims can produce `ClaimHit`, contribute to
`ClaimCompositeHit`, or seed a claim-aware `PartialHit` after explicit bounded
capability promotion. Built-in capability modes remain `Shadow` by default.
Runtime-flow and focused-test claim authority is covered by deterministic
offline validation only; no provider-backed claim-authority observation exists.

## Cache scopes

`SnapshotExact` is the safe fallback. It requires the exact captured source
snapshot.

`WorktreeSemantic` requires a validator-proven claim-to-dependency closure. An
unrepresentable search negative, evidence gap, ambiguity, or unknown dependency
downgrades scope or bypasses reuse.

## Partial recomputation

A partial hit may run only operators already declared by the route plan for
missing typed obligations. The worker receives:

- missing obligations;
- covered artifacts and claims as bounded input;
- a restricted output schema;
- an instruction not to repeat certified discovery.

Relevant mutation invalidates affected evidence and proofs. Irrelevant mutation
preserves independent evidence. The resolver never fills an unstructured
mandatory residual with guessed equivalence.

## Focused tests

Identifying a representative test and executing it are separate facts.
`FocusedTests` can be certified as `located` from trusted static evidence.
Execution is optional unless the contract explicitly requires it. If executed,
the exact approved command produces separate command evidence and may upgrade
the certificate to `executed`.

## Concurrency and failures

- Single-flight coalesces the same artifact request.
- Negative attempts are keyed narrowly to the exact attempt identity.
- Plans use bounded exact bitmask selection and deterministic tie-breaking.
- Hot immutable caches are content-addressed; SQLite remains authoritative.
- Memory and cold paths must make the same validity decision.

## Main projection

The main receives a bounded `FrontierView` containing selected evidence,
obligations satisfied and missing, resolution, proof identity, pending needs,
and a prohibition on repeating covered discovery. Worker transcripts, raw logs,
and full test output never enter the main context.

## Evidence and status

See [Project status](../PROJECT_STATUS.md) for current authority and validation,
and [Benchmarking](BENCHMARKING.md) for evidence interpretation.
