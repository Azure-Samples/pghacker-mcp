// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import {
  assertPreparable, assertReady, createIndexSql, createManifest, edgeIndexName,
  inventory, prepareIndexes, validateManifest,
} from "../scripts/lib/graph-indexes.mjs";
import { psql, psqlJson, psqlSession, run } from "../scripts/lib/psql.mjs";

const url = "postgresql://owner:not-a-real-password@127.0.0.1:5432/fixture";
const identity = { database: "fixture", database_oid: 42, server_address: "127.0.0.1" };
function requirements() {
  const base = { schema: "pghacker_graph", table: "MENTIONS", table_oid: 10,
    owner: "graph_owner", indexes: [], usable_indexes: [], state: "missing", buildable: true,
    estimated_rows: 100, columns: ["id"], kind: "edge_id", label: "MENTIONS", index_name: "MENTIONS_id_idx" };
  return [
    base,
    { ...base, kind: "vertex_id", label: "Message", table: "Message", table_oid: 11,
      index_name: "Message_pkey", state: "ready", buildable: false, usable_indexes: ["Message_pkey"] },
    { ...base, kind: "mapping_endpoint", label: null, schema: "pghacker", table: "graph_edges",
      table_oid: 12, owner: "pgh_indexer", index_name: "graph_edges_start_key_idx",
      columns: ["start_key", "label", "end_key"] },
    { ...base, kind: "mapping_endpoint", label: null, schema: "pghacker", table: "graph_edges",
      table_oid: 12, owner: "pgh_indexer", index_name: "graph_edges_end_key_idx",
      columns: ["end_key", "label", "start_key"] },
  ];
}

test("index names preserve case and never rely on identifier truncation", () => {
  assert.equal(edgeIndexName("MENTIONS"), "MENTIONS_id_idx");
  assert.equal(edgeIndexName("x".repeat(56)).length, 63);
  assert.match(edgeIndexName("x".repeat(57)), /^pgh_eid_[a-f0-9]{32}$/);
  assert.match(edgeIndexName("\u03b1".repeat(30)), /^pgh_eid_[a-f0-9]{32}$/);
});

test("concurrent DDL is one native, quoted, non-unique statement", () => {
  const r = requirements()[0];
  assert.equal(createIndexSql(r),
    'CREATE INDEX CONCURRENTLY "MENTIONS_id_idx" ON ONLY "pghacker_graph"."MENTIONS" USING btree ("id")');
  assert.match(createIndexSql({ ...r, table: 'edge"table', index_name: 'idx"name' }), /"idx""name".*"edge""table"/);
  assert.throws(() => createIndexSql({ ...r, state: "invalid" }), /cannot build/);
  assert.throws(() => createIndexSql(requirements()[1]), /cannot build/);
});

test("manifests contain no credentials and allow only expected resume transitions", () => {
  const before = requirements();
  const manifest = createManifest(url, identity, before);
  assert.ok(!JSON.stringify(manifest).includes("not-a-real-password"));
  const resumed = structuredClone(before);
  resumed[0].state = "ready";
  resumed[0].usable_indexes = ["equivalent_id"];
  validateManifest(manifest, url, identity, resumed);
  assert.throws(() => validateManifest(manifest, url.replace(":5432", ":5433"), identity, before), /scope changed/);
  resumed[0].table_oid++;
  assert.throws(() => validateManifest(manifest, url, identity, resumed), /scope changed/);
});

test("unreviewed labels, missing formerly-ready indexes, and incompatible leftovers fence apply", () => {
  const before = requirements();
  const manifest = createManifest(url, identity, before);
  const extra = [...before, { ...before[0], label: "NEW_EDGE", table: "NEW_EDGE", index_name: "NEW_EDGE_id_idx", table_oid: 99 }];
  assert.throws(() => validateManifest(manifest, url, identity, extra), /scope changed/);
  const invalid = structuredClone(before);
  invalid[0].state = "invalid";
  assert.throws(() => assertPreparable(invalid), /invalid/);
  const plan = createManifest(url, identity, invalid);
  assert.equal(plan.blocked[0].state, "invalid");
  assert.throws(() => assertReady(before), /not ready/);
  const ready = before.map((r) => ({ ...r, state: "ready", usable_indexes: [r.index_name] }));
  const readyPlan = createManifest(url, identity, ready);
  ready[0].state = "missing";
  assert.throws(() => validateManifest(readyPlan, url, identity, ready), /unapproved/);
});

