import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { copyFileSync, createReadStream, createWriteStream, mkdirSync, renameSync, statSync, writeFileSync } from "node:fs";
import { once } from "node:events";
import path from "node:path";
import { parseArgs } from "node:util";
import { readLocalConfig } from "./lib/config.mjs";
import {
  EXCLUDED_TABLES, EXTENSIONS_SQL, MIGRATIONS_SQL, REPOS_SQL, buildManifest, corpusId, countsSql,
} from "./lib/corpus.mjs";
import { execInDatabase } from "./lib/processes.mjs";

// Dumps the local corpus for an Azure deployment to start with
// (deploy/README.md, "First deployment", step 6):
//   npm run corpus:dump [-- --out <dir>]
// Writes <dir>/<id>/corpus.dump (schema pghacker's rows, without the graph's
// bookkeeping, which the target rebuilds), manifest.json (migrations, row
// counts, extensions, embedding model) and the Dockerfile of the seed image
// that the restore job runs. The database must be running (npm start),
// and better idle: a crawl in progress leaves leases, which
// the restore releases.
const { values: options } = parseArgs({
  options: { out: { type: "string", default: "" } },
});

const sha256File = async (file) => {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
};

try {
  const config = readLocalConfig();
  const { POSTGRES_USER: user, POSTGRES_DB: database } = config.values;
  const query = async (sql) => {
    const text = (await execInDatabase(config, [
      "psql", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1",
      "--username", user, "--dbname", database, "--command", sql,
    ])).trim();
    return text ? JSON.parse(text) : null;
  };
  const migrations = await query(MIGRATIONS_SQL);
  const tables = await query(countsSql());
  const extensions = await query(EXTENSIONS_SQL);
  const repos = await query(REPOS_SQL);
  const busy = await query(`SELECT json_build_object(
      'leases', (SELECT count(*) FROM pghacker.extraction_leases WHERE released_at IS NULL AND expires_at > now()),
      'jobs', (SELECT count(*) FROM pghacker.jobs WHERE state = 'running' AND key <> 'recurring'))`);
  if (busy.leases || busy.jobs) {
    console.warn(`[corpus] ${busy.leases} leases and ${busy.jobs} jobs are active: the dump has their work so far.`);
  }

  const root = options.out ? path.resolve(options.out) : path.join(config.runtimeDir, "corpus");
  const staging = path.join(root, `.staging-${process.pid}`);
  mkdirSync(staging, { recursive: true });
  const dumpFile = path.join(staging, "corpus.dump");
  console.log(`[corpus] Dumping schema pghacker of ${database}.`);
  const out = createWriteStream(dumpFile, { mode: 0o600 });
  await execInDatabase(config, [
    "pg_dump", "--username", user, "--dbname", database, "--format=custom", "--compress=6",
    "--data-only", "--schema=pghacker",
    ...EXCLUDED_TABLES.map((table) => `--exclude-table-data=pghacker.${table}`),
  ], { stdout: out });
  // The pipe ends the file when pg_dump's output ends.
  if (!out.writableFinished) await once(out, "finish");
  const sha256 = await sha256File(dumpFile);
  const bytes = statSync(dumpFile).size;
  const createdAt = new Date();
  const id = corpusId(createdAt, sha256);
  let commit = null;
  try {
    commit = execFileSync("git", ["rev-parse", "HEAD"], { cwd: config.root, encoding: "utf8" }).trim();
  } catch {
    // Not a git checkout.
  }
  const manifest = buildManifest({
    id,
    createdAt: createdAt.toISOString(),
    commit,
    database,
    migrations,
    tables,
    extensions,
    embedding: {
      model: config.values.HORIZON_EMBED_MODEL ?? null,
      dimensions: Number(config.values.HORIZON_EMBED_DIM) || null,
    },
    repos,
    dump: { file: "corpus.dump", bytes, sha256 },
  });
  writeFileSync(path.join(staging, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  copyFileSync(path.join(config.root, "deploy", "seed.Dockerfile"), path.join(staging, "Dockerfile"));
  const target = path.join(root, id);
  renameSync(staging, target);
  const rows = Object.values(tables).reduce((sum, n) => sum + Number(n), 0);
  console.log(`[corpus] ${target}: ${(bytes / 1e6).toFixed(1)} MB, ${rows} rows in ${Object.keys(tables).length} tables, ` +
    `${migrations.length} migrations, embeddings ${manifest.embedding.model}.`);
} catch (error) {
  console.error(`[corpus] ${error.message}`);
  process.exitCode = 1;
}
