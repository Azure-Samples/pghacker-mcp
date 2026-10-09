// Checks for the evaluation's answers: the same quote rule as the indexer's
// citations (pghacker_core::text::quote_in), CSV from the query tool, and the
// answer's JSON block.

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

