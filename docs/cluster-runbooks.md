# Operating a Grafeno cluster

How to install a cluster, watch it, and what to do when a member, a
majority or a disk is lost, or when a new version comes out. Every
runbook here was run on the three-member compose cluster under load,
with a linearizability checker watching (see [The drill](#the-drill)).

## Installing

A cluster is three (or five) `grafeno serve` processes, each with the
same `[cluster]` section in its `grafeno.toml` but its own `id` (see
[install.md](install.md#a-cluster-of-servers)). Members talk on their own
port (`addr` in `[cluster]`, 7811 here); open it between members only, as
traffic between members is authenticated by the shared secret but not
encrypted.

- **Docker compose** (to try a cluster on one machine and to run the
  drills; not a way to install Grafeno): `packaging/compose/` holds a
  three-member cluster with two Raft groups (`docker-compose.yml`,
  `m1.toml` to `m3.toml`) and the image its members run:

  ```bash
  docker build -f packaging/compose/Dockerfile -t grafeno .
  ```

  ```bash
  cd packaging/compose
  GRAFENO_TOKEN=$(openssl rand -hex 16) docker compose up -d
  ```

  The APIs are at `127.0.0.1:17841` to `17843`. Compose refuses to start
  without a token. The members' `secret` in the settings files is a
  placeholder that each member warns about when it starts: change it
  before using the cluster for anything real. A member with an empty
  secret does not start.

- **Linux (systemd)**: install the package on each machine (`.deb` or
  `.rpm`), put the `[cluster]` section in `/etc/grafeno/grafeno.toml`,
  and `systemctl restart grafeno`. The Raft state (`grafeno.redb.raft`),
  snapshots (`grafeno.redb.snapshots/`) and the files of more groups
  (`grafeno.redb.group-1.redb`) live beside the database in
  `/var/lib/grafeno`, which the service may write.

- **Windows (service)**: the same section in
  `C:\ProgramData\Grafeno\grafeno.toml` (`grafeno config path` prints
  where), then restart the "Grafeno" service. Allow the members' port
  through the firewall between members.

Start all members within a few seconds of each other: they elect a
leader in about a second.

## Watching

- `grafeno cluster status --url http://m1:7810 --token …` (and
  `--group 1` for another group): role, term, leader, commit and applied
  positions, elections, leader changes, snapshots sent and installed, the
  entry format the group writes, what each member runs, and, on the
  leader, how many entries each member lacks.
- `GET /v1/metrics` has `grafeno_cluster_*` series per group: `leader`,
  `term`, `commit_index`, `applied_index`, `last_index`,
  `snapshot_index`, `entry_format`, `elections_total`,
  `leader_changes_total`, `snapshots_sent_total`,
  `snapshots_installed_total`, and `peer_lag{member}` on the leader.
  Alert on `peer_lag` growing, on `leader_changes_total` rising more than
  once in a while, and on a group with no member where `leader` is 1.
- Grafeno Studio (installed apart) has a **Cluster** view: the
  groups as the member it talks to sees them, the lag of the others when
  that member leads, what each member runs, where namespaces live, and
  moves.

## Backups

A backup is taken on any member, followers included (`grafeno backup`,
or `GET /v1/backup`), so the leader does not pay for it. With more Raft
groups, back up each group's file too (`GET /v1/backup?group=1`).

**Restore into a new cluster**: copy the backup to every new member as
its database file (and each group's backup as
`grafeno.redb.group-<id>.redb`), with no Raft state beside it, and start
them with the usual settings. A database restored from a backup starts
Raft after the position it holds, so the members agree from there.

## Runbooks

### Losing one member

Nothing to do at once: a majority still holds every write, a new leader
is elected within about a second if the lost one led, and clients that
know several members move on (writes in flight answer busy or time out;
retry them with an idempotency key). Bring the member back with its data
and it catches up from the leader's log, or from a snapshot if the log
was compacted past it. If it will not come back, replace it as below.

### Losing a majority

The cluster stops taking writes (they answer busy) and linearizable
reads; plain reads keep answering from what each member holds. Nothing
acknowledged is lost. Bring members back until a majority runs; they
elect a leader and go on. Do not start a member with an empty disk to
make up the numbers: it would vote without the log it had. Never start
a member on a data directory from another cluster either: members with
the same ids and secret cannot tell the clusters apart.

### Replacing a disk

1. Take the member out of the voters, in every group it is in:
   `grafeno cluster voters 1 2 --url <leader>` (and `--group 1`).
2. Take a backup on a follower (`GET /v1/backup`, and `?group=1`).
3. On the new disk, put the backups as the member's database files, with
   no Raft state, and start it with `voters = []` in `[cluster]` and
   `join = true` in its groups (`packaging/compose/join-3.toml`): it waits
   to be added.
4. Add it back: `grafeno cluster voters 1 2 3` (and `--group 1`). It
   catches up from the log after the backup's position, without a
   snapshot when the log still holds it.

### A rolling upgrade

Upgrade one member at a time: stop it, install the new version, start
it, and wait until `grafeno cluster status` shows it applied what the
leader committed before going on to the next. Members say what they run
with every message; while a member of the older version remains, the
leader writes no entry format it cannot read (work that needs the newer
format, such as a namespace move, answers that every member must be
upgraded first). Database layout upgrades happen as each member opens
its file. The order does not matter for safety; upgrading the leader
last saves an election per member.

## The drill

`packaging/compose/drill.sh` runs every runbook above on the compose
cluster: it starts three members of an older image, runs the `drill`
example (clients writing distinct values to one register and reading it
linearizably, on random members) through each step, checks each step's
history with the linearizability checker, and checks at the end that
every member holds the same:

```bash
cargo build --release -p grafeno-server --example drill
OLD_IMAGE=grafeno:13.5 NEW_IMAGE=grafeno:13.6 packaging/compose/drill.sh
```

On 2026-11-06, from an image of 13.5 (commit `275a196`) to one of 13.6,
three clients, `docs/data/drill.csv`:

| Step | Operations | Writes succeeded in | Longest stretch without a write | Linearizable |
| --- | --- | --- | --- | --- |
| rolling upgrade, the leader first (90 s) | 11 218 | 92% of 100 ms windows | 4.8 s | yes |
| losing one member, the leader, 10 s | 1 754 | 65% | 2.3 s | yes |
| losing a majority, 10 s | 1 220 | 28% | 19.6 s | yes |
| replacing a disk | 1 779 | 63% | 3.0 s | yes |

During the upgrade, once group 0's leader ran 13.6 while a 13.5 member
remained, a namespace move into group 0 was refused ("this write needs
replication entry format 2, and a member reads only up to 1: upgrade
every member first"); with every member upgraded the group wrote format 2
and the move went through. The replaced member caught up from the log
(no snapshot installed), and at the end every member held the same
registers. Availability is low because the steps are short and each
holds an outage; the drill's clients pick a member at random, including
one that is down.

**What an earlier run found**: a member whose data volume survived from
a previous run of the drill joined the new cluster with that cluster's
data and Raft state, and served a value of the old one (the checker
caught it as reads of a value never written). Grafeno cannot tell two
clusters apart when their members share ids and the secret: never start
a member on another cluster's data directory, and give every cluster its
own secret. The drill now removes its volumes itself.
