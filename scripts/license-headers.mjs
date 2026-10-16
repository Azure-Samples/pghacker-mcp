// Checks that every source file starts with the MIT license header, or, with
// --fix, adds the header to the files that lack it, after any #! line,
// Dockerfile parser directive or HTML doctype (scripts/lib/license-headers.mjs).
// npm test runs the check too.
//   npm run license-headers             # list the files without the header
//   npm run license-headers -- --fix    # add it to them
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { APP_ROOT } from "./lib/config.mjs";
import { addHeader, missingHeaders } from "./lib/license-headers.mjs";

try {
  const { values } = parseArgs({ options: { fix: { type: "boolean", default: false } } });
  const missing = missingHeaders(APP_ROOT);
  if (values.fix) {
    for (const file of missing) {
      const full = path.join(APP_ROOT, file);
      writeFileSync(full, addHeader(file, readFileSync(full, "utf8")));
    }
    console.log(`[license-headers] Added the header to ${missing.length} files.`);
  } else if (missing.length) {
    console.error(`[license-headers] ${missing.length} source files lack the license header; npm run license-headers -- --fix adds it:`);
    for (const file of missing) console.error(`  ${file}`);
    process.exitCode = 1;
  } else {
    console.log("[license-headers] Every source file has the license header.");
  }
} catch (error) {
  console.error(`[license-headers] ${error.message}`);
  process.exitCode = 1;
}
