# M2 composed semantic path

The M2 exit contract is implemented in `mutiny-semantic`. Release admission remains separate and
blocked by the component and later composed-product gates.

The operator is pinned to one immutable Prism embedding space, normalizes vectors with
`prism-types`, ranks with the same exact dot-product and event-id tie break as PrismDB, fuses tenant,
time, cost, and error predicates, and updates its ranking in `O(log n)` per inserted or retracted
row. Each epoch is transactional: a bad weight, dimension, generation, duplicate, mismatched
retraction, or state-ceiling breach leaves both state and answer unchanged. Answer changes are
emitted as ordinary `-1/+1` Z-set-style rows.

The bridge mapping converts one complete `BridgeDelta` to embedded changes as one fallible batch,
uses Prism's tenant-scoped ingest purpose, derives the immutable space from model id and version,
and stores scalar cost in integer micro-units before conversion. A malformed row or embedding
refusal rejects the epoch; a different model generation cannot reuse the plan.

Fixed normalized anchors provide incremental semantic groups with exact retraction, deterministic
exemplars, scalar filtering, a declared byte ceiling, and atomic merge of disjoint partitions.
Dual-generation migration maintains two complete operator sets but compares only row identities and
rank positions at the explicit cutover gate—never scores from different geometries. Cold one-shot
routing carries its generation in the result, validates the source's rank sequence and bound, and
fails closed on a different space.

The gate includes randomized per-epoch comparison with an independent exact oracle, real PrismDB
`Engine::exact_search`, PrismDB's actual candidate/rerank path with every partition probed, and a
frozen bridge→top-k→group→dual-generation→cold-route corpus. The maintained and one-shot answers
are bit-for-bit equal on those gates. This does not mark PrismDB or MutinyDB release admission
complete; M3–M8 and external evidence remain open.
