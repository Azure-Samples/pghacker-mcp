// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { FOUNDRY_PROXY, PORTS, databaseUrls, embedEnvironment, processEnvironments, readAzureConfig } from "./lib/azure.mjs";
import { PLACEHOLDER_KEY, managedIdentityTokens, startFoundryProxy } from "./lib/foundry.mjs";
import { psqlJson, run } from "./lib/psql.mjs";

// Checks the Azure deployment from inside its network: the probe job runs
// this before the app exists (deploy/README.md, "First deployment"). It
// bootstraps the database as the app would (TLS verified, roles, extensions,
// schema, graph, migrations), then reports in one JSON line: the server,
// its extensions and the roles; whether BM25, vector and graph queries run
// as pgh_reader; the managed identity's token and each Foundry deployment,
// through the same proxy as the app's; and the outside services the app
// reaches. Exits 1 when anything the app needs failed.
const started = Date.now();
const report = { ok: false };
const failures = [];

async function timed(fn) {
  const at = Date.now();
  try {
    return { ok: true, ...(await fn()), ms: Date.now() - at };
  } catch (error) {
    return { ok: false, error: String(error.message).slice(0, 500), ms: Date.now() - at };
  }
}

async function post(url, headers, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(90_000),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${text.slice(0, 300)}`);
  return { status: response.status, json: JSON.parse(text) };
}

async function get(url) {
  const response = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(30_000) });
  await response.arrayBuffer();
  return { status: response.status };
}

try {
  const config = readAzureConfig(process.env, { serve: false });
  const urls = databaseUrls(config);

  report.bootstrap = await timed(async () => {
    const result = await run("/usr/local/bin/pghacker-indexer", ["bootstrap"], {
      env: processEnvironments(config).bootstrap,
      timeoutMs: 15 * 60_000,
    });
    if (result.code !== 0) throw new Error(result.stderr.trim().split("\n").slice(-8).join(" | "));
    return {};
  });
  if (!report.bootstrap.ok) failures.push("bootstrap");

  report.database = await timed(async () => psqlJson(urls.admin, `SELECT json_build_object(
      'server_version', current_setting('server_version'),
      'tls', (SELECT json_build_object('ssl', ssl, 'version', version) FROM pg_stat_ssl WHERE pid = pg_backend_pid()),
      'administrator', (SELECT json_build_object('superuser', rolsuper, 'createrole', rolcreaterole, 'createdb', rolcreatedb)
                        FROM pg_roles WHERE rolname = current_user),
      'extensions', (SELECT json_object_agg(name, json_build_object('available', default_version, 'installed', installed_version))
                     FROM pg_available_extensions WHERE name IN ('age', 'vector', 'pg_trgm', 'pg_textsearch')),
      'shared_preload_libraries', current_setting('shared_preload_libraries'),
      'max_connections', current_setting('max_connections')::int,
      'roles', (SELECT json_agg(rolname ORDER BY rolname) FROM pg_roles WHERE rolname LIKE 'pgh\\_%'),
      'migrations', (SELECT count(*) FROM pghacker.schema_migrations),
      'graph_labels', (SELECT count(*) FROM ag_catalog.ag_label l JOIN ag_catalog.ag_graph g ON g.graphid = l.graph
                       WHERE g.name = 'pghacker_graph'))`));
  if (!report.database.ok) failures.push("database");

  // What pghacker-mcp's tools run, as its role.
  report.queries = await timed(async () => psqlJson(urls.reader, `SELECT json_build_object(
      'bm25', (SELECT count(*) FROM (SELECT 1 FROM pghacker.message_chunks
               ORDER BY content <@> to_bm25query('planner statistics', 'pghacker.message_chunks_bm25') LIMIT 3) x),
      'vector', ('[1,0,0]'::vector <=> '[0,1,0]'::vector),
      'trigram', similarity('JSON_TABLE', 'json table'),
      'graph', (SELECT count(*) FROM ag_catalog.cypher('pghacker_graph', $$ MATCH (n) RETURN n LIMIT 1 $$) AS (n ag_catalog.agtype)))`));
  if (!report.queries.ok) failures.push("queries");

  const foundry = config.foundry;
  const token = managedIdentityTokens({
    endpoint: process.env.IDENTITY_ENDPOINT,
    header: process.env.IDENTITY_HEADER,
    clientId: foundry.clientId,
  });
  report.identity = await timed(async () => {
    await token();
    return { client_id: foundry.clientId };
  });
  if (!report.identity.ok) failures.push("identity");
  const proxy = await startFoundryProxy({ upstream: foundry.endpoint, token, port: PORTS.foundry });
  const key = { "api-key": PLACEHOLDER_KEY };
  report.foundry = {};
  for (const deployment of [foundry.sol, foundry.luna]) {
    report.foundry[deployment] = await timed(async () => {
      const { json } = await post(`${FOUNDRY_PROXY}/openai/v1/responses`, key,
        { model: deployment, input: "Reply with OK.", max_output_tokens: 64 });
      return { model: json.model, status: json.status };
    });
  }
  const embed = embedEnvironment(foundry);
  report.foundry[foundry.embedding] = await timed(async () => {
    const { json } = await post(embed.HORIZON_EMBED_URL, key, { input: ["ping"], model: foundry.embedding });
    const dimensions = json.data?.[0]?.embedding?.length;
    if (String(dimensions) !== embed.HORIZON_EMBED_DIM) throw new Error(`${dimensions} dimensions, not ${embed.HORIZON_EMBED_DIM}`);
    return { dimensions };
  });
  for (const [name, result] of Object.entries(report.foundry)) if (!result.ok) failures.push(`foundry ${name}`);
  proxy.close();

  report.network = {
    archive: await timed(async () => {
      const { status } = await get("https://www.postgresql.org/list/pgsql-hackers/");
      if (status !== 200) throw new Error(`HTTP ${status}`);
      return { status };
    }),
    git: await timed(async () => {
      const result = await run("git", ["ls-remote", "https://git.postgresql.org/git/postgresql.git", "HEAD"], {
        env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
        timeoutMs: 60_000,
      });
      if (result.code !== 0) throw new Error(result.stderr.trim());
      return { head: result.stdout.split("\t")[0] };
    }),
    github: await timed(async () => get("https://github.com/login/oauth/authorize")),
    github_api: await timed(async () => get("https://api.github.com/zen")),
  };
  for (const [name, result] of Object.entries(report.network)) if (!result.ok) failures.push(`network ${name}`);
} catch (error) {
  failures.push(error.message);
}
report.ok = failures.length === 0;
report.failures = failures;
report.seconds = Math.round((Date.now() - started) / 1000);
console.log(JSON.stringify(report));
// The proxy's keep-alive connections would keep the job running.
process.exit(report.ok ? 0 : 1);
