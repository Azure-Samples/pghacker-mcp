// The home page's server, and the local edge around it.
//
// createHomeHandler serves the built home page (home/dist): index.html, which
// browsers must check again on each visit since its asset names change with
// every build, the fingerprinted /static/* files for a year, and a few
// files of its own. It also sends the portal's share links, which PilotSwarm
// builds as /?session=... and /?canvasShare=..., on to the portal. In Azure
// it serves alone behind oauth2-proxy (scripts/home.mjs); locally, the edge
// puts it and the services on one origin with the routes of
// scripts/lib/routes.mjs, as oauth2-proxy does in Azure, without sign-in.
import { readFile } from "node:fs/promises";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import { APP_ROOT } from "./config.mjs";
import { PORTAL_PATH, ROUTES, matchRoute } from "./routes.mjs";

export const HOME_DIR = path.join(APP_ROOT, "home", "dist");

const TYPES = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
  ".woff2": "font/woff2",
};

export const CACHE = {
  page: "no-cache",
  static: "public, max-age=31536000, immutable",
  file: "public, max-age=3600",
  none: "no-store",
};

/** The page loads only its own files and is never framed. */
export const SECURITY_HEADERS = {
  "content-security-policy": [
    "default-src 'self'",
    "img-src 'self' data:",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
  ].join("; "),
  "x-content-type-options": "nosniff",
  "referrer-policy": "strict-origin-when-cross-origin",
  "cross-origin-opener-policy": "same-origin",
};

/** The files the page has at its root, besides index.html. */
const ROOT_FILES = new Set(["favicon.svg", "robots.txt"]);
/** A fingerprinted asset's name: Vite writes them flat, in static/. */
const STATIC_NAME = /^\/static\/([A-Za-z0-9][A-Za-z0-9._-]*)$/;
/** The query parameters of the portal's share links. */
const PORTAL_LINK_PARAMS = ["session", "canvasShare"];

const NOT_FOUND = `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Not found · pghacker-mcp</title></head>
<body><h1>Not found</h1><p><a href="/">pghacker-mcp's home page</a></p></body>
</html>
`;

function send(request, response, status, headers, body = "") {
  response.writeHead(status, { ...SECURITY_HEADERS, ...headers });
  response.end(request.method === "HEAD" ? undefined : body);
}

function parseUrl(url) {
  try {
    return new URL(url, "http://localhost");
  } catch {
    return undefined;
  }
}

/** A request handler for the home page in `root`, a directory Vite built. */
export function createHomeHandler({ root = HOME_DIR } = {}) {
  async function file(request, response, name, cacheControl) {
    const full = path.join(root, name);
    // Names are checked before they get here; this is a second guard.
    if (path.relative(root, full).startsWith("..")) return notFound(request, response);
    let body;
    try {
      body = await readFile(full);
    } catch (error) {
      if (error.code === "ENOENT" || error.code === "EISDIR") return notFound(request, response);
      throw error;
    }
    send(request, response, 200, {
      "content-type": TYPES[path.extname(name)] ?? "application/octet-stream",
      "content-length": body.length,
      "cache-control": cacheControl,
    }, body);
  }

  function notFound(request, response) {
    send(request, response, 404, { "content-type": TYPES[".html"], "cache-control": CACHE.page }, NOT_FOUND);
  }

  return async function home(request, response) {
    try {
      if (request.method !== "GET" && request.method !== "HEAD") {
        send(request, response, 405, { allow: "GET, HEAD", "content-type": TYPES[".txt"], "cache-control": CACHE.none }, "Method not allowed\n");
        return;
      }
      const url = parseUrl(request.url);
      if (!url) return notFound(request, response);
      const { pathname } = url;
      if (pathname === "/healthz") {
        send(request, response, 200, { "content-type": TYPES[".txt"], "cache-control": CACHE.none }, "ok\n");
      } else if (pathname === "/" && PORTAL_LINK_PARAMS.some((name) => url.searchParams.has(name))) {
        send(request, response, 302, { location: `${PORTAL_PATH}${url.search}`, "cache-control": CACHE.none });
      } else if (pathname === "/") {
        await file(request, response, "index.html", CACHE.page);
      } else if (pathname === "/index.html") {
        send(request, response, 301, { location: `/${url.search}`, "cache-control": CACHE.file });
      } else if (STATIC_NAME.test(pathname)) {
        await file(request, response, path.join("static", STATIC_NAME.exec(pathname)[1]), CACHE.static);
      } else if (ROOT_FILES.has(pathname.slice(1))) {
        await file(request, response, pathname.slice(1), CACHE.file);
      } else {
        notFound(request, response);
      }
    } catch (error) {
      console.error(`[home] ${request.method} ${request.url}: ${error.message}`);
      if (!response.headersSent) {
        send(request, response, 500, { "content-type": TYPES[".txt"], "cache-control": CACHE.none }, "Internal error\n");
      } else {
        response.destroy();
      }
    }
  };
}

