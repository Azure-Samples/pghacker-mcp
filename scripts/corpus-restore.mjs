// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { createHash } from "node:crypto";
import { createReadStream, readFileSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { databaseUrls, processEnvironments, readAzureConfig } from "./lib/azure.mjs";
import {
  CORPUS_TABLES, EXCLUDED_TABLES, FOREIGN_KEYS_SQL, MIGRATIONS_SQL, checkManifest, countDifferences, countsSql,
  foreignKeyScripts, migrationDifferences, quoteIdent,
} from "./lib/corpus.mjs";
import { psql, psqlJson, run, verifiedUrl } from "./lib/psql.mjs";

// Restores a corpus dump (scripts/corpus-dump.mjs) into the Azure
// deployment's database: the restore job runs it from the seed image, which
// holds /seed/corpus.dump and /seed/manifest.json.
//   node scripts/corpus-restore.mjs [--seed /seed] [--replace]
// It bootstraps and migrates the database, checks that its migrations and
// embedding model are the corpus's, refuses a database that has a corpus
// (unless --replace, which empties it and the graph), loads the rows as
// pgh_indexer with the foreign keys dropped, adds them back, releases
// leases and running jobs, analyzes, checks every table's row count, and
// asks for a full graph sync, which rebuilds the graph when the app runs.
const { values: options } = parseArgs({
  options: {
    seed: { type: "string", default: "/seed" },
    replace: { type: "boolean", default: false },
    jobs: { type: "string", default: "4" },
  },
});
const log = (message) => console.log(`[restore] ${message}`);

async function sha256File(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

try {
  const manifest = checkManifest(JSON.parse(readFileSync(path.join(options.seed, "manifest.json"), "utf8")));
  const dumpFile = path.join(options.seed, manifest.dump.file);
  if (await sha256File(dumpFile) !== manifest.dump.sha256) throw new Error(`${dumpFile} doesn't match the manifest's checksum.`);
  const config = readAzureConfig(process.env, { serve: false });
  if (manifest.embedding.model && manifest.embedding.model !== config.foundry.embedding) {
    throw new Error(`The corpus's embeddings are ${manifest.embedding.model}'s; this deployment embeds with ${config.foundry.embedding}.`);
  }
  const urls = databaseUrls(config);
  log(`Corpus ${manifest.id}: ${Object.keys(manifest.tables).length} tables, from ${manifest.source?.commit ?? "an unknown commit"}.`);

  log("Bootstrapping and migrating the database.");
  const bootstrap = await run("/usr/local/bin/pghacker-indexer", ["bootstrap"], {
    env: processEnvironments(config).bootstrap,
  });
  if (bootstrap.code !== 0) throw new Error(`bootstrap failed: ${bootstrap.stderr.trim().slice(-2000)}`);

  const differences = migrationDifferences(manifest.migrations, await psqlJson(urls.indexer, MIGRATIONS_SQL));
  if (differences.length) {
    throw new Error(`The database's migrations aren't the corpus's: ${differences.join("; ")}. Dump the corpus again with this image's migrations.`);
  }

  const present = await psqlJson(urls.indexer, `SELECT json_build_object(${CORPUS_TABLES
    .map((table) => `'${table}', (SELECT count(*) FROM pghacker.${quoteIdent(table)})`).join(", ")})`);
  const rows = Object.values(present).reduce((sum, n) => sum + Number(n), 0);
  if (rows > 0 && !options.replace) {
    throw new Error(`The database already has a corpus (${JSON.stringify(present)}). Use --replace to empty it first.`);
  }
  const indexers = Number(await psql(urls.indexer, `SELECT count(*) FROM pg_stat_activity
    WHERE datname = current_database() AND application_name LIKE 'pghacker-indexer%'`));
  if (indexers > 0) {
    throw new Error("pghacker-indexer is connected: restore before the app runs, or stop it (scale the app to 0) first.");
  }
  const keys = await psqlJson(urls.indexer, FOREIGN_KEYS_SQL);
  const scripts = foreignKeyScripts(keys);
  const tables = Object.keys(manifest.tables).map((table) => `pghacker.${quoteIdent(table)}`);
  // Even without a corpus, a started indexer has queued its recurring jobs,
  // whose keys the corpus's would collide with.
  log(rows > 0 ? "Emptying the corpus and the graph." : "Emptying the tables the corpus fills.");
  const bookkeeping = EXCLUDED_TABLES.filter((t) => t !== "schema_migrations").map((t) => `pghacker.${quoteIdent(t)}`);
  await psql(urls.indexer, `BEGIN;
    TRUNCATE ${[...bookkeeping, ...tables].join(", ")} CASCADE;
    DELETE FROM pghacker_graph._ag_label_edge;
    DELETE FROM pghacker_graph._ag_label_vertex;
    COMMIT;`);

  log(`Dropping ${keys.length} foreign keys for the load.`);
  await psql(urls.indexer, scripts.drop);
  log(`Loading ${(manifest.dump.bytes / 1e6).toFixed(1)} MB with ${options.jobs} connections.`);
  const restore = await run("pg_restore", [
    "--data-only", "--no-owner", "--no-privileges", "--exit-on-error", `--jobs=${Number(options.jobs) || 4}`,
    `--dbname=${verifiedUrl(urls.indexer)}`, dumpFile,
  ]);
  if (restore.code !== 0) {
    throw new Error(`pg_restore failed: ${restore.stderr.trim().slice(-2000)}. The foreign keys are still dropped: rerun with --replace.`);
  }
  log("Adding the foreign keys back.");
  await psql(urls.indexer, scripts.add);

  // The load's rows were captured as graph changes; a full sync covers them.
  log("Releasing leases and running jobs, and asking for a full graph sync.");
  await psql(urls.indexer, `BEGIN;
    TRUNCATE pghacker.graph_changes;
    SELECT pghacker.graph_request_full('corpus restore');
    UPDATE pghacker.extraction_leases SET released_at = now() WHERE released_at IS NULL;
    UPDATE pghacker.jobs SET state = 'queued', locked_by = NULL, locked_at = NULL, run_after = now() WHERE state = 'running';
    UPDATE pghacker.jobs SET run_after = now() WHERE key = 'recurring' AND state = 'queued';
    COMMIT;`);
  await psql(urls.indexer, `ANALYZE ${tables.join(", ")};`);

  const counted = await psqlJson(urls.indexer, countsSql());
  const mismatches = countDifferences(manifest.tables, counted);
  if (mismatches.length) throw new Error(`The restored rows don't match the manifest: ${mismatches.join("; ")}.`);
  const total = Object.values(counted).reduce((sum, n) => sum + Number(n), 0);
  console.log(JSON.stringify({ ok: true, corpus: manifest.id, tables: Object.keys(counted).length, rows: total, counts: counted }));
  log("Done. The app rebuilds the graph from the tables within minutes of starting (ingest_status shows it).");
} catch (error) {
  console.log(JSON.stringify({ ok: false, error: error.message }));
  console.error(`[restore] ${error.message}`);
  process.exitCode = 1;
}
