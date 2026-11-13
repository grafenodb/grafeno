# Security

How Grafeno protects what it holds, what an operator has to do for the
rest, and what two outside reviews found in November 2026 and what was done
about each finding.

- [What the engine does](#what-the-engine-does)
- [What the operator does](#what-the-operator-does)
- [Review of November 2026](#review-of-november-2026)
- [Open decisions](#open-decisions)

## What the engine does

- **Credentials.** Passwords are hashed with Argon2; a wrong name costs as
  much as a wrong password. API keys and session tokens are stored as
  hashes. Sign-ins lock an account after five failures and an address after
  twenty, for fifteen minutes; an attempt is counted when it begins, so
  attempts in parallel cannot pass the limit together. Password checks run
  a few at a time, and names, passwords and sign-in bodies past their size
  are refused before any work is done on them.
- **Rights.** Roles and privileges down to labels, properties and records
  (phase 14). A key may be narrowed to some namespaces or to reading, and a
  key made with a scoped key never reaches further than it. A writing
  transaction needs a right to write.
- **Audit.** Sign-ins, lockouts, sessions, keys, changes to accounts and
  roles, and refused requests are written to the audit log. Refusals are
  sampled: ten a minute from one source, the rest counted in
  `grafeno_audit_refusals_left_out_total`.
- **Limits.** Queries have limits on time, reads, rows and memory; request
  bodies on size; open transactions on number; streaming ingestion on the
  length of a line (4 MiB); an MCP message on its length (16 MiB); edit
  distances on the texts they compare.
- **Failures.** A panic in a request answers 500 with a reference; what
  failed is in the server's log under that reference.
- **Webhooks** reach only public addresses unless
  `[background] webhooks_to_private = true`; redirects are not followed.
- **Clusters.** Members prove themselves to each other with the shared
  secret on every message, forward and status request. An empty secret is
  refused; the example one is warned about.
- **Files.** On Unix the settings file is `0640` and data folders made by
  Grafeno `0750`. On Windows they are SYSTEM's, the administrators' and the
  account's that made them; the installer does the same to
  `C:\ProgramData\Grafeno` and the data folder on every install and upgrade.
  `grafeno doctor` reports files other users can read.

## What the operator does

- **Transport.** Grafeno speaks plain HTTP, between clients and servers and
  between members. Put a TLS proxy in front of the API and keep the
  members' port (`[cluster] addr`) on a private network. Tokens, keys,
  sessions and the cluster secret travel in headers and are readable by
  anyone on the path otherwise.
- **First run.** A server with no token and no account lets every caller do
  everything, including make its first superuser. It warns when it listens
  on a network address in that state. Set a token (`grafeno config init`)
  or create the first account before the server is reachable.
- **Backups.** The `backup` privilege copies the whole database file, every
  namespace in it. Grant it as you would read access to everything.
- **Secrets.** Change the example `secret` of `packaging/compose` before
  using that cluster for anything. Compose refuses to start without
  `GRAFENO_TOKEN`.
- **Large administrative operations.** Backups, exports, imports and the
  audit log in JSON Lines are built in memory. Run them when the server has
  room, and few at a time.

## Review of November 2026

Two reviews of version 0.2.1, with tests run against local processes. Each
finding was confirmed in the code before it was changed, and each change
has a test that fails without it.

| Finding | Severity given | What was done |
| --- | --- | --- |
| A scoped key made an unscoped key for its superuser account | Critical | Fixed: a new key takes the caller's scope, a wider one is refused. |
| Settings and data readable by every local user on Windows | Critical | Fixed in `config init`, the instance commands and the installer; `doctor` reports it. The installer change was not run here. |
| A client marked its own request as forwarded and got a stale "linearizable" read | High | Fixed: forwarded only with the cluster secret. |
| Every wrong credential wrote an audit event | High | Fixed: sampled per source and counted. |
| Sign-in throttle passed parallel attempts; its maps and Argon2 unbounded | High | Fixed: counted at the start, maps bounded, checks a few at a time, sizes capped. |
| A reader began a writing transaction and held the writer | High | Fixed: a writing transaction needs a right to write. |
| A webhook to loopback, private networks or the metadata address | High | Fixed: public addresses only, checked at creation and before each delivery. |
| An ingested line held whole, past the body limit | High | Fixed: 4 MiB per line. |
| An `Append` at the snapshot's index stopped a member's Raft thread | High | Fixed: malformed appends and appends over committed entries are dropped. |
| An empty cluster secret let anyone in | High | Fixed: refused at start, and never matches. |
| `COUNT { … }` ignored the query's `VALID AT` | High | Fixed, also in split reads. |
| `text.damerau` kept the whole table | High | Fixed: three rows; long comparisons refused. |
| `GET /status` on the members' port answered anyone | Medium | Fixed: needs the secret. |
| The members' port read a body before checking the secret | Medium | Fixed: the secret first, 64 KiB at most when refused. |
| Exporting everything needed only read rights | Medium | Fixed: needs the export right, as a namespace by name does. |
| A year past 292 000 wrapped the clock | Medium | Fixed: dates up to year 200 000, checked arithmetic. |
| A panic's message reached the client | Medium | Fixed: a reference instead. |
| An MCP line had no length limit | Medium-Low | Fixed: 16 MiB. |
| A vector search reserved room for its `limit` | — | Fixed: room for what was found. |
| `docker compose` defaulted to the token `change-me` | — | Fixed: no default. |
| No token and no account: every caller is the owner | High | Warned about; see [open decisions](#open-decisions). |
| `backup` copies every namespace | High | Documented; see open decisions. |
| Backup, export, import and audit built in memory | High | Documented; streaming is planned. |
| No TLS | Medium | Documented: a TLS proxy and a private network. |
| CI actions and packaging tools not pinned by digest | Medium | Not changed. |
| `/health` and `/v1/about` say the version publicly | Low | Not changed: clients read them before signing in. |
| `k-hop` accepts any depth | Medium | Not changed: a walk is bounded by the size of its component. |
| A member whose Raft thread stopped still answered `/health` 200 | — | The panic is fixed; `/health` does not check the thread yet. |

## Open decisions

These change what existing installations do, and are the maintainer's:

1. **An open server answers only on this machine.** A server with no token
   and no account would refuse requests from other addresses, and the
   first account could be made only locally. It would stop a container
   reached through a mapped port, or a desktop interface on another
   machine, from making the first account.
2. **`backup` is a superuser's.** Or a backup would hold only what the
   account may read, which is an export rather than a copy of the file.
3. **Streaming** of backups, exports, imports and the audit log.
4. **TLS** in the server, or a refusal to start on a network address
   without a proxy that says it terminates TLS.
5. **Pinning** CI actions and images by digest, with signed releases.
