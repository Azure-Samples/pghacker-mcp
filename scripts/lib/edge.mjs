// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The home page's server, and the local edge around it.
//
// createHomeHandler serves the built home page (home/dist): index.html, which
// browsers must check again on each visit since its asset names change with
// every build, the fingerprinted /static/* files for a year, and a few
// files of its own. It also sends the portal's share links, which PilotSwarm
// builds as /?session=... and /?canvasShare=..., on to the portal: in Azure,
// on the portal's own host, Front Door's, since a custom domain doesn't
// serve it (deploy/terraform/frontdoor.tf). Given star counts
// (scripts/lib/github-stars.mjs), it adds the GitHub repository's stars to
// the page it serves, once GitHub shows them; and, given the deployment's
// privacy statement and copyright link, the privacy-url and copyright-link
// tags, which the footer shows. Locally, npm start also gives it a probe of
// what the database has loaded, and it adds the data-status tag when the
// mail or the code is missing. In Azure it serves alone behind oauth2-proxy
// (scripts/home.mjs); locally, the edge puts it and the services on one origin
// with the routes of scripts/lib/routes.mjs, as oauth2-proxy does in Azure,
// without sign-in.
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

/**
 * The page loads only its own files, and HorizonDB's logo from Microsoft's
 * CDN (home/src/content.ts), and is never framed.
 */
