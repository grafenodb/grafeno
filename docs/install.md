# Installing Grafeno

[Español](instalacion.md) | **English**

This guide installs **the engine**: the `grafeno` program, as a service
that starts with the system. It has no graphical interface, as a database
server has none: work with it from its console (`grafeno shell`), its HTTP
API, MCP, or any client. **Grafeno Studio**, the graphical interface, is a
separate product with its own installers, which connects to engines through
their API.

## The Grafeno server

Every way of installing the server gives the same `grafeno` program: the
HTTP server, the MCP server, the shell and the administration commands, and
nothing else. The installers also run it as a service that starts with the
system.

| System | File | What it sets up |
| ------ | ---- | --------------- |
| Windows 10/11, Server 2019+ (x64) | `grafeno-<version>-windows-x64-setup.exe`, or `winget install GrafenoDB.Grafeno` | `C:\Program Files\Grafeno`, the `Grafeno` service, PATH, Start menu shortcuts |
| Debian, Ubuntu and derivatives (amd64, arm64) | Grafeno's APT repository, or `grafeno_<version>-1_<arch>.deb` | `/usr/bin/grafeno`, the `grafeno` user, the default instance and its systemd service |
| Fedora, RHEL, Rocky, Alma, Oracle Linux, Amazon Linux (x86_64, aarch64) | Grafeno's DNF repository, or `grafeno-<version>-1.<arch>.rpm` | the same |
| Any Linux (x86_64, aarch64) | `grafeno-<arch>-unknown-linux-musl.tar.gz`, or `install.sh` | the binary; the service with `--service` |

The Linux binary is static (musl): the same file runs on any distribution,
whatever its glibc version.

After installing, the engine serves on `127.0.0.1:7810`. Its console, a
client or Grafeno Studio connects with the token the installer generated:

```bash
grafeno config token
```

(On Linux, as root: `sudo grafeno config token`.) `grafeno doctor` checks
the installation: the settings and their permissions, the data folder, the
database's layout, free disk space, the port and the service.

### The one-liner: any Linux, AlmaLinux and RHEL included

```bash
curl -fsSL https://raw.githubusercontent.com/grafenodb/grafeno/main/install.sh | sudo sh
```

As root, it finds the distribution in `/etc/os-release` and the processor
(x86_64 or aarch64), adds Grafeno's signed repository and installs the
package with the system's package manager, so that upgrades come with the
system's (`dnf upgrade`, `apt upgrade`). The result is the same as
installing the package by hand: the `grafeno` user, the default instance,
the systemd service, started.

| Distribution | What it installs |
| --- | --- |
| RHEL, AlmaLinux, Rocky, Oracle Linux, CentOS Stream, Fedora, Amazon Linux | the DNF repository and the `.rpm`, with `dnf` (or `yum`) |
| Debian, Ubuntu and their derivatives | the APT repository and the `.deb`, with `apt` |
| Anything else | the static binary to `/usr/local/bin` (add `--service` for the service) |

If the repository cannot be reached (or with `--no-repo`), it installs the
release's package file instead. Without root (no `sudo`), or with
`--binary`, it installs the binary into `~/.local/bin`; `--service` then
adds a systemd **user** service, with its settings in
`~/.config/grafeno/grafeno.toml` and its data in `~/.local/share/grafeno`:

```bash
curl -fsSL https://raw.githubusercontent.com/grafenodb/grafeno/main/install.sh | sh -s -- --service
```

Every file it downloads is checked against the SHA-256 published with the
release, and against its minisign signature when a key is known
(`GRAFENO_MINISIGN_KEY`, and `minisign` installed); a mismatch stops it. On
SELinux systems it labels a binary it installs so systemd may run it.
Options, as flags or environment variables (for unattended use):
`--version 0.2.1` (`GRAFENO_VERSION`), `--binary` (`GRAFENO_METHOD=binary`),
`--no-repo` (`GRAFENO_REPO=0`), `--prefix DIR` (`GRAFENO_PREFIX`),
`--service`, `--no-start`; `GRAFENO_BASE_URL` and `GRAFENO_REPO_URL` point
it at mirrors.

### Debian and Ubuntu

