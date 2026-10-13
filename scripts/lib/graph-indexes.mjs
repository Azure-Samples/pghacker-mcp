import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { quoteIdent } from "./corpus.mjs";
import { psqlJson, psqlSession } from "./psql.mjs";

export const CATALOG_SQL = readFileSync(new URL("../../config/graph-indexes.sql", import.meta.url), "utf8");
export const IDENTITY_SQL = `SELECT jsonb_build_object(
  'database', current_database(),
  'database_oid', (SELECT oid::bigint FROM pg_database WHERE datname = current_database()),
  'server_address', inet_server_addr()::text, 'server_port', inet_server_port(),
  'postgres', current_setting('server_version'),
  'age', (SELECT extversion FROM pg_extension WHERE extname = 'age'),
  'graph_oid', (SELECT graphid::bigint FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))`;

const quoteLiteral = (value) => `'${String(value).replaceAll("'", "''")}'`;
export const indexKey = (r) => `${r.schema}.${r.index_name}`;

function readOnlyUrl(url) {
  const parsed = new URL(url);
  parsed.searchParams.set("options", `${parsed.searchParams.get("options") || ""} -c default_transaction_read_only=on -c statement_timeout=5000 -c search_path=pg_catalog,ag_catalog`.trim());
  return parsed.toString();
}

export function edgeIndexName(label) {
  const name = `${label}_id_idx`;
  return Buffer.byteLength(name, "utf8") <= 63 ? name : `pgh_eid_${createHash("md5").update(label).digest("hex")}`;
}

function target(url) {
  const parsed = new URL(url);
  return { host: parsed.hostname, port: parsed.port || "5432", database: decodeURIComponent(parsed.pathname.slice(1)) };
}

function descriptor(r) {
  return {
    kind: r.kind, label: r.label, schema: r.schema, table: r.table,
    table_oid: r.table_oid, owner: r.owner, columns: r.columns,
    index_name: r.index_name, buildable: r.buildable,
  };
}

function fingerprint(scope) {
  return createHash("sha256").update(JSON.stringify(scope)).digest("hex");
}

export function validateCatalog(requirements) {
  if (!Array.isArray(requirements) || requirements.length === 0) throw new Error("empty graph index catalog");
  const keys = new Set();
  for (const r of requirements) {
    if (!r || !["edge_id", "edge_endpoint", "vertex_id", "mapping_endpoint"].includes(r.kind)
        || !["ready", "missing", "invalid", "name_conflict", "missing_relation", "unsupported"].includes(r.state)
        || typeof r.schema !== "string" || typeof r.table !== "string"
        || typeof r.index_name !== "string" || !Array.isArray(r.columns) || !r.columns.length
        || r.columns.some((c) => typeof c !== "string") || !Array.isArray(r.indexes)
        || !Array.isArray(r.usable_indexes) || typeof r.buildable !== "boolean"
        || (r.state === "ready" && (!r.usable_indexes.length || !Number.isSafeInteger(r.table_oid)
          || typeof r.owner !== "string"))) throw new Error("invalid graph index catalog response");
    if (keys.has(indexKey(r))) throw new Error(`duplicate graph index requirement ${indexKey(r)}`);
    keys.add(indexKey(r));
    if (r.kind === "edge_id" && r.index_name !== edgeIndexName(r.label)) throw new Error("edge index naming drift");
  }
  if (!requirements.some((r) => r.kind === "edge_id")
      || !requirements.some((r) => r.kind === "vertex_id")
      || requirements.filter((r) => r.kind === "mapping_endpoint").length !== 2) {
    throw new Error("incomplete AGE/mapping index catalog");
  }
  return requirements;
}

export function assertPreparable(requirements) {
  validateCatalog(requirements);
  const blocked = requirements.filter((r) => r.state !== "ready" && !(r.state === "missing" && r.buildable));
  if (blocked.length) {
    throw new Error(`graph index prerequisites require operator review: ${blocked.map((r) => `${indexKey(r)}=${r.state}`).join(", ")}`);
  }
}

export function assertReady(requirements) {
  validateCatalog(requirements);
  const missing = requirements.filter((r) => r.state !== "ready");
  if (missing.length) throw new Error(`graph indexes are not ready: ${missing.map((r) => `${indexKey(r)}=${r.state}`).join(", ")}`);
}

export function createManifest(url, identity, requirements) {
  validateCatalog(requirements);
  const scope = { target: target(url), identity, requirements: requirements.map(descriptor) };
  return {
    version: 1, scope, fingerprint: fingerprint(scope),
    operations: requirements.filter((r) => r.state === "missing" && r.buildable).map(indexKey),
    blocked: requirements.filter((r) => r.state !== "ready" && !(r.state === "missing" && r.buildable))
      .map((r) => ({ index: indexKey(r), state: r.state })),
    observed: requirements,
  };
}

