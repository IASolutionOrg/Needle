# Synthetic evaluator fixtures

These answer-bearing JSON documents are retained solely for deterministic
legacy/evaluator tests. They are not referenced by the public
`needle.frozen-corpus/4` manifest, are not sealed production material, and
must never be used as independent provider evidence.

The checked-in `../synthetic-sealed/` index and four documents exercise the
sealed evaluator contract and exact byte commitments. They are synthetic and
remain provider-ineligible. Private production bundles must stay unmounted and
inaccessible to the runner identity; this fixture does not claim ACL or
process-isolation proof.