From the repository (what the one-liner does):

```bash
sudo install -d /etc/apt/keyrings && curl -fsSL https://grafenodb.github.io/grafeno/grafeno.asc | sudo tee /etc/apt/keyrings/grafeno.asc >/dev/null
```

```bash
echo "deb [signed-by=/etc/apt/keyrings/grafeno.asc] https://grafenodb.github.io/grafeno/deb stable main" | sudo tee /etc/apt/sources.list.d/grafeno.list
```

```bash
sudo apt update && sudo apt install grafeno
```

Or a downloaded package:

```bash
sudo apt install ./grafeno_0.2.1-1_amd64.deb
```

### Fedora, RHEL and derivatives

From the repository (what the one-liner does):

```bash
sudo curl -fsSL https://grafenodb.github.io/grafeno/grafeno.repo -o /etc/yum.repos.d/grafeno.repo
```

```bash
sudo dnf install grafeno
```

The repository's packages and metadata are signed; `dnf` asks to trust the
key the first time (`gpgkey` in `grafeno.repo`). A downloaded package is
not signed (the repository's copy is): install it with `--nogpgcheck` after
checking its SHA-256:

```bash
sudo dnf install --nogpgcheck ./grafeno-0.2.1-1.x86_64.rpm
```

#### What the packages do

- Create the `grafeno` system user and `/var/lib/grafeno` (its data).
- On a first install, create the **default instance** (`grafeno setup`):
  `/etc/grafeno/grafeno.toml` with a new token, readable by root and the
  `grafeno` group only, and the `grafeno` systemd service, enabled and
  started. Upgrades keep the settings and restart every running instance.
- Run the service sandboxed, so that it can write its data folder and
  nothing else (`systemd-analyze security grafeno` rates it 1.3, "OK"),
  and confined: the `.deb` loads an **AppArmor** profile (enforced); the
  `.rpm` installs an **SELinux** module (the `grafeno_t` domain and labels
  for the program, the settings, the data and ports 7810-7819; the domain
  is permissive for now: it logs, and denies nothing).
- Install nothing graphical: no menu entry, no icon, no dependency on a
  desktop stack.

```bash
systemctl status grafeno
```

```bash
journalctl -u grafeno -f
```

Removing the package stops every instance and keeps `/etc/grafeno` and
`/var/lib/grafeno`. Purging the `.deb` (`apt purge grafeno`) removes them,
as Debian expects: take a `grafeno backup` first if the data matters. With
the `.rpm` (which has no purge), `grafeno instances drop <name> --purge`
removes an instance's data before the package is removed.

### Instances: `grafeno setup`

An instance is a settings file, a data folder and a service: one database
server among others on the same machine. The packages make the default
one; `grafeno setup` makes others, asking what its flags leave open:

```bash
sudo grafeno setup --instance team
```

It asks for the data folder (`/var/lib/grafeno/team`), the port (the next
free one after 7810), whether other computers may connect, and the first
administrator (a superuser who signs in with a password); then it writes
`/etc/grafeno/team.toml` (with a new token), creates the database and the
administrator, and enables and starts `grafeno@team.service`. Unattended,
every answer is a flag, or a line of an answers file:

```bash
sudo grafeno setup --unattended --instance team --data /srv/grafeno-team --port 7820 --network --admin root --admin-password-file /root/pw
```

```toml
# answers.toml, for grafeno setup --answers answers.toml --unattended
instance = "team"
data = "/srv/grafeno-team"
port = 7820
network = true
durability = "eventual"
memory_mb = 1024
admin = "root"
admin_password_file = "/root/pw"
```

A data folder outside `/var/lib/grafeno` is added to the service's
`ReadWritePaths` (a drop-in), to the AppArmor profile (`local/grafeno`)
and to SELinux's labels. `--no-admin` leaves only the token (until
`grafeno users create` makes an account); `--no-start` enables the service
without starting it.

```bash
grafeno instances
```

```text
NAME         ADDRESS                SERVICE                  DATABASE
default      127.0.0.1:7810         running                  /var/lib/grafeno/grafeno.redb
team         0.0.0.0:7820           running                  /srv/grafeno-team/grafeno.redb
```

