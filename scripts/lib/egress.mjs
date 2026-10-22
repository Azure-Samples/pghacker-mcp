// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Whether a destination answers, for the egress check
// (scripts/egress-check.mjs). A server that answers is reached, whatever it
// says: an HTTP status, or a TLS certificate that this process doesn't
// trust, as the Kubernetes API's, from the cluster's own CA, would be. A
// reset, a refusal or silence isn't: Azure Firewall resets the HTTPS
// connections it denies, and a NetworkPolicy drops what it denies.
import net from "node:net";

/** The TLS errors of a certificate that didn't verify, which only a server that answered can cause. */
const CERTIFICATE_ERROR = /CERT|^UNABLE_TO_|^INVALID_CA$|^PATH_LENGTH_EXCEEDED$|^INVALID_PURPOSE$|^HOSTNAME_MISMATCH$/;

/** Whether `code`, a TLS error's, is a certificate's that didn't verify. */
export function isCertificateError(code) {
  return typeof code === "string" && CERTIFICATE_ERROR.test(code);
}

/**
 * Asks `url` for an answer within `timeoutMs`: { answered: true, status } for
 * an HTTP answer, { answered: true, error } for a certificate that didn't
 * verify, { answered: false, error } for none.
 */
export async function reach(url, { timeoutMs = 5000, headers = {}, fetchImpl = fetch } = {}) {
  const at = Date.now();
  try {
    const response = await fetchImpl(url, { headers, redirect: "manual", signal: AbortSignal.timeout(timeoutMs) });
    await response.arrayBuffer();
    return { answered: true, status: response.status, ms: Date.now() - at };
  } catch (error) {
    const code = String(error.cause?.code ?? (error.name === "TimeoutError" ? "timeout" : error.message));
    return { answered: isCertificateError(code), error: code.slice(0, 200), ms: Date.now() - at };
  }
}

/** Whether a TCP connection to `host`:`port` opens within `timeoutMs`: { connected, error }. */
export function connects(host, port, { timeoutMs = 5000 } = {}) {
  const at = Date.now();
  return new Promise((resolve) => {
    let settled = false;
    const socket = net.connect({ host, port: Number(port) });
    const done = (result) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      resolve({ ...result, ms: Date.now() - at });
    };
    socket.setTimeout(timeoutMs, () => done({ connected: false, error: "timeout" }));
    socket.once("connect", () => done({ connected: true }));
    socket.once("error", (error) => done({ connected: false, error: String(error.code ?? error.message).slice(0, 200) }));
  });
}
