// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import {
  HistoryProgress, QUICK_PATH, REPO_URL, graphCaughtUp, graphLabels, graphWaiting, historyArgs, historyLine, mailHeader,
  mailLines, mailProgress, monthRange, parseHistoryLine, parseLoadArgs, parseSlices, planSteps, summaryLines,
} from "../scripts/lib/load.mjs";

test("npm run load's flags", () => {
  assert.deepEqual(parseLoadArgs([]), { quick: false, mailOnly: false, codeOnly: false, force: false });
  assert.deepEqual(parseLoadArgs(["--quick", "--force"]), { quick: true, mailOnly: false, codeOnly: false, force: true });
  assert.equal(parseLoadArgs(["--mail-only"]).mailOnly, true);
  assert.equal(parseLoadArgs(["--code-only", "--quick"]).codeOnly, true);
  assert.throws(() => parseLoadArgs(["--mail-only", "--code-only"]), /exclude each other/);
  assert.throws(() => parseLoadArgs(["--quick", "--mail-only"]), /--quick narrows the code/);
  assert.throws(() => parseLoadArgs(["--everything"]), /Unknown option/);
  assert.throws(() => parseLoadArgs(["mail"]), /positional/);
});

test("the plan loads everything by default, and skips what the flags leave out or what's installed", () => {
  const all = planSteps(parseLoadArgs([]));
  assert.deepEqual(all.mail, { run: true });
  assert.deepEqual(all.code, { run: true, rebuild: false, paths: [] });
  assert.deepEqual(all.embeddings, { run: true });
  assert.deepEqual(planSteps(parseLoadArgs(["--quick"])).code.paths, [QUICK_PATH]);

  const mailOnly = planSteps(parseLoadArgs(["--mail-only"]));
  assert.deepEqual(mailOnly.code, { run: false, reason: "skipped (--mail-only)" });
  assert.equal(mailOnly.mail.run, true);
  const codeOnly = planSteps(parseLoadArgs(["--code-only"]));
  assert.equal(codeOnly.mail.run, false);
  assert.equal(codeOnly.embeddings.run, false);
  assert.equal(codeOnly.code.run, true);
  assert.equal(codeOnly.graph.run, true);

  const history = { head: "4b97fd1aa", commits: 62114, paths: [] };
  assert.deepEqual(planSteps(parseLoadArgs([]), { history }).code, { run: false, reason: "installed", history });
  assert.deepEqual(planSteps(parseLoadArgs(["--force"]), { history }).code, { run: true, rebuild: true, paths: [] });
  // A history without a head isn't installed.
  assert.equal(planSteps(parseLoadArgs([]), { history: null }).code.run, true);
});

test("the history build fetches into the clone, builds and installs, the whole tree or one folder", () => {
  assert.deepEqual(historyArgs("/r/.runtime/git/postgres-history.git"), [
    "history", "build", "--git-dir", "/r/.runtime/git/postgres-history.git", "--url", REPO_URL, "--fetch", "--install",
  ]);
  assert.deepEqual(historyArgs("g", { quick: true }).slice(-2), ["--path", "src/backend/parser"]);
});

test("the test slices are read from config/test-slices.toml", () => {
  const slices = parseSlices(readFileSync(path.join(APP_ROOT, "config", "test-slices.toml"), "utf8"));
  assert.equal(slices.list, "pgsql-hackers");
  assert.deepEqual(slices.months, ["2026-06", "2026-07"]);
  assert.equal(slices.seeds.length, 14);
  assert.equal(slices.seeds[0].topic, "SQL Property Graph Queries (SQL/PGQ)");
  assert.equal(slices.seeds[0].message_id, "CAExHW5suNPNFcphpabGmJnmF6EmQ2=oJ+KLWAE+o6WWScaSuDQ@mail.gmail.com");
  assert.equal(mailHeader(slices), "Mail: pgsql-hackers, June–July 2026 and 14 seed threads");

  const other = parseSlices('# x\n[mail]\nlist = "pgsql-bugs"\nmonths = ["2025-12", "2026-01"]  # two\n[other]\nlist = "no"\n');
  assert.deepEqual(other, { list: "pgsql-bugs", months: ["2025-12", "2026-01"], seeds: [] });
  assert.equal(mailHeader(other), "Mail: pgsql-bugs, December 2025–January 2026");
});

