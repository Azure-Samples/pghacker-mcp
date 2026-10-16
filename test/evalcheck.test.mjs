// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import {
  checkCommits, checkReview, citedCommits, lastJsonBlock, MAX_DIFF_CHARS, normalizeForQuote, parseCsv, quoteIn, resultsTable,
  reviewPrompt, reviewTable, verifySkill,
} from "../scripts/lib/evalcheck.mjs";

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

test("a review prompt shows the patch, the reviewer and the date, and the skill only when given", () => {
  const plain = reviewPrompt({ reviewer: "Tom Lane", asOf: "2026-10-05", subject: "Show xmin", diff: "+int x;" });
  assert.match(plain, /as Tom Lane would have when it was posted, on 2026-10-05\./);
  assert.match(plain, /only evidence from before 2026-10-05/);
  assert.match(plain, /````diff\n\+int x;\n````/);
  assert.doesNotMatch(plain, /<skill>/);
  const withSkill = reviewPrompt({ reviewer: "Tom Lane", asOf: "2026-10-05", subject: "Show xmin", diff: "+int x;", skill: "---\nname: s\n---\nSteps.\n" });
  assert.match(withSkill, /<skill>\n---\nname: s\n---\nSteps\.\n<\/skill>/);
  assert.equal(withSkill.replace(/\n\nFollow this skill[\s\S]*<\/skill>/, ""), plain);
  const long = reviewPrompt({ reviewer: "A", asOf: "2026-01-01", subject: "s", diff: "x".repeat(MAX_DIFF_CHARS + 10) });
  assert.match(long, /the rest of the patch is cut/);
});

test("a review's citations count the reviewer's own words and catch what came after", () => {
  const found = new Map([
    ["own@x", { author: "Tom Lane", sent_at: "2026-09-01T10:00:00+00:00", body_clean: "Adding a field there breaks the ABI." }],
    ["quoted@x", { author: "Tom Lane", sent_at: "2026-09-02T10:00:00+00:00", body_clean: "I agree." }],
    ["other@x", { author: "Andres Freund", sent_at: "2026-09-03T10:00:00+00:00", body_clean: "Adding a field there breaks the ABI." }],
    ["late@x", { author: "Tom Lane", sent_at: "2026-10-05T00:00:00+00:00", body_clean: "This patch is wrong." }],
    ["gold@x", { author: "Tom Lane", sent_at: "2026-09-04T00:00:00+00:00", body_clean: "Not like this." }],
  ]);
  const citations = [
    { message_id: "own@x", quote: "breaks the ABI." },
    { message_id: "own@x", quote: "Adding a field there" },
    { message_id: "quoted@x", quote: "something someone else wrote" },
    { message_id: "other@x", quote: "breaks the ABI." },
    { message_id: "late@x", quote: "This patch is wrong." },
    { message_id: "gold@x", quote: "Not like this." },
    { message_id: "missing@x", quote: "anything at all" },
  ];
  const checked = checkReview(citations, found, { reviewer: " tom lane ", asOf: "2026-10-05", goldIds: ["gold@x"] });
  // own@x once, late@x and gold@x are the reviewer's own words too, but leak.
  assert.equal(checked.own, 3);
  assert.deepEqual(checked.leaked.sort(), ["gold@x", "late@x"]);
  assert.deepEqual(checkReview([], found, { reviewer: "Tom Lane", asOf: "2026-10-05" }), { own: 0, leaked: [] });
});

test("the review table shows the reviewer, the score and the review checks", () => {
  const [header, , ...rows] = reviewTable([
    { id: "r01", subject: "Show xmin | slots", reviewer: "Andres Freund", correct: true, score: 0.5, citations_valid: true, own_citations: 3, leaked: [], seconds: 120 },
    { id: "r02", subject: "x", reviewer: "Tom Lane", pass: false, error: "no JSON answer block" },
  ]);
  assert.match(header, /\| Reviewer's own \| No leak \| Seconds \|$/);
  assert.equal(rows[0], "| r01: Show xmin \\| slots | Andres Freund | yes | 0.50 | yes | 3 | yes | 120 |");
  assert.equal(rows[1], "| r02: x | Tom Lane | no |  | no | 0 | no |  |");
});

test("a skill read from a server must match its entry's digests and sizes", () => {
  const text = "---\nname: s\ndescription: d\n---\nÉtape.\n";
  const digest = `sha256:${createHash("sha256").update(Buffer.from(text, "utf8")).digest("hex")}`;
  const size = Buffer.byteLength(text, "utf8");
  const entry = { uri: "skill://s/SKILL.md", resources: [{ uri: "skill://s/SKILL.md", digest, size }] };
  assert.deepEqual(verifySkill(entry, new Map([["skill://s/SKILL.md", text]])), []);
  assert.match(verifySkill(entry, new Map([["skill://s/SKILL.md", `${text} `]])).join("\n"), /bytes, not/);
  assert.deepEqual(verifySkill(entry, new Map()), ["skill://s/SKILL.md: not read"]);
  assert.deepEqual(verifySkill({ uri: "skill://s/SKILL.md", resources: "dynamic" }, new Map()), ["the entry lists no files"]);
});

test("each review question names its reviewer, date and patch, and quotes the actual review", async () => {
  const { readFileSync } = await import("node:fs");
  const reviews = JSON.parse(readFileSync(new URL("../eval/reviews.json", import.meta.url), "utf8"));
  assert.ok(reviews.length >= 10);
  const ids = new Set();
  for (const review of reviews) {
    assert.match(review.id, /^r\d{2}$/);
    assert.ok(!ids.has(review.id), review.id);
    ids.add(review.id);
    assert.equal(review.kind, "review", review.id);
    assert.ok(review.reviewer && review.subject && review.patch_message_id && review.question, review.id);
    assert.match(review.as_of, /^\d{4}-\d{2}-\d{2}$/, review.id);
    assert.match(review.gold_answer, /^\S[\s\S]*Supporting detail: \S/, review.id);
    assert.ok(review.gold_answer.startsWith(review.reviewer), review.id);
    assert.ok(review.gold_citations.length > 0, review.id);
    for (const citation of review.gold_citations) assert.ok([...normalizeForQuote(citation.quote)].length >= 12, review.id);
  }
});
