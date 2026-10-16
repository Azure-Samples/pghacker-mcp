// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { createHash } from "node:crypto";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

export const PILOTSWARM_VERSION = "0.8.2";
export const RELEASE_URL = `https://github.com/microsoft/PilotSwarm/releases/download/v${PILOTSWARM_VERSION}`;
export const RELEASE_ASSETS = [
  {
    name: "pilotswarm",
    file: "pilotswarm-0.8.2.tgz",
    sha256: "4372a2177bf9b995f40c09959145f13a269a2efbabde6b8348e58de13da0cea1",
  },
  {
    name: "pilotswarm-horizon-store",
    file: "pilotswarm-horizon-store-0.8.2.tgz",
    sha256: "6c6a72e18b0656caebc3500eb583699dac48700bc3d432cb3928bd84e5fca837",
  },
  {
    name: "pilotswarm-sdk",
    file: "pilotswarm-sdk-0.8.2.tgz",
    sha256: "e66ed7aa73a4fc987f84b18532641962266d6aa644b7a3565d34c5442b8ba5ff",
  },
  {
    file: "LICENSE",
    sha256: "d6a63c1f42286bbcd74a0ef0d22d1101e36af6deb14cf4c5c927573108a4502d",
  },
];

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
