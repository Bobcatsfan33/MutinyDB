# M2 semantic operator status

M2 is in progress. `mutiny-semantic` establishes the exact top-k standing-query primitive before
semantic grouping is added.

The operator is pinned to one immutable Prism embedding space, normalizes vectors with
`prism-types`, ranks with the same exact dot-product and event-id tie break as PrismDB, fuses tenant,
time, cost, and error predicates, and updates its ranking in `O(log n)` per inserted or retracted
row. Each epoch is transactional: a bad weight, dimension, generation, duplicate, mismatched
retraction, or state-ceiling breach leaves both state and answer unchanged. Answer changes are
emitted as ordinary `-1/+1` Z-set-style rows.

The M2 gate includes randomized per-epoch comparison with an independent exact oracle and an
end-to-end comparison with PrismDB's real `Engine::exact_search` over a corpus ingested through its
real generation, part, and catalog code. Two embedding generations can be live in separate
operators; attempting to feed one generation into another is a named refusal, never a score merge.

Remaining M2 work is explicit: incremental semantic grouping with bounded mergeable state,
embedding-at-bridge integration, generation migration over two live operator sets, cold-tier
one-shot routing, and the frozen golden-corpus hybrid gate across those paths. This document does
not mark M2 or PrismDB release admission complete.
