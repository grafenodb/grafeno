# Phase 16: Integrations

[All phases](README.md) · [← Phase 15: Engine and interface apart](phase-15-engine-and-interface-apart.md)

Grafito first, Python bindings.

## Integration with Grafito

- When a note syncs, one batch: upsert the `Note` node with Grafito's id,
  replace its chunks and its edges for `[[links]]` (`links_to`) and `#tags`
  (`tagged`). Namespace = the user's vault.
- `search_notes` and the agent loop use `recall` / `context_for`.
- The assistant keeps its own memory per user (preferences, facts), with
  provenance pointing to the conversation.
- `GraphView` can ask the engine for a `subgraph` instead of computing the
  graph in the client.
- Deployment: a `grafeno` container in `docker-compose.yml` (sidecar).