Each instance has its own settings, data, port and logs
(`journalctl -u grafeno@team`). `sudo grafeno instances drop team` stops
and removes one (its service and settings; `--purge` its data too).

### Upgrades protect the data

When a new version opens a database whose layout is older (an upgrade that
changes the file's format), it first copies the file, as it is, to
`grafeno.layout-<old>.redb` beside it, and only then upgrades it. If the
copy cannot be made (no room on the disk), the server does not start and
the file is untouched; `grafeno doctor` warns ahead of time when free space
is short. One copy is kept per instance (the next upgrade's replaces it);
`grafeno doctor` lists it with its size, to remove when it is no longer
needed. `copy_before_upgrade = false` in the settings turns it off.

To go back to the previous version after such an upgrade:

```bash
sudo grafeno instances rollback default
```

```bash
sudo dnf downgrade grafeno
```

(or `sudo apt install grafeno=<version>-1`). The first command stops the
service and puts the copy back (what was written since is kept aside as
`grafeno.before-rollback-<time>.redb`); installing the previous version
starts it again.

### Windows

Run `grafeno-<version>-windows-x64-setup.exe` (it asks for administrator
rights), or `winget install GrafenoDB.Grafeno`. It asks:

- the **program folder** (`C:\Program Files\Grafeno` by default);
- the **data folder**, where the service keeps its database
  (`C:\ProgramData\Grafeno\data` by default; another disk is fine);
- the **port** (7810 by default);
- **who may connect**: only this computer (`127.0.0.1`), or other computers
  too (every network interface, with a firewall rule for the port); every
  request needs the token or an account either way;
- the **account the service runs as**: `NT SERVICE\Grafeno`, a virtual
  account that may use only Grafeno's folders (the default), or
  LocalSystem;
- the **first administrator**: a name and a password (or none, and the
  token works meanwhile);
- whether to **run Grafeno as a Windows service** (it starts with Windows)
  and **add grafeno to the system PATH**.

It writes `C:\ProgramData\Grafeno\grafeno.toml` (address, data, a new
token) and starts the `Grafeno` service. The Start menu gets *Grafeno Shell*
(connected to the service) and *Grafeno settings*. The service writes its
events to the Event Log (Windows Logs → Application, source `Grafeno`) and
to `C:\ProgramData\Grafeno\grafeno.log`, which turns into `grafeno.log.1`
at 10 MiB.

Unattended (scripts, provisioning), the same answers are parameters:

```powershell
.\grafeno-0.2.1-windows-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /DIR="C:\Grafeno" /DATADIR="D:\GrafenoData" /PORT=7810 /NETWORK=0 /ACCOUNT=virtual /ADMIN=admin /ADMINPASSWORD="..." /TASKS="service,path"
```

`/NETWORK=1` listens on every interface and opens the port in the firewall;
`/ACCOUNT=system` runs the service as LocalSystem; `/LOG=setup.log` keeps a
log of the install.

Manage the service like any other (`services.msc`, or PowerShell as
administrator):

```powershell
Restart-Service Grafeno
```

Upgrading runs the new installer over the old one (or `winget upgrade
GrafenoDB.Grafeno`): it stops the service, replaces the program, keeps the
settings, the data and the service's account (it asks nothing about them
again) and starts the service. Uninstalling (Settings → Apps) removes the
program, the service, the firewall rule and the PATH entry, and asks
whether to remove the settings and the data too (no by default);
unattended, `unins000.exe /VERYSILENT` keeps them and
`unins000.exe /VERYSILENT /PURGE=1` removes them.

Without the installer, `grafeno-x86_64-pc-windows-msvc.zip` has the bare
`grafeno.exe`; as administrator, `grafeno setup` creates the settings, the
first administrator and the service in one step, or by hand:

```powershell
grafeno config init
```

```powershell
grafeno service install --account "NT SERVICE\Grafeno"
```

(The virtual account then needs Modify on `C:\ProgramData\Grafeno` and the
data folder, which the setup grants.)

### Where everything is

| What | Linux (packages) | Windows (setup) |
| ---- | ---------------- | --------------- |
| The program | `/usr/bin/grafeno` | `C:\Program Files\Grafeno\grafeno.exe` |
| Settings, default instance | `/etc/grafeno/grafeno.toml` | `C:\ProgramData\Grafeno\grafeno.toml` |
| Settings, other instances | `/etc/grafeno/<name>.toml` | (one instance) |
| Data, default instance | `/var/lib/grafeno/grafeno.redb` | `C:\ProgramData\Grafeno\data\grafeno.redb` |
| Data, other instances | `/var/lib/grafeno/<name>/grafeno.redb` | |
| Copy kept before a layout upgrade | `grafeno.layout-<old>.redb` beside the database | the same |
| The service | `grafeno.service`, `grafeno@<name>.service` | the `Grafeno` service |
| Logs | journald (`journalctl -u grafeno`) | Event Log (Application, `Grafeno`), `C:\ProgramData\Grafeno\grafeno.log` |
| Confinement | systemd sandbox; AppArmor `/etc/apparmor.d/grafeno`; SELinux module `grafeno` | the virtual account `NT SERVICE\Grafeno` |
| Package scripts | `/usr/share/grafeno/setup.sh`, `stop-all.sh` | |

## A cluster of servers

Three (or five) servers can hold the same data and keep serving when one
fails (phases 13.2 and 13.3). Each gets the same `[cluster]` section in its
`grafeno.toml`, with its own `id` and `addr`:

```toml
[cluster]
id = 1                         # 2 and 3 on the others
addr = "10.0.0.1:7811"         # where this member listens for the others
secret = "a long random string, the same on every member"
voters = [1, 2, 3]             # the first members; [] on a member that joins later
peers = [                      # addr: the members' port; api: the HTTP API, for forwarding
  { id = 1, addr = "10.0.0.1:7811", api = "10.0.0.1:7810" },
  { id = 2, addr = "10.0.0.2:7811", api = "10.0.0.2:7810" },
  { id = 3, addr = "10.0.0.3:7811", api = "10.0.0.3:7810" },
]
```

Start all three: they elect a leader within a second or so. Send requests
to any member: a follower forwards writes to the leader (with `api` in
`peers`; without it, it answers 421 `not_leader` naming the leader), and
reads are served by the member itself. A read can ask for more with the
header `x-grafeno-consistency`: `bounded` goes to the leader when the
member lags far behind, `linearizable` sees every write acknowledged
before it. Every response carries `x-grafeno-position`; a request with
`x-grafeno-after: N` waits until its member has applied N (read your
writes). A GQ write sent with an `Idempotency-Key` header runs once,
however many times it is retried. The shell (`--url` with several
addresses, comma-separated) and the TypeScript client (`url: [...]`)
move to another member when one fails and read their own writes. When
the leader dies, writes resume in about half a second.

A costly read (estimated cost from `split_cost` in `[cluster]`, 200 000
by default, as `EXPLAIN` shows it) is split across the members: each
runs a share at the same position, and the member asked merges the
parts into the same rows one server returns. `x-grafeno-split: on`
splits a read whenever its shape allows, `off` never. Members that
may take parts need `api` in `peers`.

Writes of one group go through one leader at a time. To write more, run
more groups, each with its own leader and database file
(`<db>.group-<id>.redb`) on its voters:

```toml
[cluster]
# … as above, then:
groups = [{ id = 1, voters = [1, 2, 3] }, { id = 2, voters = [1, 2, 3] }]
```

Namespaces live in group 0 until placed elsewhere:

```bash
curl -X POST http://10.0.0.1:7810/v1/cluster/placement -H "Authorization: Bearer …" -H 'content-type: application/json' -d '{"namespace": "vault", "group": 1}'
```

A namespace that already has data moves while it keeps serving: its
writes pause for tens of milliseconds (they answer busy; clients retry)
and none is lost. Any member takes requests for any namespace. Back up
each group's file (`GET /v1/backup?group=1`; backups can be taken on a
follower). `grafeno cluster status --url http://10.0.0.1:7810 --token …`
(`--group 1` for another group) shows a member's view, with elections,
leader changes, snapshots, the entry format the group writes, what each
member runs and, on the leader, each member's lag; `/v1/metrics` has the
same as `grafeno_cluster_*` series, and Grafeno Studio a Cluster view; `grafeno cluster voters 1 2 3 4` (on the leader)
changes the members, the new one listed in every member's `peers` and
started with `voters = []`; `grafeno cluster compact` snapshots a member's
database and trims its log. Each member keeps its Raft state in
`<db>.raft` and snapshots in `<db>.snapshots/`.

