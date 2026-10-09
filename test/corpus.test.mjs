import assert from "node:assert/strict";
import test from "node:test";
import {
  EXCLUDED_TABLES, buildManifest, checkManifest, corpusId, countDifferences, countsSql, foreignKeyScripts,
  migrationDifferences,
} from "../scripts/lib/corpus.mjs";

const migrations = [
  { version: "0001_core", sha256: "a".repeat(64) },
  { version: "0002_mail", sha256: "b".repeat(64) },
];

function manifest(overrides = {}) {
  return buildManifest({
    id: "20261009-2115-0123abcd",
    createdAt: "2026-10-09T21:15:00.000Z",
    commit: "3bd286d",
    database: "pghacker_mcp",
    migrations,
    tables: { messages: 6327, threads: 880 },
    extensions: { age: "1.7.0", vector: "0.8.7" },
    embedding: { model: "text-embedding-3-small", dimensions: 1536 },
    repos: ["postgres"],
    dump: { file: "corpus.dump", bytes: 1000, sha256: "c".repeat(64) },
    ...overrides,
  });
}

test("a dump's ID is when it was taken and the start of its checksum", () => {
  assert.equal(corpusId(new Date("2026-10-09T21:15:42Z"), "0123abcd".padEnd(64, "0")), "20261009-2115-0123abcd");
});

test("manifests describe the corpus and refuse code that isn't PostgreSQL's", () => {
  const m = manifest();
  assert.equal(m.version, 1);
  assert.deepEqual(m.excluded_tables, EXCLUDED_TABLES);
  assert.throws(() => manifest({ repos: ["postgres", "study-repo"] }), /study-repo/);
  assert.throws(() => manifest({ migrations: [] }), /no migrations/);
  assert.throws(() => checkManifest({ ...m, dump: { file: "corpus.dump" } }), /checksum/);
  assert.throws(() => checkManifest({ ...m, version: 2 }), /version 1/);
});

test("a restore checks migrations and row counts against the manifest", () => {
  assert.deepEqual(migrationDifferences(migrations, migrations), []);
  assert.deepEqual(migrationDifferences(migrations, [migrations[0]]), ["0002_mail isn't applied to the target"]);
  assert.deepEqual(
    migrationDifferences(migrations, [migrations[0], { version: "0002_mail", sha256: "d".repeat(64) }, { version: "0003_x", sha256: "e" }]),
    ["0002_mail differs", "0003_x is applied to the target but not to the corpus"],
  );
  assert.deepEqual(countDifferences({ messages: 5, threads: 2 }, { messages: "5", threads: 2 }), []);
  assert.deepEqual(countDifferences({ messages: 5, threads: 2 }, { messages: 4 }), [
    "messages: 4 rows, expected 5", "threads: missing rows, expected 2",
  ]);
});

test("foreign keys are dropped for the load and added back with qualified references", () => {
  const scripts = foreignKeyScripts([
    { table: "pghacker.messages", name: "messages_thread_id_fkey", definition: "FOREIGN KEY (thread_id) REFERENCES pghacker.threads(id)" },
  ]);
  assert.equal(scripts.drop,
    'BEGIN; SET LOCAL search_path = pg_catalog; ALTER TABLE pghacker.messages DROP CONSTRAINT "messages_thread_id_fkey"; COMMIT;');
  assert.equal(scripts.add,
    'BEGIN; SET LOCAL search_path = pg_catalog; ALTER TABLE pghacker.messages ADD CONSTRAINT "messages_thread_id_fkey" FOREIGN KEY (thread_id) REFERENCES pghacker.threads(id); COMMIT;');
});

test("row counts leave out the graph's bookkeeping and the migrations", () => {
  const sql = countsSql();
  for (const table of EXCLUDED_TABLES) assert.ok(sql.includes(`'${table}'`));
  assert.match(sql, /NOT IN/);
});
