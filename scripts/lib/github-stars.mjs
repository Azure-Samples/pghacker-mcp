// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The star counts of GitHub repositories, for the home page's link to its
// own (scripts/lib/edge.mjs). GitHub's REST API gives them without a token,
// for public repositories only: an internal or private one is a 404, and has
// no count. A count is checked again at most every 10 minutes, in the
// background, so that no page waits for GitHub; until GitHub answers, and
// whenever it can't be reached, the last count known stands.

const API = "https://api.github.com/repos/";

/** How long a count stands before it's checked again. */
export const STARS_TTL_MS = 10 * 60_000;

/** A repository's owner/name, as GitHub allows them. */
export const REPOSITORY_NAME = /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/;

export function createStarCounts({ fetchImpl = fetch, ttlMs = STARS_TTL_MS, now = Date.now, log = console.error } = {}) {
  const entries = new Map();

  async function check(repository, entry) {
    try {
      const response = await fetchImpl(`${API}${repository}`, {
        headers: {
          accept: "application/vnd.github+json",
          "user-agent": "pghacker-mcp",
          "x-github-api-version": "2022-11-28",
        },
        signal: AbortSignal.timeout(10_000),
      });
      if (response.status === 404) {
        await response.body?.cancel();
        entry.stars = undefined;
        return;
      }
      if (!response.ok) {
        await response.body?.cancel();
        throw new Error(`HTTP ${response.status}`);
      }
      const { visibility, stargazers_count: stars } = await response.json();
      entry.stars = visibility === "public" && Number.isSafeInteger(stars) && stars >= 0 ? stars : undefined;
    } catch (error) {
      log(`[home] GitHub's stars for ${repository}: ${error.message}`);
    } finally {
      entry.checkedAt = now();
      entry.checking = undefined;
    }
  }

  return {
    /** The count known for `repository` (owner/name), or undefined; checks it again when it's old. */
    get(repository) {
      if (!REPOSITORY_NAME.test(repository)) return undefined;
      let entry = entries.get(repository);
      if (!entry) entries.set(repository, (entry = { stars: undefined, checkedAt: -Infinity, checking: undefined }));
      if (!entry.checking && now() - entry.checkedAt >= ttlMs) entry.checking = check(repository, entry);
      return entry.stars;
    },
    /** Resolves once the checks under way have finished. */
    async settled() {
      await Promise.all([...entries.values()].map((entry) => entry.checking));
    },
  };
}
