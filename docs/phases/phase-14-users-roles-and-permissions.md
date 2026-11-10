# Phase 14: Users, roles and permissions (RBAC)

[All phases](README.md) · [← Phase 13: Distributed operation](phase-13-distributed-operation.md) · [Phase 15: Engine and interface apart →](phase-15-engine-and-interface-apart.md)

**Status:** done, in five steps.

A complete role-based access control, as most databases have: users who sign in, roles and privileges granted with `GRANT` and `REVOKE`, rights down to labels, properties and records, and an audit log.

Steps:

- [14.1 Accounts and sign-in (done)](#141-accounts-and-sign-in-done)
- [14.2 Roles and privileges (done)](#142-roles-and-privileges-done)
- [14.3 Managing access in GQ, the API and the tools (done)](#143-managing-access-in-gq-the-api-and-the-tools-done)
- [14.4 Enforcement down to labels, properties and records (done)](#144-enforcement-down-to-labels-properties-and-records-done)
- [14.5 Audit and accountability (done)](#145-audit-and-accountability-done)

**Where it starts from (10.1).** Access is by principals: named API keys
(stored as SHA-256), each with grants per namespace or namespace prefix of
one of three fixed roles (`read`, `write`, `admin`), a global `owner` flag,
and an `own_only` rule (change only what you recorded). The token in
`grafeno.toml` is the owner; a server without one is open until its first
key. Every surface (HTTP, MCP, shell, GQ, export, the desktop bridge)
checks the same grants, and writes record who made them (`recorded_by`).
What is missing next to a mature database server: people who
sign in with a password and get a session, roles you define and nest,
privileges finer than three levels (per operation, per label, per
property, per record), `GRANT` and `REVOKE` in the query language, expiry
and rotation of credentials, and a record of who did what to security.

The guidelines hold: local and CPU only (no identity service required),
agent-agnostic (agents and services are accounts like people), and a
single-user setup stays as simple as now: one owner, no ceremony. In a
cluster (phase 13) users, roles, grants and sessions are replicated state,
written through the log and enforced the same on every member. Each step
ends with tests (the 10.1 leak matrix grows with each privilege), a
measurement where it costs something, documentation and its own commit.

## 14.1 Accounts and sign-in (done)



- Accounts of two kinds: *users* (people: a name, a password, sessions)
  and *service accounts* (agents and applications: API keys only). Both can
  hold API keys; a key belongs to an account, can expire, be rotated and be
  scoped down (a subset of the account's rights, a namespace, read only).
- Passwords hashed with Argon2id (parameters measured on the target
  hardware), a password policy (length, a list of common passwords), and
  throttling and lockout after failed attempts, per account and per address.
- Sign-in over HTTP returns a session (a short-lived token and a refresh
  token, revocable, listed per user); the Studio and the desktop app get a
  sign-in screen instead of pasting a token; the shell signs in with a
  prompt; MCP keeps using keys.
- Bootstrapping: `grafeno users create --superuser` (or the first sign-in
  on an open server) creates the first administrator. The static token
  becomes an optional break-glass credential, off unless configured.
- Migration: every existing principal becomes a service account with the
  same rights (its grants become roles, below); `recorded_by` keeps the
  names it had. A new layout version, with its upgrade and fixture.
- Done when: sign-in, refresh, expiry, revocation, lockout and key scopes
  are tested over HTTP, the shell and the Studio, and the 10.1 matrix passes
  with users, service accounts and scoped keys.

As built (`grafeno/src/access.rs`, `grafeno-server/src/auth.rs`,
`accounts.rs`, `users.rs`, the shell, the TypeScript client, the Studio):

- Accounts (`Account`): a name, a kind (`user` or `service`), an Argon2id
  password hash for users, grants per namespace pattern (read, write,
  admin), `superuser`, `own_only`, `disabled`. Keys (`ApiKey`) belong to
  an account: a SHA-256 of the secret, an optional expiry, an optional
  scope (namespace patterns and/or read only, which only narrows the
  account's rights), and when they were rotated. Sessions (`Session`): an
  access token and a refresh token, each stored as a SHA-256, with their
  expiries and where they signed in from. Tokens carry their session's id
  (`gfs_<id>.<secret>`, `gfr_<id>.<secret>`), so a request finds its
  session without an index.
- Layout 15: `accounts`, `api_keys` and `sessions` replace `principals`;
  each principal becomes a service account with its grants, owner flag
  (as `superuser`) and `own_only`, and a key of the same name and secret,
  so existing secrets keep working and `recorded_by` keeps its names.
  Fixture `layout-v14` (with an owner key and an own-only key) checks the
  upgrade, and every older fixture upgrades through it. All three are
  replicated state: they travel as entry format 3, and an entry of a
  member before 14.1 that carries a principal is applied as an account
  and a key.
- Passwords: Argon2id with OWASP's parameters (19 MiB, 2 passes, 1 lane),
  measured at 19 ms a check on the laptop of the measurements: 53 sign-ins
  a second on one core, 141 on 16 threads (memory bound). A wrong name
  costs a check too. The policy: 10 to 128 characters, not containing the
  account's name, not one of the commonest passwords, not one repeated
  character. Five failures in 15 minutes lock an account, twenty refuse an
  address (`[auth]` sets them); the counts live in each member's memory.
- Sign-in over HTTP: `POST /v1/auth/signin` gives a 15-minute access token
  and a 30-day refresh token (`[auth]`); `/v1/auth/refresh` replaces both,
  and a refresh token used a second time ends the session (one of the two
  users stole it); `/v1/auth/signout`, `GET /v1/auth/sessions` (a
  superuser's: anyone's), `DELETE /v1/auth/sessions/{id}`,
  `/v1/auth/password` (asks the current password, ends the other
  sessions). Disabling an account or setting its password ends its
  sessions; a disabled account's keys stop working.
- Accounts are managed by superusers (`/v1/accounts`, `grafeno users`);
  any account lists, makes (scoped or not), rotates and revokes its own
  keys (`/v1/keys`, `/v1/keys/{name}/rotate`); a key made the 10.1 way
  (`grants`, `owner`) makes a service account of its name.
- Bootstrapping: `grafeno users create NAME --superuser` (on the file or
  through the server), or, on an open server, the first sign-up
  (`/v1/auth/setup`, or the Studio's "Create the first account") makes a
  superuser and closes the server. The static token stays as a
  break-glass credential when it is configured.
- Clients: the shell signs in with `--user` (a prompt, or
  `--password-stdin`) and renews the session when it expires; the
  TypeScript client has `signIn`, `refreshSession` (also done by itself),
  `signOut`, sessions, password, accounts, `rotateKey`, and takes kept
  tokens with `onSession` to keep new ones; Grafeno Studio (and so the
  desktop app) has a sign-in screen, a first-account screen on an open
  server, keeps and renews the session, and signs out on the server. MCP
  keeps using the database directly.
- In a cluster: keys and sessions written on the leader reach the other
  members through the log; a member that does not find one yet looks
  again for a second. This fixed a bug of 13.2 to 13.6: a key made
  through the leader did not work on followers until they restarted, as
  only the member that served the request reloaded its keys.
- Tests: `grafeno-server/tests/accounts.rs` (the first account of an open
  server; sign-in, refresh, replaced tokens, expiry, a refresh token used
  twice, sessions listed and ended, sign-out; lockouts per account and
  per address; the password policy and changes; disabled accounts;
  service accounts without passwords; keys scoped to namespaces or to
  reading, scopes that cannot widen rights, rotation, users' own keys,
  expiry), `access.rs` (the 10.1 matrix with service accounts),
  `shell.rs` (`grafeno users` and the shell signing in), `cluster.rs` (a
  key made through the leader used on followers; a session made on one
  follower used on the other and refreshed on the leader),
  `grafeno/tests/upgrade.rs` (layout 15 from every layout),
  `replication.rs` (accounts, keys and sessions replayed), the TypeScript
  client's `accounts.test.ts`, and the Studio's screens checked in a
  browser.
- Not built in this step: roles beyond the three levels (14.2), managing
  access in GQ (14.3), finer enforcement (14.4), the audit log (14.5),
  lockout counts shared across members, and a
  prompt in the Studio of an open server to protect it (its first-account
  screen shows when it is not connected).

## 14.2 Roles and privileges (done)



- A catalog of privileges, each on a kind of object:
  - the server: create namespaces, manage accounts, manage roles and
    grants, back up and restore, see metrics and the audit log, operate the
    cluster;
  - a namespace: read (match, search, recall), and separately create,
    update, delete, merge, retract, import, export, manage indexes and
    constraints, run procedures and algorithms, drop;
  - labels inside a namespace: read or write nodes with a label, edges with
    a label (`Person` readable, `Salary` facts not);
  - properties: read or write one property of a label (`Person.email`
    hidden from a role that can read people).
- Roles: built-in ones that map today's levels (`reader`, `editor`,
  `admin` per namespace; `superuser` for the server; `public`, which every
  account has) and roles defined by administrators. Roles are granted to
  accounts and to other roles (a hierarchy, cycles refused); an account's
  rights are the union of its roles'. Privileges can be granted and, where
  a broad grant needs an exception, denied; a denial wins over any grant,
  so "read everything in `crm` except `Salary`" is one grant
  and one denial.
- Effective rights are computed once per account and cached, recomputed
  when a grant changes, so checking a request stays a lookup.
- Done when: a property test draws random role graphs, grants and denials
  and checks the computed rights against a direct model; the cost of a
  check is measured with thousands of roles.

As built (`grafeno/src/rights.rs`, `grafeno-server/src/auth.rs`,
`roles.rs`):

- Privileges (`Privilege`): an action (`Act`), an object and a `deny`
  flag. On the server: `create_namespace`, `manage_accounts`,
  `manage_roles`, `backup`, `metrics`, `audit`, `cluster`. On a namespace
  pattern (a name, a prefix with `*`, or `*`): `read`, `create`,
  `update`, `delete`, `merge`, `retract`, `import`, `export`, `schema`
  (indexes, constraints, match rules, rebuilds, webhooks), `algorithms`,
  `drop`, `create_namespace`. On a label (of nodes, or of edges) or a
  property of a label, in a namespace pattern: `read` and `write`. A
  privilege that does not fit its object is refused. `GET /v1/privileges`
  lists them.
- Roles (`RoleDef`, table `roles`, layout 16): privileges, built-in levels
  on namespace patterns (`reader` is read, export and algorithms;
  `editor` adds create, update, delete, merge and retract; `admin` adds
  import, schema, drop and creating the namespace) and other roles. An
  account holds grants (the levels of 14.1), `roles` and `superuser`; a
  role named `public`, once defined, is every account's. Writing a role
  that would reach itself, or names a role that does not exist, is
  refused (the check walks only what the role reaches); deleting a role
  takes it from every account and role. The built-in names are taken.
  Roles travel in the replication log as entry format 4, and backups
  carry them.
- Rights (`Rights`): the union of the account's grants and of every role
  it reaches, as bit sets per pattern (exact names in a hash map,
  prefixes in a list); a denial at any level wins. A label check allows
  the action on the namespace or `read`/`write` on the label, and a
  property check also the property's; a denial on any of them refuses.
- The cache: each account's rights are computed once and kept with the
  database's access generation, a counter that every change of an
  account or a role bumps (on every member, as the log applies it), so
  a request checks a number and a bit; a change applies to the next
  request.
- Routes map to actions: reads to `read`; creating nodes and facts,
  ingesting and extracting to `create`; `PATCH`, `PUT`, touching and
  archiving to `update`; `DELETE` to `delete`; merges and undoing them to
  `merge`; retracting and invalidating to `retract`; imports and exports
  to `import` and `export`; algorithms to `algorithms`; indexes,
  constraints, match rules, rebuilds and webhooks to `schema`; dropping a
  namespace to `drop`; `/v1/metrics`, `/v1/backup`, `/v1/cluster`,
  `/v1/accounts` and `/v1/roles` to the server privileges. A writing GQ
  statement needs `create`, `update` and `delete` until 14.4 tells them
  apart. A key's scope still narrows: its namespaces only, reading only
  (`read`, `export`, `algorithms`) when read-only, and no server
  privilege. Managing accounts does not reach superusers (making,
  changing or deleting one), or it would be a way to become one; keys
  and sessions of other accounts stay with superusers.
- The API: `GET /v1/roles`, `GET`, `PUT` and `DELETE /v1/roles/{name}`,
  `roles` in `/v1/accounts`, and `effective_roles` in `/v1/whoami`.
- Measured with 5 000 roles, each with four privileges and two roles
  below it, and an account holding 1 000 of them (2 811 reached):
  computing the rights takes 17 ms (once per change of access), a
  namespace check 93 ns and a label or property check 271 ns; writing the
  5 000 roles takes 0.6 s (33 s before the cycle check walked only what
  a role reaches).
- Tests: `grafeno/tests/rights.rs` (2 000 random role graphs, grants and
  denials against a direct model of the rules, for the server,
  namespaces, labels and properties; roles in the database: cycles,
  unknown roles, deletion), `grafeno-server/tests/roles.rs` (patterns
  with a denial, a role reached through another, a custom editor without
  `delete`, cycles and misplaced privileges refused, server privileges,
  managing accounts but not superusers, `public`, scoped keys), the
  route table in `auth.rs`, `replication.rs` (roles replayed) and
  `upgrade.rs` (layout 16 from every layout, fixture `layout-v15`).
- Not built in this step: label and property rights are computed and
  checked by `Rights` but requests do not consult them yet (14.4 filters
  what queries and routes return by them); `GRANT`, `REVOKE` and roles in
  GQ, the CLI, the TypeScript client and the Studio (14.3); `audit` waits
  for the audit log (14.5).

## 14.3 Managing access in GQ, the API and the tools (done)



- GQ statements, run by accounts that may manage access: `CREATE USER`,
  `ALTER USER` (password, status, expiry), `DROP USER`, `CREATE ROLE`,
  `DROP ROLE`, `GRANT` / `DENY` / `REVOKE` privileges `ON` a namespace,
  labels or properties `TO` a role, `GRANT ROLE … TO` an account,
  `SHOW USERS`, `SHOW ROLES`, `SHOW PRIVILEGES [FOR …]`, `SHOW CURRENT
  USER`; `EXPLAIN` of a denied statement says which privilege it lacks.
- The same over HTTP (`/v1/admin/users`, `/roles`, `/grants`), in the CLI
  (`grafeno users`, `grafeno roles`, `grafeno grants`), in the TypeScript
  client, and as views in the Studio and the desktop app.
- Done when: every statement and endpoint is in the conformance and HTTP
  suites, including those refused for lack of the privilege to manage.

As built (`grafeno-query/src/access.rs`, `grafeno/src/rights.rs`,
`grafeno-server/src/admin.rs`, `access_cli.rs`, the TypeScript client,
the Studio's Access view):

- Statements: `CREATE USER name [IF NOT EXISTS] SET PASSWORD …`,
  `CREATE SERVICE ACCOUNT name`, `ALTER USER name SET PASSWORD | STATUS
  ACTIVE | SUSPENDED | EXPIRES <time or null> | SUPERUSER true | false |
  DESCRIPTION …` (several `SET`s at once), `DROP USER name [IF EXISTS]`,
  `CREATE ROLE name [IF NOT EXISTS] [SET DESCRIPTION …]`, `DROP ROLE`,
  `GRANT` / `DENY` privileges `ON SERVER | NAMESPACE p | NODES Label IN p |
  EDGES label IN p | PROPERTY Label.key IN p TO` roles, `REVOKE [GRANT |
  DENY] … FROM` roles, `GRANT ROLE r TO` accounts or roles, `REVOKE ROLE
  … FROM`, `SHOW USERS`, `SHOW ROLES` (the built-in ones first), `SHOW
  PRIVILEGES` (the catalog), `SHOW PRIVILEGES FOR name` (each with the
  role it comes from), `SHOW CURRENT USER`. Privileges are the actions of
  14.2 or the levels `reader`, `editor`, `admin` on a namespace (denying a
  level denies each of its actions). Names with `-` and patterns are
  quoted (`'team-*'`).
- Who may: the server passes the caller in the query's limits
  (`AccessContext`): managing accounts needs `manage_accounts`, roles and
  grants `manage_roles`, a role given to an account `manage_accounts` and
  to a role `manage_roles`; only superusers make, change, suspend or drop
  superusers; anyone lists their own privileges and the catalog. The
  statement checks this as it runs, so it needs no privilege on the
  namespace it is sent to (`POST /v1/{ns}/query` leaves the check to the
  handler, which still asks `read` for everything else, `EXPLAIN`
  included). Passwords go through the policy and Argon2id (a hasher the
  server and the local shell give; the library alone cannot set them).
  Suspending an account or a new password ends its sessions.
- `EXPLAIN` of a statement the caller may not run describes it and adds a
  `DENIED` step naming the privilege missing (`` "ana" has no
  `manage_accounts` privilege on the server ``, or `` no `create`
  privilege on the namespace "team-a" ``), and runs nothing.
- Accounts can expire (`expires_at`, a new field read as absent from
  older records, so no layout change): an expired account neither signs
  in nor uses its keys or sessions (401 `account_expired`).
- Without a token, accounts close the server: a change that would leave
  accounts but no active superuser, or no accounts at all after there
  were some, is refused, in GQ and over HTTP. This closes a gap of 14.1,
  where `POST /v1/accounts` on an open server could make a first account
  that was not a superuser, and nobody could manage the server after.
- The API: `/v1/admin/users` and `/v1/admin/roles` (the accounts and
  roles of 14.1 and 14.2 under the design's names; `PATCH` takes
  `expires_at`), `GET /v1/admin/grants?holder=`, `POST /v1/admin/grants`
  (`{privileges, on, to, deny}`), `/v1/admin/grants/revoke` (`{…, from,
  which}`), `/v1/admin/grants/roles` and `/v1/admin/grants/roles/revoke`;
  they run the same code as the statements. `roles`, `privileges` and
  `admin` became reserved namespace names.
- The tools: `grafeno roles list | create | delete | show` and `grafeno
  grants catalog | grant | deny | revoke | add-role | remove-role` (on a
  file or through the server; each is an access statement); `grafeno
  shell` runs them too and sets passwords through the policy; the
  TypeScript client has `roles`, `putRole`, `deleteRole`, `grant`,
  `revoke`, `grantRoles`, `revokeRoles`, `privilegesOf` and
  `privilegeCatalog`; the Studio (and so the desktop app) has an Access
  view: who the caller is, the accounts with their roles (give, take,
  suspend), and each role's privileges (grant, deny, revoke, create and
  delete roles).
- Tests: the conformance suite (every statement, 26 scenarios, with `*`
  for values such as creation times), `grafeno-query/tests/access.rs`
  (managing accounts and roles apart, superusers only a superuser's,
  passwords through the hasher, suspension ending sessions, what
  `EXPLAIN` lacks), `grafeno-server/tests/admin.rs` (statements over
  HTTP: a user made in GQ signs in, grants without reading the
  namespace, refusals for lack of `manage_accounts`, `EXPLAIN` with
  `DENIED`, expiry, a dropped account's key; the admin endpoints; an open
  server keeping a superuser), `shell.rs` (`grafeno roles`, `grafeno
  grants`, a local shell setting a password), the client's
  `accounts.test.ts`, and the Studio's view checked in a browser.
- Not built in this step: label and property rights are still not
  enforced by queries (14.4); the audit log (14.5); the Studio sets no
  passwords or expiry dates (the API and GQ do).

## 14.4 Enforcement down to labels, properties and records (done)



- Reads: nodes and edges a caller may not read are absent from every
  result, as if they did not exist (patterns, `count`, paths, `EXISTS`);
  hidden properties are absent from records, maps and projections, and a
  filter on one finds nothing. The planner adds these restrictions where it
  starts a pattern, so they cost no extra pass.
- Search and memory: full-text, vector (the open decision about filtering
  inside HNSW instead of after it is settled here, with recall measured),
  fuzzy search, `recall` and `context` see only what the caller may read;
  graph algorithms run on the visible subgraph; exports, the change log and
  server-sent events carry only what the subscriber may read.
- Writes: each change is checked against the label, property and operation
  privileges it needs; a statement that would touch something forbidden
  changes nothing and fails with the privilege it lacks.
- Record-level policies, generalizing `own_only`: a role can be limited to
  records that match a condition (`n.@recorded_by = current_user()`,
  `n.tenant = $account.tenant`), for reads, writes or both (row-level
  security).
- Done when: the leak matrix covers every privilege on every surface, and
  property tests check that random queries under random rights never return
  a hidden node, edge or property, and never change one; the overhead of
  enforcement on reads is measured with and without restrictions.

As built (`grafeno/src/view.rs`, `rights.rs`, the reads of `db.rs`,
`traverse.rs`, `changes.rs`, `dump.rs` and `algo`, the checks of the
write transaction, `grafeno-query`'s `Context` and procedures, the server's
routes):

- A `View` is an account's rights record by record. A node is visible when
  no label of it is denied reading and the namespace or one of its labels
  grants it; an edge when its label may be read and both ends are visible;
  a property is hidden when it is denied on a label of its record. In a
  namespace without label or property rules or policies, all its records
  are visible or none, and nothing is checked per record (a check is a
  cached lookup per namespace).
- Record policies (`Policy`, held by roles): on a namespace pattern,
  optionally the nodes of a label, for reads, writes or both, a condition:
  `@recorded_by = current_user()`, or a property equal to the caller's
  name, one of its attributes (`$account.team`, `Account.attributes`, new)
  or a value. They narrow, like denials: a record must match every policy
  that covers it (restrictive policies), so a role cannot
  widen what another one limited. In GQ: `RESTRICT role ON NAMESPACE p
  [NODES Label] [FOR READS | FOR WRITES] WHERE …`, `UNRESTRICT …`, `ALTER
  USER … SET ATTRIBUTE key = value`; `SHOW PRIVILEGES FOR` lists policies.
  Roles with policies and accounts with attributes travel as entry format
  5, so a leader does not send them to a member that would drop them.
- Reads in GQ: the query's `Context` (every node, edge, page, neighbor,
  index, text, embedding and memory read) leaves out what is hidden and
  strips hidden properties, so patterns, `count`, paths, `EXISTS`, maps
  and projections never see them; a filter on a hidden property finds
  nothing, and an index on one finds nothing. Paging keeps reading until a
  page is full, so pages stay what they were. Searches (`db.search`,
  `db.searchVector`, `db.searchFuzzy`) ask again for more while hidden hits
  crowd out the visible; `db.labels`, `db.edgeLabels`, `db.propertyKeys`
  and `db.stats` count what is visible; algorithms run on a projection of
  the visible subgraph (a hidden weight weighs 1); `history()` of a hidden
  property is empty; `EXPLAIN` shows no counts or estimates and `SHOW
  NAMESPACES` no counts where they would tell how much is hidden.
- Reads elsewhere: a snapshot can carry a view (`ReadTx::with_view`, which
  takes the snapshot, so a shared one never gets a reader's view), and its
  reads filter: records, pages, scans, traversals (which never go through a
  hidden node or edge), searches, names, indexes, statistics, exports (JSON
  Lines, GraphML, CSV) and the change log (a change is seen when its record
  is; a deleted one by its labels, unless policies apply). The server gives
  every route's snapshot its caller's view, the server-sent events stream
  its subscriber's, and the namespace list counts what each caller sees.
- Vector search, the open decision: filtering after the graph walk, and
  widening the walk (four times as many candidates each round) until
  enough visible hits are found. With 45% of 50 000 vectors visible, the
  recall@10 among the visible is 0.972 (0.912 for the same search without a
  view, which walks less), at 4.8 ms instead of 1.2.
- Features that read every record of a namespace and whose results would
  tell what is hidden (entity resolution and `db.match`, duplicates, match
  pairs, merges, community summaries, extraction, the review queue, decay
  and consolidation, integrity checks and vector recall) answer 403 to a
  caller who sees the namespace record by record.
- Writes: the write transaction checks each change against its actor's
  view: the record must be visible (or it is not found), its labels allow
  the operation (create, update, delete, merge, retract: at the namespace,
  or `write` on the label) and no label denies it, the properties written
  are not denied writing, write policies hold before and after, and an
  edge's ends are visible. A node with an edge its caller cannot see cannot
  be deleted by it (refused without naming the edge). A GQ statement that
  would make a forbidden change fails whole, naming the privilege; one that
  matches hidden records does not see them. A writing statement needs a way
  to write in the namespace (any of create, update, delete, merge, retract,
  or `write` on a label), no longer all three of 14.2.
- Measured on 50 000 people (a tenth also `Salary`), best of three: a view
  that reads the whole namespace costs nothing; one decided per record
  (`Salary` denied, `email` hidden, a team policy) adds 25% to 51% to
  scans, filters and traversals, and nothing to full-text search (the
  table under Measurements).
- Tests: `grafeno-query/tests/visibility.rs` (300 random graphs and rights
  each: seven read queries never return a hidden node, edge or property and
  return exactly the visible, counts and reachability included; six write
  statements never change a hidden record, and change nothing when they
  fail; checked sensitive by breaking the view twice),
  `grafeno-server/tests/leaks.rs` (the leak matrix: listings, reads by id,
  neighbors, k-hop, path, subgraph, neighborhoods, read-many, text and
  vector search, recall, context, GQ and its procedures and algorithms,
  JSON Lines, GraphML and CSV exports, the change log, statistics, the
  namespace list, history; the refused features; writes stopped by
  visibility, labels and properties), and the client's `accounts.test.ts`.
- Not built in this step: policies on edges by label (a policy without a
  label covers every node and edge), conditions beyond equality, and
  filtering inside the HNSW walk; webhooks carry every change (they are
  set up with `schema`, an administrator's privilege); the change log's
  `last` still counts hidden changes.

## 14.5 Audit and accountability (done)



- An audit log of security events: sign-ins and failures, lockouts, session
  and key creation and revocation, every change to accounts, roles, grants
  and policies, and refused requests, with who, when, from where and what.
  Stored like the change log (numbered, retained by age and size, its own
  privilege to read), queryable in GQ, streamable over server-sent events,
  exportable as JSON lines.
- `recorded_by` names the account (and the key or session) behind every
  write; administrators can list and end any account's sessions.
- Done when: every security event appears exactly once, refused requests
  included, and the log survives restarts, failover and backups.

As built (`grafeno/src/audit.rs`, the access tables' writes,
`grafeno-server/src/audit.rs`, the server's middleware and sign-in):

- An event (`AuditEvent`) has a sequence number, a time, a kind, the
  account that acted, the credential behind the request (`key:name`,
  `session:id`, `token`), the address, the target, the outcome (`ok`,
  `refused`, `failed`) and a detail without secrets or hashes. Layout 17
  adds the `audit` table; the log is kept by count and age (`[audit]`
  `max_entries`, `max_age_days`; ten million events and a year by
  default), pruning at most a hundred events per append.
- Changes to accounts, keys, sessions and roles append their event in the
  transaction that makes them (`account_created` / `_changed` / `_deleted`
  with what changed, a new password named but never shown;
  `key_created` / `_rotated` / `_changed` / `_revoked`;
  `session_created` / `_refreshed` / `_ended`; `role_created` /
  `_changed` / `_deleted` with the role's privileges, levels, roles and
  policies, so every grant, denial and policy is there), whether they come
  from the API, GQ or the command line: an event is there exactly when its
  change is. The server appends what happens outside the database:
  `signin_failed` (wrong name or password, locked, disabled, expired),
  `lockout` (when a failure locks an account or an address),
  `refresh_reused`, and `refused` for every 401 and 403, refused by the
  middleware or by a handler (a GQ statement, a record), with the method
  and path. Signing in audits itself (`session_created`).
- Writes carry their origin (`Origin`: account, credential, address),
  which the audit log and the change log record: a change's entry names
  the key or session it was written with (`via`), beside `by`.
  `recorded_by` on nodes and edges keeps naming the account.
- Replicated state: events travel in the replication log as themselves
  (entry format 6), a member that applies an entry writes them as they are
  and numbers its own after them. On a member that does not lead, an
  event the server records goes to the leader (`POST
  /v1/cluster/audit`, with the cluster secret), so it is written once
  wherever it happened. Backups carry the log.
- Reading it needs `audit` on the server: `GET /v1/audit?since=&limit=` (a
  page with `last`, `oldest` and `gap`), the same with `Accept:
  text/event-stream` (a stream that goes on, resumable with
  `Last-Event-ID`), `?format=jsonl` (JSON Lines), and `CALL db.audit(since,
  limit)` in GQ (from a namespace the caller reads); the TypeScript client
  has `audit()`.
- Sessions: those who manage accounts list and end other accounts'
  sessions (`/v1/auth/sessions?account=`), not superusers'.
- Measured in memory: 24 600 events a second appended one write each (a
  refused request), 211 000 a thousand a write, 1.4 million a second read
  back; a change of an account with its event takes 49 µs.
- Tests: `grafeno/tests/audit.rs` (each change of access audited once,
  in order, with its origin and no secret; none for a rolled back change;
  replicated once, numbering going on on the member; surviving a reopen
  and a backup; retention), `grafeno-server/tests/audit.rs` (every
  security event over HTTP exactly once: sign-ins, failures, the lockout,
  sessions, keys, accounts and roles from the API and GQ, refusals by the
  middleware and by a handler; reading needs the privilege; pages, JSON
  Lines, GQ and the stream; managing other accounts' sessions), the
  cluster test (a request refused on a follower recorded once, the log
  whole after the leader dies and numbered on without a gap), `upgrade.rs`
  (layout 17 from every layout, fixture `layout-v16`), and the client.
- Not built in this step: the Studio shows no audit view; events of a
  request refused while a cluster has no leader are lost (reported on the
  member's standard error).

Identities from outside (OpenID Connect, client certificates) are not
planned: as in most database servers, the accounts are the engine's own,
created in the engine (`CREATE USER`, `grafeno users`). An application with
its own users signs them in itself and reaches Grafeno as its accounts.

**Measures for the phase:** the cost of authorization on reads and writes
(none, a namespace grant, label and property restrictions, record
policies) against 13's numbers; sign-ins per second and their CPU with the
chosen Argon2id parameters; vector search recall and latency with
visibility filters.