A three-member compose cluster, systemd and Windows service setups, and
the runbooks for losing a member, losing a majority, replacing a disk and
upgrading are in [cluster-runbooks.md](cluster-runbooks.md).

For now: traffic between members is authenticated by the secret but not
encrypted (keep them on a private network); imports go into a single server
before it starts a cluster. Webhooks and background work (extraction,
summaries) run on the leader, and a new leader resumes them.

## Accounts

A new server is open to anyone who reaches it (fine on your own
machine) until it has accounts, or a token in `grafeno.toml`. Make the
first account, a superuser, with the server stopped or through it:

```bash
grafeno users create admin --superuser --config /etc/grafeno/grafeno.toml
```

(On an open server, Grafeno Studio also offers to create it when it connects.) Then make
the people who sign in, with grants per namespace, and service accounts
for agents and applications, which use API keys:

```bash
grafeno users create ana --grant notes:write --config /etc/grafeno/grafeno.toml
```

```bash
grafeno users create indexer --service --grant "team-*:write" --config /etc/grafeno/grafeno.toml
```

Users sign in from the Studio, the shell (`grafeno shell --url … --user
ana`) or the API (`POST /v1/auth/signin`), and can make their own API
keys, scoped down to some namespaces or to reading. `grafeno users
password`, `disable`, `enable` and `delete` manage them. Keys from before
14.1 kept working: each became a service account of its name. The token
in `grafeno.toml`, when set, still does everything: keep it for
emergencies. `[auth]` (below) sets how long sessions last and when
failed sign-ins lock an account.

