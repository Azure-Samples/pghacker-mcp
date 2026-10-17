// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { readCopyright, readPrivacyUrl } from "./footer.ts";

const privacy = (content: string) => ({ content });
const copyright = (content: string, text?: string) => ({ content, dataset: text === undefined ? {} : { text } });

test("the privacy link goes to the https URL the meta tag gives", () => {
  assert.equal(readPrivacyUrl(privacy("https://example.com/privacy")), "https://example.com/privacy");
  assert.equal(readPrivacyUrl(privacy("https://example.com")), "https://example.com/");
  assert.equal(
    readPrivacyUrl(privacy("https://privacy.example.com:8443/en-us/privacy?a=1&b=2#top")),
    "https://privacy.example.com:8443/en-us/privacy?a=1&b=2#top",
  );
});

test("no privacy tag is no link", () => {
  assert.equal(readPrivacyUrl(null), undefined);
});

const NOT_HTTPS_URLS = [
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
];

test("only an https URL with a host and no credentials is a privacy link", () => {
  for (const content of NOT_HTTPS_URLS) assert.equal(readPrivacyUrl(privacy(content)), undefined, content);
});

test("the copyright link says what the meta tag's data-text says, and goes to its https URL", () => {
  assert.deepEqual(readCopyright(copyright("https://example.com/", "© 2026 Example")), {
    url: "https://example.com/",
    text: "© 2026 Example",
  });
  assert.deepEqual(readCopyright(copyright("https://example.com", "  Example & Sons  ")), {
    url: "https://example.com/",
    text: "Example & Sons",
  });
});

test("no copyright tag is no link", () => {
  assert.equal(readCopyright(null), undefined);
});

test("a copyright link needs an https URL, and text without control characters, of up to 100 characters", () => {
  for (const content of NOT_HTTPS_URLS) {
    assert.equal(readCopyright(copyright(content, "© 2026 Example")), undefined, content);
  }
  for (const text of [undefined, "", "   ", "© 2026\nExample", "© 2026\u0000Example", "x".repeat(101)]) {
    assert.equal(readCopyright(copyright("https://example.com/", text)), undefined, JSON.stringify(text));
  }
  assert.equal(readCopyright(copyright("https://example.com/", "x".repeat(100)))?.text.length, 100);
  // Characters, not UTF-16 units: 100 emoji fit, 101 don't.
  assert.equal([...(readCopyright(copyright("https://example.com/", "😀".repeat(100)))?.text ?? "")].length, 100);
  assert.equal(readCopyright(copyright("https://example.com/", "😀".repeat(101))), undefined);
});
