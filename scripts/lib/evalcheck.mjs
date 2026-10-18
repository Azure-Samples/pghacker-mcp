// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Checks for the evaluation's answers: the same quote rule as the indexer's
// citations (pghacker_core::text::quote_in), CSV from the query tool, the
// answer's JSON block, and the commits it cites. For the review questions
// (eval/reviews.json): the prompt, whose citations are the reviewer's own and
// came before the review, and a skill fetched from a server, checked against
// its entry's digests.

import { createHash } from "node:crypto";

/** Whitespace- and quote-marker-insensitive text, as pghacker_core::text::normalize_for_quote. */
export function normalizeForQuote(text) {
  const words = [];
  for (const line of text.split(/\r?\n/)) {
    const stripped = line.trimStart().replace(/^[>\s]+/, "");
    words.push(...stripped.split(/\s+/).filter(Boolean));
  }
  return words.join(" ").replace(/[\u2018\u2019]/g, "'").replace(/[\u201c\u201d]/g, "\"");
}

export function quoteIn(body, quote) {
  const q = normalizeForQuote(quote);
  return [...q].length >= 12 && normalizeForQuote(body).includes(q);
}

/** RFC 4180 rows. */
export function parseCsv(text) {
  const rows = [];
  let row = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quoted) {
      if (c === "\"" && text[i + 1] === "\"") { field += "\""; i++; } else if (c === "\"") quoted = false; else field += c;
    } else if (c === "\"") quoted = true;
    else if (c === ",") { row.push(field); field = ""; } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i++;
      row.push(field); rows.push(row); row = []; field = "";
    } else field += c;
  }
  if (field !== "" || row.length) { row.push(field); rows.push(row); }
  return rows;
}

export function lastJsonBlock(reply) {
  const blocks = [...String(reply ?? "").matchAll(/```json\s*([\s\S]*?)```/g)];
  for (const block of blocks.reverse()) {
    try {
      return JSON.parse(block[1]);
    } catch {
      // Try an earlier block.
    }
  }
  return null;
}

/**
 * The results table, one row per question. The commit columns show "—" when
 * they don't apply: no gold commits, or none cited and none expected.
 */
export function resultsTable(results) {
  const yesNo = (value) => (value === null || value === undefined ? "—" : value ? "yes" : "no");
  return [
    "| Question | Kind | Correct | Citations valid | On a gold thread | Commits valid | Gold commit cited | Seconds |",
    "| --- | --- | --- | --- | --- | --- | --- | --- |",
    ...results.map((r) => [
      `${r.id}: ${String(r.question ?? "").replaceAll("|", "\\|")}`,
      r.kind ?? "",
      r.correct ? "yes" : "no",
      r.citations_valid ? "yes" : "no",
      r.on_gold_thread ? "yes" : "no",
      yesNo(r.commits_valid),
      yesNo(r.on_gold_commit),
      r.seconds ?? "",
    ].join(" | ")).map((row) => `| ${row} |`),
  ];
}

const FULL_SHA = /^[0-9a-f]{40}$/;

/** An answer's `commits`: the full SHAs, lower-cased and deduplicated, and the entries that aren't one. */
export function citedCommits(list) {
  const shas = new Set();
  const malformed = [];
  for (const item of Array.isArray(list) ? list : []) {
    const sha = String(item ?? "").trim().toLowerCase();
    if (FULL_SHA.test(sha)) shas.add(sha);
    else malformed.push(String(item));
  }
  return { shas: [...shas], malformed };
}

/**
 * Checks an answer's `commits` against `known`, the SHAs among them that the
 * history's code_commits holds: valid when every entry is a full SHA the
 * history has, and on a gold commit when one of them is among `gold`.
 */
export function checkCommits(list, known, gold = []) {
  const { shas, malformed } = citedCommits(list);
  const have = new Set(known.map((sha) => String(sha).toLowerCase()));
  const golden = new Set(gold.map((sha) => String(sha).toLowerCase()));
  const unknown = shas.filter((sha) => !have.has(sha));
  return {
    commits: shas,
    malformed,
    unknown,
    valid: malformed.length === 0 && unknown.length === 0,
    onGold: shas.some((sha) => golden.has(sha)),
  };
}

/** The most of a patch a review question shows, in characters. */
export const MAX_DIFF_CHARS = 60_000;

/**
 * A review question's prompt: the patch as it was posted, the reviewer and
 * the as-of date (the patch's), and the skill's text when the run has one.
 * Runs with and without the skill get the same prompt otherwise.
 */
