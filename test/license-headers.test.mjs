// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import test from "node:test";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import { addHeader, hasHeader, headerLines, isSource, missingHeaders } from "../scripts/lib/license-headers.mjs";

function inGitCheckout() {
  try {
    execFileSync("git", ["rev-parse", "--is-inside-work-tree"], { cwd: APP_ROOT, stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

test("every source file starts with the license header", { skip: !inGitCheckout() && "not a git checkout" }, () => {
  assert.deepEqual(missingHeaders(APP_ROOT), [], "npm run license-headers -- --fix adds the header");
});

test("source files take the header in their comment syntax", () => {
  assert.deepEqual(headerLines("crates/x/src/lib.rs"), ["// Copyright (c) Microsoft Corporation.", "// Licensed under the MIT license."]);
  for (const file of ["scripts/x.mjs", "home/src/x.ts", "home/src/x.tsx", "home/src/x.js"]) {
    assert.equal(headerLines(file)[0], "// Copyright (c) Microsoft Corporation.", file);
  }
  assert.equal(headerLines("migrations/0001_core.sql")[1], "-- Licensed under the MIT license.");
  for (const file of ["deploy/terraform/main.tf", "deploy/terraform/tests/x.tftest.hcl", "deploy/scripts/x.sh", "Dockerfile", "deploy/seed.Dockerfile"]) {
    assert.equal(headerLines(file)[0], "# Copyright (c) Microsoft Corporation.", file);
  }
  assert.deepEqual(headerLines("home/src/styles/x.css"), ["/* Copyright (c) Microsoft Corporation. */", "/* Licensed under the MIT license. */"]);
  assert.equal(headerLines("home/index.html")[0], "<!-- Copyright (c) Microsoft Corporation. -->");
});

test("vendored code, generated files, data and configuration take no header", () => {
  for (const file of [
    "vendor/tokio-postgres/src/lib.rs", "deploy/terraform/.terraform.lock.hcl", "package.json", "package-lock.json",
    "Cargo.lock", "Cargo.toml", "compose.yaml", "deploy/environments/example.tfvars", "README.md", "docs/assets/x.svg",
    "crates/pghacker-mail/tests/fixtures/x.eml",
  ]) {
    assert.equal(isSource(file), false, file);
  }
  assert.equal(isSource("crates/pghacker-core/src/lib.rs"), true);
});

test("the header goes first, with a blank line after it", () => {
  assert.equal(
    addHeader("a.rs", "//! A module.\nfn main() {}\n"),
    "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT license.\n\n//! A module.\nfn main() {}\n",
  );
  assert.equal(
    addHeader("a.sql", "CREATE TABLE t ();\n"),
    "-- Copyright (c) Microsoft Corporation.\n-- Licensed under the MIT license.\n\nCREATE TABLE t ();\n",
  );
  assert.equal(addHeader("a.mjs", ""), "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT license.\n");
});

test("#! lines, Dockerfile parser directives and HTML doctypes stay above the header", () => {
  assert.equal(
    addHeader("run.sh", "#!/usr/bin/env bash\nset -eu\n"),
    "#!/usr/bin/env bash\n# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT license.\n\nset -eu\n",
  );
  assert.equal(
    addHeader("Dockerfile", "# syntax=docker/dockerfile:1.7\n# escape=`\nFROM scratch\n"),
    "# syntax=docker/dockerfile:1.7\n# escape=`\n# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT license.\n\nFROM scratch\n",
  );
  assert.equal(
    addHeader("app.Dockerfile", "# The app.\nFROM scratch\n"),
    "# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT license.\n\n# The app.\nFROM scratch\n",
  );
  assert.equal(
    addHeader("index.html", "<!doctype html>\n<html></html>\n"),
    "<!doctype html>\n<!-- Copyright (c) Microsoft Corporation. -->\n<!-- Licensed under the MIT license. -->\n\n<html></html>\n",
  );
});

test("adding the header twice adds it once, and keeps CRLF line ends", () => {
  const once = addHeader("a.ts", "export {};\r\n");
  assert.equal(once, "// Copyright (c) Microsoft Corporation.\r\n// Licensed under the MIT license.\r\n\r\nexport {};\r\n");
  assert.equal(addHeader("a.ts", once), once);
  assert.equal(hasHeader("a.ts", once), true);
});

test("a header that differs, or isn't first, doesn't count", () => {
  assert.equal(hasHeader("a.rs", "// Copyright (c) Microsoft Corporation. All rights reserved.\n// Licensed under the MIT license.\n"), false);
  assert.equal(hasHeader("a.rs", "use x;\n// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT license.\n"), false);
  assert.equal(hasHeader("a.rs", "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT license.\nuse x;\n"), true);
});
