// PilotSwarm 0.8.2's portal server offers the "proxy" auth provider, which
// takes the identity an authenticating proxy (here oauth2-proxy) puts on each
// request. Its prebuilt SPA has no browser half for it, though (it knows only
// none, entra and dev), so behind a proxy it stops at 'Unsupported portal auth
// provider "proxy"'. Upstream main is the same. The patch adds that half.
//
// The anchor is minified code, so the patch is pinned to the bundle's checksum
// and refuses any other bundle: re-check it when upgrading PilotSwarm.
import { createHash } from "node:crypto";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

// oauth2-proxy signed the user in before the SPA loaded, so the SPA asks the
// portal who that is, and sends sign-in and sign-out to oauth2-proxy. The token
// only tells the SPA that someone is signed in: for proxy, the portal ignores
// it and reads the identity off oauth2-proxy's headers.
export const PROXY_BROWSER_AUTH = `function pghProxyBrowserAuth() {
  let account = null;
  return {
    async initialize() {
      const response = await fetch("/api/auth/me", { cache: "no-store" });
      const principal = response.ok ? (await response.json())?.principal : null;
      account = principal ? { name: principal.displayName || principal.subject, username: principal.email || "" } : null;
      return { account, accessToken: account ? "proxy" : null };
    },
    async signIn() {
      window.location.assign("/oauth2/start?rd=" + encodeURIComponent(window.location.pathname + window.location.search));
      return { redirected: true };
    },
    async signOut() {
      window.location.assign("/oauth2/sign_out?rd=%2F");
      return { redirected: true };
    },
    async getAccessToken() {
      return account ? "proxy" : null;
    },
    getAccount() {
      return account;
    },
  };
}
`;

export const PORTAL_SPA = {
  entry: "index-BrOn9Elg.js",
  sha256: "25c9d041535f6801c2c9b38a4f8f3e6f995fed25563072022f6efbaaa476bebc",
  // The function that picks the browser half for the portal's provider.
  anchor: 'function vy(u){return!u||u==="none"?hy():',
  replacement: `${PROXY_BROWSER_AUTH}function vy(u){return!u||u==="none"?hy():u==="proxy"?pghProxyBrowserAuth():`,
};

const sha256 = (data) => createHash("sha256").update(data).digest("hex");
const occurrences = (text, needle) => text.split(needle).length - 1;

// Patches the SPA in an installed pilotswarm package, and returns the patched
// bundle's file name.
export function patchPortalSpa(packageDir, spa = PORTAL_SPA) {
  const dist = path.join(packageDir, "web", "dist");
  const indexFile = path.join(dist, "index.html");
  const entryFile = path.join(dist, "assets", spa.entry);
  if (!existsSync(entryFile)) {
    throw new Error(`${entryFile} doesn't exist: this isn't the PilotSwarm release the portal patch was written for, or it's patched already.`);
  }

  const source = readFileSync(entryFile);
  if (sha256(source) !== spa.sha256) {
    throw new Error(`${spa.entry} isn't the bundle the portal patch was written for (sha256 ${sha256(source)}); re-check the patch for this PilotSwarm release.`);
  }
  const original = source.toString("utf8");
  const anchors = occurrences(original, spa.anchor);
  if (anchors !== 1) throw new Error(`Expected one ${JSON.stringify(spa.anchor)} in ${spa.entry}, found ${anchors}.`);
  const code = original.replace(spa.anchor, () => spa.replacement);

  // /assets is served immutable for a year, so browsers that loaded the
  // original would keep it: the patched bundle takes a new name, which
  // index.html (served no-store) points them at.
  const reference = `"/assets/${spa.entry}"`;
  const html = readFileSync(indexFile, "utf8");
  const references = occurrences(html, reference);
  if (references !== 1) throw new Error(`Expected one ${reference} in index.html, found ${references}.`);
  const patched = `${path.basename(spa.entry, ".js")}-${sha256(code).slice(0, 8)}.js`;

  writeFileSync(path.join(dist, "assets", patched), code);
  writeFileSync(indexFile, html.replace(reference, () => `"/assets/${patched}"`));
  rmSync(entryFile);
  return patched;
}
