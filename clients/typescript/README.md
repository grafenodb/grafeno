# grafeno-client

TypeScript client for the Grafeno HTTP API (`grafeno serve`). It has no
dependencies and uses the global `fetch`.

```ts
import { Grafeno } from "grafeno-client";

const grafeno = new Grafeno({ url: "http://127.0.0.1:7810", token: process.env.GRAFENO_TOKEN });
const vault = grafeno.namespace("vault-123");

await vault.batch([
  { op: "upsert_node", ref: "$note", node: { id: noteId, labels: ["Note"], props: { title }, text } },
  { op: "delete_edges_from", from: "$note", label: "links_to" },
  ...linkedIds.map((to) => ({ op: "create_edge" as const, edge: { from: "$note", label: "links_to", to } })),
]);

const context = await vault.context({ text: question, budget: 1500 });
prompt += context.rendered;

// GQ queries and graph algorithms.
const people = await vault.query("MATCH (p:Person) WHERE p.age > $age RETURN p.name", { age: 30 });
const ranked = await vault.algorithm("pageRank", { settings: { labels: "Person" } });
await vault.algorithm("louvain", { write: "community" });
const cores = await vault.algorithm("kCore", { stats: true }); // [column, count, min, max, mean, sum]
const field = await vault.algorithm("distanceField", { args: [[exitId]] }); // nearest exit per node
```

Users sign in with a password; the client renews the session by itself
and tells the application the new tokens to keep:

```ts
const client = new Grafeno({ url, onSession: (tokens) => save(tokens) });
await client.signIn("ana", password);
// Later, from kept tokens:
const again = new Grafeno({ url, session: load(), onSession: save });
await again.signOut();
```

`createKey({ name, scope: { read_only: true } })` makes a key of the
signed-in account; `accounts()`, `createAccount()`, `changeAccount()`
and `rotateKey()` manage the rest.

For a cluster, pass several members: the client uses one, moves to the
next when it does not answer, and reads its own writes on any of them (it
sends the last `position` it saw). Reads can ask for more consistency, and
a write with an idempotency key runs once however often it is retried:

```ts
const cluster = new Grafeno({ url: ["http://10.0.0.1:7810", "http://10.0.0.2:7810", "http://10.0.0.3:7810"], token });
const ns = cluster.namespace("vault-123");
await ns.query("CREATE (:Note {title: $t})", { t: "hi" }, { idempotencyKey: crypto.randomUUID() });
await ns.query("MATCH (n:Note) RETURN count(n)", {}, { consistency: "linearizable" });
// Split a costly read across the members (`split` in the result says so).
await ns.query("MATCH (p:Person) RETURN p.city, count(*)", {}, { split: "on" });
```

`spread: true` starts each request on the next member, spreading reads
over the cluster.

Field names follow the API (snake_case); see
[the server README](../../crates/grafeno-server/README.md) for every endpoint.
Errors are thrown as `GrafenoError`, with the HTTP `status` and the API's
`code`.

## Versions and capabilities (15.3)

The client is the published contract of the HTTP API. It has its own
version (`CLIENT_VERSION`, 0.2.0) and speaks the API versions in
`API_VERSIONS` (`[1]`). Before using an engine, check it:

```ts
const grafeno = new Grafeno({ url, token });
const about = await grafeno.connect(); // fails with `incompatible` if no API version is shared
console.log(about.version, about.api.version, about.capabilities);
if (grafeno.supports("audit")) showAuditLog();
if (grafeno.supports("cluster.groups")) showPlacement();
```

`connect()` reads `GET /v1/about`; an engine from before it (0.1.0) is told
from `/health` (`about.legacy`, with the capabilities of that time), so the
same client works with it. Offer only what `supports()` allows, rather
than failing on an older engine. `settings()` reads what a server runs with
(superusers). `test/contract.test.ts` runs the client against the real
engine binaries listed in `GRAFENO_CONTRACT_BINS` (the CI gives it the
engine being built and the latest release).

## Development

```bash
npm run build
```

The end-to-end tests start the real server binary, so build it first:

```bash
cargo build -p grafeno-server
```

```bash
npm test
```

## Read nodes and edges together

```ts
const records = await vault.readMany({ nodes: [noteId, otherId], edges: [edgeId] });
```

One request reads one snapshot, preserving input order and duplicate IDs. Missing
records are `null`; nodes include the same text and memory fields as `getNode`.
The server accepts at most 512 IDs across both lists and requires read access to
that namespace. This is a read operation, separate from the writing `batch` API.
See the [method and measurements](../../docs/read-many.md).

## Roles and grants (14.3)

```ts
const admin = new Grafeno({ url, token });
await admin.putRole("team-reader", { description: "reads the teams" });
await admin.grant(["read", "export"], { on: "namespace", namespace: "team-*" }, ["team-reader"]);
await admin.grant(["read"], { on: "namespace", namespace: "team-secret" }, ["team-reader"], { deny: true });
await admin.grantRoles(["team-reader"], ["ana"]);
const held = await admin.privilegesOf("team-reader"); // {columns, rows}
await admin.changeAccount("ana", { expires_at: "2027-01-01" });
```

`revoke`, `revokeRoles`, `roles`, `role`, `deleteRole` and
`privilegeCatalog` complete them. Roles take record `policies` (14.4), and
`changeAccount(name, { attributes: { team: "red" } })` sets what they compare
with; GQ access statements (`GRANT …`, `SHOW
USERS`) also run through `namespace(…).query`.

## The audit log (14.5)

```ts
const page = await admin.audit({ since: 0, limit: 100 }); // {events, last, oldest, gap}
for (const event of page.events) console.log(event.seq, event.kind, event.target, event.outcome);
```

It needs the `audit` privilege.

## Expand neighborhoods together

```ts
const around = await vault.neighborhoods([noteId, otherId], {
  filter: { valid_at: "2026-06-01" },
});
```

Up to eight roots return separate depth-one subgraphs in their original order,
using one snapshot. A missing root fails the batch, matching `subgraph`.
Studio uses this operation for graph expansion. See [English](../../docs/graph-expansion.md)
or [Español](../../docs/expansion-grafo.md) for compatibility and measurements.
