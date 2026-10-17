// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * The GitHub repository the top bar links to: index.html's github-repo meta
 * tag names it, owner/name, and the home page's server adds its star count as
 * data-stars once GitHub shows it, which it does only for a public repository.
 */

export interface Repository {
  /** owner/name. */
  readonly name: string;
  readonly url: string;
  readonly stars?: number;
}

/** What the page reads of the meta tag. */
export interface RepositoryMeta {
  readonly content: string;
  readonly dataset: { readonly stars?: string };
}

const NAME = /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/;

/** The repository the meta tag names, with its stars if it has them, or undefined. */
export function readRepository(meta: RepositoryMeta | null): Repository | undefined {
  if (!meta || !NAME.test(meta.content)) return undefined;
  const repository = { name: meta.content, url: `https://github.com/${meta.content}` };
  const stars = meta.dataset.stars;
  if (stars === undefined || !/^\d{1,15}$/.test(stars)) return repository;
  return { ...repository, stars: Number(stars) };
}

/** A star count as GitHub shows it: 999, 1k, 1.2k, 12.3k, 1.5m. */
export function formatStars(stars: number): string {
  if (stars < 1000) return String(stars);
  const [value, unit] = stars < 999_950 ? [stars / 1000, "k"] : [stars / 1_000_000, "m"];
  return `${Number(value.toFixed(1))}${unit}`;
}