test("months read as people say them", () => {
  assert.equal(monthRange(["2026-07", "2026-06"]), "June–July 2026");
  assert.equal(monthRange(["2026-06"]), "June 2026");
  assert.equal(monthRange([]), "");
});

const slices = { list: "pgsql-hackers", months: ["2026-06", "2026-07"], seeds: [{ message_id: "a" }, { message_id: "b" }] };

test("the mail's progress comes from ingest_status's months and seed threads", () => {
  const status = {
    mail: {
      months: [
        { list: "pgsql-hackers", month: "2026-07", state: "running", messages: 2104 },
        { list: "pgsql-hackers", month: "2026-06", state: "done", messages: 3000 },
        { list: "pgsql-bugs", month: "2026-07", state: "queued", messages: 0 },
      ],
      seed_threads: [
        { topic: "SQL/JSON revisited", message_id: "a", state: "done" },
        { topic: "remaining sql/json patches", message_id: "b", state: "queued" },
      ],
      messages: 5104, threads: 700, authors: 300,
    },
  };
  const mail = mailProgress(status, slices);
  assert.equal(mail.finished, false);
  assert.deepEqual([mail.months.done, mail.months.total, mail.months.current.month], [1, 2, "2026-07"]);
  assert.deepEqual([mail.seeds.done, mail.seeds.total, mail.seeds.current.topic], [1, 2, "SQL/JSON revisited"]);
  assert.deepEqual(mailLines(mail, ""), [
    `months   ${"█".repeat(8)}${"░".repeat(7)} 1/2  {spin} 2026-07: 2,104 messages`,
    `threads  ${"█".repeat(8)}${"░".repeat(7)} 1/2  "SQL/JSON revisited"`,
    "so far   5,104 messages in 700 threads from 300 authors",
  ]);
  assert.equal(mailLines(mail, "", { live: false }).length, 2);

  // Running: the spinner marks it; a month downloads before it has messages. Nothing yet: queued.
  const running = mailProgress({ mail: {
    months: [{ list: "pgsql-hackers", month: "2026-06", state: "running", messages: 0 }],
    seed_threads: [], messages: 0, threads: 0, authors: 0,
  } }, slices);
  assert.match(mailLines(running, "")[0], /\{spin\} 2026-06: downloading$/);
  const queued = mailProgress({ mail: { months: [], seed_threads: [] } }, slices);
  assert.match(mailLines(queued, "")[0], /0\/2  queued$/);

  status.mail.months[0].state = "done";
  status.mail.seed_threads[1] = { ...status.mail.seed_threads[1], state: "failed", error: "not found" };
  const finished = mailProgress(status, slices);
  assert.equal(finished.finished, true);
  assert.deepEqual(finished.failures, [{ key: "seed b", what: '"remaining sql/json patches"', error: "not found" }]);

  // Right after add_source, before the jobs show: not finished.
  assert.equal(mailProgress({ mail: { months: [], seed_threads: [] } }, slices).finished, false);
});

