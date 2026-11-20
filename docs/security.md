# Security

How Grafeno protects what it holds, what an operator has to do for the
rest, and what the outside reviews of November 2026 found and what was
done about each finding.

- [What the engine does](#what-the-engine-does)
- [What the operator does](#what-the-operator-does)
- [Review of November 2026](#review-of-november-2026)
- [Review of version 0.3.0](#review-of-version-030)
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
| CI actions and packaging tools not pinned by digest | Medium | Not changed then; pinned since the review of 0.3.0. |
| `/health` and `/v1/about` say the version publicly | Low | Not changed: clients read them before signing in. |
| `k-hop` accepts any depth | Medium | Not changed: a walk is bounded by the size of its component. |
| A member whose Raft thread stopped still answered `/health` 200 | — | The panic is fixed; `/health` does not check the thread yet. |

## Review of version 0.3.0

A review of 0.3.0, focused on who may do what. Its access findings are
network and packaging findings are below. A fix to the code came with a
test that failed before it (`crates/grafeno-server/tests/security.rs`,
`groups_open.rs`, `cluster.rs` and `cluster_secret.rs` there); a fix to
the packaging was checked by building it.

| Finding | Severity given | What was done |
| --- | --- | --- |
| A cluster group's empty file made a server with accounts open again | High | Fixed: each group keeps its own keys; whether the server is open is the first group's. |
| The `cluster` privilege wrote accounts, keys, sessions and roles through a group import | High | Fixed: only a superuser's import carries them. |
| `batch` retracted and invalidated without the right to retract, and wrote as nobody | High | Fixed: needs `retract`, and records its caller. |
| A GQ idempotency key returned another caller's result | High | Fixed: a key stands for its caller and its statement. |
| A webhook delivered what its creator could not read | High | Fixed: a webhook needs read access to the whole namespace. |
| GraphML and CSV imports ignored the caller's write restrictions | High | Fixed: they write as their caller. |
| A group's rights cache did not tell group files apart | Medium | Fixed with the first finding. |
| A refresh with a wrong secret ended the session it named | Medium | Fixed: only a reused refresh token ends it; a wrong one is refused. |
| `recall` and `context` with `touch` recorded accesses for a reader | Medium | Fixed: `touch` needs the right to update. |
| An account named `owner` shared the static token's transactions | Medium | Fixed: transactions belong to a caller by kind and name. |
| A task's failure message reached the client | Low | Fixed: a reference, the message in the log. |
| A secret's comparison ended early on a different length | Low | Fixed: hashes compared whole. |
| A webhook delivery looked its name up again after the check, or let a proxy of the environment do it | High | Fixed: it connects to the addresses checked, through no proxy. |
| IPv6 addresses carrying a private IPv4 one (NAT64, 6to4, Teredo, compatible and translated forms) passed for public | Medium | Fixed: as public as the address they carry; Teredo and local-use NAT64 refused. |
| With the cluster secret, `/raft` read a body of any size and kept each refused snapshot | Medium | Fixed: a message is refused past 1 GiB before it is read, only a snapshot carries data, and a refused snapshot is dropped. |
| The compose cluster's members shared the example settings' secret, and a member only warned | Medium | Fixed: a member refuses it; `GRAFENO_CLUSTER_SECRET` gives the secret, and compose requires it. |
| Release actions, toolchain, images and packaging tools taken by name; every release job could write; a dispatched build got the signing certificate | Medium | Fixed: pinned by commit, version, digest or hash; write only where a release is published; the certificate on tags only. Reviewers for the signing secrets (a protected environment) are a setting of the repository. |
| `install.sh` checks a download against a checksum from the same place, with no key of its own | Medium | Not changed yet: the project has no signing key. Once one is made, its public key goes into the script and a missing `minisign` stops the install. |
| `install.sh --service` without root wrote a user unit with no confinement | Low | Fixed: the user unit writes its data folder only, with the system and the home read-only. |
| The SELinux module is installed permissive | Low | Not changed yet: it is made enforcing after a run on an enforcing host. |
| A refused request waited while an explicit transaction held the writer (found while fixing the above) | — | Fixed: the refusal is answered; its event is written once the writer is free, 64 at most waiting, the rest counted as left out. |

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
5. **Signed releases.** A minisign key for the release's files and an
   OpenPGP key for the package repositories, their public halves in
   `install.sh`, and the secret halves in a protected environment of the
   repository. Actions, images and tools are pinned since the review of
   0.3.0.
