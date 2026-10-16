// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The corpus an Azure deployment starts with: what a dump of the local
// corpus records (manifest.json) and the checks that a restore applies.
// Only schema pghacker's rows move; the graph is rebuilt from them, so no
// AGE internals do.

/**
 * Tables whose rows aren't moved: the graph's bookkeeping and sync state,
 * rebuilt with the graph, and the migrations, which the target applies itself.
 */
export const EXCLUDED_TABLES = [
  "graph_nodes", "graph_edges", "graph_sync_runs", "graph_changes", "graph_sync_state", "graph_label_counts",
  "graph_committers", "schema_migrations",
];

/** Tables whose rows mean a corpus is already there. */
export const CORPUS_TABLES = ["messages", "threads", "features", "decisions", "positions", "code_commits"];

/** The repositories whose code may be in a corpus: PostgreSQL's. */
export const CODE_REPOS = ["postgres"];

export const MANIFEST_VERSION = 1;

/** The dump's ID: when, and the start of its checksum. */
export function corpusId(date, sha256) {
  const stamp = date.toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 13);
  return `${stamp}-${sha256.slice(0, 8)}`;
}

/** Statements that count every moved table's rows, as one JSON object. */
export function countsSql(schema = "pghacker") {
  const excluded = EXCLUDED_TABLES.map((name) => `'${name}'`).join(", ");
  return `SELECT json_object_agg(c.relname,
      (xpath('/row/n/text()', query_to_xml(format('SELECT count(*) AS n FROM %I.%I', n.nspname, c.relname), false, true, '')))[1]::text::bigint
      ORDER BY c.relname)
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = '${schema}' AND c.relkind IN ('r', 'p') AND c.relname NOT IN (${excluded})`;
}

export const MIGRATIONS_SQL = `SELECT coalesce(json_agg(json_build_object('version', version, 'sha256', sha256) ORDER BY version), '[]')
  FROM pghacker.schema_migrations`;

export const EXTENSIONS_SQL = `SELECT json_object_agg(extname, extversion ORDER BY extname) FROM pg_extension
  WHERE extname IN ('age', 'vector', 'pg_trgm', 'pg_textsearch')`;

// The commit history is PostgreSQL's.
export const REPOS_SQL = `SELECT CASE WHEN EXISTS (SELECT FROM pghacker.code_history) THEN '["postgres"]'::json ELSE '[]'::json END`;

/** Schema pghacker's foreign keys, read with search_path empty so that their references are qualified. */
export const FOREIGN_KEYS_SQL = `SET search_path = pg_catalog; SELECT coalesce(json_agg(json_build_object(
      'table', format('%I.%I', n.nspname, t.relname), 'name', c.conname, 'definition', pg_get_constraintdef(c.oid))
      ORDER BY t.relname, c.conname), '[]')
    FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
    WHERE c.contype = 'f' AND n.nspname = 'pghacker'`;

export function quoteIdent(name) {
  return `"${String(name).replace(/"/g, '""')}"`;
}

/**
 * Scripts that drop schema pghacker's foreign keys before a restore and add
 * them back after: a parallel data-only restore loads tables in any order.
 * The definitions' references are qualified (FOREIGN_KEYS_SQL), and the
 * scripts run with the same search_path.
 */
export function foreignKeyScripts(keys) {
  const drop = keys.map((key) => `ALTER TABLE ${key.table} DROP CONSTRAINT ${quoteIdent(key.name)};`);
  const add = keys.map((key) => `ALTER TABLE ${key.table} ADD CONSTRAINT ${quoteIdent(key.name)} ${key.definition};`);
  const wrap = (statements) => `BEGIN; SET LOCAL search_path = pg_catalog; ${statements.join(" ")} COMMIT;`;
  return { drop: wrap(drop), add: wrap(add) };
}

/** The manifest of a dump. */
export function buildManifest({ id, createdAt, commit, database, migrations, tables, extensions, embedding, repos, dump }) {
  const manifest = {
    version: MANIFEST_VERSION,
    id,
    created_at: createdAt,
    source: { database, commit },
    migrations,
    tables,
    extensions,
    embedding,
    code_repos: repos,
    excluded_tables: EXCLUDED_TABLES,
    dump,
  };
  checkManifest(manifest);
  return manifest;
}

/** Problems that make a manifest unusable; throws on the first. */
export function checkManifest(manifest) {
  if (manifest?.version !== MANIFEST_VERSION) throw new Error(`The manifest isn't version ${MANIFEST_VERSION}.`);
  for (const field of ["id", "migrations", "tables", "embedding", "dump"]) {
    if (!manifest[field]) throw new Error(`The manifest has no ${field}.`);
  }
  if (!Array.isArray(manifest.migrations) || manifest.migrations.length === 0) {
    throw new Error("The manifest lists no migrations.");
  }
  const foreign = (manifest.code_repos ?? []).filter((repo) => !CODE_REPOS.includes(repo));
  if (foreign.length) {
    throw new Error(`The corpus holds code of ${foreign.join(", ")}, which isn't PostgreSQL's: dump only the pghacker corpus.`);
  }
  if (!/^[0-9a-f]{64}$/.test(manifest.dump.sha256 ?? "")) throw new Error("The manifest has no dump checksum.");
  return manifest;
}

/** How the target's migrations differ from the manifest's: empty when equal. */
export function migrationDifferences(expected, actual) {
  const want = new Map(expected.map((m) => [m.version, m.sha256]));
  const have = new Map(actual.map((m) => [m.version, m.sha256]));
  const differences = [];
  for (const [version, sha] of want) {
    if (!have.has(version)) differences.push(`${version} isn't applied to the target`);
    else if (have.get(version) !== sha) differences.push(`${version} differs`);
  }
  for (const version of have.keys()) {
    if (!want.has(version)) differences.push(`${version} is applied to the target but not to the corpus`);
  }
  return differences;
}

/** Tables whose row counts differ from the manifest's: empty when equal. */
export function countDifferences(expected, actual) {
  return Object.entries(expected)
    .filter(([table, rows]) => Number(actual?.[table] ?? -1) !== Number(rows))
    .map(([table, rows]) => `${table}: ${actual?.[table] ?? "missing"} rows, expected ${rows}`);
}
