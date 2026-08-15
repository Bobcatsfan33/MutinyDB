# M3 mounted trust plane

`mutiny-trust` mounts Loom's real branch, capability, policy, and action implementations over
MutinyDB's standing semantic result stores. It does not copy those controls into a second model.

The mount returns two different Rust types. `AgentTrustPlane` can open and fork sessions, maintain
and read branch-local answers after Loom validates the exact capability, ask the deny-overrides
policy engine, and create an inert `AgentStore` proposal. It owns no `ActionGateway` and exposes no
execute method. `OperatorTrustPlane` alone owns the gateway; execution therefore always traverses
Loom's kill switch, evidence eligibility, policy, simulation containment, idempotency, and receipt
checks.

Session and hypothesis forks clone their parent materialized state while holding the mount's write
lock. A sibling token cannot observe or update it. The production-facing session API accepts a
caller-supplied collision-resistant session id; Loom's millisecond convenience id remains available
only for serialized embedded use. Standing state is still memory-resident at M3—durable copy-on-write
operator state is M5, and this document does not claim it early.

The hosted M3 gate runs:

- the mounted result/capability and agent/operator type-separation tests;
- Loom's complete scripted Q3 buyer demo verbatim through its existing MCP server; and
- Loom's four unmodified randomized models: branch/merge, taint/recall, retrieval isolation, and
  policy truth table.

M3 is a composition milestone, not release admission. Taint propagation through every MutinyDB
circuit (M4), durable forked state (M5), the unified process and doors (M6), fleet behavior (M7), and
external security/reliability evidence (M8) remain open.
