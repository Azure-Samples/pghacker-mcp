// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The lockfiles resolve packages from public hosts only, with SHA-512
// integrity: an install from a private mirror records the mirror's URLs, and
// some mirrors give SHA-1 only. npm rewrites registry.npmjs.org to the
// registry a user configures (replace-registry-host).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";

const ROOT = path.resolve(import.meta.dirname, "..");
const PUBLIC = ["https://registry.npmjs.org/", "https://github.com/"];

for (const file of ["package-lock.json", "home/package-lock.json"]) {
  test(`${file} resolves packages from public hosts with SHA-512 integrity`, () => {
    const lock = JSON.parse(readFileSync(path.join(ROOT, file), "utf8"));
    const resolved = Object.entries(lock.packages).filter(([, entry]) => entry.resolved?.startsWith("http"));
    assert.ok(resolved.length > 0);
    for (const [name, entry] of resolved) {
      assert.ok(PUBLIC.some((host) => entry.resolved.startsWith(host)), `${name}: ${entry.resolved}`);
      assert.match(entry.integrity ?? "", /^sha512-/, `${name}: ${entry.integrity}`);
    }
  });
}
