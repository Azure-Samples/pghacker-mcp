// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { psqlJson, psqlSession, run, verifiedUrl } from "../scripts/lib/psql.mjs";

test("libpq require remains verified and existing run reports failures", async () => {
  const result = new URL(verifiedUrl("postgresql://localhost/fixture?sslmode=require"));
  assert.equal(result.searchParams.get("sslmode"), "verify-full");
  assert.equal(result.searchParams.get("sslrootcert"), "system");
  assert.equal(new URL(verifiedUrl("postgresql://localhost/fixture?sslmode=disable")).searchParams.get("sslmode"), "disable");
  const options = new URL("postgresql://owner:p%2Bass@localhost/fixture?sslmode=disable");
  options.searchParams.set("options", "-c statement_timeout=5000");
  const native = verifiedUrl(options.toString());
  assert.ok(native.includes("-c%20statement_timeout%3D5000"));
  assert.equal(new URL(native).password, "p%2Bass");
  const failed = await run(process.execPath, ["-e", "console.error('fixture error');process.exit(7)"]);
  assert.equal(failed.code, 7);
  assert.match(failed.stderr, /fixture error/);
});

function fixtureUrl() {
  const url = process.env.PGHACKER_INDEX_TEST_URL;
  const parsed = new URL(url);
  assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(parsed.hostname));
  assert.match(parsed.pathname, /^\/pgh_test_[a-f0-9]+$/);
  return url;
}
const isolated = { skip: !process.env.PGHACKER_INDEX_TEST_URL };

test("persistent psql uses separate autocommit statements and concurrent DDL", isolated, async () => {
  const session = psqlSession(fixtureUrl());
  try {
    assert.deepEqual(await session.json("SELECT json_build_object('n', 1)"), { n: 1 });
    assert.deepEqual(await session.json("SELECT json_build_object('n', 2); -- trailing comment"), { n: 2 });
    await session.query("CREATE TABLE public.psql_session_probe (id integer)");
    await session.query("CREATE INDEX CONCURRENTLY psql_session_probe_id ON public.psql_session_probe (id)");
    await session.query("DROP TABLE public.psql_session_probe");
  } finally {
    await session.close();
  }
});

test("client cancellation waits for server termination and surfaces SQLSTATE", isolated, async () => {
  const url = fixtureUrl();
  const session = psqlSession(url);
  const pid = await session.json("SELECT pg_backend_pid()");
  await assert.rejects(session.query("SELECT pg_sleep(30)", { timeoutMs: 50 }), /57014/);
  await assert.rejects(session.close(), /57014/);
  let active = true;
  for (let attempt = 0; attempt < 10 && active; attempt++) {
    active = await psqlJson(url, `SELECT to_jsonb(EXISTS (
      SELECT FROM pg_stat_activity WHERE pid = ${pid} AND datname = current_database() AND state = 'active'))`);
    if (active) await sleep(20);
  }
  assert.equal(active, false);
});
