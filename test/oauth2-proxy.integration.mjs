// The routes through the real oauth2-proxy, as the container runs it: its
// release binary for this machine (checked against the release's checksums,
// the Linux ones the Dockerfile's), with the container's flags from
// scripts/lib/azure.mjs, in front of stand-ins for pghacker-mcp, the portal
// and the home page. Nobody signs in: this checks what passes without it.
//   npm run test:routes
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test, { after, before } from "node:test";
import { oauth2ProxyArgs, readAzureConfig } from "../scripts/lib/azure.mjs";
import { APP_ROOT } from "../scripts/lib/config.mjs";

const VERSION = "7.15.5";
const SHA256 = {
  "darwin-amd64": "ad0a6bec165f6c88749ab94833a14c7be0e8ebcad07987eb080de78892d732bb",
  "darwin-arm64": "b8c9c86e5ad35808753c724e2a64af59ae066f31664973fd68f85bd0a7911ee3",
  "linux-amd64": "f63f94bf72c5f46ab002a0a275aa8b3cf19b4d828aed08a13978cb9a62c3a1fd",
  "linux-arm64": "ebef73d06a05f76a03b0b2f95aa8424c10201228bd64621a2a7d375ce44dd637",
};

/** A synthetic deployment's settings: no real name, host or secret. */
const ENV = {
  PGHACKER_DB_HOST: "pghacker-example-hdb.example.invalid",
  PGHACKER_DB_NAME: "pghacker",
  PGHACKER_DB_ADMIN_USER: "pghacker_admin",
  PGHACKER_DB_ADMIN_PASSWORD: "unused-admin-password",
  PGH_INDEXER_PASSWORD: "unused-indexer-password",
  PGH_READER_PASSWORD: "unused-reader-password",
  PGH_SWARM_PASSWORD: "unused-swarm-password",
  AZURE_CLIENT_ID: "unused-client-id",
  PGHACKER_FOUNDRY_ENDPOINT: "https://pghacker-example.openai.azure.com",
  PGHACKER_FOUNDRY_SOL: "gpt-6.1-sol",
  PGHACKER_FOUNDRY_LUNA: "gpt-6-luna",
  PGHACKER_FOUNDRY_EMBEDDING: "text-embedding-3-small",
  PGHACKER_PUBLIC_URL: "https://pghacker-example.z01.azurefd.net",
  PGHACKER_GITHUB_ORG: "example-org",
  PGHACKER_GITHUB_TEAM: "example-team",
  OAUTH2_PROXY_CLIENT_ID: "example-client-id",
  OAUTH2_PROXY_CLIENT_SECRET: "example-client-secret",
  OAUTH2_PROXY_COOKIE_SECRET: "0123456789abcdef0123456789abcdef",
};

function platform() {
  const os = { darwin: "darwin", linux: "linux" }[process.platform];
  const arch = { x64: "amd64", arm64: "arm64" }[process.arch];
  const name = os && arch ? `${os}-${arch}` : undefined;
  if (!name || !SHA256[name]) throw new Error(`No oauth2-proxy ${VERSION} for ${process.platform}-${process.arch} here.`);
  return name;
}

/** oauth2-proxy's binary, downloaded once into .runtime/cache and checked. */
async function oauth2Proxy() {
  const name = `oauth2-proxy-v${VERSION}.${platform()}`;
  const dir = path.join(APP_ROOT, ".runtime", "cache", "oauth2-proxy");
  const binary = path.join(dir, name, "oauth2-proxy");
  if (existsSync(binary)) return binary;
  const response = await fetch(`https://github.com/oauth2-proxy/oauth2-proxy/releases/download/v${VERSION}/${name}.tar.gz`);
  if (!response.ok) throw new Error(`Downloading ${name} failed: HTTP ${response.status}.`);
  const archive = Buffer.from(await response.arrayBuffer());
  const sha = createHash("sha256").update(archive).digest("hex");
  if (sha !== SHA256[platform()]) throw new Error(`${name}.tar.gz has sha256 ${sha}, not the release's.`);
  mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `${name}.tar.gz`);
  writeFileSync(file, archive);
  const tar = spawn("tar", ["-xzf", file, "-C", dir], { stdio: "inherit" });
  const [code] = await once(tar, "exit");
  rmSync(file);
  if (code !== 0 || !existsSync(binary)) throw new Error(`Unpacking ${name} failed.`);
  return binary;
}

const cleanups = [];
let base;

