# MutinyDB TypeScript SDK

```ts
import { MutinyDB } from "@mutinydb/client";

const db = new MutinyDB({baseUrl: "http://localhost:7654", tenant: "quickstart"});
const handle = await db.register("SELECT branch, COUNT(*) FROM telemetry GROUP BY branch");
console.log(await db.read(handle));
console.log(await db.semanticAnswer("sess-a", "incident-similar"));
```

The semantic argument is the configured standing-query ID. Node 18+ and modern browsers are
supported through the standard `fetch` API. Pass `operatorToken` only to trusted operator-side
processes.
