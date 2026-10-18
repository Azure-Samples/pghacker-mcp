// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import {
  THEME_STORAGE_KEY,
  applyTheme,
  otherTheme,
  parseTheme,
  readTheme,
  writeTheme,
  type KeyValueStore,
} from "./theme.ts";

/** <html>, as far as the theme goes. */
function fakeRoot() {
  const classes = new Set<string>();
  return {
    classes,
    classList: {
      toggle(token: string, force?: boolean) {
        const on = force ?? !classes.has(token);
        if (on) classes.add(token);
        else classes.delete(token);
        return on;
      },
    },
    style: { colorScheme: "" },
  };
}

const shown = (root: ReturnType<typeof fakeRoot>) => ({
  theme: root.classes.has("dark") ? "dark" : "light",
  colorScheme: root.style.colorScheme,
});

const STORED = [null, "light", "dark", "system", "Dark", ""];

test("the page is light unless dark is kept, a kept \"system\" included", () => {
  assert.equal(parseTheme(null), "light");
  assert.equal(parseTheme("light"), "light");
  assert.equal(parseTheme("dark"), "dark");
  assert.equal(parseTheme("system"), "light");
  assert.equal(parseTheme("Dark"), "light");
});

test("the toggle switches between the two themes", () => {
  assert.equal(otherTheme("light"), "dark");
  assert.equal(otherTheme("dark"), "light");
});

test("a theme is the dark class and the color scheme on <html>", () => {
  const root = fakeRoot();
  applyTheme(root, "dark");
  assert.deepEqual(shown(root), { theme: "dark", colorScheme: "dark" });
  applyTheme(root, "light");
  assert.deepEqual(shown(root), { theme: "light", colorScheme: "light" });
});

test("the theme is kept, and storage that fails leaves the page light", () => {
  const items = new Map<string, string>();
  const store: KeyValueStore = {
    getItem: (key) => items.get(key) ?? null,
    setItem: (key, value) => void items.set(key, value),
  };
  assert.equal(readTheme(store), "light");
  writeTheme(store, "dark");
  assert.equal(items.get(THEME_STORAGE_KEY), "dark");
  assert.equal(readTheme(store), "dark");

  const refusing: KeyValueStore = {
    getItem: () => {
      throw new Error("SecurityError");
    },
    setItem: () => {
      throw new Error("QuotaExceededError");
    },
  };
  assert.equal(readTheme(refusing), "light");
  assert.doesNotThrow(() => writeTheme(refusing, "dark"));
  assert.equal(readTheme(undefined), "light");
});

test("theme-boot.js shows the theme that theme.ts would, before the page's script", () => {
  const source = readFileSync(new URL("./theme-boot.js", import.meta.url), "utf8");
  const boot = (globals: Record<string, unknown>, { refuseStorage = false } = {}) => {
    const root = fakeRoot();
    const context: Record<string, unknown> = { document: { documentElement: root }, ...globals };
    if (refuseStorage) {
      Object.defineProperty(context, "localStorage", {
        get() {
          throw new Error("SecurityError");
        },
      });
    }
    vm.runInNewContext(source, context);
    return shown(root);
  };

  for (const stored of STORED) {
    const expected = parseTheme(stored);
    const localStorage = { getItem: (key: string) => (key === THEME_STORAGE_KEY ? stored : null) };
    assert.deepEqual(boot({ localStorage }), { theme: expected, colorScheme: expected }, `kept ${JSON.stringify(stored)}`);
  }

  // No storage: light, as theme.ts.
  assert.deepEqual(boot({}, { refuseStorage: true }), { theme: "light", colorScheme: "light" });
});
