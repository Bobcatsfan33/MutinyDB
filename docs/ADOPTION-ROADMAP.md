# The adoption roadmap

MutinyDB's category is the **truth-maintenance database for AI agents**: durable memory whose
answers stay current, whose derivations remain accountable, and whose hypothetical worlds are
cheap to create and safe to discard. It should not compete as another vector index.

## v0.1 — usable by an agent team

Shipped in the developer release:

- one binary, one non-root multi-architecture container, `mutinyd init`, and checksum-addressed
  release artifacts;
- standing SQL, typed HTTP, and MCP over one admission boundary;
- branch-scoped semantic recall and grouping, provenance-enforced writes, taint-as-retraction,
  fork/merge/rewind, and propose-not-execute actions;
- OpenAPI 3.1 plus zero-runtime-dependency Python and TypeScript clients;
- deterministic crash, differential, oracle, maintenance, tenancy, and no-egress gates.

Exit signal: three independent projects run the end-to-end incident workflow from a tagged binary
without maintainer help.

## v0.2 — the native agent-memory loop

1. Finish the combined MutinySQL binder for semantic ranking, branch time travel, provenance
   predicates, novelty, and semantic diff. The README north-star query becomes an executable CI
   fixture; until then these tokens continue to be refused by name.
2. Add structured JSON response representations without removing canonical text or frame formats.
3. Publish LangGraph, LlamaIndex, CrewAI, AutoGen, and OpenAI Agents adapters as thin packages
   over the stable clients; each gets the same recall/branch/taint conformance corpus.
4. Add pluggable embedding providers with generation pinning, batch backfill, dual-generation
   cutover, and a local deterministic provider for tests.
5. Add explainable retrieval: every memory hit returns score components, source lineage, branch,
   age, and policy decision.

Exit signal: a new agent framework integration takes under 100 lines and passes one shared
conformance suite.

## v0.3 — familiar database ergonomics

1. PostgreSQL wire compatibility for the proven SQL subset, with explicit refusal for unsupported
   transactions and extensions.
2. Schema migration commands, backup/restore verification, import/export, and an operator CLI.
3. WebSocket or SSE streaming layered on the existing cursorless resume-token semantics.
4. OpenTelemetry traces and metrics, Grafana dashboards, SLO templates, and capacity guidance.
5. Helm chart and Kubernetes operator for tenant placement, sleep/wake, backup, and safe upgrades.

Exit signal: standard SQL tools can inspect MutinyDB and an operator can restore a verified backup
into a new cluster without bespoke steps.

## v1.0 — production trust and ecosystem

- external-KMS custody evidence, third-party security review, signed provenance and SBOMs;
- replicated control plane, quorum durability, rolling upgrades, and documented disaster-recovery
  objectives;
- remote object tier wired through the composed product, preserving the measured one-RTT wake
  property;
- stable public protocol and durable-format compatibility policy;
- published cloud-neutral benchmark against recompute-on-read and vector-plus-metadata stacks;
- multi-maintainer governance and predictable release cadence.

## What adoption is measured by

Stars are an output, not the operating metric. Track time-to-first-healed-answer, weekly active
deployments (opt-in, never telemetry by default), independent production users, adapter coverage,
successful upgrades, issue response time, contributor concentration, and p95 docs-to-working-demo
time. Every benchmark must publish corpus, configuration, commit, runner, raw measurements, and a
command that reproduces it.
