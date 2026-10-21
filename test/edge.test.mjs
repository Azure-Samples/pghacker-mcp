// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { POWERED_BY } from "../home/src/content.ts";
import {
  CACHE, SECURITY_HEADERS, createEdgeServer, createHomeHandler, parseCopyright, parsePrivacyUrl, withCopyright, withDataStatus,
  withPrivacy, withStars,
} from "../scripts/lib/edge.mjs";

const PAGE = '<!doctype html><meta name="application-name" content="pghacker-mcp"><script type="module" src="/static/index-abc.js"></script>';

/** A built home page, as Vite writes it. */
function distFixture(t) {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-home-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  mkdirSync(path.join(dir, "static"));
  writeFileSync(path.join(dir, "index.html"), PAGE);
  writeFileSync(path.join(dir, "static", "index-abc.js"), "console.log(1);\n");
  writeFileSync(path.join(dir, "favicon.svg"), "<svg xmlns='http://www.w3.org/2000/svg'/>");
  writeFileSync(path.join(dir, "robots.txt"), "User-agent: *\n");
  return dir;
}

async function listen(t, server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => {
    server.closeAllConnections?.();
    server.close();
  });
  return server.address().port;
}

/** A stand-in for the portal or pghacker-mcp: it says what it got. */
async function fakeUpstream(t, name) {
  const requests = [];
  const server = http.createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    requests.push({ method: request.method, url: request.url, body: Buffer.concat(chunks).toString() });
    if (request.url.endsWith("/stream")) {
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write("data: one\n\n");
      setTimeout(() => response.end("data: two\n\n"), 300);
      return;
    }
    response.writeHead(200, { "content-type": "application/json", "x-upstream": name, "set-cookie": ["a=1", "b=2"] });
    response.end(JSON.stringify({ name, url: request.url }));
  });
  server.on("upgrade", (request, socket) => {
    requests.push({ upgrade: request.url, headers: request.headers, closed: once(socket, "close") });
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    socket.on("data", (data) => socket.write(data));
    // As a WebSocket server does: the client's close ends the connection.
    socket.on("end", () => socket.end());
  });
  const port = await listen(t, server);
  return { requests, target: { host: "127.0.0.1", port } };
}

async function siteFixture(t) {
  const portal = await fakeUpstream(t, "portal");
  const mcp = await fakeUpstream(t, "mcp");
  const edge = createEdgeServer({
    home: createHomeHandler({ root: distFixture(t) }),
    targets: { portal: portal.target, mcp: mcp.target },
  });
  const port = await listen(t, edge);
  return { base: `http://127.0.0.1:${port}`, port, portal, mcp };
}

/** A request with exactly this path, which fetch would normalize. */
function rawGet(port, rawPath) {
  return new Promise((resolve, reject) => {
    http.get({ host: "127.0.0.1", port, path: rawPath }, (response) => {
      response.resume();
      response.once("end", () => resolve(response.statusCode));
    }).once("error", reject);
  });
}

test("the home page is checked again on each visit, under a strict policy", async (t) => {
  const { base } = await siteFixture(t);
  const page = await fetch(`${base}/`);
  assert.equal(page.status, 200);
  assert.equal(page.headers.get("content-type"), "text/html; charset=utf-8");
  assert.equal(page.headers.get("cache-control"), CACHE.page);
  assert.match(page.headers.get("content-security-policy"), /default-src 'self'.*frame-ancestors 'none'/);
  assert.equal(page.headers.get("x-content-type-options"), "nosniff");
  assert.equal(await page.text(), PAGE);

  const head = await fetch(`${base}/`, { method: "HEAD" });
  assert.equal(head.status, 200);
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(PAGE)));
  assert.equal(await head.text(), "");
});

