# MutinyDB Python SDK

```python
from mutinydb import MutinyDB

db = MutinyDB("http://localhost:7654", tenant="quickstart")
handle = db.register("SELECT branch, COUNT(*) FROM telemetry GROUP BY branch")
print(db.read(handle))
print(db.semantic_answer("sess-a", "incident-similar"))
```

The semantic argument is the configured standing-query ID. The client has no runtime dependencies
and supports Python 3.9+. Operator-only calls require `operator_token=` when constructing the
client.
