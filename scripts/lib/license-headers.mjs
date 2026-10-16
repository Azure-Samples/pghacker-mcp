import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

/** The license header's lines, without comment syntax. */
export const HEADER = ["Copyright (c) Microsoft Corporation.", "Licensed under the MIT license."];

/** Third-party code, which keeps its own license, and files that tools generate. */
const SKIPPED = [/^vendor\//, /(^|\/)\.terraform\.lock\.hcl$/];

const lineComment = (prefix) => (text) => `${prefix} ${text}`;
const blockComment = (open, close) => (text) => `${open} ${text} ${close}`;

/**
 * The source files' types and their comment syntax. JSON, lockfiles, Markdown,
 * configuration (TOML, YAML, .tfvars, .env) and images take no header.
 */
const STYLES = [
  { pattern: /\.(rs|mjs|cjs|js|jsx|ts|tsx|mts|cts)$/, comment: lineComment("//") },
  { pattern: /\.sql$/, comment: lineComment("--") },
  { pattern: /\.(tf|hcl|sh)$/, comment: lineComment("#") },
  { pattern: /(^|\/)Dockerfile$|\.Dockerfile$/, comment: lineComment("#"), dockerfile: true },
  { pattern: /\.css$/, comment: blockComment("/*", "*/") },
  { pattern: /\.html$/, comment: blockComment("<!--", "-->"), html: true },
];

/** Dockerfile parser directives, which must stay above every comment. */
const DOCKERFILE_DIRECTIVE = /^#\s*(syntax|escape|check)\s*=/i;

function styleOf(file) {
  return STYLES.find((style) => style.pattern.test(file));
}

/** Whether `file`, a path relative to the repository's root, must start with the header. */
export function isSource(file) {
  return !SKIPPED.some((pattern) => pattern.test(file)) && styleOf(file) !== undefined;
}

/** The header as `file`'s comments. */
export function headerLines(file) {
  const style = styleOf(file);
  if (!style) throw new Error(`${file} takes no license header.`);
  return HEADER.map(style.comment);
}

/** How many of `lines` stay above the header: a #! line, Dockerfile parser directives, an HTML doctype. */
function preamble(lines, style) {
  let count = lines[0]?.startsWith("#!") ? 1 : 0;
  if (style.dockerfile) while (count < lines.length && DOCKERFILE_DIRECTIVE.test(lines[count])) count++;
  if (style.html && /^<!doctype\b/i.test(lines[count] ?? "")) count++;
  return count;
}

function parse(text) {
  const bom = text.startsWith("\uFEFF") ? "\uFEFF" : "";
  const body = text.slice(bom.length);
  const eol = body.includes("\r\n") ? "\r\n" : "\n";
  return { bom, body, eol, lines: body.split(eol) };
}

/** Whether `text`, the contents of `file`, has the header, right after its preamble. */
export function hasHeader(file, text) {
  const header = headerLines(file);
  const { lines } = parse(text);
  const start = preamble(lines, styleOf(file));
  return header.every((line, i) => lines[start + i] === line);
}

/**
 * `text` with the header after its preamble and a blank line after the
 * header; `text` itself when it has the header. Migrations' checksums leave
 * out exactly this header and blank line (crates/pghacker-core/src/migrate.rs).
 */
export function addHeader(file, text) {
  if (hasHeader(file, text)) return text;
  const header = headerLines(file);
  const { bom, body, eol, lines } = parse(text);
  if (body === "") return `${bom}${header.join(eol)}${eol}`;
  const start = preamble(lines, styleOf(file));
  return bom + [...lines.slice(0, start), ...header, "", ...lines.slice(start)].join(eol);
}

/** The source files in the repository at `root`, tracked or new, but not ignored. */
export function sourceFiles(root) {
  const listed = execFileSync("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
    cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], maxBuffer: 64 * 1024 * 1024,
  });
  return [...new Set(listed.split("\0"))]
    .filter((file) => file && isSource(file) && existsSync(path.join(root, file)))
    .sort();
}

/** The source files at `root` without the header. */
export function missingHeaders(root) {
  return sourceFiles(root).filter((file) => !hasHeader(file, readFileSync(path.join(root, file), "utf8")));
}
