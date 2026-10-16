// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { readFileSync, writeFileSync } from "node:fs";
import { parseArgs } from "node:util";
import {
  assertReady, createManifest, inventory, prepareIndexes,
} from "./lib/graph-indexes.mjs";

const { values } = parseArgs({ options: {
  plan: { type: "boolean", default: false },
  check: { type: "boolean", default: false },
  apply: { type: "boolean", default: false },
  out: { type: "string" },
  "plan-file": { type: "string" },
  "build-timeout-ms": { type: "string" },
  only: { type: "string" },
} });
const emit = (result) => console.log(JSON.stringify(result));
const controller = new AbortController();
process.once("SIGINT", () => controller.abort(new Error("preparation interrupted")));
process.once("SIGTERM", () => controller.abort(new Error("preparation terminated")));

try {
  if ([values.plan, values.check, values.apply].filter(Boolean).length > 1) {
    throw new Error("choose only one of --plan, --check, --apply");
  }
  if (!values.apply && (values["plan-file"] || values.only || values["build-timeout-ms"])) {
    throw new Error("--plan-file, --only and --build-timeout-ms require --apply");
  }
  if ((values.check || values.apply) && values.out) throw new Error("--out requires --plan");
  const url = values.apply ? process.env.PGHACKER_ADMIN_DATABASE_URL
    : process.env.PGHACKER_INDEXER_DATABASE_URL || process.env.PGHACKER_ADMIN_DATABASE_URL;
  if (!url) throw new Error("set the established PGHACKER_INDEXER_DATABASE_URL or PGHACKER_ADMIN_DATABASE_URL; --apply requires the existing graph owner");
  if (values.apply) {
    if (!values["plan-file"]) throw new Error("--apply requires a reviewed --plan-file");
    const manifest = JSON.parse(readFileSync(values["plan-file"], "utf8"));
    const result = await prepareIndexes(url, manifest, {
      buildTimeoutMs: Number(values["build-timeout-ms"]),
      only: values.only, progress: emit, signal: controller.signal,
    });
    emit({ ok: result.ready, ...result });
    if (!result.ready) process.exitCode = 2;
  } else {
    const { identity, requirements } = await inventory(url);
    if (values.check) {
      assertReady(requirements);
      emit({ ok: true, ready: true, identity });
    } else {
      const manifest = createManifest(url, identity, requirements);
      if (values.out) writeFileSync(values.out, `${JSON.stringify(manifest, null, 2)}\n`, { flag: "wx", mode: 0o600 });
      emit(manifest);
    }
  }
} catch (error) {
  emit({
    ok: false, error: error.message, sqlstate: error.sqlstate ?? null,
    completed: error.completed ?? [], build_active: error.build_active ?? null,
    index: error.index ?? null, index_state: error.index_state ?? null, indexes: error.indexes ?? [],
    observation_error: error.observation_error ?? null, close_error: error.close_error ?? null,
  });
  process.exitCode = 1;
}