Roles (14.2) bundle privileges: define one with `PUT /v1/roles/{name}`
(say, reading every `team-*` namespace but `team-secret`, or seeing
metrics only, for a monitoring agent) and give it to accounts with
`PATCH /v1/accounts/{name}` and `{"roles": [...]}`. A role named
`public` is everyone's. The server's README lists the privileges.

The same in GQ (14.3), from the shell or the Studio's Scripts view, by an
account that may manage roles:

```text
CREATE ROLE team_reader;
GRANT read, export ON NAMESPACE 'team-*' TO team_reader;
DENY read ON NAMESPACE 'team-secret' TO team_reader;
GRANT ROLE team_reader TO ana;
```

or from the command line (`grafeno grants grant read --on namespace:team-* --to team_reader`,
`grafeno roles show team_reader`), and in the Studio's Access view.

Rights reach labels, properties and records (14.4): `DENY read ON NODES
Salary IN crm TO team_reader` hides those nodes everywhere, `DENY read ON
PROPERTY Person.email IN crm` hides one property, and `RESTRICT team_reader
ON NAMESPACE crm NODES Customer WHERE tenant = $account.tenant` keeps each
account to its tenant's customers (`ALTER USER ana SET ATTRIBUTE tenant =
'acme'`). What is hidden is absent, as if it did not exist.

Every security event goes to the audit log (14.5): sign-ins and failures,
lockouts, keys, sessions, changes to accounts and roles, refused requests.
Read it with an account that has the `audit` privilege (`GET /v1/audit`,
`?format=jsonl` to keep a copy); `[audit]` sets how much is kept:

```toml
[audit]
max_entries = 10000000
max_age_days = 365
```

## Backups

A backup is a consistent copy of the whole database file (every
namespace, and the API keys), taken while the service keeps running:

```bash
grafeno backup grafeno-backup.redb --config /etc/grafeno/grafeno.toml
```

(On Windows, the settings are in `C:\ProgramData\Grafeno\grafeno.toml`;
`grafeno config path` prints where.) To restore, stop the service, put the
copy where `db` in `grafeno.toml` points, and start it again. Before using
a file that may be damaged (a failing disk, a bad copy), check it with the
service stopped:

```bash
grafeno check --db grafeno-backup.redb
```

It verifies every page against its checksum, then the indexes. Exports
(`grafeno export`) are the other kind of copy: JSON Lines, readable and
portable between versions, but without the keys.

To move a namespace to or from other tools (spreadsheets, graph drawing
and analysis programs, other databases), use CSV or GraphML. They hold current knowledge, without
retracted facts or the history behind them, so they do not replace a
backup:

```bash
grafeno export --format csv -n notes -o notes-csv
```

```bash
grafeno import --format csv -n notes notes-csv/nodes.csv --edges notes-csv/edges.csv
```

```bash
grafeno export --format graphml -n notes -o notes.graphml
```

```bash
grafeno import --format graphml -n notes notes.graphml
```

## Settings

`grafeno.toml` is read by the service, and by `grafeno serve --config` and
`grafeno shell --config`. Restart the service after changing it.

```toml
db = "/var/lib/grafeno/grafeno.redb"
addr = "127.0.0.1:7810"     # "0.0.0.0:7810" listens on every network interface
token = "…"                 # every /v1 request needs `Authorization: Bearer <token>`
cors_origins = []           # origins allowed to call the API from a browser ("*": any)
durability = "immediate"    # or "eventual": faster writes, a crash loses at most 0.5 s
max_requests = 512          # served at once; more get 503 with Retry-After
copy_before_upgrade = true  # copy the database before a new version upgrades its layout

