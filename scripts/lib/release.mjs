// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { createHash } from "node:crypto";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

export const PILOTSWARM_VERSION = "0.8.5";
export const RELEASE_URL = `https://github.com/microsoft/PilotSwarm/releases/download/v${PILOTSWARM_VERSION}`;
export const RELEASE_ASSETS = [
  {
    name: "pilotswarm",
    file: "pilotswarm-0.8.5.tgz",
    sha256: "045cab279a8861880e9bf7a6ade5f24ec18add02d52ce1c2360faa15c0ac2796",
  },
  {
    name: "pilotswarm-horizon-store",
    file: "pilotswarm-horizon-store-0.8.5.tgz",
    sha256: "1a657352318f95ebc0ed6726584979fc90ffada1001cca308220d402bc43f139",
  },
  {
    name: "pilotswarm-sdk",
    file: "pilotswarm-sdk-0.8.5.tgz",
    sha256: "02f9330910523964f7b876bd97c23761de4cd42e8ce07c051cb888c66e0de000",
  },
  {
    file: "LICENSE",
    sha256: "d6a63c1f42286bbcd74a0ef0d22d1101e36af6deb14cf4c5c927573108a4502d",
  },
];

/**
 * The Node.js releases that PilotSwarm's packages support (their engines): its
 * security baseline, without the unpatched 24.x and 26.x releases or 25.x.
 */
export const NODE_REQUIREMENT = "^24.22.1 || >=26.12.1";

/** Whether Node.js `version`, such as process.versions.node, meets NODE_REQUIREMENT. */
export function supportedNode(version) {
  const [major, minor, patch] = String(version).replace(/^v/, "").split(/[.-]/).map(Number);
  const atLeast = (wantMinor, wantPatch) => minor > wantMinor || (minor === wantMinor && patch >= wantPatch);
  if (major === 24) return atLeast(22, 1);
  if (major === 26) return atLeast(12, 1);
  return major > 26;
}

export function verifyAsset(bytes, asset) {
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== asset.sha256) {
    throw new Error(`Checksum mismatch for ${asset.file}; expected ${asset.sha256}, received ${actual}.`);
  }
}

export async function restoreAsset(directory, asset, fetchImpl = fetch) {
  const destination = path.join(directory, asset.file);
  try {
    verifyAsset(await readFile(destination), asset);
    return;
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const response = await fetchImpl(`${RELEASE_URL}/${asset.file}`, {
    signal: AbortSignal.timeout(90_000),
  });
  if (!response.ok) throw new Error(`Download failed for ${asset.file}: HTTP ${response.status}.`);
  const bytes = Buffer.from(await response.arrayBuffer());
  verifyAsset(bytes, asset);
  await writeFile(destination, bytes, { flag: "wx" });
}

export async function restoreRelease(root) {
  const directory = path.join(root, "dist-tarballs");
  await mkdir(directory, { recursive: true });
  await Promise.all(RELEASE_ASSETS.map((asset) => restoreAsset(directory, asset)));
  return directory;
}
