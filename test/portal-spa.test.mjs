// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import { PORTAL_SPA, PROXY_BROWSER_AUTH, patchPortalSpa } from "../scripts/lib/portal-spa.mjs";

const INSTALLED_DIST = path.join(APP_ROOT, "node_modules", "pilotswarm", "web", "dist");

// A pilotswarm package holding the installed release's index.html and bundle.
function packageFixture(t) {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-portal-spa-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const dist = path.join(dir, "web", "dist");
  mkdirSync(path.join(dist, "assets"), { recursive: true });
  cpSync(path.join(INSTALLED_DIST, "index.html"), path.join(dist, "index.html"));
  cpSync(path.join(INSTALLED_DIST, "assets", PORTAL_SPA.entry), path.join(dist, "assets", PORTAL_SPA.entry));
  return { dir, dist };
}

// The bundle's choice of browser auth half, run from the bundle's own code
// with stand-ins for the halves it already had and for the browser.
function browserAuthFor(code, browser = {}) {
  const start = code.includes(PROXY_BROWSER_AUTH) ? code.indexOf(PROXY_BROWSER_AUTH) : code.indexOf("function vy(u)");
  const end = code.indexOf("}", code.indexOf("function vy(u)")) + 1;
  const vy = new Function("hy", "py", "gy", "fetch", "window", `${code.slice(start, end)}; return vy;`)(
    () => "none",
    () => "entra",
    () => "dev",
    browser.fetch,
    browser.window,
  );
  return (provider) => vy(provider);
}

function browser({ status = 200, principal = null } = {}) {
  const visited = [];
  return {
    visited,
    fetch: async (url) => {
      assert.equal(url, "/api/auth/me");
      return { ok: status === 200, status, json: async () => ({ principal }) };
    },
    window: { location: { pathname: "/sessions/1", search: "?tab=chat", assign: (url) => visited.push(url) } },
  };
}

test("the patched portal has a browser half for proxy, and browsers fetch it under a new name", (t) => {
  const { dir, dist } = packageFixture(t);
  const original = readFileSync(path.join(dist, "assets", PORTAL_SPA.entry), "utf8");
  assert.equal(browserAuthFor(original)("proxy"), null, "the release's SPA has no browser half for proxy");

  const patched = patchPortalSpa(dir);
  assert.match(patched, /^index-BrOn9Elg-[0-9a-f]{8}\.js$/);
  assert.ok(!existsSync(path.join(dist, "assets", PORTAL_SPA.entry)), "the unpatched bundle is gone");
  const html = readFileSync(path.join(dist, "index.html"), "utf8");
  assert.ok(html.includes(`src="/assets/${patched}"`));
  assert.ok(!html.includes(PORTAL_SPA.entry));

  const code = readFileSync(path.join(dist, "assets", patched), "utf8");
  assert.equal(code, original.replace(PORTAL_SPA.anchor, () => PORTAL_SPA.replacement));
  const choose = browserAuthFor(code);
  assert.deepEqual(["none", undefined, "entra", "dev", "other"].map(choose), ["none", "none", "entra", "dev", null]);
  assert.deepEqual(Object.keys(choose("proxy")).sort(), ["getAccessToken", "getAccount", "initialize", "signIn", "signOut"]);
});

test("the proxy half shows who oauth2-proxy signed in, and signs in and out through it", async (t) => {
  const { dir, dist } = packageFixture(t);
  const code = readFileSync(path.join(dist, "assets", patchPortalSpa(dir)), "utf8");

  const signedIn = browser({ principal: { subject: "example-user", displayName: "example-user", email: "example-user@example.com" } });
  const auth = browserAuthFor(code, signedIn)("proxy");
  assert.equal(await auth.getAccessToken(), null);
  const account = { name: "example-user", username: "example-user@example.com" };
  assert.deepEqual(await auth.initialize(), { account, accessToken: "proxy" });
  assert.deepEqual(auth.getAccount(), account);
  assert.equal(await auth.getAccessToken(), "proxy");
  assert.deepEqual(await auth.signOut(), { redirected: true });
  assert.deepEqual(await auth.signIn(), { redirected: true });
  assert.deepEqual(signedIn.visited, ["/oauth2/sign_out?rd=%2F", "/oauth2/start?rd=%2Fsessions%2F1%3Ftab%3Dchat"]);

  const expired = browserAuthFor(code, browser({ status: 401 }))("proxy");
  assert.deepEqual(await expired.initialize(), { account: null, accessToken: null });
  assert.equal(await expired.getAccessToken(), null);
});

test("the portal patch refuses any other bundle and changes nothing", (t) => {
  const { dir, dist } = packageFixture(t);
  const entry = path.join(dist, "assets", PORTAL_SPA.entry);
  const html = readFileSync(path.join(dist, "index.html"), "utf8");
  writeFileSync(entry, `${readFileSync(entry, "utf8")}\n`);
  assert.throws(() => patchPortalSpa(dir), /isn't the bundle the portal patch was written for/);
  assert.equal(readFileSync(path.join(dist, "index.html"), "utf8"), html);

  rmSync(entry);
  assert.throws(() => patchPortalSpa(dir), /doesn't exist/);
});

test("the image patches the installed portal", () => {
  const dockerfile = readFileSync(path.join(APP_ROOT, "Dockerfile"), "utf8");
  assert.match(dockerfile, /COPY scripts\/lib\/portal-spa\.mjs scripts\/lib\/portal-spa\.mjs/);
  assert.match(dockerfile, /npm ci [^\n]*\\\n\s*&& node --input-type=module -e "import \{ patchPortalSpa \} from '\.\/scripts\/lib\/portal-spa\.mjs'; patchPortalSpa\('\/app\/node_modules\/pilotswarm'\);"/);
});