test("images may come from HorizonDB's logo's host, and from no other", () => {
  const imgSrc = SECURITY_HEADERS["content-security-policy"]
    .split("; ")
    .find((directive) => directive.startsWith("img-src "))
    .split(" ")
    .slice(1);
  assert.equal(new URL(POWERED_BY.logo).protocol, "https:");
  assert.deepEqual(imgSrc.filter((source) => source.includes("://")), [new URL(POWERED_BY.logo).origin]);
});

test("the page's views are heard, for usage telemetry: GETs of the page alone", async (t) => {
  const heard = [];
  const home = createHomeHandler({ root: distFixture(t), onPage: (request) => heard.push(request.url) });
  const port = await listen(t, http.createServer(home));
  const base = `http://127.0.0.1:${port}`;
  for (const [path, method] of [
    ["/?utm_source=newsletter", "GET"], ["/", "HEAD"], ["/static/index-abc.js", "GET"], ["/favicon.svg", "GET"],
    ["/nope", "GET"], ["/?session=abc", "GET"], ["/index.html", "GET"], ["/healthz", "GET"],
  ]) {
    const response = await fetch(`${base}${path}`, { method, redirect: "manual" });
    await response.arrayBuffer();
  }
  assert.deepEqual(heard, ["/?utm_source=newsletter"]);

  // A recorder that fails doesn't fail the page.
  const failing = createHomeHandler({ root: distFixture(t), onPage: () => { throw new Error("broken"); } });
  const failingPort = await listen(t, http.createServer(failing));
  const page = await fetch(`http://127.0.0.1:${failingPort}/`);
  assert.equal(page.status, 200);
  assert.equal(await page.text(), PAGE);
});

test("given star counts, the page carries its repository's stars once GitHub shows them", async (t) => {
  const root = distFixture(t);
  const page = (repo) => `<!doctype html><head>${repo}<meta name="application-name" content="pghacker-mcp"></head>`;
  const tag = '<meta name="github-repo" content="Azure-Samples/pghacker-mcp" />';
  writeFileSync(path.join(root, "index.html"), page(tag));
  const asked = [];
  const counts = new Map();
  const stars = { get: (repository) => (asked.push(repository), counts.get(repository)) };
  const port = await listen(t, http.createServer(createHomeHandler({ root, stars })));
  const base = `http://127.0.0.1:${port}`;

  // Before GitHub answers, and while the repository is internal: the link alone.
  assert.equal(await (await fetch(`${base}/`)).text(), page(tag));

  counts.set("Azure-Samples/pghacker-mcp", 1234);
  const counted = page('<meta name="github-repo" content="Azure-Samples/pghacker-mcp" data-stars="1234" />');
  assert.equal(await (await fetch(`${base}/`)).text(), counted);
  const head = await fetch(`${base}/`, { method: "HEAD" });
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(counted)));
  assert.deepEqual([...new Set(asked)], ["Azure-Samples/pghacker-mcp"]);
  assert.equal(await (await fetch(`${base}/static/index-abc.js`)).text(), "console.log(1);\n");
});

test("a count goes only into a well-formed github-repo tag, and only a whole one", () => {
  const seven = { get: () => 7 };
  const tag = '<meta name="github-repo" content="o/r">';
  assert.equal(withStars(`<head>${tag}</head>`, seven), '<head><meta name="github-repo" content="o/r" data-stars="7" /></head>');
  assert.equal(withStars("<head></head>", seven), "<head></head>");
  for (const count of [undefined, -1, 1.5, "12", Number.NaN]) {
    assert.equal(withStars(`<head>${tag}</head>`, { get: () => count }), `<head>${tag}</head>`, String(count));
  }
  const odd = '<meta name="github-repo" content="o/r/x">';
  assert.equal(withStars(odd, seven), odd);
});

