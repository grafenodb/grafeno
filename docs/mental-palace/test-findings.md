# Mental palace tests: findings

Tests run against Grafeno 0.3.0 tools, namespace `pruebas` on the local
Windows server unless said otherwise. Each finding: what was done, what
happened, what was expected. Nothing here is fixed yet.

## 1. `palace_enter` shows a superseded memory as if it were current

- Done: two Decisions in a room, the newer `supersedes` the older (with a
  reason); `palace_enter` on the room.
- Happened: both are listed as `[1] (Decision)` and `[2] (Decision)` with
  nothing on the item; only the Facts block says `supersedes`.
- Reproduced on the Windows service (0.3.0), namespace `pruebas`, palace `Trabajo`, room `Facturación`.
- Expected: the replaced one marked (or left out, or placed after the
  current one), so an agent reading the context does not take it as current.

## 2. `palace_reflect` ignores an explicit `supersedes`

- Done: same room; `palace_reflect` with `room`.
- Happened: `stale` is empty; the replaced decision is not listed.
- Reproduced on the Windows service (0.3.0), namespace `pruebas`, same room.
- Expected: a memory that another `supersedes` listed under `stale`
  ("replaced by …"), as walks already do through `justification`. Today
  reflection only finds overtaken memories by word overlap (`superseded`).
