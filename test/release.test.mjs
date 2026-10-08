import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import { RELEASE_ASSETS, PILOTSWARM_VERSION, restoreAsset, verifyAsset } from "../scripts/lib/release.mjs";

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