function fixtureUrl() {
  const value = process.env.PGHACKER_INDEX_TEST_URL;
  const parsed = new URL(value);
  assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(parsed.hostname));
  assert.match(parsed.pathname, /^\/pgh_test_[a-f0-9]+$/);
  return value;
}
const isolated = { skip: !process.env.PGHACKER_INDEX_TEST_URL };

test("real operator builds and resumes without changing graph rows", isolated, async () => {
  const db = fixtureUrl();
  await psql(db, `BEGIN;
    INSERT INTO pghacker.graph_nodes (key, label, graph_id, props)
    SELECT key, 'Message', ag_catalog._graphid(l.id, nextval('pghacker_graph."Message_id_seq"')),
           jsonb_build_object('key', key)
    FROM (VALUES ('message:index-one'), ('message:index-two')) keys(key),
         ag_catalog.ag_label l JOIN ag_catalog.ag_graph g ON g.graphid = l.graph
    WHERE g.name = 'pghacker_graph' AND l.name = 'Message';
    INSERT INTO pghacker_graph."Message" (id, properties)
      SELECT graph_id, props::text::ag_catalog.agtype FROM pghacker.graph_nodes;
    INSERT INTO pghacker.graph_edges (label, start_key, end_key, graph_id)
    SELECT 'MENTIONS', 'message:index-one', 'message:index-two',
           ag_catalog._graphid(l.id, nextval('pghacker_graph."MENTIONS_id_seq"'))
    FROM ag_catalog.ag_label l JOIN ag_catalog.ag_graph g ON g.graphid = l.graph
    WHERE g.name = 'pghacker_graph' AND l.name = 'MENTIONS';
    INSERT INTO pghacker_graph."MENTIONS" (id, start_id, end_id, properties)
    SELECT e.graph_id, s.graph_id, t.graph_id, '{}'::ag_catalog.agtype
    FROM pghacker.graph_edges e JOIN pghacker.graph_nodes s ON s.key = e.start_key
         JOIN pghacker.graph_nodes t ON t.key = e.end_key;
    COMMIT;`);
  const snapshotSql = `SELECT jsonb_build_object(
    'nodes', (SELECT jsonb_agg(to_jsonb(n) ORDER BY key) FROM pghacker.graph_nodes n),
    'edges', (SELECT jsonb_agg(to_jsonb(e) ORDER BY graph_id) FROM pghacker.graph_edges e),
    'vertices', (SELECT jsonb_agg(jsonb_build_object('id', id::text, 'props', properties::text)
      ORDER BY id) FROM ONLY pghacker_graph."Message"),
    'mentions', (SELECT jsonb_agg(jsonb_build_object('id', id::text, 'start', start_id::text,
      'end', end_id::text, 'props', properties::text) ORDER BY id) FROM ONLY pghacker_graph."MENTIONS"))`;
  const snapshot = await psqlJson(db, snapshotSql);
  await psql(db, 'DROP INDEX pghacker_graph."MENTIONS_id_idx"');
  const before = await inventory(db);
  const manifest = createManifest(db, before.identity, before.requirements);
  const events = [];
  const result = await prepareIndexes(db, manifest, { buildTimeoutMs: 10000, progress: (event) => events.push(event) });
  assert.deepEqual(result.completed, ["pghacker_graph.MENTIONS_id_idx"]);
  assert.equal(result.ready, true);
  assert.ok(events.some((event) => event.event === "completed"));
  const resumed = await prepareIndexes(db, manifest, { buildTimeoutMs: 10000 });
  assert.equal(resumed.ready, true);
  assert.deepEqual(resumed.completed, []);
  assert.deepEqual(await psqlJson(db, snapshotSql), snapshot);
});

