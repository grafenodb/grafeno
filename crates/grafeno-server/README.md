# grafeno-server

[Español](README.es.md) | **English**

The `grafeno` binary serves and manages a Grafeno database (installers:
[docs/install.md](../../docs/install.md)):

- `grafeno serve`: a JSON HTTP API, for applications and agents, and
  no interface (Grafeno Studio is installed apart and connects to this API).
- `grafeno setup`: initializes an instance: its data
  folder, settings (a fresh token), first administrator and service,
  enabled and started; interactive, or every answer as a flag or an
  answers file (`--unattended`, `--answers FILE`).
- `grafeno instances [list | drop NAME [--purge] | rollback NAME]`: the
  instances on this machine (`default` is `grafeno.toml` and
  `grafeno.service`, others `<name>.toml` and `grafeno@<name>.service`);
  `rollback` puts back the copy the last layout upgrade kept.
- `grafeno doctor [--instance NAME]`: checks the settings and their
  permissions, the data folder, the database's layout, free disk space,
  the port and the service; exits with an error if a check fails.
- `grafeno config init | path | token`: the settings file of an installed
  server (`grafeno.toml`).
- `grafeno service install [--account "NT SERVICE\Grafeno"] | uninstall`:
  the Windows service (LocalSystem unless an account is given).
- `grafeno mcp`: an MCP server over stdio, for agents (any MCP client).
- `grafeno shell`: an interactive console for GQ queries and
  administration, on a file or on a running server.
- `grafeno namespaces | stats | check | rebuild | export | import`:
  administration commands on a file.
- `grafeno keys list | create | revoke`: API keys, on a file or through a
  running server (see *Access* below).
- `grafeno backup OUT`: a consistent copy of the whole database, from a
  file or (with `--config`) from a running server, which keeps serving.
  `grafeno restore BACKUP NEW` recovers a backup into a new database file
  (pages checked, layout upgraded, namespaces verified; the backup is only
  read and an existing file is never replaced).
  `grafeno check` verifies every page of the file against its checksum
  before checking the indexes: run it on a file that may be damaged.
  `grafeno check --recall 100` also estimates the recall of vector
  searches, and reports how many vectors were removed since each vector
  graph was built; `grafeno compact -n NS` (or `--all`) rebuilds a
  namespace's vector graph and each of its vector indexes. The server
  also compacts on its own a graph worn past `compact_vectors_at` (see
  `[background]` in [docs/install.md](../../docs/install.md#settings)).

```bash
cargo install --path crates/grafeno-server
```

```bash
grafeno serve --db memory.redb --token change-me
```

```bash
grafeno mcp --db ~/.grafeno/memory.redb --namespace me
```

A database file can be open in only one process at a time, so an HTTP server
and an MCP server cannot share a file. Use `--memory` for a throwaway
in-memory database.

`grafeno serve` options (each also from the environment or `--config`):

| Option            | Environment     | Default          |
| ----------------- | --------------- | ---------------- |
| `--config FILE`   | `GRAFENO_CONFIG`| none             |
| `--db FILE`       | `GRAFENO_DB`    | `grafeno.redb`   |
| `--memory`        |                 | off              |
| `--addr ADDR`     | `GRAFENO_ADDR`  | `127.0.0.1:7810` |
| `--token TOKEN`   | `GRAFENO_TOKEN` | none (no auth)   |
| `--cors-origin O` (repeatable, `*` for any) | | none   |
| `--stop-with-stdin` |               | off: stop cleanly when standard input closes (for a program that starts the engine, such as Grafeno Desktop) |

## Shell

```bash
grafeno shell --db memory.redb -n vault
```

```bash
grafeno shell --url http://127.0.0.1:7810 --token change-me -n vault
```

For a cluster, `--url` takes several members, comma-separated; the shell
moves to the next when one does not answer, and reads its own writes.

```text
vault=> MATCH (p:Person)-[:works_at]->(c)
vault->   RETURN p.name, c.name LIMIT 3;
 p.name | c.name
--------+--------
 Ana    | Acme
 Bob    | Acme
(2 rows)
vault=> \context who works at Acme?
```

Statements are [GQ](../../docs/query-language.md) and end with `;`; lines
that start with `\` are meta-commands (`\?` lists them): `\ns` (and
`\ns create NAME`, `\ns drop NAME --yes`), `\use`,
`\stats`, `\check`, `\rebuild`, `\search`, `\recall`, `\context`,
`\export`, `\import`, `\format table|json`, `\timing`, `\trace` (each
statement as `TRACE`, its events in a table), `\i FILE`, `\q`.
`BEGIN` (or `BEGIN READ ONLY`), `COMMIT` and `ROLLBACK` run the statements
between them in one transaction, on a file or on a server; the prompt shows
`*` while one is open, and a script or session that ends inside one rolls
it back.
History is kept in `~/.grafeno_history`.

Without a terminal it runs scripts and exits, with an error code if a
statement failed: `-c "…"`, `-f FILE` (`-` for standard input), or a script
piped into it. `--json` prints results as JSON; `--stop-on-error` stops at
the first statement that fails (by default the rest still runs). Scripts
can create their namespace with `CREATE NAMESPACE … IF NOT EXISTS`
(see the query language guide).

```bash
grafeno shell --db memory.redb -n vault -c "MATCH (n) RETURN count(*);"
```

## Administration commands

They open the file directly (stop the server first):

| Command                                   | What it does                                                   |
| ----------------------------------------- | -------------------------------------------------------------- |
| `grafeno namespaces --db F`               | Namespaces with their node and edge counts.                    |
| `grafeno namespaces --db F create NAME [--description TEXT] [--if-not-exists]` | Creates an empty namespace. |
| `grafeno namespaces --db F describe NAME [TEXT]` | Sets (or, without TEXT, removes) its description.      |
| `grafeno namespaces --db F drop NAME --yes` | Deletes a namespace and everything in it.                    |
| `grafeno stats --db F [-n NS]...`         | Statistics as JSON: counts per label, texts, vectors.          |
| `grafeno check --db F [-n NS]...`         | Integrity check; exits with an error if anything is wrong.     |
| `grafeno rebuild --db F (-n NS... \| --all)` | Rebuilds indexes and counters from the records.             |
| `grafeno export --db F [-n NS]... [-o FILE]` | JSON Lines export (standard output by default).             |
| `grafeno extract --db F -n NS` (`--all`, `--force`) | Reads new and changed texts and links what they mention; prints what waits for review. |
| `grafeno import --db F FILE`              | Imports an export (`-` reads standard input), all or nothing; `--bulk` loads large exports in chunks. |

```bash
grafeno export --db memory.redb -o backup.jsonl
```

```bash
grafeno import --db restored.redb backup.jsonl
```

| Option        | Environment         | Default            | Command         |
| ------------- | ------------------- | ------------------ | --------------- |
| `--db`        | `GRAFENO_DB`        | `grafeno.redb`     | `serve`, `mcp`  |
| `--memory`    |                     | off                | `serve`, `mcp`  |
| `--addr`      | `GRAFENO_ADDR`      | `127.0.0.1:7810`   | `serve`         |
| `--token`     | `GRAFENO_TOKEN`     | none (no auth)     | `serve`         |
| `--namespace` | `GRAFENO_NAMESPACE` | `default`          | `mcp`           |

## Access

Callers are **accounts**: **users**, who sign in with a password and get a
session, and **service accounts**, which use API keys (14.1). Every
request under `/v1` carries `Authorization: Bearer <secret>`: a session's
access token (`gfs_…`), an API key (`gfk_…`), or the server's **token**
(`grafeno.toml`, `--token`), a break-glass credential that may do
everything. An account has grants: a role on a namespace (`vault`), on
every namespace starting with a prefix (`team-*`), or on all (`*`). Roles
add up:

| Role | May |
| --- | --- |
| `read` | read, search, recall, run read queries and algorithms |
| `write` | also create and change nodes and facts, merge, archive, touch |
| `admin` | also indexes and constraints, rebuild, describe and drop the namespace, create it |

**Roles** (14.2) go further: a role holds privileges, built-in levels on
namespace patterns and other roles, and accounts hold roles. A privilege
is an action on an object, granted or denied; a denial wins over any
grant. On the server: `create_namespace`, `manage_accounts`,
`manage_roles`, `backup`, `metrics`, `audit`, `cluster`. On a namespace
pattern: `read`, `create`, `update`, `delete`, `merge`, `retract`,
`import`, `export`, `schema`, `algorithms`, `drop`, `create_namespace`,
`events` (event subscriptions and their values, 16.10)
(the levels above are bundles of these: `GET /v1/privileges` lists them).
On a label or a property: `read` and `write` (checked by queries from
14.4). A role named `public`, when defined, is every account's.

```bash
curl -X PUT localhost:7810/v1/roles/team-reader -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"privileges": [
        {"action": "read", "object": {"on": "namespace", "namespace": "team-*"}},
        {"action": "read", "object": {"on": "namespace", "namespace": "team-secret"}, "deny": true}]}'
```

```bash
curl -X PATCH localhost:7810/v1/accounts/ana -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"roles": ["team-reader"]}'
```

Access can be managed in GQ too (14.3): `CREATE USER`, `ALTER USER`,
`CREATE ROLE`, `GRANT`, `DENY`, `REVOKE`, `GRANT ROLE`, `SHOW USERS`,
`SHOW ROLES`, `SHOW PRIVILEGES [FOR name]`, `SHOW CURRENT USER` (see
[the query language](../../docs/query-language.md#users-roles-and-privileges)),
with `grafeno roles` and `grafeno grants`, and in the Studio's Access view.

```bash
grafeno roles create team_reader --config grafeno.toml
```

```bash
grafeno grants grant read,export --on "namespace:team-*" --to team_reader --config grafeno.toml
```

```bash
grafeno grants deny read --on property:Person.email@crm --to team_reader --config grafeno.toml
```

```bash
grafeno grants add-role team_reader --to ana --config grafeno.toml
```

**Enforcement** (14.4): what a caller may not read is absent from every
answer, as if it did not exist: nodes with a label it may not read (or
that a policy keeps from it), edges it may not read or that lead to such
nodes, and hidden properties. Listings, reads by id (404), traversals,
searches, recall and context, GQ, algorithms, exports, the change log
(and its stream), statistics and the namespace list all leave them out.
Entity resolution, duplicates, merges, community summaries, extraction,
the review queue, decay, consolidation, `check` and vector recall answer
403 to a caller who sees a namespace record by record. Every change is
checked against labels, properties and the operation; a write that would
touch something forbidden fails whole. **Record policies** limit a role to
records that match a condition:

```bash
curl -X PUT localhost:7810/v1/roles/analyst -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"grants": [{"namespace": "crm", "role": "read"}],
       "policies": [{"namespace": "crm", "label": "Customer", "applies": "read",
                     "condition": {"kind": "property", "key": "tenant",
                                   "equals": {"from": "attribute", "name": "tenant"}}}]}'
