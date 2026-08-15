# Source-scoped retraction

`retract_source(source_id, table?, predicate?)` removes facts by provenance without editing history.
It resolves the source's current net contribution from the authenticated snapshot provenance ledger plus
the retained log, negates the selected rows, and seals those deltas through the normal engine path.

## Contract

- `source_id` is exact and case-sensitive.
- With no table, every current contribution from the source is selected.
- A predicate requires a table. It is the same scalar SQL expression accepted in `WHERE` and sees that
  table's qualified columns.
- Only `TRUE` matches. `FALSE` and `NULL` do not.
- An empty selection is a successful no-op and does not advance the epoch.
- A completed request is idempotent. Repeating it observes a zero net contribution and changes nothing.
- The response reports the sealed epoch, tables, rows, and absolute multiplicity retracted.

The HTTP form is:

```text
POST /retract-source?source=<id>
POST /retract-source?source=<id>&table=<table>
POST /retract-source?source=<id>&table=<table>   body: <SQL predicate>
```

This operation can be expensive: it scans the source-provenance ledger and retained suffix, then emits
one negative entry for each selected canonical row. It is bounded by the selected source, not by the
number of standing queries. All query maintenance remains shared through the memo.

## Compaction and recovery

Snapshot v2 contains `PROVENANCE`, a deterministic sequence of checksummed log-format frames, plus its
whole-file checksum in `MANIFEST`. Publication follows the existing P2–P7 ordering, so the log prefix is
not discarded until both table integrals and source attribution are durable and named by the pointer.

Snapshot v1 has no source attribution. It remains valid for reads and query recovery, but a source
retraction that would need its discarded prefix fails closed with `ProvenanceUnavailable`.
