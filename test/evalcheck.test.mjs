import assert from "node:assert/strict";
import test from "node:test";
import { lastJsonBlock, normalizeForQuote, parseCsv, quoteIn } from "../scripts/lib/evalcheck.mjs";

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
