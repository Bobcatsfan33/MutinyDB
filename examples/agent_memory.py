"""A minimal agent-memory loop using MutinyDB's public Python client."""

import time

from mutinydb import MutinyDB


db = MutinyDB("http://localhost:7654", tenant="agent")
session = "support-agent-42"
db.session_open(session)

receipt = db.write(
    actor="support-agent",
    session=session,
    branch=session,
    intent="remember a verified tool outcome",
    sources=[{"system": "ticketing", "record": "ticket-884"}],
    table="memory",
    rows=[
        [
            "mem-1",
            session,
            "customer prefers email follow-up",
            0,
            False,
            int(time.time()),
        ]
    ],
)
print("committed", receipt)

handle = db.register(
    "SELECT memory.branch AS branch, COUNT(*) AS memories FROM memory GROUP BY memory.branch"
)
print("standing", db.read(handle))
print("recall", db.semantic_answer(session, "agent-recall"))

# Hypotheses live on isolated branches and merge only after policy is re-run.
db.fork(session, session, f"{session}-hypothesis")
