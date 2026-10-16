// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { checkCommits, citedCommits, lastJsonBlock, normalizeForQuote, parseCsv, quoteIn, resultsTable } from "../scripts/lib/evalcheck.mjs";

test("quotes match across wrapping and quote markers, like the indexer's check", () => {
  const body = "On Monday Tom wrote:\n> I think we should not\n> commit this patch yet.\n\nAgreed.";
  assert.equal(quoteIn(body, "I think we should not commit this patch yet."), true);
  assert.equal(quoteIn(body, "we should commit this patch"), false);
  assert.equal(quoteIn(body, "Agreed."), false);
  assert.equal(normalizeForQuote("it\u2019s  \u201cfine\u201d"), "it's \"fine\"");
});

test("CSV fields with quotes, commas and newlines parse", () => {
  assert.deepEqual(parseCsv('a,b\n"x, ""y""","line1\nline2"\n'), [["a", "b"], ["x, \"y\"", "line1\nline2"]]);
});

test("the last parseable JSON block is the answer", () => {
  const reply = "Text\n```json\n{\"answer\": \"first\"}\n```\nmore\n```json\n{\"answer\": \"second\", \"citations\": []}\n```";
  assert.equal(lastJsonBlock(reply).answer, "second");
  assert.equal(lastJsonBlock("no json"), null);
});

const A = "0fdadfb0192935e0f8bec7fa5ec9dbb470d6d587";
const B = "8f32bacc006692e63f137924d5849b3cb9f2d4d3";
const C = "2dedf4d9a899b36d1a8ed29be5efbd1b31a8fe85";

test("cited commits are full SHAs, lower-cased once each; anything else is malformed", () => {
  assert.deepEqual(citedCommits([A.toUpperCase(), ` ${A} `, B, "0fdadfb0192", "REL_11_STABLE", null]),
    { shas: [A, B], malformed: ["0fdadfb0192", "REL_11_STABLE", "null"] });
  assert.deepEqual(citedCommits(undefined), { shas: [], malformed: [] });
  assert.deepEqual(citedCommits("not a list"), { shas: [], malformed: [] });
});

test("cited commits must exist in the history and include a gold commit", () => {
  assert.deepEqual(checkCommits([A, C], [A, C], [A, B]),
    { commits: [A, C], malformed: [], unknown: [], valid: true, onGold: true });
  // A gold commit, but another doesn't exist: on gold, yet invalid.
  assert.deepEqual(checkCommits([B, C], [B], [A, B]),
    { commits: [B, C], malformed: [], unknown: [C], valid: false, onGold: true });
  // Real commits, none of them gold.
  assert.equal(checkCommits([C], [C], [A, B]).onGold, false);
  // An abbreviated SHA is malformed even when it names a gold commit.
  const short = checkCommits([A.slice(0, 12)], [], [A]);
  assert.equal(short.valid, false);
  assert.equal(short.onGold, false);
  // Case doesn't matter, in the answer, the history or the gold list.
  assert.equal(checkCommits([A.toUpperCase()], [A.toUpperCase()], [A.toUpperCase()]).onGold, true);
  assert.deepEqual(checkCommits([], [], []), { commits: [], malformed: [], unknown: [], valid: true, onGold: false });
});

test("the results table reports the commit checks, with a dash where they don't apply", () => {
  const [header, , ...rows] = resultsTable([
    { id: "q01", kind: "decision", question: "Why | how?", correct: true, citations_valid: true, on_gold_thread: true, commits_valid: null, on_gold_commit: null, seconds: 30 },
    { id: "q24", kind: "time-travel", question: "JIT?", correct: true, citations_valid: true, on_gold_thread: false, commits_valid: true, on_gold_commit: false, seconds: 41 },
    { id: "q25", kind: "time-travel", question: "SCRAM?", pass: false, error: "no JSON answer block" },
  ]);
  assert.match(header, /\| Commits valid \| Gold commit cited \| Seconds \|$/);
  assert.equal(rows[0], "| q01: Why \\| how? | decision | yes | yes | yes | — | — | 30 |");
  assert.equal(rows[1], "| q24: JIT? | time-travel | yes | yes | no | yes | no | 41 |");
  assert.equal(rows[2], "| q25: SCRAM? | time-travel | no | no | no | — | — |  |");
});