test("given the deployment's privacy statement, the page carries it for its footer; without one, it carries nothing", async (t) => {
  const root = distFixture(t);
  const page = (head) => `<!doctype html><head><meta name="application-name" content="pghacker-mcp">${head}</head><body></body>`;
  writeFileSync(path.join(root, "index.html"), page(""));
  const privacy = "https://example.com/privacy?lang=en&page=1";
  const tagged = page(`<meta name="privacy-url" content="https://example.com/privacy?lang=en&amp;page=1" />`);

  const port = await listen(t, http.createServer(createHomeHandler({ root, privacy })));
  const base = `http://127.0.0.1:${port}`;
  assert.equal(await (await fetch(`${base}/`)).text(), tagged);
  const head = await fetch(`${base}/`, { method: "HEAD" });
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(tagged)));
  // The page alone: not its files.
  assert.equal(await (await fetch(`${base}/static/index-abc.js`)).text(), "console.log(1);\n");

  // Unset or empty: the page as it was built.
  for (const options of [{}, { privacy: "" }]) {
    const barePort = await listen(t, http.createServer(createHomeHandler({ root, ...options })));
    assert.equal(await (await fetch(`http://127.0.0.1:${barePort}/`)).text(), page(""));
  }

  // With the repository's stars too, both go in.
  writeFileSync(path.join(root, "index.html"), page('<meta name="github-repo" content="o/r" />'));
  const counted = createHomeHandler({ root, privacy, stars: { get: () => 5 } });
  const countedPort = await listen(t, http.createServer(counted));
  assert.equal(
    await (await fetch(`http://127.0.0.1:${countedPort}/`)).text(),
    page(`<meta name="github-repo" content="o/r" data-stars="5" /><meta name="privacy-url" content="https://example.com/privacy?lang=en&amp;page=1" />`),
  );
});

test("a privacy statement is an https URL with a host: anything else is refused, not shown", () => {
  for (const good of [
    "https://example.com/privacy",
    "https://example.com",
    "https://privacy.example.com:8443/en-us/privacy?a=1&b=2#top",
  ]) {
    assert.equal(parsePrivacyUrl(good), good);
    assert.doesNotThrow(() => createHomeHandler({ privacy: good }), good);
  }
  for (const bad of [
    "http://example.com/privacy", "HTTPS://example.com/", "javascript:alert(1)", "//example.com/privacy", "/privacy",
    "example.com/privacy", "https://", "https://:8443/", "https://user:secret@example.com/", "https://example.com/a b",
    'https://example.com/"', "https://example.com/'", "https://example.com/<b>", "https://example.com/\\x",
    `https://example.com/${"a".repeat(2048)}`, 5, {},
  ]) {
    assert.equal(parsePrivacyUrl(bad), undefined, String(bad));
  }
  assert.throws(() => createHomeHandler({ privacy: "http://example.com/privacy" }), /privacy statement must be given as an https URL/);
  assert.throws(() => createHomeHandler({ privacy: "javascript:alert(1)" }), /privacy statement/);
});

test("the privacy tag goes before the end of the head, once, with the URL escaped and as it is", () => {
  const tag = (content) => `<meta name="privacy-url" content="${content}" />`;
  assert.equal(withPrivacy("<head><title>x</title></head>", "https://example.com/p"), `<head><title>x</title>${tag("https://example.com/p")}</head>`);
  // Replacement patterns in a URL, such as $&, stay as they are.
  assert.equal(withPrivacy("<head></head>", "https://example.com/$&x$1"), `<head>${tag("https://example.com/$&amp;x$1")}</head>`);
  assert.equal(withPrivacy("<HEAD></HEAD>", "https://example.com/p"), `<HEAD>${tag("https://example.com/p")}</HEAD>`);
  assert.equal(withPrivacy("<head></head><head></head>", "https://example.com/p").split("privacy-url").length, 2);
  // Nothing to say, or nowhere to say it: the page as it is.
  assert.equal(withPrivacy("<head></head>", ""), "<head></head>");
  assert.equal(withPrivacy("<head></head>", undefined), "<head></head>");
  assert.equal(withPrivacy("<p>no head</p>", "https://example.com/p"), "<p>no head</p>");
});