```

```bash
curl -X PATCH localhost:7810/v1/accounts/ana -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"attributes": {"tenant": "acme"}}'
```

**The audit log** (14.5) keeps every security event once: sign-ins and
their failures, lockouts, sessions, keys, changes to accounts and roles
(grants, denials and policies included) and every refused request, with
the account, the key or session, the address, the target and the outcome.
Reading it needs the `audit` privilege:

```bash
curl "localhost:7810/v1/audit?since=0&limit=100" -H "Authorization: Bearer $TOKEN"
```

```bash
curl -N localhost:7810/v1/audit -H "Authorization: Bearer $TOKEN" -H "Accept: text/event-stream"
```

```bash
curl "localhost:7810/v1/audit?format=jsonl" -H "Authorization: Bearer $TOKEN" > audit.jsonl
```

In GQ, `CALL db.audit(0, 1000) YIELD seq, at, kind, account, via, address,
target, outcome, detail`. `[audit]` in `grafeno.toml` sets how much is
kept (`max_entries`, `max_age_days`). Those who manage accounts list and
end other accounts' sessions (not superusers').

Without a token, accounts close the server: the first account must be a
superuser, and a change that would leave no active superuser (or no
accounts) is refused. Accounts can expire (`ALTER USER … SET EXPIRES`,
`expires_at`): an expired account answers 401 `account_expired`.

An account may also be a **superuser** (everything, including managing
accounts; `owner` in older answers) and may be limited to **its own
records**: then it changes or deletes only
nodes and facts it recorded (others' are `403 forbidden`, in GQ too).
Every write records its account's name: `recorded_by` on nodes and edges,
`n.@recorded_by` in GQ. A key sees only the namespaces it may read: in
`/v1/namespaces`, `SHOW NAMESPACES` and exports.

```bash
grafeno keys create research-agent --grant vault:write --grant "team-*:read" --config grafeno.toml
```

```bash
grafeno keys list --config grafeno.toml
```

```bash
grafeno keys revoke research-agent --config grafeno.toml
```

With `--config`, the commands go through the running server (its address
and token); with `--db`, they open a database file whose server is
stopped. The secret is printed once; only its SHA-256 is stored. A key
made with grants this way is a service account of its name holding it.
`grafeno users create|list|password|disable|enable|delete` manage
accounts (`--superuser`, `--service`, `--grant`). A server with neither a
token nor accounts is open to anyone who reaches it; its first account
(`grafeno users create … --superuser`, or the first sign-up) must be a
superuser, and from then on every request needs a credential.

### Accounts and sign-in (14.1)

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/v1/auth/status` | | `{open, setup, password_sign_in}` (anyone) |
| `POST` | `/v1/auth/setup` | `{name, password}` | on an open server with no accounts: the first, a superuser, signed in (201) |
| `POST` | `/v1/auth/signin` | `{name, password}` | `{access_token, expires_in, refresh_token, refresh_expires_in, session, account}`; 401 when wrong, 429 when locked |
| `POST` | `/v1/auth/refresh` | `{refresh_token}` | new tokens, both replaced; a refresh token used twice ends the session |
| `POST` | `/v1/auth/signout` | | ends the caller's session |
| `GET` | `/v1/auth/sessions` | `?account=` (superusers; `*` for all) | the sessions, `current` marking the caller's |
| `DELETE` | `/v1/auth/sessions/{id}` | | ends one of the caller's (a superuser: anyone's) |
| `POST` | `/v1/auth/password` | `{current_password, new_password}` | changes it; the other sessions end |
| `GET`, `POST` | `/v1/accounts` | `{name, kind: user\|service, password?, grants, roles, superuser, own_only, description}` | `manage_accounts`; only superusers make superusers |
| `GET`, `PATCH`, `DELETE` | `/v1/accounts/{name}` | `{password?, grants?, roles?, superuser?, own_only?, disabled?, description?}` | `manage_accounts`, not on superusers; disabling or a new password ends its sessions |
| `GET` | `/v1/privileges` | | every action by object, and the built-in levels (anyone signed in) |
| `GET` | `/v1/audit` | `?since=&limit=`, `?format=jsonl`, or `Accept: text/event-stream` | audit events (`audit`): `{events, last, oldest, gap}`, JSON Lines, or a stream |
| `GET`, `POST` | `/v1/admin/users` | as `/v1/accounts`, plus `expires_at` (microseconds, ISO 8601, or `null`) | the accounts under the design's name (14.3) |
| `GET`, `PATCH`, `DELETE` | `/v1/admin/users/{name}` | as `/v1/accounts/{name}`, plus `expires_at` | one account |
| `GET` | `/v1/admin/roles` | | the roles (`manage_roles`) |
| `GET`, `PUT`, `DELETE` | `/v1/admin/roles/{name}` | as `/v1/roles/{name}` | one role |
| `GET` | `/v1/admin/grants?holder=name` | | `{columns: [source, access, privilege, on], rows}`: an account's or a role's privileges (its own: anyone; others: `manage_roles` or `manage_accounts`) |
| `POST` | `/v1/admin/grants` | `{privileges: ["read", "editor", …], on: {on: …}, to: [roles], deny?}` | grants or denies; `{rows: [["granted 2 privileges …"]]}` (`manage_roles`) |
| `POST` | `/v1/admin/grants/revoke` | `{privileges, on, from: [roles], which?: grants\|denials\|both}` | revokes |
| `POST` | `/v1/admin/grants/roles`, `/v1/admin/grants/roles/revoke` | `{roles, to}` / `{roles, from}` | gives or takes roles: to accounts `manage_accounts`, to roles `manage_roles` |
| `GET` | `/v1/roles` | | the roles (`manage_roles`) |
| `GET`, `PUT`, `DELETE` | `/v1/roles/{name}` | `{privileges: [{action, object: {on: server\|namespace\|label\|property, namespace?, label?, edges?, property?}, deny?}], grants, roles, description}` | a role; `PUT` defines or replaces it (201 when new), refusing cycles; `DELETE` takes it from everyone |
| `GET`, `POST` | `/v1/keys` | `{name, account?, scope?: {namespaces, read_only}, expires_in_days?, description?}` | an account's own keys (superusers: all); the secret once |
| `POST` | `/v1/keys/{name}/rotate` | | a new secret, once; the old one stops working |
| `DELETE` | `/v1/keys/{name}` | | revokes it |

