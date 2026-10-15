import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { APP_ROOT, ensureRolePasswords, initializeEnv, readLocalConfig } from "./lib/config.mjs";
import { PILOTSWARM_VERSION, RELEASE_ASSETS, restoreRelease } from "./lib/release.mjs";

try {
  if (Number(process.versions.node.split(".")[0]) < 24) {
    throw new Error("Node.js 24 or later is required.");
  }
  const created = initializeEnv();
  const roles = !created && ensureRolePasswords();
  const config = readLocalConfig();
  console.log(`[setup] ${created ? "Created private .env with random database passwords." : "Preserved existing .env."}`);
  if (roles) console.log("[setup] Added passwords for the pgh_indexer and pgh_reader roles to .env.");
  if (!config.values.HORIZON_EMBED_URL) {
    console.log("[setup] Semantic search needs an embeddings endpoint: set HORIZON_EMBED_URL, HORIZON_EMBED_MODEL, HORIZON_EMBED_DIM=1536 and HORIZON_EMBED_API_KEY in .env (see README).");
  }

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
  // The home page is its own package: its React, Vite and TypeScript stay out
  // of the app's dependencies, and only its build reaches the image.
  const home = path.join(APP_ROOT, "home");
  console.log("[setup] Installing and building the home page (home/).");
  execFileSync("npm", ["ci", "--no-audit", "--no-fund"], { cwd: home, stdio: "inherit" });
  execFileSync("npm", ["run", "build", "--silent"], { cwd: home, stdio: "inherit" });
  try {
    execFileSync("cargo", ["--version"], { stdio: "ignore" });
  } catch {
    throw new Error("Rust is required: install it from https://rustup.rs (the toolchain in rust-toolchain.toml).");
  }
  console.log("[setup] Building pghacker-indexer and pghacker-mcp.");
  execFileSync("cargo", ["build", "--release", "--locked", "-p", "pghacker-indexer", "-p", "pghacker-mcp"], { cwd: APP_ROOT, stdio: "inherit" });
  console.log("[setup] Ready. Run npm start.");
} catch (error) {
  console.error(`[setup] ${error.message}`);
  process.exitCode = 1;
}