test("the page's own index.html takes the footer's tags, once each and in order, and has none of its own", () => {
  const html = readFileSync(new URL("../home/index.html", import.meta.url), "utf8");
  const names = ["privacy-url", "copyright-link"];
  for (const name of names) assert.ok(!new RegExp(`<meta[^>]+name="${name}"`).test(html), name);
  const tagged = withCopyright(withPrivacy(html, "https://example.com/privacy"), { url: "https://example.com/", text: "© 2026 Example" });
  for (const name of names) assert.equal(tagged.match(new RegExp(`<meta name="${name}"`, "g")).length, 1, name);
  assert.ok(tagged.indexOf('name="privacy-url"') < tagged.indexOf('name="copyright-link"'));
  assert.ok(tagged.indexOf('name="copyright-link"') < tagged.indexOf("</head>"));
});

test("given the deployment's copyright link, the page carries it after its privacy statement; without one, it carries nothing", async (t) => {
  const root = distFixture(t);
  const page = (head) => `<!doctype html><head><meta name="application-name" content="pghacker-mcp">${head}</head><body></body>`;
  writeFileSync(path.join(root, "index.html"), page(""));
  const copyright = { url: "https://example.com/?a=1&b=2", text: "© 2026 Example & Sons" };
  const copyrightTag = '<meta name="copyright-link" content="https://example.com/?a=1&amp;b=2" data-text="© 2026 Example &amp; Sons" />';
  const privacyTag = '<meta name="privacy-url" content="https://example.com/privacy" />';
  const serve = async (options) => `http://127.0.0.1:${await listen(t, http.createServer(createHomeHandler({ root, ...options })))}`;

  const only = await serve({ copyright });
  assert.equal(await (await fetch(`${only}/`)).text(), page(copyrightTag));
  // Its length is the bytes' (the © takes two), not the characters'.
  const head = await fetch(`${only}/`, { method: "HEAD" });
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(page(copyrightTag))));
  assert.equal(await (await fetch(`${only}/static/index-abc.js`)).text(), "console.log(1);\n");

  const both = await serve({ privacy: "https://example.com/privacy", copyright });
  assert.equal(await (await fetch(`${both}/`)).text(), page(privacyTag + copyrightTag));

  // Unset or empty: the page as it was built.
  for (const options of [{}, { copyright: undefined }, { copyright: { url: "", text: "" } }]) {
    assert.equal(await (await fetch(`${await serve(options)}/`)).text(), page(""), JSON.stringify(options));
  }

  // With the repository's stars too, all three go in.
  writeFileSync(path.join(root, "index.html"), page('<meta name="github-repo" content="o/r" />'));
  const counted = await serve({ privacy: "https://example.com/privacy", copyright, stars: { get: () => 5 } });
  assert.equal(
    await (await fetch(`${counted}/`)).text(),
    page(`<meta name="github-repo" content="o/r" data-stars="5" />${privacyTag}${copyrightTag}`),
  );
});

test("a copyright link is an https URL with its text: anything else is refused, not shown", () => {
  const good = { url: "https://example.com/", text: "© 2026 Example" };
  assert.deepEqual(parseCopyright(good), good);
  assert.doesNotThrow(() => createHomeHandler({ copyright: good }));
  // Up to 100 characters, counted as characters: 100 emoji are 200 UTF-16 units.
  for (const text of ["x".repeat(100), "😀".repeat(100)]) assert.notEqual(parseCopyright({ url: good.url, text }), undefined);
  for (const bad of [
    undefined, null, {}, { url: good.url }, { text: good.text }, { url: "http://example.com/", text: "x" },
    { url: "javascript:alert(1)", text: "x" }, { url: "https://user:secret@example.com/", text: "x" },
    { url: good.url, text: "" }, { url: good.url, text: "   " }, { url: good.url, text: "x\ny" },
    { url: good.url, text: "x\u0000y" }, { url: good.url, text: "x".repeat(101) }, { url: good.url, text: "😀".repeat(101) },
    { url: good.url, text: 5 },
  ]) {
    assert.equal(parseCopyright(bad), undefined, JSON.stringify(bad));
  }
  // Given in part, or not valid, it is an error, not a silent omission; given as nothing, it is none.
  for (const bad of [{ url: good.url }, { text: good.text }, { url: "http://example.com/", text: "x" }, { url: good.url, text: "x\ny" }]) {
    assert.throws(() => createHomeHandler({ copyright: bad }), /copyright link must be given as an https URL/, JSON.stringify(bad));
  }
  for (const none of [undefined, {}, { url: "", text: "" }]) assert.doesNotThrow(() => createHomeHandler({ copyright: none }));
});