test("the history build's log lines, with or without colors, drive its progress", () => {
  const lines = [
    "2026-10-21T16:21:03.574Z  INFO fetched the history's clone url=https://git.postgresql.org/git/postgresql.git refs=1523 secs=65",
    "2026-10-21T16:21:04.000Z  INFO read the commit graph commits=62114 lanes=40 refs=1523",
    "\x1b[2m2026-10-21T16:21:10.000Z\x1b[0m \x1b[32m INFO\x1b[0m walking the history \x1b[3mcommits\x1b[0m\x1b[2m=\x1b[0m35000 \x1b[3mof\x1b[0m\x1b[2m=\x1b[0m62114 \x1b[3mcommits_per_sec\x1b[0m\x1b[2m=\x1b[0m1850 \x1b[3msecs\x1b[0m\x1b[2m=\x1b[0m18",
    "2026-10-21T16:21:40.000Z  INFO walked the history instances=1 versions=2 lineages=3 secs=34",
    '2026-10-21T16:22:00.000Z  INFO built step="line instances" secs=20',
    '2026-10-21T16:22:10.000Z  INFO built step="file versions" secs=10',
    "2026-10-21T16:22:11.000Z  INFO built constraint=code_commits_pkey secs=1",
    "2026-10-21T16:22:15.000Z  INFO built index=CREATE INDEX code_file_versions_path_idx secs=4",
    "2026-10-21T16:22:30.000Z  INFO installed the history in schema pghacker",
  ];
  const events = lines.map(parseHistoryLine);
  assert.deepEqual(events.map((event) => event.type), ["fetched", "graph", "walking", "walked", "built", "built", "built", "built", "installed"]);
  assert.deepEqual(events[0], { type: "fetched", refs: 1523, secs: 65 });
  assert.equal(events[1].commits, 62114);
  assert.deepEqual(events[2], { type: "walking", commits: 35000, of: 62114, rate: 1850, secs: 18 });
  assert.deepEqual(events[4], { type: "built", what: "step", name: "line instances", secs: 20 });
  assert.deepEqual(events[6], { type: "built", what: "constraint", name: "code_commits_pkey", secs: 1 });
  assert.deepEqual(events[7], { type: "built", what: "index", name: "code_file_versions_path_idx", secs: 4 });
  // Fields before the message, too.
  assert.deepEqual(parseHistoryLine("INFO commits=5000 of=10000 commits_per_sec=2500 secs=2 walking the history"),
    { type: "walking", commits: 5000, of: 10000, rate: 2500, secs: 2 });
  assert.equal(parseHistoryLine("2026  WARN a merge commit: following its first parent id=abc").type, "warning");
  assert.equal(parseHistoryLine("Error: cannot connect"), null);
  assert.equal(parseHistoryLine(""), null);
  assert.equal(parseHistoryLine("{"), null);

  let now = 0;
  const history = new HistoryProgress({ now: () => now });
  history.cloneBytes = 412_000_000;
  now = 65_000;
  assert.deepEqual(history.lines(""), ["cloning  {spin} 412 MB received (1 m 05 s)"]);
  history.apply(events[0]);
  history.apply(events[1]);
  history.apply(events[2]);
  now = 83_000;
  assert.deepEqual(history.lines(""), [
    "cloning  ✔ 412 MB (1 m 05 s)",
    `walking  ${"█".repeat(8)}${"░".repeat(7)} 35,000/62,114 commits (1,850/s, ~15 s left)`,
  ]);
  history.apply(events[3]);
  assert.equal(history.lines("")[2], "building {spin} step 1/3: writing the commits and lines");
  history.apply(events[4]);
  assert.equal(history.lines("")[2], "building {spin} step 2/3: file versions");
  history.apply(events[5]);
  history.apply(events[6]);
  history.apply(events[7]);
  assert.equal(history.lines("")[2], "building {spin} step 3/3: constraints and indexes (2 built: code_file_versions_path_idx)");
  history.apply(events[8]);
  assert.equal(history.phase, "installed");
  assert.match(history.lines("")[1], /^walking {2}✔ 62,114 commits/);
  assert.match(history.lines("")[2], /^building ✔ installed in schema pghacker/);
  assert.equal(new HistoryProgress({ fetching: true }).lines("")[0].startsWith("fetching {spin}"), true);
});

test("the code's line says what's installed", () => {
  assert.equal(
    historyLine({ commits: 62114, branches: 40, tags: 420, master: "master", head: "4b97fd1c0ffee", paths: [] }),
    "62,114 commits on 40 branches and 420 tags, at master 4b97fd1",
  );
  assert.match(historyLine({ commits: 10, head: "abc", paths: ["src/backend/parser"] }), /\(only src\/backend\/parser\)$/);
});

