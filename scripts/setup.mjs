import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { APP_ROOT, initializeEnv, readLocalConfig } from "./lib/config.mjs";
import { PILOTSWARM_VERSION, RELEASE_ASSETS, restoreRelease } from "./lib/release.mjs";

try {
  if (Number(process.versions.node.split(".")[0]) < 24) {
    throw new Error("Node.js 24 or later is required.");
  }
  const created = initializeEnv();
  readLocalConfig();
  console.log(`[setup] ${created ? "Created private .env with a random database password." : "Preserved existing .env."}`);

  const directory = await restoreRelease(APP_ROOT);
  const manifest = JSON.parse(readFileSync(path.join(APP_ROOT, "package.json"), "utf8"));
  for (const asset of RELEASE_ASSETS.filter((entry) => entry.name)) {
    const expected = `file:dist-tarballs/${asset.file}`;
    if (manifest.dependencies?.[asset.name] !== expected) {
      throw new Error(`package.json must pin ${asset.name} to ${expected}.`);
    }
    const packed = JSON.parse(execFileSync("tar", ["-xOf", path.join(directory, asset.file), "package/package.json"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }));
    if (packed.name !== asset.name || packed.version !== PILOTSWARM_VERSION) {
      throw new Error(`Unexpected package identity in ${asset.file}.`);
    }
  }
  console.log(`[setup] Verified PilotSwarm ${PILOTSWARM_VERSION} packages and license.`);
  const command = existsSync(path.join(APP_ROOT, "package-lock.json")) ? "ci" : "install";
  execFileSync("npm", [command, "--no-audit", "--no-fund"], { cwd: APP_ROOT, stdio: "inherit" });
  console.log("[setup] Ready. Run npm start.");
} catch (error) {
  console.error(`[setup] ${error.message}`);
  process.exitCode = 1;
}