test("duplicate physical IDs stop preparation without repairing data", isolated, async () => {
  const db = fixtureUrl();
  await psql(db, `INSERT INTO pghacker_graph."MENTIONS" SELECT * FROM ONLY pghacker_graph."MENTIONS" LIMIT 1;
    DROP INDEX pghacker_graph."MENTIONS_id_idx";`);
  try {
    const current = await inventory(db);
    const manifest = createManifest(db, current.identity, current.requirements);
    await assert.rejects(prepareIndexes(db, manifest, { buildTimeoutMs: 10000 }), /duplicate physical/);
    assert.equal((await inventory(db)).requirements.find((r) => r.kind === "edge_id" && r.label === "MENTIONS").state, "missing");
    assert.equal(await psqlJson(db, 'SELECT to_jsonb(count(*)) FROM ONLY pghacker_graph."MENTIONS"'), 2);
  } finally {
    await psql(db, `DELETE FROM ONLY pghacker_graph."MENTIONS"
      WHERE ctid = (SELECT ctid FROM ONLY pghacker_graph."MENTIONS" ORDER BY ctid DESC LIMIT 1);`);
    const current = await inventory(db);
    await prepareIndexes(db, createManifest(db, current.identity, current.requirements), { buildTimeoutMs: 10000 });
  }
});

test("wrong named definitions are diagnosed, not skipped or replaced", isolated, async () => {
  const db = fixtureUrl();
  await psql(db, 'DROP INDEX pghacker_graph."MENTIONS_id_idx"; CREATE INDEX "MENTIONS_id_idx" ON pghacker_graph."MENTIONS" (start_id)');
  try {
    const current = await inventory(db);
    const r = current.requirements.find((r) => r.kind === "edge_id" && r.label === "MENTIONS");
    assert.equal(r.state, "name_conflict");
    const manifest = createManifest(db, current.identity, current.requirements);
    await assert.rejects(prepareIndexes(db, manifest, { buildTimeoutMs: 10000 }), /operator review/);
  } finally {
    await psql(db, 'DROP INDEX pghacker_graph."MENTIONS_id_idx"; CREATE INDEX "MENTIONS_id_idx" ON pghacker_graph."MENTIONS" (id)');
  }
});

