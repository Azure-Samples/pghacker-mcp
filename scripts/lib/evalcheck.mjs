// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Checks for the evaluation's answers: the same quote rule as the indexer's
// citations (pghacker_core::text::quote_in), CSV from the query tool, the
// answer's JSON block, and the commits it cites.

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