[query]                     # limits for each GQ query over HTTP and MCP; 0 turns one off
timeout_secs = 30
max_reads = 50000000        # nodes, edges and index entries read
max_rows = 1000000          # rows held at once
max_memory_mb = 512         # one value a query builds, or the rows it holds at once
max_cost = 0                # refuse queries EXPLAIN estimates above this (0: off)
scan_threads = 0            # threads large read scans share, over every query (0: one per CPU; 1: off)

[writer]                    # the group commit (see "Monitoring" below)
max_batch = 512             # writes per transaction at most
queue = 4096                # writes that may wait; more get 503
flush_ms = 500              # with eventual durability, how often commits reach the disk
import_chunk = 2000         # records per transaction of an import with ?bulk=true

[memory]                    # caches, in MiB (see "Memory" below)
cache_mb = 256              # pages of the database file
projection_cache_mb = 256   # graphs kept for graph algorithms
vector_cache_mb = 128       # vectors decoded while inserting embeddings

[changes]                   # the change log that streams and webhooks follow
enabled = true
max_entries = 1000000       # per namespace; 0 for no limit
max_age_days = 7            # 0 for no limit

[events]                    # change events with values (off by default)
enabled = false             # true: CREATE EVENT SUBSCRIPTION works, and
                            # commits record the values subscriptions ask for

[auth]                      # accounts and sign-in
session_minutes = 15        # an access token's life (sessions renew it)
refresh_days = 30           # a session's life
lockout_attempts = 5        # failed sign-ins that lock an account for a while
address_attempts = 20       # failed sign-ins that refuse an address for a while
lockout_minutes = 15

[background]                # work that follows the change log
extract = ["notes"]         # link what new and changed texts mention
communities = ["notes"]     # keep community summaries fresh
communities_every_secs = 300
communities_min_changes = 50
compact_vectors_at = 0.5    # rebuild a vector graph once removals reach this share of its vectors (0: never)
compact_vectors_min = 1000  # leave smaller graphs alone
compact_vectors_max = 50000 # and larger ones: compacting holds the writer (see below)
compact_vectors_below_recall = 0  # also when sampled recall@10 falls below this (0: never measure)
compact_vectors_every_secs = 300

