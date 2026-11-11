// End-to-end test: starts the real `grafeno serve` binary and drives it
// through the client. Build it first (`cargo build -p grafeno-server`) or
// point GRAFENO_BIN to it.

import { type ChildProcess, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { after, before, test } from "node:test";
import assert from "node:assert/strict";

import { type Conflict, Grafeno, GrafenoError } from "../src/index.ts";

const binary =
  process.env.GRAFENO_BIN ??
  fileURLToPath(
    new URL(`../../../target/debug/grafeno${process.platform === "win32" ? ".exe" : ""}`, import.meta.url),
  );
const token = "test-token";
let server: ChildProcess;
let url: string;

before(async () => {
  // Event subscriptions are off unless the settings turn them on.
  const config = join(mkdtempSync(join(tmpdir(), "grafeno-client-")), "grafeno.toml");
  writeFileSync(config, ["[events]", "enabled = true", ""].join("\n"));
  server = spawn(binary, ["serve", "--config", config, "--memory", "--addr", "127.0.0.1:0", "--token", token], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  url = await new Promise<string>((resolve, reject) => {
    let output = "";
    server.stderr!.on("data", (chunk: Buffer) => {
      output += chunk.toString();
      // Wait for the whole line: a chunk can end in the middle of the port.
      const match = output.match(/listening on (http:\/\/\S+)\r?\n/);
      if (match) resolve(match[1]);
    });
    server.once("exit", (code) => reject(new Error(`grafeno exited with ${code}: ${output}`)));
  });
});

after(async () => {
  server.kill();
  await once(server, "exit");
});

test("full-text indexes and hybrid search", async () => {
  const ns = new Grafeno({ url, token }).namespace("ft");
  await ns.query(
    "CREATE (:Note {title: 'Graph memory', body: 'Agents remember.'}), (:Note {title: 'Cooking', body: 'Rice.'})",
  );
  await ns.query("CREATE FULLTEXT INDEX notes FOR (n:Note) ON EACH [n.title, n.body]");
  const hits = await ns.searchText("\"graph memory\"", { index: "notes", fragments: true });
  assert.equal(hits.length, 1);
  assert.equal(hits[0].fragments?.[0].field, "title");
  const found = await ns.search({ text: "rice OR agents", index: "notes", limit: 5 });
  assert.equal(found.length, 2);
  assert.equal(found[0].text_rank, 1);
});

test("vector indexes, filters and similar nodes", async () => {
  const ns = new Grafeno({ url, token }).namespace("vec");
  await ns.query(
    "UNWIND range(0, 39) AS i CREATE (:Item {i: i, tier: i % 2, v: [cos(radians(i * 9)), sin(radians(i * 9))]})",
  );
  await ns.query("CREATE VECTOR INDEX angle FOR (n:Item) ON (n.v) OPTIONS {dimension: 2}");
  const odd = await ns.searchVector([1, 0], { index: "angle", limit: 3, filter: { props: { tier: 1 } } });
  assert.equal(odd.length, 3);
  const both = await ns.searchVectors([[1, 0], [0, 1]], { index: "angle", limit: 1 });
  assert.equal(both.length, 2);
  const near = await ns.similar(both[0][0].node, { index: "angle", limit: 2 });
  assert.equal(near.length, 2);
  assert.ok(near.every((hit) => hit.node !== both[0][0].node));
});

test("analysis and simulation algorithms, with their modes", async () => {
  const ns = new Grafeno({ url, token }).namespace("algos");
  await ns.query(
    "CREATE (a:Room {name: 'a', heat: 10.0})-[:Door]->(b:Room {name: 'b', heat: 0.0})-[:Door]->(c:Room {name: 'c', heat: 0.0})",
  );
  const points = await ns.algorithm("articulationPoints");
  assert.deepEqual(points.rows.map((row) => (row[0] as { name: string }).name), ["b"]);
  const ids = (await ns.query("MATCH (r:Room) RETURN r.@id ORDER BY r.name")).rows.map((row) => row[0]);
  const field = await ns.algorithm("distanceField", { args: [[ids[0]]] });
  assert.equal(field.rows.length, 3);
  const stats = await ns.algorithm("diffusion", {
    settings: { amount: "heat", steps: 5 },
    stats: true,
  });
  assert.deepEqual(stats.columns, ["column", "count", "min", "max", "mean", "sum"]);
  const amount = stats.rows.find((row) => row[0] === "amount")!;
  assert.ok(Math.abs((amount[5] as number) - 10) < 1e-9, "the total stays");
  const written = await ns.algorithm("kCore", { write: "core" });
  assert.equal(written.rows[0][0], 3);
  await assert.rejects(ns.algorithm("kCore", { settings: { mode: "stats" } }), /stats/);
});

test("a traced query gives its walk", async () => {
  const ns = new Grafeno({ url, token }).namespace("traced");
  await ns.query("CREATE (:P {name: 'a'})-[:K]->(:P {name: 'b'}), (:P {name: 'c'})");
  const result = await ns.query("MATCH (a:P {name: 'a'})-[:K]->(b) RETURN b.name", {}, { trace: true });
  assert.deepEqual(result.rows, [["b"]]);
  const kinds = result.trace!.events.map((event) => event.kind);
  assert.deepEqual(kinds.slice(0, 1), ["anchor"]);
  assert.ok(kinds.includes("expand") && kinds.includes("row"));
  assert.equal(result.trace!.cut, false);
  assert.equal((await ns.query("RETURN 1")).trace, undefined);
});

test("event subscriptions carry values", async () => {
  const ns = new Grafeno({ url, token }).namespace("watched");
  await ns.query("CREATE EVENT SUBSCRIPTION moves ON PROPERTIES FOR :Person KEYS city");
  await ns.query("CREATE (:Person {name: 'Ana', city: 'Lima'})");
  await ns.query("MATCH (p:Person) SET p.city = 'Cusco', p.age = 40");
  const page = await ns.events("moves", { since: 0 });
  assert.equal(page.changes.length, 2);
  const moved = page.changes[1].values!;
  assert.equal(moved.before!.city, "Lima");
  assert.equal(moved.after!.city, "Cusco");
  assert.deepEqual(moved.changed, ["age", "city"]);
  const rest = await ns.events("moves", { since: page.through });
  assert.equal(rest.changes.length, 0);
});

test("explicit transactions commit together or not at all", async () => {
  const ns = new Grafeno({ url, token }).namespace("tx");
  const count = async () => (await ns.query("MATCH (n:Note) RETURN count(n)")).rows[0][0];
  await ns.transaction(async (tx) => {
    await tx.query("CREATE (:Note {n: $n})", { n: 1 });
    await tx.query("CREATE (:Note {n: 2})");
    assert.equal(await count(), 0, "not visible before the commit");
  });
  assert.equal(await count(), 2);
  await assert.rejects(
    ns.transaction(async (tx) => {
      await tx.query("CREATE (:Note {n: 3})");
      throw new Error("changed my mind");
    }),
    /changed my mind/,
  );
  assert.equal(await count(), 2, "rolled back");
  const reading = await ns.begin({ readOnly: true });
  await ns.query("CREATE (:Note {n: 4})");
  assert.equal((await reading.query("MATCH (n:Note) RETURN count(n)")).rows[0][0], 2);
  assert.equal((await reading.status()).statements, 1);
  await reading.commit();
});

test("health and auth", async () => {
  const grafeno = new Grafeno({ url, token });
  assert.equal((await grafeno.health()).status, "ok");
  const anonymous = new Grafeno({ url }).namespace("vault");
  await assert.rejects(anonymous.listNodes(), (error: unknown) => {
    assert.ok(error instanceof GrafenoError);
    assert.equal(error.status, 401);
    assert.equal(error.code, "unauthorized");
    return true;
  });
});

test("sync a note as a notes application would", async () => {
  const vault = new Grafeno({ url, token }).namespace("user-1");
  const noteId = "0190f5c2-7a3e-7cde-8f01-00000000000a";
  const otherId = "0190f5c2-7a3e-7cde-8f01-00000000000b";
  await vault.putNode(otherId, { labels: ["Note"], props: { title: "Ownership" }, text: "Each value has one owner" });

  const sync = (text: string, links: string[]) =>
    vault.batch([
      { op: "upsert_node", ref: "$note", node: { id: noteId, labels: ["Note"], props: { title: "Rust" }, text } },
      { op: "delete_edges_from", from: "$note", label: "links_to" },
      ...links.map((to) => ({ op: "create_edge" as const, edge: { from: "$note", label: "links_to", to } })),
    ]);

  const first = await sync("The borrow checker, see [[Ownership]]", [otherId]);
  assert.equal(first.refs.$note, noteId);
  assert.equal((await vault.neighbors(noteId, { direction: "out" })).length, 1);

  // Saving again with no links replaces them; the note keeps its id.
  const second = await sync("The borrow checker", []);
  assert.equal(second.results[1].deleted, 1);
  assert.equal((await vault.neighbors(noteId, { direction: "out" })).length, 0);
  assert.equal((await vault.getNode(noteId)).text, "The borrow checker");
});

test("recall and context", async () => {
  const memory = new Grafeno({ url, token }).namespace("agent");
  const { refs } = await memory.batch([
    { op: "create_node", ref: "$ana", node: { labels: ["Person"], props: { name: "Ana" } } },
    {
      op: "create_node",
      ref: "$pref",
      node: { labels: ["Preference"], text: "Prefers answers in Spanish", embedding: [1, 0, 0] },
    },
    { op: "create_edge", edge: { from: "$pref", label: "about", to: "$ana", valid_from: "2026-01-01" } },
  ]);

  const recalled = await memory.recall({ text: "answers in Spanish", embedding: [0.9, 0.1, 0] });
  assert.equal(recalled[0].node.id, refs.$pref);
  assert.deepEqual(
    recalled[0].signals.map((signal) => signal.kind),
    ["text", "vector", "memory"],
  );
  assert.equal(recalled[1].node.id, refs.$ana);

  const context = await memory.context({ text: "Spanish", budget: 300 });
  assert.match(context.rendered, /^## Memory\n\[1\] \(Preference\)\nPrefers answers in Spanish\n/);
  assert.match(context.rendered, /- “Prefers answers in Spanish” about Ana \(from 2026-01-01\)/);
  assert.ok(context.tokens <= 300);

  const edge = (await memory.neighbors(refs.$ana, { direction: "in" }))[0].edge;
  const invalidated = await memory.invalidateEdge(edge.id, "2026-06-01T00:00:00Z");
  assert.equal(invalidated.valid_to, Date.UTC(2026, 5, 1) * 1000);
  assert.equal((await memory.neighbors(refs.$ana, { validAt: "2026-07-01" })).length, 0);
  assert.equal((await memory.neighbors(refs.$ana, { validAt: "2026-03-01" })).length, 1);
});

test("administration: namespaces, stats, pages, check, export and import", async () => {
  const grafeno = new Grafeno({ url, token });
  const admin = grafeno.namespace("admin-test");
  await admin.batch([
    ...[0, 1, 2, 3, 4].map((i) => ({
      op: "create_node" as const,
      ref: `$n${i}`,
      node: { labels: ["N"], text: `item number ${i}` },
    })),
    { op: "create_edge", edge: { from: "$n0", label: "next", to: "$n1" } },
  ]);

  const names = (await grafeno.namespaces()).map((info) => info.name);
  assert.ok(names.includes("admin-test"));
  const stats = await admin.stats();
  assert.equal(stats.nodes, 5);
  assert.equal(stats.node_labels.N, 5);
  assert.equal(stats.texts, 5);

  const page = await admin.listNodes({ limit: 2 });
  assert.equal(page.items.length, 2);
  assert.ok(page.next);
  const all = [];
  for await (const node of admin.allNodes({ limit: 2 })) all.push(node.id);
  assert.equal(new Set(all).size, 5);
  assert.equal((await admin.listEdges({ label: "next" })).items.length, 1);

  assert.deepEqual((await admin.check()).problems, []);
  assert.equal((await admin.rebuild()).nodes, 5);

  const dump = await grafeno.export(["admin-test"]);
  // The header, the namespace, five nodes and an edge.
  assert.equal(dump.trim().split("\n").length, 8);
  await admin.batch([{ op: "delete_node", id: all[0] }]);
  assert.equal((await admin.stats()).nodes, 4);
  const imported = await grafeno.import(dump);
  assert.deepEqual(imported, { namespaces: 1, nodes: 5, edges: 1 });
  assert.equal((await admin.stats()).nodes, 5);
  await assert.rejects(grafeno.import("garbage"), { status: 400 });

  // Namespaces are created explicitly, stay while empty and are dropped whole.
  const created = await grafeno.createNamespace("empty-test", "Nothing yet");
  assert.equal(created.nodes, 0);
  assert.ok((await grafeno.namespaces()).some((info) => info.name === "empty-test"));
  await assert.rejects(grafeno.createNamespace("empty-test"), { status: 409, code: "conflict" });
  assert.equal((await grafeno.describeNamespace("empty-test", null)).description, undefined);
  assert.deepEqual(await grafeno.dropNamespace("admin-test"), { nodes: 5, edges: 1 });
  await assert.rejects(grafeno.getNamespace("admin-test"), { status: 404 });
});

test("GQ queries", async () => {
  const graph = new Grafeno({ url, token }).namespace("query-test");
  const created = await graph.query(
    "CREATE (a:Person {name: $name})-[:works_at {@valid_from: '2024-01-01'}]->(c:Company {name: 'Acme'}) RETURN a.name",
    { name: "Ana" },
  );
  assert.deepEqual(created.rows, [["Ana"]]);
  assert.equal(created.stats.nodes_created, 2);

  const result = await graph.query(
    "MATCH (p:Person)-[w:works_at]->(c) VALID AT '2024-06-01' RETURN p.name AS person, c.name AS company, w.@valid_from",
  );
  assert.deepEqual(result.columns, ["person", "company", "w.@valid_from"]);
  assert.deepEqual(result.rows, [["Ana", "Acme", "2024-01-01T00:00:00Z"]]);
  await assert.rejects(graph.query("MATCH (n RETURN n"), { status: 400, code: "invalid_query" });
});

test("memory: names, importance, duplicates, merge and summaries", async () => {
  const memory = new Grafeno({ url, token }).namespace("memory-test");
  const { refs } = await memory.batch([
    { op: "create_node", ref: "$ana", node: { labels: ["Person"], props: { name: "Ana García" } } },
    { op: "create_node", ref: "$dup", node: { labels: ["Person"], props: { name: "ANA GARCIA" } } },
    ...["likes tea", "moved to Lima", "works at Acme"].map((text, i) => ({
      op: "create_node" as const,
      ref: `$m${i}`,
      node: { labels: ["Memory"], text: `Ana ${text}` },
    })),
    ...[0, 1, 2].map((i) => ({ op: "create_edge" as const, edge: { from: `$m${i}`, label: "about", to: "$ana" } })),
  ]);

  assert.equal((await memory.resolve("ana garcia", "Person")).length, 2);
  const [pair] = await memory.duplicates();
  assert.equal(pair.score, 1);
  const report = await memory.merge(refs.$ana, refs.$dup);
  assert.equal(report.merged, refs.$dup);
  assert.deepEqual(await memory.duplicates(), []);
  const [record] = await memory.merges({ node: refs.$dup });
  assert.equal(record.id, report.id);
  await memory.undoMerge(report.id);
  await memory.merge(refs.$ana, refs.$dup);

  const two = await memory.batch([
    { op: "create_node", ref: "$x", node: { labels: ["Person"], props: { name: "Eva", email: "eva@a.com" } } },
    { op: "create_node", ref: "$y", node: { labels: ["Person"], props: { name: "Eva", email: "eva@b.com" } } },
  ]);
  assert.equal((await memory.mergeConflicts(two.refs.$x, two.refs.$y))[0].key, "email");
  await memory.query("CREATE FUZZY INDEX FOR (p:Person) ON (p.name)");
  assert.equal((await memory.fuzzy("Person", "name", "ana garsia"))[0].node, refs.$ana);
  await memory.setMatchRule({ label: "Person", strong_keys: ["email"], fields: [{ key: "name", measure: "jaro_winkler" }] });
  assert.equal((await memory.matchRules()).length, 1);
  const [best] = await memory.match("Person", { name: "Ana Garsia" });
  assert.equal(best.node, refs.$ana);
  assert.notEqual(best.decision, "different");
  await memory.deleteMatchRule("Person");
  const extracted = await memory.extract();
  assert.ok(extracted.read >= 3, JSON.stringify(extracted));
  // "Acme" and "Lima", seen once each: new phrases wait for a second text.
  const waiting = await memory.reviewQueue();
  assert.deepEqual(waiting.map((item) => item.value).sort(), ["acme", "lima"]);
  await memory.resolveReview(waiting[0].key, { action: "dismiss" });
  assert.equal((await memory.reviewQueue()).length, 1);
  await assert.rejects(memory.merge(two.refs.$x, two.refs.$y), (error: GrafenoError) => {
    assert.equal(error.status, 409);
    assert.equal((error.details.conflicts as Conflict[])[0].kind, "strong_key");
    return true;
  });

  const node = await memory.patchNode(refs.$m0, { importance: 0.9 });
  assert.ok(Math.abs(node.memory.importance - 0.9) < 1e-6);
  await memory.touch([refs.$m0]);
  assert.equal((await memory.getNode(refs.$m0)).memory.accesses, 1);
  assert.deepEqual(await memory.weak(), []);

  const groups = await memory.consolidation({ older_than_days: 0 });
  assert.equal(groups.length, 1);
  assert.equal(groups[0].entity.id, refs.$ana);
  const { id } = await memory.summarize({
    text: "Ana likes tea, lives in Lima and works at Acme.",
    sources: groups[0].memories.map((m) => m.id),
    about: refs.$ana,
  });
  const recalled = await memory.recall({ text: "Lima", expand: false });
  assert.deepEqual(recalled.map((r) => r.node.id), [id]);
  assert.equal((await memory.recall({ text: "Lima", expand: false, include_archived: true })).length, 2);
  assert.equal(await memory.archive([id]), 1);
});

test("errors carry status and code", async () => {
  const vault = new Grafeno({ url, token }).namespace("user-1");
  await assert.rejects(vault.getNode("0190f5c2-7a3e-7cde-8f01-0000000000ff"), { status: 404, code: "not_found" });
  await assert.rejects(vault.createNode({ labels: [] }), { status: 400, code: "invalid" });
});


test("lectura por lotes conserva orden, texto, aristas y ausentes", async () => {
  const lab = new Grafeno({ url, token }).namespace("lecturas-lotes");
  const a = await lab.createNode({ labels: ["Person"], text: "Memoria de Ana" });
  const b = await lab.createNode({ labels: ["Person"] });
  const edge = await lab.createEdge({ from: a.id, to: b.id, label: "knows" });
  const full = await lab.getNode(a.id);
  const result = await lab.readMany({ nodes: [b.id, a.id, a.id], edges: [edge.id] });
  assert.equal(result.nodes[0]?.id, b.id);
  assert.deepEqual(result.nodes.slice(1), [full, full]);
  assert.equal(result.edges[0]?.id, edge.id);
  const foreign = await new Grafeno({ url, token }).namespace("otro-lote").readMany({ nodes: [a.id], edges: [edge.id] });
  assert.deepEqual(foreign, { nodes: [null], edges: [null] });
  await assert.rejects(lab.readMany({ nodes: Array(513).fill(a.id) }), (error: unknown) => error instanceof GrafenoError && error.status === 400);
});

test("vecindarios por lotes equivalen a las expansiones individuales", async () => {
  const lab = new Grafeno({ url, token }).namespace("vecindarios");
  const a = await lab.createNode({ labels: ["Person"] });
  const b = await lab.createNode({ labels: ["Person"] });
  await lab.createEdge({ from: a.id, to: b.id, label: "knows", valid_from: "2020-01-01", valid_to: "2027-01-01" });
  const seeds = [a.id, b.id, a.id];
  for (const valid_at of ["2026-06-01", "2030-01-01"]) {
    const filter = { valid_at };
    const expected = await Promise.all(seeds.map(id => lab.subgraph([id], { depth: 1, filter })));
    assert.deepEqual(await lab.neighborhoods(seeds, { filter }), expected);
  }
  await assert.rejects(lab.neighborhoods(Array(9).fill(a.id)), (error: unknown) => error instanceof GrafenoError && error.status === 400);
});