export function validateManifest(manifest, url, identity, requirements) {
  assertPreparable(requirements);
  if (!manifest || manifest.version !== 1 || !Array.isArray(manifest.operations)
      || manifest.fingerprint !== fingerprint(manifest.scope)) throw new Error("invalid graph index scope manifest");
  const current = { target: target(url), identity, requirements: requirements.map(descriptor) };
  if (manifest.fingerprint !== fingerprint(current)) throw new Error("graph index target/scope changed; review a new manifest");
  const approved = new Set(manifest.operations);
  if (approved.size !== manifest.operations.length
      || manifest.operations.some((key) => !requirements.some((r) => indexKey(r) === key && r.buildable))) {
    throw new Error("manifest contains an unsupported index operation");
  }
  for (const r of requirements) {
    if (r.state === "missing" && !approved.has(indexKey(r))) throw new Error(`unapproved index build ${indexKey(r)}`);
  }
}

export function createIndexSql(r) {
  if (!r.buildable || !["edge_id", "mapping_endpoint"].includes(r.kind) || r.state !== "missing") {
    throw new Error(`cannot build ${indexKey(r)} in state ${r.state}`);
  }
  const only = r.kind === "edge_id" ? "ONLY " : "";
  return `CREATE INDEX CONCURRENTLY ${quoteIdent(r.index_name)} ON ${only}${quoteIdent(r.schema)}.${quoteIdent(r.table)} USING btree (${r.columns.map(quoteIdent).join(", ")})`;
}

export async function inventory(url) {
  const identity = await psqlJson(readOnlyUrl(url), IDENTITY_SQL);
  const requirements = validateCatalog(await psqlJson(readOnlyUrl(url), CATALOG_SQL));
  return { identity, requirements };
}