export const SECURITY_HEADERS = {
  "content-security-policy": [
    "default-src 'self'",
    "img-src 'self' data: https://cdn-dynmedia-1.microsoft.com",
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

/** The page's link to its GitHub repository, owner/name, in index.html. */
const REPOSITORY_META = /<meta name="github-repo" content="([A-Za-z0-9-]+\/[A-Za-z0-9._-]+)"\s*\/?>/;

/**
 * `html` with its repository's star count as the github-repo meta tag's
 * data-stars, when `stars` knows it; otherwise `html` as it is, and the
 * page shows the link without a count.
 */
export function withStars(html, stars) {
  const meta = REPOSITORY_META.exec(html);
  if (!meta) return html;
  const count = stars.get(meta[1]);
  if (!Number.isSafeInteger(count) || count < 0) return html;
  return html.replace(meta[0], `<meta name="github-repo" content="${meta[1]}" data-stars="${count}" />`);
}

/** The longest privacy statement URL that a deployment may give. */
const MAX_PRIVACY_URL = 2048;

/**
 * `value` as the URL of a privacy statement, which the page's footer links
 * to: https, with a host, and without credentials, spaces, quotes, angle
 * brackets or backslashes. It returns `value` as it is, or undefined if it
 * isn't such a URL.
 */
export function parsePrivacyUrl(value) {
  if (typeof value !== "string" || value.length > MAX_PRIVACY_URL || !/^https:\/\/[^\s"'<>\\]+$/.test(value)) {
    return undefined;
  }
  const url = URL.parse(value);
  return url && url.hostname !== "" && url.username === "" && url.password === "" ? value : undefined;
}

/** The most characters that the text of the copyright link may have. */
const MAX_COPYRIGHT_TEXT = 100;

/**
 * `{ url, text }` as the footer's copyright link: `url` as parsePrivacyUrl
 * takes it, and `text`, what the link says, of 1 to 100 characters, not all
 * blank, without control characters. It returns the link as it is, or
 * undefined if it isn't one.
 */
export function parseCopyright(link) {
  const { url, text } = link ?? {};
  if (parsePrivacyUrl(url) === undefined || typeof text !== "string" || text.trim() === "") return undefined;
  if ([...text].length > MAX_COPYRIGHT_TEXT || /\p{Cc}/u.test(text)) return undefined;
  return { url, text };
}

/** The end of the page's head, where the footer's meta tags go. */
const HEAD_END = /<\/head>/i;

const ATTRIBUTE_ESCAPES = { "&": "&amp;", '"': "&quot;", "<": "&lt;", ">": "&gt;" };

/** `value`, for a double-quoted attribute. */
function escapeAttribute(value) {
  return value.replace(/[&"<>]/g, (character) => ATTRIBUTE_ESCAPES[character]);
}

/** `html` with `tag` at the end of its head; as it is, if it has no head. */
function atHeadEnd(html, tag) {
  return html.replace(HEAD_END, (end) => `${tag}${end}`);
}

/**
 * `html` with `privacy`, a URL that parsePrivacyUrl took, as a privacy-url
 * meta tag at the end of its head, which the footer's link reads. Without a
 * URL, `html` as it is, and the page shows no link.
 */
export function withPrivacy(html, privacy) {
  if (!privacy) return html;
  return atHeadEnd(html, `<meta name="privacy-url" content="${escapeAttribute(privacy)}" />`);
}

/**
 * `html` with `copyright`, a link that parseCopyright took, as a
 * copyright-link meta tag at the end of its head: the link's URL as its
 * content and its text as data-text, which the footer's link reads. Without
 * a link, `html` as it is, and the page shows none.
 */
export function withCopyright(html, copyright) {
  if (!copyright?.url || !copyright.text) return html;
  const tag = `<meta name="copyright-link" content="${escapeAttribute(copyright.url)}" data-text="${escapeAttribute(copyright.text)}" />`;
  return atHeadEnd(html, tag);
}

/**
 * `html` with `status`, what the local database has loaded when it isn't
 * everything (scripts/lib/data.mjs: "empty", "mail" for only the mail, or
 * "code" for only the code), as a data-status meta tag at the end of its
 * head, which the MCP server's panel reads (home/src/data-status.ts).
 * Anything else, such as "ready", leaves `html` as it is.
 */
export function withDataStatus(html, status) {
  if (!DATA_STATUSES.has(status)) return html;
  return atHeadEnd(html, `<meta name="data-status" content="${status}" />`);
}

const DATA_STATUSES = new Set(["empty", "mail", "code"]);
/** How long a data status is reused, and how long the page waits for one. */
const DATA_STATUS_TTL_MS = 5000;
const DATA_STATUS_WAIT_MS = 3000;

/**
 * A request handler for the home page in `root`, a directory Vite built.
 * `portal` is the origin, such as https://name.azurefd.net, whose /portal/
 * the portal's share links go on to; without it, they stay on the request's
 * own origin, as on the local edge. `privacy` is the deployment's privacy
 * statement, an https URL, and `copyright` its copyright link, `{ url, text }`
 * (parseCopyright); the footer shows each that is given, the privacy link
 * first, and has no such link without it. `onPage(request)` hears of each
 * page served to a GET, for usage telemetry. `stars`, from createStarCounts,
 * adds the repository's star count to the page. `dataStatus`, given only
 * locally by npm start, is an async probe of what the database has loaded
 * (withDataStatus); its answer is reused for a few seconds, and a probe that
 * fails or takes too long adds nothing.
 */
export function createHomeHandler({ root = HOME_DIR, portal = "", privacy = "", copyright, onPage, stars, dataStatus } = {}) {
  if (portal && URL.parse(portal)?.origin !== portal) {
    throw new Error(`The portal must be given as an origin, such as https://name.azurefd.net, not ${portal}.`);
  }
  if (privacy && parsePrivacyUrl(privacy) === undefined) {
    throw new Error(`The privacy statement must be given as an https URL, such as https://example.com/privacy, not ${privacy}.`);
  }
  const copyrightGiven = Boolean(copyright?.url || copyright?.text);
  if (copyrightGiven && parseCopyright(copyright) === undefined) {
    throw new Error(
      `The copyright link must be given as an https URL, such as https://example.com/, and its text, such as © 2026 Example, of at most ${MAX_COPYRIGHT_TEXT} characters without control characters.`,
    );
  }
  let probed;
  let probing;
  /** The data status, from the cache or a new probe; undefined if the probe failed. */
  function currentDataStatus() {
    if (probed && Date.now() - probed.at < DATA_STATUS_TTL_MS) return Promise.resolve(probed.value);
    probing ??= (async () => {
      let timer;
      let value;
      try {
        value = await Promise.race([
          dataStatus(),
          new Promise((resolve) => {
            timer = setTimeout(resolve, DATA_STATUS_WAIT_MS);
          }),
        ]);
      } catch {
        value = undefined;
      } finally {
        clearTimeout(timer);
      }
      probed = { at: Date.now(), value };
      probing = undefined;
      return value;
    })();
    return probing;
  }

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
    if (name === "index.html" && (stars || privacy || copyrightGiven || dataStatus)) {
      const html = stars ? withStars(body.toString("utf8"), stars) : body.toString("utf8");
      const status = dataStatus ? await currentDataStatus() : undefined;
      body = Buffer.from(withDataStatus(withCopyright(withPrivacy(html, privacy), copyright), status));
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
        send(request, response, 302, { location: `${portal}${PORTAL_PATH}${url.search}`, "cache-control": CACHE.none });
      } else if (pathname === "/") {
        await file(request, response, "index.html", CACHE.page);
        if (onPage && request.method === "GET" && response.statusCode === 200) {
          try {
            onPage(request);
          } catch (error) {
            console.error(`[home] usage telemetry: ${error.message}`);
          }
        }
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
