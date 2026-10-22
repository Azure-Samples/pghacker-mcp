// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import {
  NODE_REQUIREMENT, RELEASE_ASSETS, PILOTSWARM_VERSION, restoreAsset, supportedNode, verifyAsset,
} from "../scripts/lib/release.mjs";

const bytes = Buffer.from("unit-test release fixture");
const asset = { file: "fixture.tgz", sha256: createHash("sha256").update(bytes).digest("hex") };

test("every dependency uses the same pinned, verified GitHub Release", () => {
  const manifest = JSON.parse(readFileSync(path.join(APP_ROOT, "package.json"), "utf8"));
  const packages = RELEASE_ASSETS.filter((entry) => entry.name);
  assert.equal(packages.length, 3);
  assert.deepEqual(Object.keys(manifest.dependencies).sort(), packages.map((entry) => entry.name).sort());
  for (const entry of packages) {
    assert.equal(entry.file, `${entry.name}-${PILOTSWARM_VERSION}.tgz`);
    assert.equal(manifest.dependencies[entry.name], `file:dist-tarballs/${entry.file}`);
    assert.match(entry.sha256, /^[a-f0-9]{64}$/);
  }
  assert.ok(RELEASE_ASSETS.some((entry) => entry.file === "LICENSE"));
});

test("the app needs the Node.js releases that PilotSwarm's packages do", () => {
  const manifest = JSON.parse(readFileSync(path.join(APP_ROOT, "package.json"), "utf8"));
  assert.equal(manifest.engines.node, NODE_REQUIREMENT);
  for (const entry of RELEASE_ASSETS.filter((asset) => asset.name)) {
    const installed = JSON.parse(readFileSync(path.join(APP_ROOT, "node_modules", entry.name, "package.json"), "utf8"));
    assert.equal(installed.version, PILOTSWARM_VERSION, entry.name);
    assert.equal(installed.engines.node, NODE_REQUIREMENT, entry.name);
  }
  for (const version of ["24.22.1", "v24.22.1", "24.23.0", "24.30.4", "26.12.1", "26.13.0", "27.0.0", "28.1.2"]) {
    assert.ok(supportedNode(version), version);
  }
  // The unpatched releases, and 25.x, which has reached its end of life.
  for (const version of ["22.23.4", "24.0.0", "24.21.9", "24.22.0", "25.9.0", "26.10.0", "26.12.0"]) {
    assert.ok(!supportedNode(version), version);
  }
});

test("the image runs a Node.js release that PilotSwarm supports", () => {
  const dockerfile = readFileSync(path.join(APP_ROOT, "Dockerfile"), "utf8");
  const [, version] = /^ARG NODE_IMAGE=node:(\d+\.\d+\.\d+)-trixie-slim$/m.exec(dockerfile) ?? [];
  assert.ok(version, "the Dockerfile pins NODE_IMAGE to an exact Node.js release");
  assert.ok(supportedNode(version), version);
});

test("checksum verification rejects altered bytes", () => {
  assert.doesNotThrow(() => verifyAsset(bytes, asset));
  assert.throws(() => verifyAsset(Buffer.from("altered release"), asset), /Checksum mismatch for fixture\.tgz/);
});

test("verified cached files do not require a download", async () => {
  const directory = mkdtempSync(path.join(os.tmpdir(), "pghacker-release-"));
  try {
    writeFileSync(path.join(directory, asset.file), bytes);
    await restoreAsset(directory, asset, () => { throw new Error("Unexpected download"); });
    assert.deepEqual(readFileSync(path.join(directory, asset.file)), bytes);
  } finally {
    rmSync(directory, { recursive: true });
  }
});

test("a corrupted cached file is rejected, not silently replaced", async () => {
  const directory = mkdtempSync(path.join(os.tmpdir(), "pghacker-corrupt-"));
  try {
    writeFileSync(path.join(directory, asset.file), "corrupted");
    await assert.rejects(restoreAsset(directory, asset, () => { throw new Error("Unexpected download"); }), /Checksum mismatch/);
    assert.equal(readFileSync(path.join(directory, asset.file), "utf8"), "corrupted");
  } finally {
    rmSync(directory, { recursive: true });
  }
});

test("downloads are verified before any package file is written", async () => {
  const directory = mkdtempSync(path.join(os.tmpdir(), "pghacker-download-"));
  try {
    await assert.rejects(restoreAsset(directory, asset, async () => new Response("wrong bytes")), /Checksum mismatch/);
    assert.equal(existsSync(path.join(directory, asset.file)), false);
    await restoreAsset(directory, asset, async () => new Response(bytes));
    assert.deepEqual(readFileSync(path.join(directory, asset.file)), bytes);
  } finally {
    rmSync(directory, { recursive: true });
  }
});

test("HTTP download failures stop setup explicitly", async () => {
  const directory = mkdtempSync(path.join(os.tmpdir(), "pghacker-http-failure-"));
  try {
    await assert.rejects(restoreAsset(directory, asset, async () => new Response("unavailable", { status: 503 })), /HTTP 503/);
    assert.equal(existsSync(path.join(directory, asset.file)), false);
  } finally {
    rmSync(directory, { recursive: true });
  }
});
