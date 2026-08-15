# Schweep v0.1 supported API and compatibility promise

This document freezes the supported Schweep surface for the `current-v0.1` line. It is intentionally
narrower than the set of Rust items that happen to be `pub`: this workspace is not published as crates,
and an implementation detail does not become a compatibility promise by accident.

## Supported deployment boundary

The supported v0.1 deployment is one `schweepd` process, one writer, one epoch clock, and local durable
storage. The process binds loopback on an ephemeral port. A catalog file is required at startup:

```text
schweepd <data-dir> <catalog.txt> [--port-file <path>] [--queue-bound N]
```

The catalog has one table per line: `table: column:type:nullable, ...`. The accepted types are `Int64`,
`Utf8`, and `Boolean` (with the aliases `int`, `text`, and `bool`); the optional third field is
`notnull`. `--queue-bound` overrides the per-source batch count, while the byte bound remains the
compiled policy default. `SyncPolicy::Full` and a checkpoint interval of eight epochs are fixed by the
binary in v0.1.

This server is a compute/storage primitive for a trusted local composition. Its HTTP listener is
plaintext and unauthenticated; production remote exposure requires the MutinyDB gateway or another
authenticated, encrypted proxy. That missing standalone boundary is tracked in
[#16](https://github.com/Bobcatsfan33/schweep/issues/16).

## HTTP contract

HTTP/1.1 uses one request and response per connection, `Content-Length`, and no chunking, compression,
or keep-alive pipeline. Successful responses are `200`. Batch request bodies and framed reads use
`schweep_log::Record` frames. Text answers use the canonical rendering defined by `docs/SEMANTICS.md`.

| Method and path | Required input | Result |
| --- | --- | --- |
| `POST /ingest` | query `source`, `table`, `token`; one framed batch body | `appended` or `duplicate` |
| `POST /seal` | none | sealed epoch |
| `POST /txn` | query `source`; zero or more framed batches | atomically sealed epoch |
| `POST /retract-source` | query `source`; optional `table`; optional SQL predicate body | retraction receipt |
| `POST /register` | SQL body; optional non-empty `unbounded` justification | durable query handle |
| `POST /deregister` | query `handle` | `ok` |
| `GET /read` | query `handle`; optional `format=frames` | epoch and canonical answer, or framed answer |
| `GET /oneshot` | SQL body, or query `sql` | canonical answer |
| `GET /subscribe` | query `handle`, `from` epoch | next epoch token and retained epoch deltas |
| `GET /plan` | query `handle` | canonical plan |
| `GET /counters` | none | execution counters |
| `GET /fingerprint` | none | durable-state fingerprint |
| `GET /explain-state` | none | per-operator/query state accounting |
| `GET /explain-maintenance` | none | maintenance work counters |
| `GET /health` | none | health and admission state |
| `POST /shutdown` | none | checkpoint/drain receipt and process shutdown |

The status/error taxonomy is part of the contract: `400 Refused`, `404 NotFound`, `409 Rejected`,
`429 Overloaded`, and `500 Internal`. The response body starts with that stable kind name. Only
`Overloaded` is retryable. A subscription resume token is the epoch number; the server stores no cursor.

## Embedded contract

The supported embedded entry points are:

- `schweep_server::{Engine, Server, ServerConfig, Client, Policy}` and their public operation methods;
- `schweep_batch::{answer, answer_sql, answer_over_log, answer_over_integrals}` for one-shot execution;
- `schweep_plan::{Query, Source, Expr, GroupBy, AggFunc, BinOp, Catalog}` for typed plans;
- `schweep_zset` schemas, rows, scalar values, and Z-set batches used by those calls.

Other public modules and items are available for this repository's own crates and tests but are not a
v0.1 compatibility commitment. Crates remain `publish = false`; the supported distribution is the
release binary and source tree.

## Semantic and durable compatibility

The supported SQL constructs and their exact null, error, weight, typing, ordering, and overflow rules
are those in `docs/SEMANTICS.md`. Unsupported SQL is refused by name. Source retraction, epoch atomicity,
exactly-once deduplication, canonical answer bytes, and the five error kinds are stable for v0.1.x.

Snapshot v1 and v2 are readable throughout the v0.1 line. New compactions write authenticated snapshot
v2 provenance. A v1 snapshot that discarded source attribution remains query-readable, but source
retraction fails closed rather than inventing ownership. A v0.1.x disk-format addition must read every
earlier v0.1.x format and must use the existing publish-then-swap crash protocol.

## Versioning promise

- Patch releases `0.1.x` do not remove or change the meaning of a supported endpoint, status kind,
  semantic rule, embedded entry point, subscription token, wire frame, or readable disk format.
- Additions in `0.1.x` must be safe for an older client to ignore. Existing response lines keep their
  order and meaning; new optional material is appended.
- A breaking supported-surface change requires `0.2`, a decision record, and migration notes.
- Security fixes may make an invalid or unsafe request fail closed where an older patch accepted it;
  the release notes must identify the affected request class.

The compatibility promise does not convert the explicitly listed open limitations into features. The
README's issue-linked limitations are part of this freeze.