function gateway(response, error) {
  if (response.headersSent) {
    response.destroy(error);
    return;
  }
  response.writeHead(502, { "content-type": TYPES[".txt"], "cache-control": CACHE.none });
  response.end("Bad gateway\n");
}

/** Passes `request` to `target` ({ host, port }) and its answer back, streamed both ways. */
function proxyRequest(request, response, target) {
  const upstream = http.request({
    host: target.host,
    port: target.port,
    method: request.method,
    path: request.url,
    headers: request.headers,
  }, (answer) => {
    response.writeHead(answer.statusCode, answer.statusMessage, answer.headers);
    answer.pipe(response);
    answer.once("error", (error) => response.destroy(error));
  });
  upstream.once("error", (error) => gateway(response, error));
  // A client that leaves stops the upstream request too, such as a stream.
  response.once("close", () => {
    if (!response.writableFinished) upstream.destroy();
  });
  request.pipe(upstream);
}

/** Passes an upgrade (a WebSocket) to `target` and splices the two sockets. */
function proxyUpgrade(request, socket, head, target) {
  const upstream = net.connect(target.port, target.host);
  // An orderly close on one side ends the other once its writes are done;
  // a failure on either destroys both.
  const destroy = () => {
    upstream.destroy();
    socket.destroy();
  };
  upstream.once("connect", () => {
    const lines = [`${request.method} ${request.url} HTTP/${request.httpVersion}`];
    for (let i = 0; i < request.rawHeaders.length; i += 2) {
      lines.push(`${request.rawHeaders[i]}: ${request.rawHeaders[i + 1]}`);
    }
    upstream.write(`${lines.join("\r\n")}\r\n\r\n`);
    if (head?.length) upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.once("error", destroy);
  socket.once("error", destroy);
  upstream.once("close", (failed) => (failed ? socket.destroy() : socket.end()));
  socket.once("close", (failed) => (failed ? upstream.destroy() : upstream.end()));
}

/**
 * The local edge: one server for the home page (`home`, a request handler)
 * and the services in `targets` ({ mcp: { host, port }, portal: ... }), routed
 * as oauth2-proxy routes them in Azure.
 */
export function createEdgeServer({ home, targets, routes = ROUTES }) {
  const server = http.createServer((request, response) => {
    const url = parseUrl(request.url);
    const route = url && matchRoute(url.pathname, routes);
    if (!route || route.target === "home") {
      home(request, response);
    } else {
      proxyRequest(request, response, targets[route.target]);
    }
  });
  server.on("upgrade", (request, socket, head) => {
    // Node takes its own error handler off an upgraded socket: without one,
    // a client that resets the connection would crash the process.
    socket.on("error", () => socket.destroy());
    const url = parseUrl(request.url);
    const route = url && matchRoute(url.pathname, routes);
    if (!route || route.target === "home" || !targets[route.target]) {
      socket.end("HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n");
      return;
    }
    proxyUpgrade(request, socket, head, targets[route.target]);
  });
  return server;
}