test("the copyright tag goes before the end of the head, with its URL and text escaped and as they are", () => {
  const link = { url: "https://example.com/$&", text: '© $& "2026" <Example>' };
  assert.equal(
    withCopyright("<head></head>", link),
    '<head><meta name="copyright-link" content="https://example.com/$&amp;" data-text="© $&amp; &quot;2026&quot; &lt;Example&gt;" /></head>',
  );
  // Nothing to say, or nowhere to say it: the page as it is.
  for (const none of [undefined, null, {}, { url: "https://example.com/" }, { text: "x" }, { url: "", text: "" }]) {
    assert.equal(withCopyright("<head></head>", none), "<head></head>", JSON.stringify(none));
  }
  assert.equal(withCopyright("<p>no head</p>", link), "<p>no head</p>");
});

test("given a data probe, as npm start gives it, the page says what's loaded when something is missing", async (t) => {
  const root = distFixture(t);
  const page = (head) => `<!doctype html><head><meta name="application-name" content="pghacker-mcp">${head}</head><body></body>`;
  writeFileSync(path.join(root, "index.html"), page(""));
  let status = "empty";
  let probes = 0;
  const dataStatus = async () => {
    probes += 1;
    return status;
  };
  const port = await listen(t, http.createServer(createHomeHandler({ root, dataStatus })));
  const base = `http://127.0.0.1:${port}`;
  const tagged = page('<meta name="data-status" content="empty" />');
  assert.equal(await (await fetch(`${base}/`)).text(), tagged);
  const head = await fetch(`${base}/`, { method: "HEAD" });
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(tagged)));
  // The answer is reused for a few seconds: one probe for both requests.
  assert.equal(probes, 1);
  // Not the page's files.
  assert.equal(await (await fetch(`${base}/static/index-abc.js`)).text(), "console.log(1);\n");

  // Everything loaded: the page as it was built, as in Azure, which has no probe.
  status = "ready";
  const ready = await listen(t, http.createServer(createHomeHandler({ root, dataStatus })));
  assert.equal(await (await fetch(`http://127.0.0.1:${ready}/`)).text(), page(""));
  const azure = await listen(t, http.createServer(createHomeHandler({ root })));
  assert.equal(await (await fetch(`http://127.0.0.1:${azure}/`)).text(), page(""));

  // A probe that fails adds nothing, and doesn't fail the page.
  const failing = await listen(t, http.createServer(createHomeHandler({ root, dataStatus: async () => { throw new Error("down"); } })));
  const response = await fetch(`http://127.0.0.1:${failing}/`);
  assert.equal(response.status, 200);
  assert.equal(await response.text(), page(""));
});

test("the data-status tag says only empty, mail or code, before the end of the head", () => {
  for (const status of ["empty", "mail", "code"]) {
    assert.equal(withDataStatus("<head></head>", status), `<head><meta name="data-status" content="${status}" /></head>`);
  }
  for (const none of [undefined, null, "ready", "", '"><script>', "EMPTY"]) {
    assert.equal(withDataStatus("<head></head>", none), "<head></head>", String(none));
  }
  assert.equal(withDataStatus("<p>no head</p>", "empty"), "<p>no head</p>");
});

