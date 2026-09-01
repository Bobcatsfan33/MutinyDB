import assert from "node:assert/strict";
import test from "node:test";
import {MutinyDB} from "../dist/index.js";

test("register uses the tenant-scoped SQL endpoint", async () => {
  let seen;
  const db = new MutinyDB({baseUrl: "http://localhost:7654/", tenant: "acme", fetch: async (url, init) => {
    seen = {url, init};
    return new Response("42\n", {status: 200});
  }});
  assert.equal(await db.register("SELECT * FROM facts"), 42);
  assert.equal(seen.url, "http://localhost:7654/v1/acme/sql/register");
  assert.equal(seen.init.body, "SELECT * FROM facts");
});

test("semantic answers are typed", async () => {
  let seen;
  const db = new MutinyDB({baseUrl: "http://localhost:7654", tenant: "acme", fetch: async (url) => {
    seen = url;
    return new Response('[{"rank":1,"key":"evt-7","score":0.875}]');
  }});
  assert.deepEqual(await db.semanticAnswer("main", "timeout"), [{rank: 1, key: "evt-7", score: 0.875}]);
  await db.semanticAnswer("main", "tool timed out");
  assert.match(seen, /query=tool%20timed%20out/);
});