An expired access token answers 401 `session_expired` (refresh it); an
expired key `key_expired`; a disabled account `account_disabled`.
`/v1/whoami` names the account, how it came (`via: {kind, name}`), a
key's `scope` and every role it reaches (`effective_roles`). A refusal
names the privilege missing (`` the account "carl" has no `delete`
privilege on the namespace "other" ``). Sessions last 15 minutes (access) and 30 days (refresh) by
default (`[auth]` in `grafeno.toml`).

## HTTP API

All bodies are JSON. Everything under `/v1` needs `Authorization: Bearer
<secret>` (the token or a key) unless the server is open. Errors look like
`{"error": {"code": "not_found", "message": "…"}}`, with codes `bad_request`
(malformed body), `invalid` (rejected input), `invalid_query` (a GQ error),
`query_limit` (a GQ query over its time, reads, rows, memory or cost limit;
see `[query]` in `grafeno.toml`), `not_found`, `conflict` (409: it exists already),
`unauthorized` (401: no valid secret), `forbidden` (403: the key may not do
this), `busy` (503 with `Retry-After`:
too many requests at once, or too many writes waiting) and `internal`.

Writes from concurrent requests are committed together (group commit): each
request still gets its own all-or-nothing result, and a failing write does
not affect the others. Reads run on snapshots, side by side and alongside
the writer; a large read-only GQ scan also splits across threads, which
all queries share (`scan_threads` in `[query]`, one per CPU by default).

Ids are UUIDs. Times in responses are microseconds since the Unix epoch;
requests accept either that or an ISO 8601 string (`2024-05-01`,
`2024-05-01T10:30:00Z`).

