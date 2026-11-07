# Phase 11: Memory that keeps itself

[All phases](README.md) · [← Phase 10: Hardening](phase-10-hardening.md) · [Phase 12: Compatibility and speed →](phase-12-compatibility-and-speed.md)

**Status:** done, in five steps.

Entity resolution, incremental extraction and reconciliation of entities, automatic invalidation of contradicting facts, hierarchical community summaries, continuous ingestion and events.

Phases 10 to 12 were planned together: their guidelines are in [Phase 10](phase-10-hardening.md).

Steps:

- [11.0 Entity resolution and text similarity (done)](#110-entity-resolution-and-text-similarity-done)
- [11.1 Incremental extraction and reconciliation of entities (done)](#111-incremental-extraction-and-reconciliation-of-entities-done)
- [11.2 Automatic invalidation of contradicting facts (done)](#112-automatic-invalidation-of-contradicting-facts-done)
- [11.3 Hierarchical community summaries (done)](#113-hierarchical-community-summaries-done)
- [11.4 Continuous ingestion and events (done)](#114-continuous-ingestion-and-events-done)

## 11.0 Entity resolution and text similarity (done)

Deciding that two records
are the same thing (record linkage), for any kind of entity: the base of
11.1's reconciliation, and useful on its own (contacts from several
systems, products from several suppliers, companies, addresses, authors,
assets).

- Measures on normalized text: Levenshtein, Damerau-Levenshtein, Jaro and
  Jaro-Winkler, Jaccard and Dice over n-grams, longest common
  subsequence; phonetic codes as an option per language; numbers and
  dates with tolerances (a day and a month swapped). As GQ functions
  (`text.jaroWinkler(a, b)` and the like) and in the library.
- Comparison rules declared per label, not coded: which properties
  identify (strong keys, from 10.5), which compare and how, with weights
  and thresholds for "same", "maybe, to review" and "different"
  (Fellegi-Sunter style), and an explanation of every score field by
  field.
- Candidates found at query time, not only in batch: a trigram index (or
  MinHash with LSH) answers "who looks like this?" without comparing
  with everything, so the same rules serve deduplication of a namespace
  and linking a new record as it arrives.
- The graph as evidence: two similar records that share neighbors (an
  address, an employer, contacts) are likelier the same.
- Merges keep the rules and the undo of 10.5. No language model, CPU
  only; an agent may add its own judgment through the API.

As built:

- `grafeno::textsim` holds the measures: Levenshtein, Damerau-Levenshtein
  (optimal string alignment, with a bounded version that stops early),
  edit similarity, Jaro and Jaro-Winkler (checked against the published
  examples), n-grams with word padding, Jaccard and Dice over them, shared
  words, the longest common subsequence, American Soundex, a Spanish
  phonetic key, and date and number similarities with tolerances. A
  `Measure` names one with its settings and compares normalized text. GQ
  exposes them as `text.*` functions (and APOC's names for the common ones).
- A fuzzy index is an `IndexDef` with `fuzzy: true` whose entries, in the
  same table as the others, are the trigrams of the normalized text (of
  each string of a list), tagged apart from values. Creating, dropping,
  checking, rebuilding, exporting and backing up it needs nothing of its
  own; the planner never uses it for equality or ranges. A search reads
  the entries of each trigram of the query (skipping trigrams shared by
  more than 20 000 nodes), takes the nodes sharing the most, and scores
  them with a measure against their value. `CREATE FUZZY INDEX`,
  `db.searchFuzzy`, `GET /v1/{ns}/fuzzy`. Layout version 9 marks files that
  may hold one.
- A `MatchRule` per label (table `match_rules`): strong keys decide (equal
  values trimmed and lowercased: same; different: different); otherwise
  the score is the weighted mean of the fields present on both sides
  (the best pair for lists such as aliases), plus shared neighbors
  (Jaccard of the neighbor sets) as one more field when both sides have
  neighbors (no links known says nothing). `same` and `review` cut the
  score into three decisions, and every `Match` carries its fields:
  measure, both values, similarity and weight. `find_matches` takes its
  candidates from the indexes on the rule's keys (exact and fuzzy), or
  scans the first 20 000 nodes of the label when there are none;
  `match_pairs` matches every node of a label against the others for
  deduplication, leaving out pairs marked `distinct_from`. Exposed as
  `db.match`, `POST /v1/{ns}/match`, `GET /v1/{ns}/matches`, the rules at
  `/v1/{ns}/match-rules` (admin to change), the `find_matches` MCP tool and
  the TypeScript client. `find_duplicates` (10.5) uses the same edit
  distance.
- Measured (`examples/resolve_bench.rs`, numbers in `docs/benchmarks.md`):
  among 100 000 synthetic people, a fuzzy search for a damaged name takes
  20 ms (p50) against 469 ms comparing with every node, and finds the
  true name in the top 10 for 93% of the damaged names. A rule on name
  (Dice and shared words), birth date and city finds 95% of 2 000 damaged
  duplicates as `same` or `review` (81% as `same`), with a precision of
  0.994 for `same`; 9 of 2 000 namesakes were called `same`. With a
  365-day date tolerance instead of 30, 115 namesakes were: a rule is only
  as good as its tolerances, which is why every decision explains itself.
- Not yet: rules travel with exports (they are kept in backups);
  candidate generation by MinHash/LSH for very common names; phonetic
  keys for other languages; a learned weighting (Fellegi-Sunter m and u
  probabilities from labeled pairs).

## 11.1 Incremental extraction and reconciliation of entities (done)



- Extractors without a model: a gazetteer of the namespace's known names
  and aliases (Aho-Corasick over the names index), patterns (emails, URLs,
  dates, amounts, mentions, hashtags, identifiers), and capitalized
  phrases with statistics of how often they recur.
- Incremental: only new or changed text is read, from the change log of
  11.4; each mention becomes a `mentions` edge with its provenance and
  position.
- Reconciliation with the rules of 10.5: a mention links to an existing
  entity when a strong key or an unambiguous name matches, creates a new
  one when nothing does, and is queued for review when it is ambiguous.
- Extractions sent by an agent (from its own LLM) go through the same
  reconciliation.

As built (`grafeno-memory`: `extract` finds, `reconcile` decides):

- Finds carry character offsets in the original text and a normalized
  value. Patterns: emails, URLs (trailing punctuation dropped), `@handles`
  (not inside emails), `#hashtags`, dates (`YYYY-MM-DD`, and `d/m/yyyy`
  day first unless `month_first`, checked for valid months and days),
  amounts with a currency before or after (`S/ 1.250,50` → `PEN 1250.5`:
  the last separator followed by one or two digits is the decimal point),
  and identifier patterns declared with a label (`INV-\d{4}-\d{4}` →
  `Invoice`). Phrases: up to five capitalized words with `de`, `del`,
  `van`, `of` and the like inside, without a leading common word ("El",
  "The") or a sentence's first word followed by a connector ("Hablamos de
  Proyecto Atlas" is "Proyecto Atlas"); a single word that opens a
  sentence is not a phrase. The gazetteer is an Aho-Corasick automaton
  (leftmost-longest) over every normalized name and alias of the entity
  nodes, matched on the normalized text at word boundaries and mapped back
  to the original offsets.
- Sources are the texts of `Memory`, `Note`, `Episode`, `Document`,
  `Chunk`, `Message` and `Summary` nodes by default; entities are the named
  nodes that are not sources. A run hashes each text and keeps the hash in
  the `state` table (layout version 10, a small key-value store per
  namespace for features like this one): only new or changed texts are
  read, in batches of 100 per transaction (in the bulk lane over HTTP).
  Reading a text again replaces the edges extraction wrote for it
  (`extracted: true`) and its occurrences in the review queue.
- Reconciliation: a name with one node links to it. With several, the
  candidate linked to what the same text already links (its neighbors
  among the text's other entities) wins when it is the only one; otherwise
  the name waits in the review queue as `ambiguous`, every occurrence
  grouped under one item. Emails link to the node whose `email` it is
  (through an exact index on `email`), else to an `Email` node; URLs,
  handles, hashtags and identifiers link to a node per value; dates and
  amounts are kept on the text's node (`mentioned_dates`,
  `mentioned_amounts`). An unknown phrase waits as `phrase` until it shows
  up in `min_phrase_sources` (2) different texts, then becomes an `Entity`
  named by its most frequent spelling, with the texts as its `sources`. A
  phrase that became known in the meantime links like a name.
- Mentions are `mentions` edges from the text's node, with the surface,
  `start` and `end` (characters), the `kind` of find and the recorder (the
  transaction's actor). The queue is settled with `link`, `create` or
  `dismiss`; the decision is kept, so the same name links (or is ignored)
  in later runs without asking again.
- An agent's mentions (`ingest`, `POST /v1/{ns}/nodes/{id}/mentions`, the
  `add_mentions` MCP tool) go through the label's match rule when there is
  one (same → link, review → queued with the candidates, different → a new
  node with the agent's properties), else by name; their edges have `kind:
  agent` and stay when the text is extracted again.
- Exposed as `grafeno extract`, `POST /v1/{ns}/extract`, `GET
  /v1/{ns}/review`, `POST /v1/{ns}/review/{key}`, the `extract`, `review`
  and `resolve_review` MCP tools, and the TypeScript client.
- Measured (`examples/extract_bench.rs`, numbers in `docs/benchmarks.md`):
  20 000 memories naming 5 000 entities (3 982 of which share a name with
  another) are read at about 3 200 texts a second; a second run with
  nothing changed takes 0.06 s, and with 1% changed 0.15 s. 99.4% of the
  mentions link to the right entity and none to a wrong one; of 39 713
  mentions of a shared name, the text's other entities settled all but
  263 names, which wait for review.
- Until 11.4's change log, runs find changes by hashing every text; texts
  are read from nodes by label; and the review queue is scanned whole when
  texts change, which is fine for thousands of items, not millions.

## 11.2 Automatic invalidation of contradicting facts (done)



- Edge cardinality constraints: `CREATE CONSTRAINT FOR ()-[r:works_at]->()
  REQUIRE ONE PER SOURCE` (one current fact per source), also per target
  and per pair.
- A new fact that breaks a constraint ends the old one in valid time
  (its `valid_to` becomes the new fact's `valid_from`) instead of failing
  or keeping both; record time keeps the history, so `KNOWN AT` still
  shows what was believed.
- The same for functional properties; a report of what was invalidated,
  and an option to only flag instead of invalidating.

As built (`crates/grafeno/src/constraints.rs`, table `constraints`, layout
version 11):

- `Constraint::Edge { label, scope, on_conflict }` with scope `source`,
  `target` or `pair`, declared in GQ as `CREATE CONSTRAINT [name] FOR
  ()-[r:works_at]->() REQUIRE ONE PER SOURCE [ON CONFLICT INVALIDATE |
  FLAG | REJECT]`. Every `create_edge` checks the constraints of its
  label (read once per transaction): the current, not retracted facts
  that overlap the new one in valid time, from the same source (to
  another target), to the same target (from another source), or between
  the same pair. The same fact again is no contradiction.
- `invalidate`: a new fact without a start begins when it is recorded. An
  old fact that starts earlier ends where the new one starts; it is not
  edited in place but versioned: the old record is expired (record time)
  and a copy with the new `valid_to` is written, so `KNOWN AT` an earlier
  moment still sees the fact as it was believed then. A new fact older
  than a current one (a late report) ends where the current one starts
  (`shortened`); two facts that start at the same moment: the old record
  is retracted (`replaced`). An `own_only` agent cannot end someone else's
  fact: the contradiction is flagged instead. `flag` keeps both; `reject`
  refuses the new fact (409).
- Every action is recorded (`ConstraintEvent`: constraint, action, new
  and old fact, the replacement version, where it now ends, who, when) in
  the state table: `constraint_events`, `GET /v1/{ns}/contradictions`, and
  per transaction `take_constraint_events` (GQ counts them in
  `facts_invalidated` and `conflicts_flagged`; the MCP `relate` tool tells
  the agent what its fact contradicted). Declaring a constraint on facts
  that already contradict each other flags them, and changes nothing.
- `Constraint::Functional { label, key }` (`REQUIRE p.city IS
  FUNCTIONAL`): a node property has no valid time, so each change of the
  property keeps the earlier value with when it held (since the change
  before, or the node's creation) and who changed it: `property_history`,
  GQ `history(n, 'city')`, `GET /v1/{ns}/nodes/{id}/history/{key}`.
- `SHOW CONSTRAINTS` lists unique, edge and functional constraints
  (`name`, `kind`, `label`, `property`, `rule`); `DROP CONSTRAINT` drops
  either kind.
- Measured (`examples/constraint_bench.rs`): 50 000 job changes for
  10 000 people go from 44 000 to 14 000 facts a second with a one
  employer constraint (each write reads the source's other facts of the
  label); every person ends with exactly one current employer, and 40 069
  earlier jobs stay in valid time.

## 11.3 Hierarchical community summaries (done)



- Louvain levels kept as a hierarchy (communities of communities).
- A summary per community without a model: its most central nodes, its
  distinctive terms (TF-IDF against the namespace), label mix, key facts
  and time span; stored as `Summary` nodes linked to their members and
  parent summaries, refreshed incrementally when the community changes.
- Recall and context use them for broad questions (global search over
  summaries, then drill down); an agent may replace a summary's text with
  its own.

As built (`crates/grafeno-memory/src/communities.rs`, and
`algo::louvain_levels` in the engine; no layout change, summaries are
ordinary nodes):

- `louvain_levels` returns the communities after each aggregation of
  Louvain, finest first; each level is a union of communities of the one
  below. `louvain` returns the last of them, as before.
- `refresh_communities` projects the namespace (every node but summaries
  and archived memories, or the labels asked for; every current fact, or
  the edge labels asked for, optionally weighted) and runs it. A community
  gets a summary when it has at least `min_size` (3) nodes and is not the
  same as the one community under it; levels without any are left out and
  the rest numbered from 0.
- A summary is a `Community` node: `name` (its three most central
  members), `level`, `size`, `central` (members by weighted degree inside
  the community), `terms` (TF-IDF of the members' words against the
  namespace; a word in every node is left out), `labels` (the label mix),
  `facts` (the edges inside it between its most central members),
  `from`/`to` (its time span, from the members and the facts' valid
  dates), and a text that says all that, indexed for full-text search.
  Each node links to the lowest summary that holds it (`member_of`); a
  summary to its parent (`part_of`).
- Incremental: the work is split in `plan_communities` (on a read
  transaction: Louvain, centrality, terms) and `apply_communities` (one
  write transaction), so the server runs the plan off the writer. New
  communities are matched to stored summaries by the members they share
  (Jaccard of at least one half, best pairs first, at any level, so a
  community keeps its summary when the levels around it move); a
  fingerprint of the members, their update times and the facts inside
  decides whether a matched summary is rewritten; unmatched summaries are
  deleted; `member_of` and `part_of` edges are written only where they
  differ.
- `set_community_text` puts an agent's own text on a summary (`custom`);
  refreshes keep it, update the rest, and mark it `stale` when the
  community changed; `None` goes back to the generated text.
- `overview` is the global search: BM25 over summaries (`level` to keep
  one), or the top of the hierarchy, biggest first, without a query;
  `community_members` drills down into the communities a summary is made
  of and its direct members, central ones first. Recall leaves summaries
  out unless asked for by label; `ContextOptions::communities` puts the
  ones that match first, as an `## Overview`, in the items' budget (the
  MCP `context` tool uses 3).
- Server: `POST /v1/{ns}/communities/refresh`, `GET
  /v1/{ns}/communities?q=&level=`, `GET /v1/{ns}/communities/{id}`, `PUT
  /v1/{ns}/communities/{id}/text`; MCP `overview`, `summarize_communities`
  and `set_summary`.
- Measured (`examples/community_bench.rs`, 1 000 teams of 50 in 125
  departments, 160 000 edges): the first refresh takes 3.2 s for about
  9 100 summaries in 4 levels; with nothing changed 1.35 s and nothing
  written; after 1% new nodes 2.2 s, with three quarters of the summaries
  untouched (Louvain moves some boundaries, so some communities split or
  merge). Level 1 matches the teams (purity 0.84 against teams, 0.999
  against departments), level 2 the departments (167 summaries, purity
  0.999); a team's topic word finds its summary first for 200 of 200
  teams, in 0.06 ms.

## 11.4 Continuous ingestion and events (done)



- A change log: every commit appends its changes with a sequence number
  (layout change), kept for a configurable window.
- `GET /v1/{ns}/changes?since=` as a stream (server-sent events) and
  webhooks, so agents and services react to writes.
- Streaming ingestion: NDJSON posted continuously, with idempotency keys
  so a retried event is not written twice.
- Extraction (11.1), invalidation (11.2) and summaries (11.3) run from the
  log, in the background, at their own pace.

As built (`crates/grafeno/src/changes.rs`, table `changes`, layout version
12; `crates/grafeno-server/src/events.rs`):

- Every write (`put_node`, `put_edge`, deletes, texts, embeddings) adds
  to the transaction's pending changes, folded per record: created then
  changed stays `created`, anything then deleted is `deleted`, the parts
  (`record`, `text`, `embedding`) add up. At commit they get sequence
  numbers per namespace (after the last one in the table), the commit's
  time and the actor, and are written in the same transaction, so the log
  and the graph never disagree; an aborted or failed write (a job dropped
  from a group commit) leaves nothing. Entries name the record, not its
  content: a follower reads it.
- The window (`Retention`: on, a million entries and seven days per
  namespace by default; `[changes]` in `grafeno.toml`) is kept at commit,
  at most 10 000 entries pruned per commit. `changes(ns, since, limit)`
  returns a page with `last`, `oldest` and `gap` (changes after `since`
  were pruned: start over from the graph). `on_commit` listeners hear each
  commit that appended, with the namespaces and their last numbers.
- `GET /v1/{ns}/changes?since=` is a page, or with `Accept:
  text/event-stream` (or `?stream=true`) a stream of server-sent events:
  `id` is the sequence number (a client resumes with `Last-Event-ID`),
  `event: change`, and one `gap` event when it started behind the window.
  It wakes on each commit (a watch channel fed by `on_commit`). MCP agents
  poll with the `changes` tool.
- Webhooks (`POST /v1/{ns}/webhooks {url, secret, labels, since}`, admin
  role, since a webhook makes the server send requests): a task delivers
  each one's changes in order, 200 at a time, after each commit and every
  2 s, as `{namespace, webhook, changes, last, gap}`, signed with
  HMAC-SHA256 (`X-Grafeno-Signature: sha256=…`) when it has a secret; only
  labels it asks for (the others move its cursor). Its cursor moves only
  on a 2xx; failures wait 2, 4, … up to 600 s and are recorded
  (`failures`, `last_error`).
- `POST /v1/{ns}/ingest` reads NDJSON as it arrives, a node or an edge per
  line, and writes 500 lines per transaction in the writer's bulk lane. A
  line's `key` is kept (state kind `ingest.key`): sent again it is a
  duplicate and writes nothing, and an edge's ends may be `$key` of nodes
  ingested before. Each line fails alone, with its number in `errors`.
- `[background]` names namespaces where extraction and community
  summaries follow the log: a task per namespace and job reads the changes
  after its cursor (state kind `runner`), runs extraction when a source
  text changed and summaries when enough of the graph changed (not their
  own `Community`, `member_of`, `part_of` writes) and the last refresh is
  old enough, then moves its cursor; `GET /v1/background` says what each
  did. Invalidation (11.2) stays in the write: a constraint that held only
  later would let a reader see two current employers.
- Measured: the log costs about 20% of write throughput (35 000 to 29 500
  records a second, `examples/changes_bench.rs`); it reads back at 1.4
  million entries a second; a commit reaches a server-sent events stream
  in 0.15 ms (p99 0.36 ms); 100 000 NDJSON lines ingest at 25 000 a
  second, and sent again at 360 000 a second, all duplicates
  (`grafeno-server/examples/events_bench.rs`).
