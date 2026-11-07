# Phase 7: Memory

[All phases](README.md) · [← Phase 6: Studio and installers](phase-06-studio-and-installers.md) · [Phase 8: Performance, query language and analysis →](phase-08-performance-query-language-and-analysis.md)

**Status:** done.

Entity resolution by normalized names and aliases, importance, accesses and decay in ranking, duplicates and merging, archiving, consolidation into summaries.

## Memory (phase 7)

- **Names**: the `names` index holds each node's `name`, `title` and
  `label` properties and its `aliases` (a list of strings), normalized:
  lowercase, without accents or punctuation, spaces collapsed. "Ana García",
  "ana  garcia" and "ANA GARCÍA" are the same key. `find_by_name` and
  `GET /v1/{ns}/resolve` look it up; MCP `remember` with a `name` uses it to
  update an existing entity (with a label in common) instead of creating a
  duplicate.
- **Importance and accesses**: each node has a memory state, importance
  from 0 to 1 (0.5 by default), accesses and the last access (the last update
  until the first access). Only states that differ from the default are
  stored. Recall touches what it returns when asked (`touch`; MCP always
  does).
- **Ranking**: recall multiplies each fused score by `1 + 0.5 × importance +
  0.3 × recency`, where recency halves every 30 days since the last access
  (all configurable, or off). The boost is applied to a pool of
  `max(5 × limit, 100)` candidates, so it can reorder results but not invent
  them.
- **Decay**: strength = importance × recency × (1 + ln(1 + accesses) / 4),
  capped at 1. `weak_memories` lists nodes below a threshold (0.1); nothing
  is deleted automatically. `archive` adds the `Archived` label, which recall
  leaves out unless `include_archived`.
- **Duplicates**: pairs with the same label and a similar normalized name (1
  for equal names, at least 0.7 when one contains the other, word Jaccard
  otherwise, typos counted at 0.8 of a word), the same value of a strong key
  (1), or a cosine similarity of at least 0.92 between embeddings. Pairs
  joined by a `distinct_from` edge, or that the identity rules rule out,
  are not proposed (see 10.5).
- **Merging**: `merge_nodes(keep, merge)` moves the edges to the kept node
  (dropping edges between the two and identical ones), joins labels and
  sources, keeps the kept node's properties and adds the missing ones, turns
  the other names into `aliases`, joins the texts, keeps one embedding,
  combines memory states (highest importance, sum of accesses) and deletes
  the merged node, all in one transaction. It refuses nodes that disagree
  unless forced, and records itself so that it can be undone (see 10.5).
- **Consolidation**: `consolidation_groups` finds entities with at least 3
  memories (`Memory`, `Episode`, `Note`) older than 7 days pointing at them.
  The summary is written by the agent (the engine calls no model):
  `write_summary` stores a `Summary` node with `summarizes` edges to its
  sources and an `about` edge, gives it the highest importance of its
  sources and archives them. Provenance is kept: the sources stay, archived.
- Exposed through the library (`grafeno` and `grafeno-memory::maintenance`),
  HTTP (`/resolve`, `/duplicates`, `/merge`, `/touch`, `/weak`, `/archive`,
  `/consolidation`, `/summaries`, `importance` in `PATCH /nodes/{id}`), MCP
  (`merge`, `maintain`, `consolidate`, `importance` in `remember` and
  `update`), GQ (`n.@importance`, `n.@accesses`, `n.@last_access`, `SET
  n.@importance`) and the Studio's Memory view. Export and import keep memory
  states.