test("fingerprinted files are kept a year, the page's other files an hour", async (t) => {
  const { base } = await siteFixture(t);
  const script = await fetch(`${base}/static/index-abc.js`);
  assert.equal(script.status, 200);
  assert.equal(script.headers.get("content-type"), "text/javascript; charset=utf-8");
  assert.equal(script.headers.get("cache-control"), "public, max-age=31536000, immutable");
  const icon = await fetch(`${base}/favicon.svg`);
  assert.equal(icon.headers.get("content-type"), "image/svg+xml");
  assert.equal(icon.headers.get("cache-control"), "public, max-age=3600");
  assert.equal((await fetch(`${base}/robots.txt`)).headers.get("content-type"), "text/plain; charset=utf-8");
});

test("the portal's share links go on to the portal, with their query", async (t) => {
  const { base } = await siteFixture(t);
  for (const query of ["?session=abc&view=canvas&slot=2", "?canvasShare=token-1"]) {
    const link = await fetch(`${base}/${query}`, { redirect: "manual" });
    assert.equal(link.status, 302, query);
    assert.equal(link.headers.get("location"), `/portal/${query}`);
  }
  assert.equal((await fetch(`${base}/?utm_source=mail`)).status, 200);
});

test("given the portal's origin, as in Azure, share links go on to the portal there, from any host", async (t) => {
  // A custom domain serves only the home page and /mcp: Front Door's host name serves the portal.
  const portal = "https://pghacker-example.z01.azurefd.net";
  const port = await listen(t, http.createServer(createHomeHandler({ root: distFixture(t), portal })));
  for (const query of ["?session=abc&view=canvas&slot=2", "?canvasShare=token-1"]) {
    const link = await fetch(`http://127.0.0.1:${port}/${query}`, { redirect: "manual", headers: { host: "mcp.example.org" } });
    assert.equal(link.status, 302, query);
    assert.equal(link.headers.get("location"), `${portal}/portal/${query}`);
  }
  // The page's own redirect stays on the request's host.
  const index = await fetch(`http://127.0.0.1:${port}/index.html`, { redirect: "manual" });
  assert.equal(index.headers.get("location"), "/");
  for (const bad of [`${portal}/`, `${portal}/portal/`, "pghacker-example.z01.azurefd.net", "file:///tmp"]) {
    assert.throws(() => createHomeHandler({ portal: bad }), /must be given as an origin/, bad);
  }
});

test("anything else is a 404; other methods a 405; the health check answers", async (t) => {
  const { base, port } = await siteFixture(t);
  const missing = await fetch(`${base}/no-such-page`);
  assert.equal(missing.status, 404);
  assert.equal(missing.headers.get("cache-control"), CACHE.page);
  assert.equal((await fetch(`${base}/static/missing.js`)).status, 404);
  assert.equal((await fetch(`${base}/static/nested/index-abc.js`)).status, 404);
  const index = await fetch(`${base}/index.html`, { redirect: "manual" });
  assert.equal(index.status, 301);
  assert.equal(index.headers.get("location"), "/");
  for (const raw of ["/static/..%2Findex.html", "/static/..%2F..%2F..%2Fetc%2Fpasswd", "/..%2F..%2Fetc%2Fpasswd"]) {
    assert.equal(await rawGet(port, raw), 404, raw);
  }
  const post = await fetch(`${base}/`, { method: "POST", body: "x" });
  assert.equal(post.status, 405);
  assert.equal(post.headers.get("allow"), "GET, HEAD");
  const health = await fetch(`${base}/healthz`);
  assert.equal(health.status, 200);
  assert.equal(await health.text(), "ok\n");
});

test("without a built page, the home page is a 404, not an error", async (t) => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-home-empty-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const port = await listen(t, http.createServer(createHomeHandler({ root: path.join(dir, "dist") })));
  assert.equal((await fetch(`http://127.0.0.1:${port}/`)).status, 404);
});