/** A stand-in for a target: it answers with its name and what it got. */
async function upstream(name) {
  const server = http.createServer((request, response) => {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ name, method: request.method, url: request.url, user: request.headers["x-forwarded-user"] ?? null }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanups.push(() => {
    server.closeAllConnections();
    server.close();
  });
  return `http://127.0.0.1:${server.address().port}`;
}

async function freePort() {
  const server = net.createServer().listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  return port;
}

before(async () => {
  const binary = await oauth2Proxy();
  const origins = { mcp: await upstream("mcp"), portal: await upstream("portal"), home: await upstream("home") };
  const listen = `127.0.0.1:${await freePort()}`;
  const config = readAzureConfig(ENV, { root: APP_ROOT, dataDir: path.join(APP_ROOT, ".runtime") });
  const proxy = spawn(binary, oauth2ProxyArgs(config, { listen, origins }), {
    env: {
      PATH: process.env.PATH,
      OAUTH2_PROXY_CLIENT_ID: ENV.OAUTH2_PROXY_CLIENT_ID,
      OAUTH2_PROXY_CLIENT_SECRET: ENV.OAUTH2_PROXY_CLIENT_SECRET,
      OAUTH2_PROXY_COOKIE_SECRET: ENV.OAUTH2_PROXY_COOKIE_SECRET,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  proxy.stdout.on("data", (chunk) => (log += chunk));
  proxy.stderr.on("data", (chunk) => (log += chunk));
  cleanups.push(() => proxy.kill("SIGTERM"));
  base = `http://${listen}`;
  for (let ready = false; !ready;) {
    if (proxy.exitCode !== null) throw new Error(`oauth2-proxy exited (${proxy.exitCode}):\n${log}`);
    ready = await fetch(`${base}/ping`).then((response) => response.ok, () => false);
    if (!ready) await delay(100);
  }
}, { timeout: 180_000 });

after(() => {
  for (const cleanup of cleanups) cleanup();
});

/** What `method route` reaches: a target's name, "sign-in", or the status. */
async function reach(route, { method = "GET", headers = {} } = {}) {
  const response = await fetch(`${base}${route}`, { method, headers, redirect: "manual" });
  const text = await response.text();
  if (method === "HEAD") return response.status;
  if (response.status === 200 && response.headers.get("content-type") === "application/json") {
    const answer = JSON.parse(text);
    assert.equal(answer.method, method, route);
    assert.equal(answer.url, route, `${route} reached ${answer.name} as ${answer.url}`);
    return answer.name;
  }
  if (response.status === 302 && /github\.com\/login\/oauth\/authorize|\/oauth2\/(start|sign_in)/.test(response.headers.get("location") ?? "")) {
    return "sign-in";
  }
  return response.status;
}

test("oauth2-proxy routes the site as the container runs it", async () => {
  const expected = [
    ["/", "home"],
    ["/?session=abc&view=canvas", "home"],
    ["/static/index-abc.js", "home"],
    ["/favicon.svg", "home"],
    ["/mcp", "mcp"],
    ["/portal", "sign-in"],
    ["/portal/", "sign-in"],
    ["/portal/?session=abc", "sign-in"],
    ["/assets/index-BrOn9Elg.js", "sign-in"],
    ["/api/v1/health", 401],
    ["/api/auth/me", 401],
    ["/index.html", "sign-in"],
    ["/no-such-page", "sign-in"],
    ["/mcp/", "sign-in"],
    ["/static", "sign-in"],
  ];
  for (const [route, where] of expected) {
    assert.equal(await reach(route), where, route);
  }
  assert.equal(await reach("/", { method: "HEAD" }), 200);
  assert.equal(await reach("/mcp", { method: "POST" }), "mcp");
  assert.notEqual(await reach("/", { method: "POST" }), "home");
});

test("oauth2-proxy answers /robots.txt itself, as the home page's file does locally", async () => {
  const robots = await fetch(`${base}/robots.txt`);
  assert.equal(robots.status, 200);
  const rules = (text) => text.replace(/^#.*\n/gm, "").trim();
  assert.equal(rules(await robots.text()), "User-agent: *\nDisallow: /");
  assert.equal(rules(readFileSync(path.join(APP_ROOT, "home", "public", "robots.txt"), "utf8")), "User-agent: *\nDisallow: /");
});

test("without sign-in, no identity reaches a target, whatever the client sends", async () => {
  const answer = await fetch(`${base}/mcp`, { headers: { "x-forwarded-user": "someone" } }).then((response) => response.json());
  assert.equal(answer.user, null);
});

test("a forged X-Forwarded-Uri doesn't skip sign-in", { todo: "Azure-Samples/pghacker-mcp#1" }, async () => {
  for (const claimed of ["/", "/mcp"]) {
    assert.equal(await reach("/portal/", { headers: { "x-forwarded-uri": claimed } }), "sign-in", claimed);
  }
});