export async function prepareIndexes(url, manifest, {
  buildTimeoutMs, only = null, progress = () => {}, signal,
} = {}) {
  if (!Number.isSafeInteger(buildTimeoutMs) || buildTimeoutMs < 1 || buildTimeoutMs > 2147483647) {
    throw new Error("--build-timeout-ms must specify an approved positive PostgreSQL statement budget");
  }
  const initial = await inventory(url);
  validateManifest(manifest, url, initial.identity, initial.requirements);
  if (only && !manifest.operations.includes(only)) throw new Error(`index ${only} is not in the approved build list`);
  const session = psqlSession(url);
  const completed = [];
  let pid = null;
  let primaryError = null;
  const cancel = () => session.cancel();
  signal?.addEventListener("abort", cancel);
  try {
    signal?.throwIfAborted();
    await session.query("SET application_name = 'pghacker-graph-indexes'");
    await session.query("SET search_path = pg_catalog, ag_catalog");
    const locked = await session.json(`SELECT jsonb_build_object(
      'locked', pg_try_advisory_lock(hashtextextended('pghacker.graph_indexes', 0)),
      'pid', pg_backend_pid(), 'user', current_user)`);
    pid = locked.pid;
    if (!locked.locked) throw new Error("another graph index preparation owns the advisory lock");
    const owners = [...new Set(initial.requirements.filter((r) => r.buildable).map((r) => r.owner))];
    const permissions = await session.json(`SELECT to_jsonb(bool_and(pg_has_role(session_user, owner, 'SET')))
      FROM (VALUES ${owners.map((owner) => `(${quoteLiteral(owner)})`).join(", ")}) owners(owner)`);
    if (!permissions) throw new Error("existing SET membership for every required graph/mapping owner is required");
    await session.query("SET lock_timeout = '2s'");
    await session.query(`SET statement_timeout = ${quoteLiteral(`${buildTimeoutMs}ms`)}`);
    await session.query("SET max_parallel_maintenance_workers = 0");

    const candidates = initial.requirements.filter((r) => manifest.operations.includes(indexKey(r))
      && (!only || indexKey(r) === only));
    candidates.sort((a, b) => {
      const rows = (r) => r.estimated_rows >= 0 ? r.estimated_rows : Number.MAX_SAFE_INTEGER;
      return Number(a.kind === "mapping_endpoint") - Number(b.kind === "mapping_endpoint")
        || rows(a) - rows(b) || indexKey(a).localeCompare(indexKey(b));
    });
    for (const candidate of candidates) {
      signal?.throwIfAborted();
      const current = await inventory(url);
      validateManifest(manifest, url, current.identity, current.requirements);
      const r = current.requirements.find((item) => indexKey(item) === indexKey(candidate));
      const permission = await session.json(`SELECT jsonb_build_object(
        'allowed', pg_has_role(session_user, ${quoteLiteral(r.owner)}, 'SET'))`);
      if (!permission.allowed) throw new Error(`existing SET membership for owner ${r.owner} is required`);
      await session.query(`SET ROLE ${quoteIdent(r.owner)}`);
      if (r.kind === "edge_id") {
        const duplicates = await session.json(`SELECT coalesce(jsonb_agg(d), '[]'::jsonb)
          FROM (SELECT id::text AS id, count(*) AS rows
            FROM ONLY ${quoteIdent(r.schema)}.${quoteIdent(r.table)}
            GROUP BY id HAVING count(*) > 1 LIMIT 1) d`);
        if (duplicates.length) throw new Error(`duplicate physical graph IDs in ${r.schema}.${r.table}; no automatic repair`);
      }
      if (r.state === "ready") {
        progress({ event: "reused", index: indexKey(r), usable_indexes: r.usable_indexes });
        await session.query("RESET ROLE");
        continue;
      }
      for (let attempt = 1; attempt <= 3; attempt++) {
        const busy = await session.json(`SELECT to_jsonb(EXISTS (
          SELECT FROM pg_locks WHERE relation = ${r.table_oid} AND granted AND pid <> ${pid}
          AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
          AND mode IN ('ShareUpdateExclusiveLock', 'ShareLock', 'ShareRowExclusiveLock',
                       'ExclusiveLock', 'AccessExclusiveLock')))`);
        if (!busy) break;
        progress({ event: "lock_wait", index: indexKey(r), attempt });
        if (attempt === 3) throw new Error(`pre-build lock contention for ${indexKey(r)}; no build started`);
        await sleep(200 * attempt, undefined, { signal });
      }
      const started = performance.now();
      progress({ event: "building", index: indexKey(r), owner: r.owner, pid });
      let observation = null;
      const timer = setInterval(() => {
        if (observation) return;
        observation = psqlJson(readOnlyUrl(url), `SELECT coalesce(jsonb_agg(jsonb_build_object(
            'phase', phase, 'blocks_done', blocks_done, 'blocks_total', blocks_total,
            'tuples_done', tuples_done, 'tuples_total', tuples_total,
            'lockers_done', lockers_done, 'lockers_total', lockers_total,
            'current_locker_pid', current_locker_pid)), '[]'::jsonb)
            FROM pg_stat_progress_create_index WHERE pid = ${pid}
              AND datid = (SELECT oid FROM pg_database WHERE datname = current_database())`)
          .then((phases) => progress({ event: "progress", index: indexKey(r), phases }))
          .catch((error) => progress({ event: "observation_error", index: indexKey(r), error: error.message }))
          .finally(() => { observation = null; });
      }, 5000);
      try {
        signal?.throwIfAborted();
        await session.query(createIndexSql(r), { timeoutMs: buildTimeoutMs + 5000 });
        const after = await inventory(url);
        validateManifest(manifest, url, after.identity, after.requirements);
        const built = after.requirements.find((item) => indexKey(item) === indexKey(r));
        if (built.state !== "ready") throw new Error(`build finished but ${indexKey(r)} is ${built.state}`);
        completed.push(indexKey(r));
        progress({ event: "completed", index: indexKey(r), elapsed_ms: performance.now() - started });
      } catch (error) {
        error.index = indexKey(r);
        let state;
        try {
          const after = await inventory(url);
          validateManifest(manifest, url, after.identity, after.requirements);
          state = after.requirements.find((item) => indexKey(item) === indexKey(r));
          error.index_state = state.state;
          error.indexes = state.indexes;
        } catch (inspectionError) {
          error.observation_error = inspectionError.message;
          throw error;
        }
        if (error.sqlstate !== "42P07" || state.state !== "ready") throw error;
        completed.push(indexKey(r));
        progress({ event: "reused", index: indexKey(r), usable_indexes: state.usable_indexes });
        // A name-race error closes psql; the remaining work needs a fresh run.
        throw new Error("a concurrent creator completed this index; resume the remaining reviewed work", { cause: error });
      } finally {
        clearInterval(timer);
        if (observation) await observation;
      }
      await session.query("RESET ROLE");
    }
    const final = await inventory(url);
    const remaining = final.requirements.filter((r) => r.state !== "ready").map((r) => ({ index: indexKey(r), state: r.state }));
    if (!only) assertReady(final.requirements);
    return { completed, remaining, ready: remaining.length === 0 };
  } catch (error) {
    session.cancel();
    primaryError = error;
    error.completed = completed;
    if (pid) {
      try {
        error.build_active = await psqlJson(readOnlyUrl(url), `SELECT to_jsonb(EXISTS (
          SELECT FROM pg_stat_progress_create_index WHERE pid = ${pid}
            AND datid = (SELECT oid FROM pg_database WHERE datname = current_database())))`);
      } catch (observationError) {
        error.observation_error = observationError.message;
      }
    }
    throw error;
  } finally {
    signal?.removeEventListener("abort", cancel);
    try {
      await session.close();
    } catch (closeError) {
      if (!primaryError) throw closeError;
      primaryError.close_error = closeError.message;
    }
  }
}
