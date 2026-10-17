// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * The privacy statement the footer links to. It belongs to the deployment, not
 * to the code: the home page's server adds it to the page as the privacy-url
 * meta tag when the deployment sets one (PGHACKER_PRIVACY_URL;
 * scripts/lib/edge.mjs). Without the tag, the footer shows no link.
 */

/** What the page reads of the meta tag. */
export interface PrivacyMeta {
  readonly content: string;
}

/**
 * The privacy statement's URL, as the meta tag gives it, if it is an https
 * URL with a host and no credentials; otherwise undefined, and no link.
 */
export function readPrivacyUrl(meta: PrivacyMeta | null): string | undefined {
  if (!meta) return undefined;
  let url: URL;
  try {
    url = new URL(meta.content);
  } catch {
    return undefined;
  }
  if (url.protocol !== "https:" || url.hostname === "" || url.username !== "" || url.password !== "") return undefined;
  return url.href;
}