test("the CLI reports canary incompleteness, uses mapping ownership, and refuses reader DDL", isolated, async () => {
  const db = fixtureUrl();
  const directory = mkdtempSync(path.join(tmpdir(), "pgh-index-manifest-"));
  const manifestPath = path.join(directory, "scope.json");
  let reader;
  try {
    reader = new URL(process.env.PGHACKER_INDEX_TEST_READER_URL);
  } catch {
    throw new Error("Rust fixture must provide PGHACKER_INDEX_TEST_READER_URL");
  }
  const owner = new URL(db);
  if (reader.host !== owner.host || reader.pathname !== owner.pathname || reader.username !== "pgh_reader") {
    throw new Error("Reader fixture URL must match the owned database and use pgh_reader");
  }
  const env = { ...process.env, PGHACKER_ADMIN_DATABASE_URL: db, PGHACKER_INDEXER_DATABASE_URL: reader.toString() };
  const cli = (args, override = {}) => run(process.execPath, ["scripts/graph-indexes.mjs", ...args],
    { env: { ...env, ...override }, timeoutMs: 30000 });
  const last = (result) => JSON.parse(result.stdout.trim().split("\n").at(-1));
  await psql(db, `DROP INDEX pghacker_graph."ABOUT_id_idx";
    DROP INDEX pghacker.graph_edges_start_key_idx; DROP INDEX pghacker.graph_edges_end_key_idx;`);
  try {
    const planned = await cli(["--plan", "--out", manifestPath]);
    assert.equal(planned.code, 0, planned.stderr || planned.stdout);
    assert.equal(statSync(manifestPath).mode & 0o777, 0o600);
    assert.ok(!readFileSync(manifestPath, "utf8").includes(new URL(db).password));
    assert.equal((await cli(["--check"])).code, 1);
    const denied = await cli(["--apply", "--plan-file", manifestPath, "--build-timeout-ms", "10000"],
      { PGHACKER_ADMIN_DATABASE_URL: reader.toString() });
    assert.equal(denied.code, 1);
    assert.match(last(denied).error, /SET membership/);
    const canary = await cli(["--apply", "--plan-file", manifestPath, "--build-timeout-ms", "10000",
      "--only", "pghacker_graph.ABOUT_id_idx"]);
    assert.equal(canary.code, 2, canary.stderr || canary.stdout);
    assert.equal(last(canary).ready, false);
    assert.equal(last(canary).remaining.length, 2);
    const completed = await cli(["--apply", "--plan-file", manifestPath, "--build-timeout-ms", "10000"]);
    assert.equal(completed.code, 0, completed.stderr || completed.stdout);
    assert.equal(last(completed).ready, true);
    const owners = await psqlJson(db, `SELECT jsonb_agg(pg_get_userbyid(c.relowner))
      FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = 'pghacker' AND c.relname IN ('graph_edges_start_key_idx', 'graph_edges_end_key_idx')`);
    assert.deepEqual(owners, ["pgh_indexer", "pgh_indexer"]);
    assert.equal((await cli(["--check"])).code, 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("a build waits for older writers longer than the lock timeout", isolated, async () => {
  // Production always has transactions older than two seconds, such as a
  // graph sync: the build must wait for them within its budget.
  const db = fixtureUrl();
  await psql(db, 'DROP INDEX pghacker_graph."TARGETS_id_idx"');
  const writer = psqlSession(db);
  try {
    await writer.query("BEGIN");
    await writer.query('LOCK TABLE ONLY pghacker_graph."TARGETS" IN ROW EXCLUSIVE MODE');
    const before = await inventory(db);
    const manifest = createManifest(db, before.identity, before.requirements);
    const prepared = prepareIndexes(db, manifest, { buildTimeoutMs: 30000, only: "pghacker_graph.TARGETS_id_idx" });
    await sleep(4000);
    await writer.query("COMMIT");
    const result = await prepared;
    assert.deepEqual(result.completed, ["pghacker_graph.TARGETS_id_idx"]);
    assert.equal((await inventory(db)).requirements
      .find((r) => r.kind === "edge_id" && r.label === "TARGETS").state, "ready");
  } finally {
    await writer.close();
  }
});

test("a timed-out concurrent build leaves explicit invalid state and needs approved recovery", isolated, async () => {
  const db = fixtureUrl();
  await psql(db, 'DROP INDEX pghacker_graph."MENTIONS_id_idx"');
  const blocker = psqlSession(db);
  const builder = psqlSession(db);
  try {
    await blocker.query("BEGIN");
    await blocker.query('LOCK TABLE ONLY pghacker_graph."MENTIONS" IN ROW EXCLUSIVE MODE');
    await builder.query("SET statement_timeout = '10s'");
    const pid = await builder.json("SELECT pg_backend_pid()");
    const cancelled = assert.rejects(builder.query(
      'CREATE INDEX CONCURRENTLY "MENTIONS_id_idx" ON ONLY pghacker_graph."MENTIONS" (id)'
    ), /57014/);
    let entered = false;
    for (let attempt = 0; attempt < 80 && !entered; attempt++) {
      entered = await psqlJson(db, `SELECT to_jsonb(EXISTS (
        SELECT FROM pg_stat_progress_create_index p JOIN pg_index i ON i.indexrelid = p.index_relid
        WHERE p.pid = ${pid} AND p.phase = 'waiting for writers before build' AND NOT i.indisvalid))`);
      if (!entered) await sleep(25);
    }
    assert.equal(entered, true, "concurrent build did not publish its invalid index before cancellation");
    builder.cancel();
    await cancelled;
    const current = await inventory(db);
    assert.equal(current.requirements.find((r) => r.kind === "edge_id" && r.label === "MENTIONS").state, "invalid");
    await assert.rejects(prepareIndexes(db, createManifest(db, current.identity, current.requirements),
      { buildTimeoutMs: 10000 }), /operator review/);
  } finally {
    await blocker.query("ROLLBACK");
    await blocker.close();
    await assert.rejects(builder.close());
    await psql(db, 'DROP INDEX CONCURRENTLY IF EXISTS pghacker_graph."MENTIONS_id_idx"');
    const after = await inventory(db);
    await prepareIndexes(db, createManifest(db, after.identity, after.requirements), { buildTimeoutMs: 10000 });
  }
});
