// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { dataNotice, readDataStatus } from "./data-status.ts";

const meta = (content: string) => ({ content });

test("the data-status tag says what's loaded: nothing, only the mail, or only the code", () => {
  assert.equal(readDataStatus(meta("empty")), "empty");
  assert.equal(readDataStatus(meta("mail")), "mail");
  assert.equal(readDataStatus(meta("code")), "code");
});

test("no tag, or anything else, is everything loaded: the panel says Available", () => {
  assert.equal(readDataStatus(null), undefined);
  for (const content of ["", "ready", "EMPTY", " empty", "mail,code", "none"]) {
    assert.equal(readDataStatus(meta(content)), undefined, content);
  }
});

test("each notice names the command that loads what's missing", () => {
  assert.deepEqual(
    [dataNotice("empty").pill, dataNotice("empty").command],
    ["No data loaded", "npm run load"],
  );
  assert.equal(dataNotice("mail").command, "npm run load -- --code-only");
  assert.match(dataNotice("mail").text, /commit history/);
  assert.equal(dataNotice("code").command, "npm run load -- --mail-only");
  assert.match(dataNotice("code").text, /mail/);
});
