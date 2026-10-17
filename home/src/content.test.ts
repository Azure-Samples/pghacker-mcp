// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { CAPABILITIES, EXAMPLES, POWERED_BY, PRODUCT, shortMessageId } from "./content.ts";

/** pghacker-mcp's tools: what the page may name. */
const TOOLS = new Set([
  "search_threads", "get_thread", "search_extracts", "author_perspective", "search_authors", "get_author",
  "get_ontology", "query", "search", "grep", "read_file", "list_files",
  "blame", "get_commit", "search_commits", "search_patches",
]);

test("the examples: three to five, each with a prompt, an answer and its tools", () => {
  assert.ok(EXAMPLES.length >= 3 && EXAMPLES.length <= 5);
  assert.equal(new Set(EXAMPLES.map((example) => example.id)).size, EXAMPLES.length);
  for (const example of EXAMPLES) {
    assert.match(example.prompt, /\?$|\.$/, example.id);
    assert.ok(example.answer.length > 0, example.id);
    assert.ok(example.tools.length > 0, example.id);
    for (const tool of example.tools) assert.ok(TOOLS.has(tool), `${example.id}: ${tool}`);
    assert.ok(example.citations.length > 0 || example.sql !== undefined, `${example.id} cites nothing`);
  }
});

test("citations are well formed", () => {
  for (const example of EXAMPLES) {
    for (const citation of example.citations) {
      if (citation.kind === "mail") {
        assert.match(citation.messageId, /^[^\s@<>]+@[^\s@<>]+$/, example.id);
        assert.match(citation.date, /^\d{4}-\d{2}-\d{2}$/, example.id);
        assert.ok(citation.quote.length > 0 && citation.author.length > 0, example.id);
      } else if (citation.kind === "commit") {
        assert.match(citation.sha, /^[0-9a-f]{40}$/, example.id);
        assert.match(citation.date, /^\d{4}-\d{2}-\d{2}$/, example.id);
        assert.ok(citation.subject.length > 0 && citation.branch.length > 0, example.id);
      } else {
        assert.match(citation.location, /^[\w./-]+:\d+$/, example.id);
        assert.ok(citation.excerpt.length > 0, example.id);
      }
    }
  }
});

test("the capabilities name only pghacker-mcp's tools", () => {
  assert.equal(CAPABILITIES.length, 4);
  for (const capability of CAPABILITIES) {
    for (const tool of capability.tools) assert.ok(TOOLS.has(tool), `${capability.id}: ${tool}`);
  }
});

test("the page names no deployment's host", () => {
  const text = JSON.stringify({ PRODUCT, POWERED_BY, CAPABILITIES, EXAMPLES });
  assert.doesNotMatch(text, /azurefd\.net|azurecontainerapps\.io|https?:\/\/[^"]*\/mcp/);
});

test("long Message-IDs are shortened in the middle, short ones kept", () => {
  assert.equal(shortMessageId("95014.1782050569@sss.pgh.pa.us"), "95014.1782050569@sss.pgh.pa.us");
  assert.equal(
    shortMessageId("CAAKRu_bEtjWYWhYxSo0o_t3DaZYRQd5PkC0abA0g9Mp4+ovH0w@mail.gmail.com"),
    "CAAKRu_bEtjWYWhYxS…@mail.gmail.com",
  );
  assert.equal(shortMessageId("no-at-sign"), "no-at-sign");
});