test("the graph has caught up when no change waits and no full sync is requested", () => {
  assert.equal(graphCaughtUp({ pending_changes: 0, full_sync: { requested: false } }), true);
  assert.equal(graphCaughtUp({ pending_changes: 3210, full_sync: { requested: false } }), false);
  assert.equal(graphCaughtUp({ pending_changes: 0, full_sync: { requested: true } }), false);
  assert.equal(graphWaiting({ pending_changes: 3210 }), "3,210 changes waiting");
  assert.equal(graphWaiting({ pending_changes: 0, full_sync: { requested: true, reason: "a commit history was installed" } }),
    "full sync (a commit history was installed)");
  assert.equal(
    graphLabels({ File: 8422, Commit: 62114, Thread: 879, Message: 6166, Author: 363, Branch: 40, Feature: 0 }),
    "Author 363, Message 6,166, Thread 879, Commit 62,114, File 8,422, Branch 40",
  );
});

const extraction = { threads_pending: 879, threads_extracted: 0, features: 0, decisions: 0 };

test("the summary ends with extraction, with the token step only when the app has no token", () => {
  const lines = summaryLines({ loaded: { mail: true, code: true }, ms: 432_000, toolCount: 17, extraction, hasToken: false });
  assert.deepEqual(lines, [
    "✔ Loaded in 7 m 12 s. All 17 tools work now: mail, code, SQL and the graph of authors, threads and commits.",
    "",
    "Next: build the full graph of features, decisions and authors' positions.",
    "  Extraction runs crawler agents on a language model (GitHub Copilot's, with a GitHub token):",
    "    1. Give the app a token, then restart it so the agents get it:",
    "         echo \"GITHUB_TOKEN=$(gh auth token)\" >> .env      (or any token with Copilot access)",
    "         npm run stop && npm start",
    "    2. Extract:",
    "         npm run crawl -- --pilot             10 threads in one session: check the results first",
    "         npm run crawl -- --concurrency 8     then the rest of the 879 threads",
    "         npm run crawl -- --dedup             then merge duplicate features and authors",
    "  Until then, search_extracts and author_perspective return nothing,",
    "  and the graph has no Feature, Decision or Position vertices.",
  ]);
  const withToken = summaryLines({ loaded: { mail: true, code: true }, ms: 1000, toolCount: 17, extraction, hasToken: true });
  assert.equal(withToken.some((line) => line.includes("GITHUB_TOKEN")), false);
  assert.ok(withToken.includes("    npm run crawl -- --pilot             10 threads in one session: check the results first"));
});

test("after some extraction, the summary counts it and suggests the rest; after all of it, says so", () => {
  const some = summaryLines({
    loaded: { mail: true, code: true }, ms: 1000, toolCount: 17, hasToken: true,
    extraction: { threads_pending: 759, threads_extracted: 120, features: 340, decisions: 210 },
  });
  assert.ok(some.includes("  Extraction has read 120 threads so far, with 340 features and 210 decisions."));
  assert.ok(some.some((line) => line.includes("the remaining 759 threads")));
  assert.equal(some.some((line) => line.includes("--pilot")), false);
  assert.equal(some.some((line) => line.startsWith("  Until then")), false);
  const all = summaryLines({
    loaded: { mail: true, code: true }, ms: 1000, toolCount: 17, hasToken: true,
    extraction: { threads_pending: 0, threads_extracted: 879, features: 1200, decisions: 800 },
  });
  assert.equal(all.at(-1), "✔ Extraction is done: 879 threads, 1,200 features and 800 decisions.");
});

test("with only one part loaded, the summary names the command for the other", () => {
  const mail = summaryLines({ loaded: { mail: true, code: false }, ms: 1000, extraction, hasToken: true });
  assert.match(mail[1], /npm run load -- --code-only/);
  const code = summaryLines({ loaded: { mail: false, code: true }, ms: 1000, extraction, hasToken: true });
  assert.match(code[1], /npm run load -- --mail-only/);
  assert.equal(code.some((line) => line.includes("crawl")), false, "extraction needs the mail");
  assert.match(summaryLines({ loaded: { mail: false, code: false }, ms: 0 })[0], /^✖ Nothing is loaded/);
  assert.match(summaryLines({ loaded: { mail: true, code: true }, ms: null, toolCount: 17 })[0], /^✔ Already loaded\. All 17 tools/);
});
