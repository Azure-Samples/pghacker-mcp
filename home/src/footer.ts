// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * What a deployment says in the page's footer: a link to its privacy statement,
 * and a copyright link. Both belong to the deployment, not to the code: the
 * home page's server adds them to the page as the privacy-url and
 * copyright-link meta tags when the deployment sets them (PGHACKER_PRIVACY_URL,
 * and PGHACKER_COPYRIGHT_URL with PGHACKER_COPYRIGHT_TEXT;
 * scripts/lib/edge.mjs). Without a tag, the footer shows no such link.
 */

/** The longest text of the copyright link, in characters. */
const MAX_COPYRIGHT_TEXT = 100;

/** What the page reads of the privacy-url meta tag. */
export interface PrivacyMeta {
  readonly content: string;
}

/** What the page reads of the copyright-link meta tag: the URL, and the text as data-text. */
export interface CopyrightMeta {
  readonly content: string;
  readonly dataset: { readonly text?: string };
}

export interface CopyrightLink {
  readonly url: string;
  readonly text: string;
}

/** `content` as an https URL with a host and no credentials, as the browser writes it; otherwise undefined. */
function httpsUrl(content: string): string | undefined {
  let url: URL;
  try {
    url = new URL(content);
  } catch {
    return undefined;
  }
  if (url.protocol !== "https:" || url.hostname === "" || url.username !== "" || url.password !== "") return undefined;
  return url.href;
}

/** The privacy statement's URL the meta tag gives, if it is an https URL; otherwise undefined, and no link. */
export function readPrivacyUrl(meta: PrivacyMeta | null): string | undefined {
  return meta ? httpsUrl(meta.content) : undefined;
}

/**
 * The copyright link the meta tag gives: an https URL, and the text it says, 1
 * to 100 characters without control characters. Otherwise undefined, and no
 * link.
 */
export function readCopyright(meta: CopyrightMeta | null): CopyrightLink | undefined {
  if (!meta) return undefined;
  const url = httpsUrl(meta.content);
  const text = meta.dataset.text?.trim();
  if (url === undefined || !text || [...text].length > MAX_COPYRIGHT_TEXT || /\p{Cc}/u.test(text)) return undefined;
  return { url, text };
}