[transactions]              # BEGIN … COMMIT across requests
idle_timeout_secs = 30      # rolled back after this long without a request
max_secs = 600              # rolled back at this age, busy or not
wait_secs = 5               # how long BEGIN waits for the writing one before it
max_open = 256
```

Flags and environment variables (`GRAFENO_DB`, `GRAFENO_ADDR`,
`GRAFENO_TOKEN`) take precedence over the file. Expose the port beyond
`127.0.0.1` only behind TLS (a reverse proxy) and with a token.

### Vector graphs

Each vector search walks a graph (HNSW). Removing or replacing a vector
repairs its neighbors' links rather than choosing them afresh, so after
many removals (deduplication, merges, forgetting, consolidation) a few
vectors can become unreachable and searches can slip. `GET
/v1/{ns}/vectors/health` counts the removals since each graph was built
(the namespace's own and each vector index), and `grafeno check` prints
them; `grafeno check --recall 100` measures recall@10 against exact
search.

The server compacts (builds again) a graph once its removals reach
`compact_vectors_at` of its vectors, looking every
`compact_vectors_every_secs`; on a cluster, the leader does, and the
members replay it. Compacting holds the writer about as long as
inserting every vector again (about 6 s for 20 000 vectors of 128
dimensions), while searches keep the old graph until it commits: graphs
over `compact_vectors_max` are left for a quiet time
(`grafeno compact -n NS`, or `POST /v1/{ns}/compact?index=…`). What it
did is in `GET /v1/background`. In a cluster, upgrade every member before
vector indexes are compacted: an older member replaying the compaction of
a vector index rebuilds it with the default similarity.

### Memory

Grafeno's caches are the main memory it keeps: pages of the database file
(`cache_mb`), graphs projected for algorithms (`projection_cache_mb`) and
vectors decoded while inserting (`vector_cache_mb`). Everything else is
per request and bounded by the `[query]` limits. With 10 million edges a
server stays near 300 MiB with the defaults; on a 512 MiB machine set
`cache_mb = 128` and `projection_cache_mb = 64` (graph algorithms on such
a graph then build their projection, about 140 MiB, each time, and may
need a longer `timeout_secs`). Loading many embeddings goes about twice as
fast with `vector_cache_mb = 1024` and large write transactions, if the
memory is there. Numbers in [benchmarks](benchmarks.md).

### Monitoring

`GET /v1/metrics` answers in the Prometheus text format: requests and
their time, GQ queries, the limits they hit, the writer's queue, groups,
commit time and write latency per lane. It needs a secret, like the rest
of `/v1`; a key with any grant will do:

```yaml
scrape_configs:
  - job_name: grafeno
    metrics_path: /v1/metrics
    authorization:
      credentials: gfk_…
    static_configs:
      - targets: ["127.0.0.1:7810"]
```

Watch `grafeno_write_queue` and `grafeno_write_seconds`: a queue that
stays up means writes arrive faster than one writer commits them (about
11 000 a second on an SSD with immediate durability, more with
`durability = "eventual"`); `grafeno_write_rejected_total` counts the 503s
past the queue's limit. Load large exports with `?bulk=true`: they then go
in chunks between the agents' writes instead of before them.

## Browsers and Grafeno Studio

The engine serves no interface (before Grafeno 15 it had the Studio at
`/studio/`; that address now says where it went). Grafeno Studio, installed
on its own, connects to the engine's address with its token. A web page that
calls the API from a browser (the Studio hosted on a web server, an app of
your own) needs its origin allowed:

```bash
grafeno serve --config grafeno.toml --cors-origin https://studio.example.com
```

Grafeno Desktop, the desktop app, needs no origin allowed (it sends the
requests itself). To open a database file it uses the engine installed
here: it starts `grafeno serve --stop-with-stdin` for that file on a
loopback port with a token of its own, and the engine stops when the file
or the app closes. Install the engine first; the app finds it on the
`PATH` or where these installers put it (or at `GRAFENO_BIN`).

## Versions and releases

The engine and Grafeno Studio / Grafeno Desktop are two products with
their own versions, change logs and releases: the engine's are tagged
`v<version>` in this repository ([CHANGELOG.md](../CHANGELOG.md)),
the desktop app's `desktop-v<version>` in its own. Each installs on its
own and leaves the other alone: the engine's packages bring nothing
graphical, the desktop's no service and no engine, and installing,
upgrading or removing either keeps the other and the engine's data. The
desktop app says which engine versions it works with (by the API version
of `GET /v1/about`, now 1).

## Building the installers

| Files | Command | Needs |
| ----- | ------- | ----- |
| Linux `.tar.gz`, `.deb`, `.rpm`, x86_64 and aarch64 | `docker build -f packaging/linux/Dockerfile --output dist/linux .` | Docker |
| The APT and DNF repositories | `GRAFENO_GPG_KEY=<key id> packaging/repo/build-repo.sh dist/linux site https://grafenodb.github.io/grafeno` | GnuPG, rpm, createrepo_c, apt-utils |
| Windows setup `.exe` | `cargo build --release -p grafeno-server`, then `iscc /DAppVersion=0.2.1 packaging\windows\grafeno.iss` | Rust (MSVC), Inno Setup 6 |
| winget manifests | `python packaging/windows/winget/make-manifests.py 0.2.1 dist\grafeno-0.2.1-windows-x64-setup.exe <url> dist\winget` | Python |

