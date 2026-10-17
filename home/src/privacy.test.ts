// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { readPrivacyUrl } from "./privacy.ts";

const meta = (content: string) => ({ content });

test("the link goes to the https URL the meta tag gives", () => {
  assert.equal(readPrivacyUrl(meta("https://example.com/privacy")), "https://example.com/privacy");
  assert.equal(readPrivacyUrl(meta("https://example.com")), "https://example.com/");
  assert.equal(
    readPrivacyUrl(meta("https://privacy.example.com:8443/en-us/privacy?a=1&b=2#top")),
    "https://privacy.example.com:8443/en-us/privacy?a=1&b=2#top",
  );
});

test("no tag is no link", () => {
  assert.equal(readPrivacyUrl(null), undefined);
});

test("only an https URL with a host and no credentials is a link", () => {
  for (const content of [
    "",
    "   ",
    "privacy",
    "/privacy",
    "//example.com/privacy",
    "example.com/privacy",
    "http://example.com/privacy",
    "javascript:alert(1)",
    "data:text/html,privacy",
    "mailto:privacy@example.com",
    "https://",
    "https://user:secret@example.com/privacy",
    "https://user@example.com/privacy",
  ]) {
    assert.equal(readPrivacyUrl(meta(content)), undefined, content);
  }
});
