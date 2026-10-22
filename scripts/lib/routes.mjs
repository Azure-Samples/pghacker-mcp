// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The site's routes, one table for both front ends: oauth2-proxy in Azure
// (scripts/lib/azure.mjs makes its --upstream, --skip-auth-route and
// --api-route flags from it) and the local edge (scripts/lib/edge.mjs).
//
//   /mcp       pghacker-mcp, public
//   /portal    the PilotSwarm portal, after GitHub sign-in in Azure. PilotSwarm
//              0.8.5 has no base path (microsoft/PilotSwarm#179): its page
//              loads at /portal/, but it asks for /assets/* and /api/*
//              (WebSockets included) at the root, so those go to it too.
//   /          everything else: the home page. Its own files are public; any
//              other path needs sign-in in Azure, as before.
//
// Matching is oauth2-proxy's: the longest path wins, and a path ending in "/"
// matches every path below it, any other only itself. oauth2-proxy's own
// redirect from /portal to /portal/ never happens with a "/" route, which
// matches first, so /portal has its own route.

/** Where requests go: pghacker-mcp, the PilotSwarm portal or the home page. */
export const TARGETS = ["mcp", "portal", "home"];

export const ROUTES = [
  { path: "/mcp", target: "mcp" },
  { path: "/portal", target: "portal" },
  { path: "/portal/", target: "portal" },
  { path: "/api/", target: "portal" },
  { path: "/assets/", target: "portal" },
  { path: "/", target: "home" },
];

/**
 * What anyone may request without signing in, as oauth2-proxy's skip-auth
 * routes: METHOD=regex, or a regex for every method. oauth2-proxy matches the
 * request's own decoded path, without the query, and trusts no forwarded
 * header (scripts/lib/azure.mjs). It answers /robots.txt itself, before
 * any route, with "Disallow: /"; the home page's robots.txt says the same.
 * A custom domain serves only these paths: keep Front Door's public_paths
 * (deploy/terraform/frontdoor.tf) in step.
 */
export const PUBLIC_ROUTES = [
  "^/mcp$",
  ...["GET", "HEAD"].flatMap((method) => [
    `${method}=^/$`,
    `${method}=^/static/`,
    `${method}=^/favicon\\.svg$`,
  ]),
];

/** Paths where oauth2-proxy answers 401 instead of sending the browser to sign in. */
export const API_ROUTE = "^/api/";

/** What the page links to after sign-in: the portal. */
export const PORTAL_PATH = "/portal/";

/** The route `pathname` takes, or undefined. */
export function matchRoute(pathname, routes = ROUTES) {
  let best;
  for (const route of routes) {
    const matches = route.path.endsWith("/") ? pathname.startsWith(route.path) : pathname === route.path;
    if (matches && (!best || route.path.length > best.path.length)) best = route;
  }
  return best;
}

/** Whether oauth2-proxy lets `method` on `pathname` through without sign-in. */
export function isPublic(method, pathname, publicRoutes = PUBLIC_ROUTES) {
  return publicRoutes.some((route) => {
    const separator = route.indexOf("=");
    const [routeMethod, pattern] = /^[A-Z]+=/.test(route)
      ? [route.slice(0, separator), route.slice(separator + 1)]
      : [undefined, route];
    return (routeMethod === undefined || routeMethod === method) && new RegExp(pattern).test(pathname);
  });
}

/**
 * oauth2-proxy's routing flags, for `origins`: each target's http origin,
 * such as { mcp: "http://127.0.0.1:3102", ... }. oauth2-proxy uses an
 * upstream URL's path only to route, and passes the request's own path on.
 */
export function oauth2ProxyRouteArgs(origins, routes = ROUTES) {
  for (const target of TARGETS) {
    if (!/^http:\/\/[^/]+$/.test(origins[target] ?? "")) throw new Error(`No http origin for ${target}.`);
  }
  return [
    ...routes.map((route) => `--upstream=${origins[route.target]}${route.path}`),
    ...PUBLIC_ROUTES.map((route) => `--skip-auth-route=${route}`),
    `--api-route=${API_ROUTE}`,
  ];
}