No Node and no graphical toolkit: the engine builds from Rust alone. The
aarch64 binary is cross-compiled with Zig (`cargo zigbuild`), and the
SELinux module is compiled in an AlmaLinux stage of the same Dockerfile.
The Linux packages also come under names without the version
(`grafeno-linux-x86_64.rpm`, `grafeno-linux-amd64.deb`,
`grafeno-linux-aarch64.rpm`, `grafeno-linux-arm64.deb`), which the
one-liner asks the latest release for. `packaging/windows/grafeno.ico`
is the icon of the executable, the installer and its shortcuts:
`packaging/windows/make-icon.sh` renders it (16 to 256 pixels, with Docker)
from `packaging/windows/grafeno-mark.svg`, the Grafeno site's mark.

`.github/workflows/release.yml` builds all of them when a `v*` tag is
pushed and publishes a GitHub release; it adds the packages to the
repositories on the `gh-pages` branch (served by GitHub Pages). What signs
comes from the repository's secrets, each optional:

| Secret | What it signs |
| ------ | ------------- |
| `MINISIGN_SECRET_KEY` | every release file (`.minisig` beside each); the one-liner checks them with the public key |
| `GPG_PRIVATE_KEY` | the repositories' metadata and the `.rpm` files in them |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` | `grafeno.exe`, the setup and its uninstaller (Authenticode) |

Making the keys, once: `minisign -G -W` (publish the public key, and set
`MINISIGN_KEY` in `install.sh` to it), and
`gpg --quick-gen-key "Grafeno releases" rsa4096 sign never` with no
passphrase, exported with `gpg --armor --export-secret-keys`. GitHub Pages
must serve the `gh-pages` branch. The winget manifests come with each
release (`grafeno-<version>-winget.zip`), to submit to
`microsoft/winget-pkgs`.

How they are tested on clean machines:

- `packaging/linux/test-install.sh [--repo] [--systemd] [--arm64] [image ...]`
  serves `dist/linux` as a release, signed with throwaway keys, and runs the
  one-liner, as root and as a user, in clean containers (AlmaLinux 9 and 8,
  Rocky 9, Fedora 42, Debian 12, Ubuntu 24.04 by default): the checksum and
  the signature (a wrong key is refused), the version, the user, the
  settings, the server answering with them, `grafeno doctor`, nothing
  graphical installed, and purging the `.deb`. `--repo` builds the
  repositories with a throwaway GnuPG key and installs from them, the
  package managers checking the signatures. `--arm64` repeats it on aarch64
  (emulated). `--systemd` boots systemd in an AlmaLinux container: the
  service runs and is enabled; `grafeno setup` makes a second instance with
  an administrator who signs in, and another with its data elsewhere that
  is then dropped with its data; an old database is copied before its
  layout is upgraded and `grafeno instances rollback` puts it back; an
  upgrade restarts the running instances; removing the package keeps the
  data.
- `.github/workflows/installers.yml` runs that, installs the `.deb` on the
  Ubuntu runner itself with its AppArmor profile enforced (the service and
  an instance with its data elsewhere confined, working, and denied
  nothing), and on a Windows runner installs the setup unattended (its own
  data folder and port, the virtual account, an administrator who signs
  in, events readable in the Event Log, `grafeno doctor`), upgrades over it
  (the settings and the account kept), uninstalls keeping the data,
  installs over the kept data, and uninstalls with `/PURGE=1`.
