// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { formatStars, readRepository } from "./repository.ts";

const meta = (content: string, stars?: string) => ({ content, dataset: stars === undefined ? {} : { stars } });

test("the link goes to the repository the meta tag names, with its stars once the server adds them", () => {
  assert.deepEqual(readRepository(meta("Azure-Samples/pghacker-mcp")), {
    name: "Azure-Samples/pghacker-mcp",
    url: "https://github.com/Azure-Samples/pghacker-mcp",
  });
  assert.equal(readRepository(meta("Azure-Samples/pghacker-mcp", "1234"))?.stars, 1234);
  assert.equal(readRepository(meta("Azure-Samples/pghacker-mcp", "0"))?.stars, 0);
});

test("no tag, or a name that isn't owner/name, is no link; a count that isn't whole is no count", () => {
  assert.equal(readRepository(null), undefined);
  for (const name of ["", "pghacker-mcp", "a/b/c", "javascript:alert(1)/x", "a/b c"]) {
    assert.equal(readRepository(meta(name)), undefined, name);
  }
  for (const stars of ["", "-1", "1.5", "12abc", "1e3"]) {
    assert.equal(readRepository(meta("o/r", stars))?.stars, undefined, stars);
  }
});

test("star counts read as GitHub shows them", () => {
  const cases: [number, string][] = [
    [0, "0"], [7, "7"], [999, "999"], [1000, "1k"], [1049, "1k"], [1050, "1.1k"], [1234, "1.2k"],
    [12_345, "12.3k"], [999_949, "999.9k"], [999_950, "1m"], [1_250_001, "1.3m"],
  ];
  for (const [stars, shown] of cases) assert.equal(formatStars(stars), shown, String(stars));
});