export function reviewPrompt({ reviewer, asOf, subject, diff, skill }) {
  const shown = diff.length > MAX_DIFF_CHARS ? `${diff.slice(0, MAX_DIFF_CHARS)}\n[... the rest of the patch is cut ...]` : diff;
  const parts = [
    `Review the patch below as ${reviewer} would have when it was posted, on ${asOf}.`,
    `Use only evidence from before ${asOf}: what ${reviewer} had written and committed by then.`,
    "",
    `The patch, posted to the pgsql-hackers thread "${subject}":`,
    "",
    "````diff",
    shown,
    "````",
  ];
  if (skill) parts.push("", "Follow this skill, in its as-of mode:", "", "<skill>", skill.trim(), "</skill>");
  parts.push(
    "",
    `End with the JSON block your instructions describe: "answer" sums up the review, the points ${reviewer} would likely raise; "citations" are the messages of ${reviewer}'s that it quotes.`,
  );
  return parts.join("\n");
}

/**
 * A review's citations, against its reviewer and as-of date: how many
 * messages it quotes in the reviewer's own words (not text they quoted), and
 * which cited messages leak what came after: sent on or after the as-of
 * date, or one of the gold messages, the actual review. `found` maps a
 * Message-ID to the message: `author`, `sent_at` and `body_clean`.
 */
export function checkReview(citations, found, { reviewer, asOf, goldIds = [] }) {
  const name = String(reviewer).trim().toLowerCase();
  const cutoff = Date.parse(`${asOf}T00:00:00Z`);
  const gold = new Set(goldIds);
  const own = new Set();
  const leaked = new Set();
  for (const citation of citations) {
    const message = found.get(citation.message_id);
    if (!message) continue;
    const theirs = String(message.author ?? "").trim().toLowerCase() === name;
    if (theirs && quoteIn(message.body_clean ?? "", citation.quote ?? "")) own.add(citation.message_id);
    if (gold.has(citation.message_id) || !(Date.parse(message.sent_at) < cutoff)) leaked.add(citation.message_id);
  }
  return { own: own.size, leaked: [...leaked] };
}

/** The tool that loads a skill, for clients without the skills extension (pghacker_core::skills::Skill::tool). */
export function skillToolName(name) {
  return `load_skill_${String(name).replaceAll("-", "_")}`;
}

/** The tool that loads the review skill. */
export const REVIEW_SKILL_TOOL = skillToolName("review-as-author");

/** The tools a session called, in order, from its events (`tool.execution_start`). */
export function toolsCalled(events) {
  return (Array.isArray(events) ? events : [])
    .filter((event) => event?.eventType === "tool.execution_start")
    .map((event) => event.data?.toolName ?? event.data?.name)
    .filter((name) => typeof name === "string");
}

/** Whether `tools` include `tool`, by its name or with the prefix a client gives a server's tools. */
export function calledTool(tools, tool) {
  return tools.some((name) => name === tool || (name.endsWith(tool) && /[^A-Za-z0-9]$/.test(name.slice(0, -tool.length))));
}

/** The review results table, one row per review question. */
export function reviewTable(results) {
  const yesNo = (value) => (value ? "yes" : "no");
  return [
    "| Review | Reviewer | Correct | Score | Citations valid | Reviewer's own | No leak | Loaded the skill | Seconds |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ...results.map((r) => [
      `${r.id}: ${String(r.subject ?? r.question ?? "").replaceAll("|", "\\|")}`,
      r.reviewer ?? "",
      yesNo(r.correct),
      typeof r.score === "number" ? r.score.toFixed(2) : "",
      yesNo(r.citations_valid),
      r.own_citations ?? 0,
      yesNo(r.error === undefined && (r.leaked ?? []).length === 0),
      yesNo(r.loaded_skill),
      r.seconds ?? "",
    ].join(" | ")).map((row) => `| ${row} |`),
  ];
}

/**
 * What's wrong with a skill read from a server: each of its entry's files
 * against the digest and size the entry gives. Empty when it checks out.
 * `texts` maps each file's URI to the text read.
 */
export function verifySkill(entry, texts) {
  if (!Array.isArray(entry?.resources)) return ["the entry lists no files"];
  const problems = [];
  for (const { uri, digest, size } of entry.resources) {
    const text = texts.get(uri);
    if (text === undefined) {
      problems.push(`${uri}: not read`);
      continue;
    }
    const bytes = Buffer.from(text, "utf8");
    if (bytes.length !== size) problems.push(`${uri}: ${bytes.length} bytes, not ${size}`);
    const actual = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    if (actual !== digest) problems.push(`${uri}: ${actual}, not ${digest}`);
  }
  return problems;
}

