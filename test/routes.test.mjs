import assert from "node:assert/strict";
import test from "node:test";
import {
  API_ROUTE, PUBLIC_ROUTES, ROUTES, TARGETS, isPublic, matchRoute, oauth2ProxyRouteArgs,
} from "../scripts/lib/routes.mjs";

const ORIGINS = { mcp: "http://127.0.0.1:3102", portal: "http://127.0.0.1:3001", home: "http://127.0.0.1:3104" };

/**
 * oauth2-proxy 7's own choice of upstream (pkg/upstream/proxy.go): its
 * upstreams sorted longest path first, a path ending in "/" matching every
 * path below it and any other only itself, the first match winning.
 */
function oauth2ProxyUpstream(args, pathname) {
  return args
    .filter((arg) => arg.startsWith("--upstream="))
    .map((arg) => new URL(arg.slice("--upstream=".length)))
    .sort((a, b) => b.pathname.length - a.pathname.length)
    .find((url) => (url.pathname.endsWith("/") ? pathname.startsWith(url.pathname) : pathname === url.pathname));
}

// A path, where it goes, and whether anyone may GET it without signing in.
const CASES = [
  ["/", "home", true],
  ["/static/index-BVRmR80n.js", "home", true],
  ["/favicon.svg", "home", true],
  ["/index.html", "home", false],
  ["/no-such-page", "home", false],
  ["/static", "home", false],
  ["/mcp", "mcp", true],
  ["/mcp/", "home", false],
  ["/portal", "portal", false],
  ["/portal/", "portal", false],
  ["/portal/sessions", "portal", false],
  ["/portalx", "home", false],
  ["/api/v1/ws", "portal", false],
  ["/api/portal-config", "portal", false],
  ["/api/auth/me", "portal", false],
  ["/assets/index-BrOn9Elg.js", "portal", false],
  ["/portal-ws", "home", false],
  ["/oauth2x", "home", false],
];

test("each path goes where oauth2-proxy sends it, and the edge sends it the same way", () => {
  const args = oauth2ProxyRouteArgs(ORIGINS);
  for (const [pathname, target] of CASES) {
    assert.equal(matchRoute(pathname)?.target, target, pathname);
    assert.equal(oauth2ProxyUpstream(args, pathname)?.origin, ORIGINS[target], pathname);
  }
  assert.deepEqual([...new Set(ROUTES.map((route) => route.target))].sort(), [...TARGETS].sort());
});

test("only /mcp and the home page's own files are public", () => {
  for (const [pathname, , open] of CASES) assert.equal(isPublic("GET", pathname), open, pathname);
  assert.equal(isPublic("HEAD", "/"), true);
  assert.equal(isPublic("HEAD", "/static/x.css"), true);
  for (const method of ["POST", "PUT", "DELETE"]) {
    assert.equal(isPublic(method, "/"), false, method);
    assert.equal(isPublic(method, "/mcp"), true, method);
  }
  assert.equal(isPublic("GET", "/portal/static/x.js"), false);
  assert.equal(isPublic("GET", "/api/static/x"), false);
});

test("oauth2-proxy gets an upstream per route, the public routes and the API route", () => {
  const args = oauth2ProxyRouteArgs(ORIGINS);
  assert.deepEqual(args.filter((arg) => arg.startsWith("--upstream=")), [
    "--upstream=http://127.0.0.1:3102/mcp",
    "--upstream=http://127.0.0.1:3001/portal",
    "--upstream=http://127.0.0.1:3001/portal/",
    "--upstream=http://127.0.0.1:3001/api/",
    "--upstream=http://127.0.0.1:3001/assets/",
    "--upstream=http://127.0.0.1:3104/",
  ]);
  assert.deepEqual(args.filter((arg) => arg.startsWith("--skip-auth-route=")), PUBLIC_ROUTES.map((route) => `--skip-auth-route=${route}`));
  assert.ok(args.includes(`--api-route=${API_ROUTE}`));
  // oauth2-proxy names each upstream by its path: they must differ.
  assert.equal(new Set(ROUTES.map((route) => route.path)).size, ROUTES.length);
});

test("public routes are plain regular expressions, which Go's regexp accepts", () => {
  for (const route of PUBLIC_ROUTES) {
    const pattern = route.replace(/^[A-Z]+=/, "");
    assert.doesNotMatch(pattern, /\(\?[=!<]/, `${route} uses a lookaround`);
    assert.match(pattern, /^\^/, `${route} isn't anchored`);
    assert.doesNotThrow(() => new RegExp(pattern), route);
  }
});

test("oauth2-proxy's origins must be plain http origins", () => {
  assert.throws(() => oauth2ProxyRouteArgs({ ...ORIGINS, home: undefined }), /home/);
  assert.throws(() => oauth2ProxyRouteArgs({ ...ORIGINS, portal: "http://127.0.0.1:3001/" }), /portal/);
  assert.throws(() => oauth2ProxyRouteArgs({ ...ORIGINS, mcp: "https://127.0.0.1:3102" }), /mcp/);
});
