// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { STARS_TTL_MS, createStarCounts } from "../scripts/lib/github-stars.mjs";

const REPO = "Azure-Samples/pghacker-mcp";

/** GitHub's API, answering from `answers` in turn, and a clock that only moves when told. */
function fakeGitHub(answers) {
  const calls = [];
  const logs = [];
  let time = 1_000_000;
  const fetchImpl = async (url, init) => {
    calls.push({ url, headers: init.headers });
    const answer = answers.shift();
    if (answer instanceof Error) throw answer;
    return new Response(JSON.stringify(answer.body ?? {}), { status: answer.status ?? 200 });
  };
  const stars = createStarCounts({ fetchImpl, now: () => time, log: (line) => logs.push(line) });
  return { stars, calls, logs, advance: (ms) => (time += ms) };
}

const publicRepo = (count) => ({ body: { visibility: "public", stargazers_count: count } });

test("a public repository's stars arrive after the first check, which no page waits for", async () => {
  const { stars, calls } = fakeGitHub([publicRepo(12)]);
  assert.equal(stars.get(REPO), undefined);
  await stars.settled();
  assert.equal(stars.get(REPO), 12);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, `https://api.github.com/repos/${REPO}`);
  assert.equal(calls[0].headers.accept, "application/vnd.github+json");
  assert.equal(calls[0].headers["user-agent"], "pghacker-mcp");
  assert.equal(calls[0].headers.authorization, undefined);
});

test("an internal repository, a 404 without a token, has no count, and that's no error", async () => {
  const { stars, logs } = fakeGitHub([{ status: 404, body: { message: "Not Found" } }]);
  stars.get(REPO);
  await stars.settled();
  assert.equal(stars.get(REPO), undefined);
  assert.deepEqual(logs, []);
});

test("only a public repository's count is shown", async () => {
  const { stars } = fakeGitHub([{ body: { visibility: "internal", stargazers_count: 3 } }]);
  stars.get(REPO);
  await stars.settled();
  assert.equal(stars.get(REPO), undefined);
});

test("a count is checked at most every 10 minutes, and stands while it's checked again", async () => {
  const { stars, calls, advance } = fakeGitHub([publicRepo(12), publicRepo(13)]);
  stars.get(REPO);
  await stars.settled();
  advance(STARS_TTL_MS - 1);
  assert.equal(stars.get(REPO), 12);
  assert.equal(calls.length, 1);
  advance(1);
  assert.equal(stars.get(REPO), 12);
  assert.equal(calls.length, 2);
  await stars.settled();
  assert.equal(stars.get(REPO), 13);
});

test("when GitHub fails or limits the rate, the last count stands, and it's tried again later", async () => {
  const { stars, calls, logs, advance } = fakeGitHub([
    publicRepo(12),
    new Error("getaddrinfo ENOTFOUND api.github.com"),
    { status: 403, body: { message: "API rate limit exceeded" } },
    publicRepo(14),
  ]);
  stars.get(REPO);
  await stars.settled();
  for (let check = 0; check < 2; check++) {
    advance(STARS_TTL_MS);
    stars.get(REPO);
    await stars.settled();
    assert.equal(stars.get(REPO), 12);
  }
  assert.equal(logs.length, 2);
  assert.match(logs[0], /ENOTFOUND/);
  assert.match(logs[1], /HTTP 403/);
  advance(STARS_TTL_MS);
  stars.get(REPO);
  await stars.settled();
  assert.equal(stars.get(REPO), 14);
  assert.equal(calls.length, 4);
});

test("a count that isn't a whole number is no count", async () => {
  const { stars } = fakeGitHub([{ body: { visibility: "public", stargazers_count: "12" } }]);
  stars.get(REPO);
  await stars.settled();
  assert.equal(stars.get(REPO), undefined);
});

test("a name that isn't owner/name is never sent to GitHub", async () => {
  const { stars, calls } = fakeGitHub([]);
  for (const name of ["", "pghacker-mcp", "a/b/c", "../x/y", "a/b?x=1", "a b/c"]) {
    assert.equal(stars.get(name), undefined, name);
  }
  await stars.settled();
  assert.equal(calls.length, 0);
});