| Method   | Path                                  | Body / query                                            | Returns                       |
| -------- | ------------------------------------- | ------------------------------------------------------- | ----------------------------- |
| `GET`    | `/health`                             |                                                         | `{status, version, layout}`   |
| `GET`    | `/v1/about`                           | (public)                                                | `{product, version, api: {version, supported}, layout, capabilities, auth: {token, accounts, open, password_sign_in}, cluster}`: see *The API as a contract* |
| `GET`    | `/v1/settings`                        | (superusers)                                            | the settings the server runs with, without the token or the cluster secret |
| `GET`    | `/v1/namespaces`                      |                                                         | `[{name, nodes, edges, created_at, description?}]`, empty ones too |
| `POST`   | `/v1/namespaces`                      | `{name, description?}`                                  | the namespace (201); 409 if it exists |
| `GET`    | `/v1/namespaces/{ns}`                 |                                                         | the namespace, or 404         |
| `PATCH`  | `/v1/namespaces/{ns}`                 | `{description}` (`null` removes it)                     | the namespace                 |
| `DELETE` | `/v1/namespaces/{ns}`                 |                                                         | `{nodes, edges}` deleted with it |
| `GET`    | `/v1/backup`                          |                                                         | the whole database file, consistent (owners) |
| `GET`    | `/v1/export`                          | `?ns=a,b` (all by default)                              | JSON Lines                    |
| `POST`   | `/v1/import`                          | JSON Lines (up to 1 GiB); `?bulk=true` loads in chunks between other writes | `{namespaces, nodes, edges}`  |
| `GET`    | `/v1/{ns}/stats`                      |                                                         | statistics                    |
| `POST`   | `/v1/{ns}/compact`                    | `?index=` (a vector index; the namespace's own vectors without it) | rebuilds that vector graph (admin): `{vectors, dangling_before, unreachable_before, unreachable_after}` |
| `GET`    | `/v1/{ns}/vectors/health`             |                                                         | `[{index, dimension, vectors, removed, repaired, churn, building}]`: each vector graph's removals since it was built (`churn` = removed / vectors), from its metadata alone |
| `GET`    | `/v1/{ns}/vectors/recall`             | `?sample=100&ef=64`                                     | `{sample, ef, recall}`: estimated recall@10 |
| `GET`    | `/v1/{ns}/check`                      |                                                         | `{…, problems}`               |
| `POST`   | `/v1/{ns}/rebuild`                    |                                                         | rebuild report                |
| `POST`   | `/v1/{ns}/nodes`                      | node (with `id`: upsert)                                | `{id, created}`               |
| `GET`    | `/v1/{ns}/nodes`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `GET`    | `/v1/{ns}/edges`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `POST`   | `/v1/{ns}/read-many`                  | `{nodes: [id], edges: [id]}` (512 at most together)     | `{nodes, edges}` in the order asked, `null` for missing ones, one snapshot (`read`) |
| `GET`    | `/v1/{ns}/nodes/{id}`                 |                                                         | node with `text` and `memory` |
| `PUT`    | `/v1/{ns}/nodes/{id}`                 | node                                                    | `{id, created}`               |
| `PATCH`  | `/v1/{ns}/nodes/{id}`                 | patch                                                   | node with `text`              |
| `DELETE` | `/v1/{ns}/nodes/{id}`                 |                                                         | 204                           |
| `GET`    | `/v1/{ns}/nodes/{id}/neighbors`       | `?direction=&label=a,b&valid_at=&known_at=`             | `[{node, edge}]`              |
| `POST`   | `/v1/{ns}/edges`                      | edge                                                    | `{id}`                        |
| `GET`    | `/v1/{ns}/edges/{id}`                 |                                                         | edge                          |
| `DELETE` | `/v1/{ns}/edges/{id}`                 |                                                         | 204                           |
| `POST`   | `/v1/{ns}/edges/{id}/invalidate`      | `{at}`                                                  | edge                          |
| `POST`   | `/v1/{ns}/edges/{id}/retract`         |                                                         | edge                          |
| `POST`   | `/v1/{ns}/neighborhoods`              | `{seeds: [id] (8 at most), direction, filter}`          | a depth-one subgraph per seed, in order, one snapshot (`read`) |
| `POST`   | `/v1/{ns}/k-hop`                      | `{start, depth, direction, filter}`                     | `[{node, hops}]`              |
| `POST`   | `/v1/{ns}/path`                       | `{from, to, max_depth, direction, filter}`              | `{path}` (or `null`)          |
| `POST`   | `/v1/{ns}/subgraph`                   | `{seeds, depth, direction, filter}`                     | `{nodes, edges}`              |
| `POST`   | `/v1/{ns}/search`                     | `{text?, index?, embedding?, mode, fusion?, k?, text_weight?, limit, candidates, fragments}` | `[{node, score, text_rank?, text_score?, vector_rank?, similarity?, fragments?}]`: `mode` `text`, `vector` or `hybrid` (16.6) |
| `POST`   | `/v1/{ns}/search/text`                | `{query, limit, index?, fragments?}`                    | `[{node, score, fragments?}]`: with `index`, a full-text index and its query language |
| `POST`   | `/v1/{ns}/search/vector`              | `{embedding \| embeddings, limit, ef?, index?, filter?: {labels, props}}` | `[{node, score}]`, or a list per embedding: a vector index with `index`, kept by `filter` during the walk (16.7) |
| `POST`   | `/v1/{ns}/similar`                    | `{node, limit, ef?, index?, filter?}`                   | `[{node, score}]`: nearest to the node's own vector, itself left out (16.7) |
| `POST`   | `/v1/{ns}/recall`                     | recall query                                            | `[{node, score, signals}]`    |
| `POST`   | `/v1/{ns}/context`                    | recall query + `{budget, fact_share, max_facts, facts, communities}` | `{overview, items, facts, tokens, rendered}`; `communities` puts that many matching community summaries first |
| `POST`   | `/v1/{ns}/batch`                      | `{ops}`                                                 | `{results, refs}`             |
| `POST`   | `/v1/{ns}/query`                      | `{query, params?, trace?}` ([GQ](../../docs/query-language.md)) | `{columns, rows, stats}`, and `trace` with `trace: true` (16.9) |
| `POST`   | `/v1/{ns}/transactions`               | `{read_only?, timeout_secs?}`                           | 201 `{id, read_only, started_at, timeout_secs, statements, failed}`: an explicit transaction (16.4) |
| `GET`    | `/v1/{ns}/transactions/{id}`          |                                                         | its state |
| `POST`   | `/v1/{ns}/transactions/{id}/query`    | `{query, params?}`                                      | a statement in it: `{columns, rows, stats}` |
| `POST`   | `/v1/{ns}/transactions/{id}/commit`   |                                                         | `{id, committed}`; 409 `rolled_back` if a statement in it failed or a cluster lost its leader |
| `POST`   | `/v1/{ns}/transactions/{id}/rollback` |                                                         | `{id, rolled_back}` |
| `POST`   | `/v1/{ns}/query/part`                 | a part of a split query (member to member, 13.4)        | the part's rows and statistics |
| `GET`    | `/v1/whoami`                          |                                                         | the caller: `{name, owner, grants, own_only}` |
| `GET`    | `/v1/metrics`                         |                                                         | counters and histograms in the Prometheus text format (any secret) |
| `GET`    | `/v1/keys`                            |                                                         | the keys, never their secrets (owners) |
| `POST`   | `/v1/keys`                            | `{name, grants: [{namespace, role}], owner?, own_only?, description?}` | `{key, secret}` (201; the secret only here) |
| `DELETE` | `/v1/keys/{name}`                     |                                                         | 204                           |
| `GET`    | `/v1/algorithms`                      |                                                         | `[{name, arguments, columns, description}]` |
| `POST`   | `/v1/{ns}/algo/{name}`                | `{args?, settings?, write?, column?, stats?}`           | `{columns, rows, stats}`: with `stats: true`, a row per numeric column (`column, count, min, max, mean, sum`) instead of the rows; `mode` and `write` do not go in `settings` |
| `GET`    | `/v1/{ns}/resolve`                    | `?name=&label=`                                         | nodes with that name or alias |
| `GET`    | `/v1/{ns}/duplicates`                 | `?label=&min_score=&vectors=&limit=`                    | `[{a, b, score, reasons, conflicts}]` |
| `POST`   | `/v1/{ns}/merge`                      | `{keep, merge, force?, dry_run?, functional?, strong_keys?}` | `{id, kept, merged, edges_moved, edges_dropped, conflicts}`; 409 with `error.conflicts` when they disagree; `{conflicts}` with `dry_run` |
| `GET`    | `/v1/{ns}/merges`                     | `?node=&limit=`                                         | merge records, newest first   |
| `POST`   | `/v1/{ns}/merges/{id}/undo`           | `{force?}` (optional)                                   | the record, marked undone     |
| `GET`    | `/v1/{ns}/fuzzy`                      | `?label=&key=&q=&limit=&measure=`                       | `[{node, score}]`: nodes whose property looks like `q` (needs a fuzzy index) |
| `GET`    | `/v1/{ns}/constraints`                |                                                         | constraints on facts and on properties: one per source, functional, not null, type (`CREATE CONSTRAINT` in GQ; unique and key constraints are indexes) |
| `GET`    | `/v1/{ns}/contradictions`             | `?limit=`                                               | `[{constraint, action, new_edge, old_edge, replacement, at, by}]`, newest first |
| `GET`    | `/v1/{ns}/nodes/{id}/history/{key}`   |                                                         | `[{value, from, to, by}]`: earlier values of a functional property |
| `GET`    | `/v1/{ns}/export.graphml`             |                                                         | the namespace as GraphML (retracted facts left out) |
| `GET`    | `/v1/{ns}/export/nodes.csv`, `/export/edges.csv` |                                              | as CSV, with typed headers (`name:int`, `:LABEL`) |
| `POST`   | `/v1/{ns}/import/graphml`, `/import/nodes.csv`, `/import/edges.csv` | the file (admin)          | `{nodes, edges, failed, errors}`; Grafeno ids update, other ids map to new nodes |
| `GET`    | `/v1/{ns}/changes`                    | `?since=&limit=`; `Accept: text/event-stream` or `?stream=true` for a stream | `{changes, last, oldest, gap}`, or server-sent events (`id` = sequence number; resume with `Last-Event-ID`) |
| `GET`    | `/v1/{ns}/events/{name}`              | `?since=&limit=`; `Accept: text/event-stream` or `?stream=true` for a stream | the events of a subscription with their values: `{changes, last, oldest, gap, through}` (continue after `through`), or server-sent events named `event`; needs `read` and `events` |
| `POST`   | `/v1/{ns}/ingest`                     | NDJSON: `{"key", "node": {…}}` or `{"key", "edge": {from, label, to, …}}` per line; ends may be `$key` | `{lines, nodes, edges, duplicates, failed, errors}`; a key sent again writes nothing |
| `GET`    | `/v1/{ns}/webhooks`                   |                                                         | the namespace's webhooks (admin) |
| `POST`   | `/v1/{ns}/webhooks`                   | `{url, secret, labels, since}`                          | a URL that receives the changes in order, signed with `X-Grafeno-Signature: sha256=<HMAC>` |
| `GET`    | `/v1/{ns}/webhooks/{id}`              |                                                         | its cursor, deliveries, failures and last error |
| `DELETE` | `/v1/{ns}/webhooks/{id}`              |                                                         | stops it |
| `GET`    | `/v1/background`                      |                                                         | what the `[background]` tasks did (owner) |
| `POST`   | `/v1/{ns}/communities/refresh`        | `{min_size, resolution, max_levels, labels, edge_labels, weight, central, terms, facts}`, all optional | `{nodes, levels, communities, created, updated, unchanged, removed, stale, modularity}` |
| `GET`    | `/v1/{ns}/communities`                | `?q=&level=&limit=`                                     | community summaries matching `q`, or the top ones without it |
| `GET`    | `/v1/{ns}/communities/{id}`           | `?limit=`                                               | `{community, children, members}`: drill down |
| `PUT`    | `/v1/{ns}/communities/{id}/text`      | `{text}` (`null` to undo)                               | an agent's own text for a summary |
| `POST`   | `/v1/{ns}/extract`                    | `{source_labels?, entity_labels?, identifiers?, min_phrase_sources?, force?, …}` (optional) | `{read, unchanged, mentions, linked, created, queued, found}` |
| `GET`    | `/v1/{ns}/review`                     | `?limit=`                                               | `[{key, kind, value, occurrences, candidates}]` |
| `POST`   | `/v1/{ns}/review/{key}`               | `{action: "link", node}` / `{action: "create", label?}` / `{action: "dismiss"}` | `{node, report}` |
| `POST`   | `/v1/{ns}/nodes/{id}/mentions`        | `{mentions: [{surface, label?, props?, start?, end?}], default_label?}` | the report |
| `GET`    | `/v1/{ns}/match-rules`                |                                                         | the match rules of the namespace |
| `PUT`    | `/v1/{ns}/match-rules/{label}`        | `{strong_keys?, fields: [{key, measure, weight?, …}], neighbors?, same?, review?}` | the rule (admin) |
| `DELETE` | `/v1/{ns}/match-rules/{label}`        |                                                         | 204 (admin)                   |
| `POST`   | `/v1/{ns}/match`                      | `{label, props, neighbors?, limit?, rule?}`             | `[{node, score, decision, decided_by, fields, neighbors}]` |
| `GET`    | `/v1/{ns}/matches`                    | `?label=&limit=`                                        | `[{a, b, match}]`: pairs the rule calls same or to review |
| `POST`   | `/v1/{ns}/touch`                      | `{ids}`                                                 | 204                           |
| `GET`    | `/v1/{ns}/weak`                       | `?half_life_days=&threshold=&label=&limit=`             | `[{node, strength, memory}]`  |
| `POST`   | `/v1/{ns}/archive`                    | `{ids}`                                                 | `{archived}`                  |
| `GET`    | `/v1/{ns}/consolidation`              | `?older_than_days=&min_size=&labels=a,b&limit=`         | `[{entity, memories}]`        |
| `POST`   | `/v1/{ns}/summaries`                  | `{text, sources, about?, name?, labels?, embedding?, archive_sources?}` | `{id}` (201)  |

- **Graph algorithms** (`pageRank`, `louvain`, `shortestPath`, … see
  [GQ](../../docs/query-language.md#graph-algorithms)): `args` are the
  arguments before the settings (node ids, `k`), `settings` the settings
  map (`{"labels": "Person", "weight": "km"}`). Rows hold nodes as `{id,
  labels, name}`. With `write: "rank"` (or `"@embedding"`) the result is
  stored on each node instead, from `column` (the one after `node` by
  default), and the answer is the number of nodes written.
- **Listings** return up to `limit` items (default 100, at most 1000) in id
  order; when `next` is set, pass it as `after` to get the following page.
- **node**: `{labels, props?, sources?, text?, embedding?}`. `PUT` and upserts
  replace the whole node; a missing `text` or `embedding` is removed, edges
  are kept.
- **patch**: `{props?, remove_props?, add_labels?, remove_labels?, text?,
  embedding?, importance?}`. `props` is merged, and a `null` in it is
  stored as a value (the key stays); `remove_props` lists keys to remove;
  `text: null` or `embedding: null` removes them.
- **edge**: `{from, label, to, props?, sources?, valid_from?, valid_to?}`.
- **filter**: `{labels?, valid_at?, known_at?}`. Without `known_at`, retracted
  edges are left out.
- **recall query**: `{text?, embedding?, limit?, labels?, candidates?,
  expand?, ranking?, include_archived?, touch?}`, where `expand` is `true`
  (default), `false`, or `{depth, direction, filter, decay, seeds, hubs,
  fanout}`, where `hubs` (default 1, 0 to turn it off) damps nodes by
  their number of edges and `fanout` (default 100, 0 for all) caps the
  neighbors followed from each node;
  `ranking` is `true` (default: importance and recency weigh in), `false`, or
  `{importance_weight, recency_weight, half_life_days}`; archived memories
  are left out unless `include_archived`; `touch` records an access to each
  result.
- **memory** (in node responses): `{importance, accesses, last_access}`.

### Batches

A batch runs in one transaction: if an operation fails, nothing is written
and the error message starts with `operation N:`. Operations can name the
nodes they create with `ref` (`"$name"`) and later operations can use that
name wherever they take a node id. Operations: `create_node`, `upsert_node`
(`node.id` required), `update_node` (`{id, patch}`), `delete_node`,
`create_edge`, `delete_edge`, `delete_edges_from` (`{from, label?}`),
`invalidate_edge` (`{id, at}`) and `retract_edge`.

Syncing a note and replacing its links in one request:

```json
{ "ops": [
  { "op": "upsert_node", "ref": "$note",
    "node": { "id": "0190f5c2-7a3e-7cde-8f01-00000000000a", "labels": ["Note"],
              "props": { "title": "Rust" }, "text": "The borrow checker…" } },
  { "op": "delete_edges_from", "from": "$note", "label": "links_to" },
  { "op": "create_edge", "edge": { "from": "$note", "label": "links_to",
                                   "to": "0190f5c2-7a3e-7cde-8f01-00000000000b" } }
] }
```

### Transactions

`POST /v1/{ns}/transactions` begins an explicit transaction; its id takes
statements at `/v1/{ns}/transactions/{id}/query` until `commit` or
`rollback`. Only the caller that began it may use it.

- **Reading** (`{"read_only": true}`): every statement sees the snapshot of
  its start; writes are refused.
- **Writing**: it holds the server's writer from its start to its end, so
  writes stay one at a time (serializable isolation). Other writes wait
  for it; a second writing transaction waits up to `wait_secs` and then
  gets 409 `transaction_busy`. Nothing it does is visible to others until
  it commits, and all of it is, at once.
- A statement that fails may have done part of its work, which cannot be
  undone alone: the transaction is then failed, its next statements are
  refused, and `commit` rolls it back (409 `rolled_back`).
- A transaction idle longer than its timeout (`timeout_secs`, at most
  `idle_timeout_secs`), or older than `max_secs`, is rolled back; its id
  then gives 404. A crash before `commit` leaves nothing of it.
- On a cluster it runs on the leader (other members forward its requests)
  in a transaction that captures what it does, and its `commit` replicates
  everything as one entry; if the leader changes meanwhile, nothing of it
  happened (409). With several groups, a transaction stays in the group of
  the namespace it began in.
- Users, roles and grants are managed outside transactions;
  `grafeno_transactions_open{kind}` in `/v1/metrics` counts the open ones.

### The API as a contract

The HTTP API is the only way into the engine: Grafeno Studio, the
TypeScript client and any other program use it and nothing else. Every
route is in the tables above (a test checks that the server answers
exactly these). `GET /v1/about` (public) says what the engine is:

```json
{
  "product": "grafeno",
  "version": "0.2.1",
  "api": { "version": 1, "supported": [1] },
  "layout": 18,
  "capabilities": ["namespaces", "nodes", "…", "cluster.groups", "query.split"],
  "auth": { "token": true, "accounts": true, "open": false, "password_sign_in": true },
  "cluster": null
}
```

Every response under `/v1` carries `x-grafeno-api: 1`. The API version
changes only when a change would break a client written for the previous
one (a route removed, a field renamed or retyped); what is added keeps the
version and shows up as a capability, so a client turns off what an older
engine lacks instead of failing. The capabilities:

| Capability | What it is |
| ---------- | ---------- |
| `namespaces`, `nodes`, `edges`, `facts` | the graph and its facts with valid and record time |
| `null_values` | a `null` property is stored as a value; `remove_props` removes keys (16.2) |
| `search.text`, `search.vector` | full-text and vector search |
| `algorithms.analysis`, `algorithms.simulation` | `algo.kCore`, `leiden`, `hits`, `eigenvector`, `harmonic`, `articulationPoints`, `bridges`, `spanningTree`, `maxFlow`, `bipartiteMatching`, `topologicalOrder`, `cycle`, `randomWalks`, `clustering`, `conductance`; `algo.distanceField`, `influence`, `floodFill`, `diffusion`, `automaton`, `assignment`, `minCostFlow` (16.8) |
| `vectors.health` | `GET /v1/{ns}/vectors/health`, `POST /v1/{ns}/compact?index=`, and vector graphs compacted on their own past `compact_vectors_at` (#12) |
| `search.vector.indexes` | named vector indexes (similarity, `m`, `ef`), filtered and several-vector searches, `/similar`, background builds (16.7) |
| `search.fulltext`, `search.hybrid` | full-text indexes with a query language and fragments; `POST /v1/{ns}/search` with a mode (16.6) |
| `recall`, `context`, `memory` | hybrid recall, context for a prompt, importance and accesses |
| `recall.hubs` | `hubs` and `fanout` in recall's `expand`: well-connected nodes damped, neighbors per node capped |
| `transactions` | `BEGIN`, `COMMIT` and `ROLLBACK` across requests (16.4) |
| `indexes.advanced` | indexes on several properties, on edges and over several labels; `NOT NULL`, type and key constraints (16.5) |
| `query`, `query.explain`, `batches`, `traversal`, `algorithms` | GQ, `EXPLAIN`, batches, k-hop, paths, subgraphs and neighborhoods, graph algorithms |
| `query.trace` | `TRACE` and `trace: true` on `/query`: the walk of a statement, event by event (16.9) |
| `resolution`, `match-rules`, `extraction`, `constraints`, `communities`, `summaries`, `indexes` | keeping knowledge in shape (phase 11) |
| `changes`, `changes.stream`, `webhooks`, `ingest` | the change log, as pages and as a stream, webhooks, NDJSON ingestion |
| `events.values` | event subscriptions (`CREATE EVENT SUBSCRIPTION`) whose events carry values, read at `/v1/{ns}/events/{name}` or posted to a webhook; off unless `[events] enabled = true` (16.10) |
| `export`, `import`, `import.csv`, `import.graphml`, `backup` | moving data in and out |
| `metrics`, `settings` | `GET /v1/metrics`, `GET /v1/settings` |
| `accounts`, `sessions`, `keys`, `roles`, `grants`, `policies`, `audit` | access (phase 14) |
| `cluster`, `cluster.groups`, `query.split` | clusters (phase 13) |

### Cluster

On a cluster member (`[cluster]` in `grafeno.toml`, see
[docs/install.md](../../docs/install.md)), any member takes any request:
a follower forwards writes (and linearizable reads) to the leader, waiting
for an election if there is none; without the leader's `api` address it
answers `not_leader` (421) naming the leader. Headers:

| Header | Direction | Meaning |
| --- | --- | --- |
| `x-grafeno-position` | response | the member's applied position (a Raft index, the same on every member) |
| `x-grafeno-after: N` | request | wait up to 5 s for the member to apply N (503 otherwise): read your writes |
| `x-grafeno-consistency` | request | `local` (default), `bounded` (to the leader when the member is over 1 000 entries behind) or `linearizable` (through the leader, with ReadIndex) |
| `x-grafeno-split` | request | `auto` (default: a read splits across members when its estimated cost reaches `split_cost`), `on` (whenever its shape allows) or `off`; a split answer has `split: {parts, here}` |
| `Idempotency-Key` | request | on `POST /v1/{ns}/query` (or `idempotency_key` in its body): a write with the same key runs once, and later sends return its first result |

A split read asks each member for its part with `POST /v1/{ns}/query/part`
(internal: the member that splits sends the plan and the position, and
the caller's credentials). Webhooks and background work run on the
leader only. Owner routes:

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/v1/cluster` | | `{id, role, term, leader, commit, applied, last_index, snapshot_index, voters, outgoing}` |
| `POST` | `/v1/cluster/voters` | `{voters: [ids]}` | `{index}` (on the leader) |
| `POST` | `/v1/cluster/compact` | | `{snapshot_index}` |
| `GET` | `/v1/cluster/groups` | | each Raft group: `{id, voters, held, status}` (13.5) |
| `GET` | `/v1/cluster/placement` | | `[{namespace, group, moving_to}]`: the namespaces placed outside group 0 |
| `POST` | `/v1/cluster/placement` | `{namespace, group}` | moves it without stopping it: `{from, to, records, catch_up_rounds, paused_ms, …}` |
| `GET` | `/v1/cluster/groups/{group}/status` | | one group's view, as `/v1/cluster` |
| `POST` | `/v1/cluster/groups/{group}/voters` | `{voters: [ids]}` | changes that group's voters |
| `POST` | `/v1/cluster/groups/{group}/compact` | | snapshots that group's database |

With several groups, a request for a namespace goes to its group (on any
member); writes during a move's pause answer `busy` (503) and should be
retried. Routes members use among themselves (the cluster secret, not for
clients):

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| `POST` | `/v1/cluster/audit` | audit events a follower saw | recorded by the leader |
| `POST` | `/v1/cluster/groups/{group}/import` | a move's copies of records | `{records}` |
| `POST` | `/v1/cluster/groups/{group}/fence` | the namespace being moved | fences its writes |
| `POST` | `/v1/cluster/groups/{group}/directory` | a namespace's new group | updates the directory |

`grafeno cluster status|voters|compact --url … --token … [--group N]` call
them; `/v1/cluster/groups/{id}/status`, `…/voters` and `…/compact` do the
same for another group. Status (13.6) also counts elections, leader
changes and snapshots sent and installed, gives the entry format the
group writes and what each member runs (`members`), and on the leader
each other member's lag (`peers`); `/v1/metrics` has them as
`grafeno_cluster_*` series per group. `GET /v1/backup?group=N` copies
another group's file. See [docs/cluster-runbooks.md](../../docs/cluster-runbooks.md).

## MCP

`grafeno mcp` speaks MCP (protocol versions 2025-06-18, 2025-03-26 and
2024-11-05) over stdin and stdout, and logs to stderr. Its tools:

| Tool       | What it does                                                                   |
| ---------- | ------------------------------------------------------------------------------ |
| `remember` | Stores a memory (text, optional name, labels, importance and links). A known `name` updates that entity instead. |
| `recall`   | Hybrid search; returns ids, names, snippets and why each result matched. Records accesses. |
| `context`  | Relevant memories and facts as compact text within a token budget, after the community summaries that match. |
| `relate`   | Records a fact (edge) between two nodes, with optional valid dates; says which facts it contradicted under a constraint (and ended). |
| `update`   | Replaces a node's text, renames it, merges properties (a `null` is stored), removes properties by name (`remove_props`), changes labels or importance. |
| `forget`   | Invalidates (default), retracts or deletes a fact; deletes a node.            |
| `get_node` | A node with its text, properties and connecting facts.                        |
| `query`    | A read-only [GQ](../../docs/query-language.md) statement; at most 200 rows.   |
| `merge`    | Folds a duplicate node into another one; refused when they disagree unless `force`. Returns a `merge_id`. |
| `undo_merge` | Undoes a merge by its `merge_id`: the merged node and its facts come back.   |
| `find_matches` | Before creating an entity, the nodes that may already be it, by the label's match rule, with a decision and each field's similarity. |
| `extract`  | Reads new and changed memories and links what they mention (names, emails, URLs, handles, hashtags, dates, amounts, recurring new names). |
| `add_mentions` | The entities a memory mentions, and the relations between them, from the agent's own reading: entities linked, created or queued; relations stored as facts with the memory as source, `inferred` unless said `stated`. |
| `review` / `resolve_review` | What extraction could not decide, and settling it (link, create, dismiss). |
| `maintain` | The namespace's size, its vector graphs' wear and contradictions on record (with `check: true`, an integrity check too); likely duplicates, weak memories and groups ready to consolidate. |
| `consolidate` | Stores a summary of several memories and archives them.                     |
| `overview` | For broad questions: community summaries that match a topic, or the top ones; with `id`, a community's parts and members. |
| `summarize_communities` | Finds the communities (and communities of communities) and writes or refreshes their summaries. |
| `set_summary` | Replaces a community summary's text with the agent's own (or undoes it). |
| `changes`  | What was written since a sequence number: created, updated and deleted nodes and facts. |
| `checkpoint` | Saves where a task stands (summary, done, next, decisions, files, the node it is about), after the task's previous checkpoint. |
| `resume`   | A task's latest checkpoint, how many came before, and what was written since; with no match, the tasks that have checkpoints. |
| `ingest`   | Stores a long text as a whole and its chunks: a document cut at headings and paragraphs, a conversation between turns, code at its definitions; then extracts what they mention. The same `source` again replaces it. |
| `contradictions` | Facts that contradicted earlier ones under a constraint, and what was done (the earlier one ended, the new one refused, or both flagged). |
| `timeline` | How a node changed, oldest first: its facts beginning and ending, earlier property values, merges, contradictions. |
| `feedback` | `useful` raises a memory's importance; `verified` records a check, with the `source` that supports it, and keeps what it was before; `obsolete` archives a memory or ends a fact; `wrong` archives a memory or retracts a fact. The verdict stays on the memory. |
| `palace_create` / `palace_create_room` | Makes a palace, or a room in one with its loci (places inside it); returns the one there is when it exists. |
| `palace_anchor` | Puts an existing memory at a locus of a room, without copying or changing it; with `remove`, takes the anchor away. |
| `palace_map` | The palaces with their rooms, loci and counts; with `room`, the memories at each locus. |
| `palace_walk` | From a topic or a node to the memories around it: the rooms that match, what is anchored there, and what the graph's relations lead to from those. |
| `palace_enter` | A room's context for the start of a session: its memories and facts as text within a token budget, and the latest checkpoint about the room. |

The `palace_*` tools arrange memories that already exist into places and
bring them back by where they are; see
[docs/mental-palace.md](../../docs/mental-palace.md).

Every tool takes an optional `namespace`, which defaults to `--namespace`.

`remember` and `relate` take `origin` (`stated`, `observed`, `inferred`,
`verified`) and `certainty` (0 to 1); `get_node`, `recall` and `context`
show them, and a record with neither reads as not stated. See
[docs/mental-palace/cognitive-memory.md](../../docs/mental-palace/cognitive-memory.md).

The MCP server opens the database file itself, so it cannot keep out the
user who starts it, but it fences the agent it serves:

| Flag | Effect |
| --- | --- |
| `--allow vault,team-*` | the namespaces the agent may use (all by default) |
| `--read-only` | only the tools that read are offered (and recall records no accesses) |
| `--as NAME` | its writes are recorded as `NAME` (`@recorded_by`) |
| `--own-only` | with `--as`: it may change only what it recorded |

Only one process can open a database file, so while an agent's MCP server
holds it, Grafeno Studio or `grafeno shell --db` cannot open it too. To
watch the agent's memory as it writes, let its MCP server also serve the
HTTP API on the same database, and connect to that:

```bash
grafeno mcp --db agent.redb --namespace me --listen 127.0.0.1:7810 --token "$GRAFENO_TOKEN"
```

Grafeno Studio connects to `http://127.0.0.1:7810` with that token, and
`grafeno shell --url http://127.0.0.1:7810 --token …` works too: what the
agent stores appears there at once, in queries and in the change log, and
what is written there the agent recalls. `--listen` takes `--token` (or
`GRAFENO_TOKEN`) and `--cors-origins`; without a token, keep it on
`127.0.0.1`. A second process on the same file is refused with a message
that says this.

An MCP client's configuration (the file and its place depend on the
client):

```json
{
  "mcpServers": {
    "grafeno": {
      "command": "grafeno",
      "args": ["mcp", "--db", "/path/to/memory.redb", "--namespace", "me"]
    }
  }
}
```