test("the edge passes the portal's paths and /mcp on as they are, and the answers back", async (t) => {
  const { base, portal, mcp } = await siteFixture(t);
  for (const route of ["/portal", "/portal/?session=abc", "/api/v1/health", "/assets/index-x.js"]) {
    const answer = await fetch(`${base}${route}`);
    assert.equal(answer.status, 200, route);
    assert.equal(answer.headers.get("x-upstream"), "portal", route);
    assert.deepEqual(answer.headers.getSetCookie(), ["a=1", "b=2"], route);
    assert.equal((await answer.json()).url, route);
  }
  const call = await fetch(`${base}/mcp`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  });
  assert.equal((await call.json()).name, "mcp");
  assert.equal(mcp.requests.at(-1).body, '{"jsonrpc":"2.0","id":1,"method":"tools/list"}');
  assert.equal(portal.requests.length, 4);
});

test("streams pass through as they come", async (t) => {
  const { port } = await siteFixture(t);
  const started = Date.now();
  const first = await new Promise((resolve, reject) => {
    http.get({ host: "127.0.0.1", port, path: "/api/v1/stream" }, (response) => {
      response.once("data", (chunk) => {
        resolve({ chunk: chunk.toString(), after: Date.now() - started });
        response.destroy();
      });
    }).once("error", reject);
  });
  assert.equal(first.chunk, "data: one\n\n");
  assert.ok(first.after < 250, `the first event took ${first.after} ms`);
});

test("an upstream that is down answers 502", async (t) => {
  const closed = net.createServer();
  closed.listen(0, "127.0.0.1");
  await once(closed, "listening");
  const port = closed.address().port;
  closed.close();
  const edge = createEdgeServer({
    home: createHomeHandler({ root: distFixture(t) }),
    targets: { portal: { host: "127.0.0.1", port }, mcp: { host: "127.0.0.1", port } },
  });
  const edgePort = await listen(t, edge);
  const answer = await fetch(`http://127.0.0.1:${edgePort}/portal/`);
  assert.equal(answer.status, 502);
});

/** Sends `request` on a raw socket to `port` and resolves with the first data. */
function rawExchange(port, request) {
  const socket = net.connect(port, "127.0.0.1");
  const next = () => new Promise((resolve, reject) => {
    socket.once("data", (data) => resolve(data.toString()));
    socket.once("error", reject);
  });
  socket.write(request);
  return { socket, next };
}

test("WebSockets to the portal are spliced through both ways", async (t) => {
  const { port, portal } = await siteFixture(t);
  const { socket, next } = rawExchange(port, [
    "GET /api/v1/ws HTTP/1.1",
    "Host: 127.0.0.1",
    "Upgrade: websocket",
    "Connection: Upgrade",
    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
    "Sec-WebSocket-Version: 13",
    "",
    "",
  ].join("\r\n"));
  t.after(() => socket.destroy());
  assert.match(await next(), /^HTTP\/1\.1 101 Switching Protocols/);
  socket.write("ping");
  assert.equal(await next(), "ping");
  const upgrade = portal.requests.at(-1);
  assert.equal(upgrade.upgrade, "/api/v1/ws");
  assert.equal(upgrade.headers["sec-websocket-key"], "dGhlIHNhbXBsZSBub25jZQ==");
  // The client's close reaches the portal, and nothing is left open.
  socket.end();
  await upgrade.closed;
  await once(socket, "close");
});

test("upgrades on the home page's paths are refused", async (t) => {
  const { port } = await siteFixture(t);
  const { socket, next } = rawExchange(port, "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
  t.after(() => socket.destroy());
  assert.match(await next(), /^HTTP\/1\.1 404 Not Found/);
});

test("a client that resets a refused upgrade doesn't take the edge down", async (t) => {
  const { base, port } = await siteFixture(t);
  for (let i = 0; i < 20; i++) {
    const socket = net.connect(port, "127.0.0.1");
    await once(socket, "connect");
    socket.write("GET /portal-ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    socket.resetAndDestroy();
    await once(socket, "close");
  }
  // Uncaught, a reset would have crashed this process before now.
  await delay(100);
  assert.equal((await fetch(`${base}/healthz`)).status, 200);
});
