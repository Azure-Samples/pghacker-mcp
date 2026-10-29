// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { enrollInMesh, meshRevision } from "./mesh.mjs";

export const MESH_JOB_SNAPSHOT = '{range .items[*]}{.metadata.name}{"\\t"}{.status.conditions[?(@.type=="Complete")].status}{"\\t"}{.status.succeeded}{"\\n"}{end}';
export const MESH_POD_SNAPSHOT = '{range .items[*]}{.metadata.name}{"\\t"}{.metadata.labels.job-name}{"\\t"}{.spec.initContainers[*].name}{"\\t"}{.spec.initContainers[?(@.name=="istio-proxy")].restartPolicy}{"\\t"}{.spec.containers[*].name}{"\\n"}{end}';
export const MESH_SERVER_SNAPSHOT = '{range .items[*]}{.metadata.name}{"\\t"}{.metadata.labels.app\\.kubernetes\\.io/name}{"\\t"}{.spec.containers[0].name}{"\\n"}{end}';

/** Transport resets alone cannot distinguish authorization from a network failure. */
export function tcpDenialEvidence(before, after, cases) {
  const expected = {};
  for (const check of cases) {
    for (const request of check.requests.filter((request) => request.expected === "denied")) {
      const service = new URL(request.url).hostname.split(".")[0];
      expected[service] = (expected[service] ?? 0) + 1;
    }
  }
  return Object.fromEntries(Object.entries(expected).map(([service, minimum]) => {
    if (!Number.isSafeInteger(before[service]) || !Number.isSafeInteger(after[service])) {
      throw new Error(`Missing TCP authorization counters for ${service}.`);
    }
    const denied = after[service] - before[service];
    if (denied < minimum) {
      throw new Error(`${service}: expected at least ${minimum} new TCP RBAC denials, observed ${denied}.`);
    }
    return [service, denied];
  }));
}

/** Full pod specs exceed ARM run-command's 512 kB log limit for a batch. */
export function meshCheckSnapshot(output) {
  const match = /MESH_JOBS_BEGIN\r?\n([\s\S]*?)MESH_PODS_BEGIN\r?\n([\s\S]*?)MESH_SNAPSHOT_END/.exec(output);
  if (!match) throw new Error("Missing compact mesh-check snapshot from kubectl.");
  const rows = (text, width) => text.split(/\r?\n/).filter(Boolean).map((line) => {
    const fields = line.split("\t");
    if (fields.length !== width) throw new Error(`Invalid compact mesh-check row: ${line.slice(0, 200)}`);
    return fields;
  });
  const jobs = rows(match[1], 3).map(([name, complete, succeeded]) => {
    if (succeeded !== "" && !/^\d+$/.test(succeeded)) throw new Error(`Invalid success count for ${name}.`);
    return {
      kind: "Job", metadata: { name },
      status: { succeeded: Number(succeeded || 0), conditions: [{ type: "Complete", status: complete }] },
    };
  });
  const pods = rows(match[2], 5).map(([name, job, init, restartPolicy, containers]) => ({
    kind: "Pod", metadata: { name, labels: { "job-name": job } },
    spec: {
      initContainers: init.split(" ").filter(Boolean).map((name) => ({
        name, ...(name === "istio-proxy" ? { restartPolicy } : {}),
      })),
      containers: containers.split(" ").filter(Boolean).map((name) => ({ name })),
    },
  }));
  return [...jobs, ...pods];
}

/** Network labels deliberately match real callers, independent of their test identity. */
export function meshCheckCases(outputs) {
  const usage = Boolean(outputs.app_settings.APPLICATIONINSIGHTS_CONNECTION_STRING);
  const cases = [
    ["agent", "swarm", { "pghacker.io/role": "agent" }, [
      ["indexer", 3101, "/healthz", 200], ["mcp", 3103, "/healthz", 200],
      ["token-proxy", 3110, "/__mesh_check__", 404],
    ]],
    ["edge", "edge", { "app.kubernetes.io/name": "edge" }, [
      ["home", 3104, "/healthz", 200], ["mcp", 3102, "/healthz", 200],
      ["portal", 3001, "/api/health", 200],
    ]],
    ["indexer", "indexer", { "app.kubernetes.io/name": "indexer" }, [
      ["token-proxy", 3110, "/__mesh_check__", 404],
    ]],
    ["portal", "swarm", { "app.kubernetes.io/name": "portal" }, [
      ["token-proxy", 3110, "/__mesh_check__", 404],
    ]],
    ["ops", "ops", { "pghacker.io/role": "ops" }, [
      ["token-proxy", 3110, "/__mesh_check__", 404],
    ]],
    ["mcp", "mcp", { "app.kubernetes.io/name": "mcp" }, [
      ["token-proxy", 3110, "/__mesh_check__", 404],
      ...(usage ? [["token-proxy", 3111, "/__mesh_check__", 404]] : []),
    ]],
    ...(usage ? [["home", "home", { "app.kubernetes.io/name": "home" }, [
      ["token-proxy", 3111, "/__mesh_check__", 404],
    ]]] : []),
    ["wrong-agent", "home", { "pghacker.io/role": "agent" }, [
      ["indexer", 3101, "/healthz", 403], ["mcp", 3103, "/healthz", 403],
      ["token-proxy", 3110, "/__mesh_check__", 403],
    ]],
    ["wrong-edge", "swarm", { "app.kubernetes.io/name": "edge" }, [
      ["home", 3104, "/healthz", "denied"], ["mcp", 3102, "/healthz", "denied"],
      ["portal", 3001, "/api/health", "denied"],
    ]],
    ...(usage ? [["wrong-monitor", "swarm", { "app.kubernetes.io/name": "mcp" }, [
      ["token-proxy", 3111, "/__mesh_check__", 403],
    ]]] : []),
    ["plaintext-agent", "swarm", { "pghacker.io/role": "agent" }, [
      ["indexer", 3101, "/healthz", "reset"], ["mcp", 3103, "/healthz", "reset"],
      ["token-proxy", 3110, "/__mesh_check__", "reset"],
    ]],
    ["plaintext-edge", "edge", { "app.kubernetes.io/name": "edge" }, [
      ["home", 3104, "/healthz", "reset"], ["mcp", 3102, "/healthz", "reset"],
      ["portal", 3001, "/api/health", "reset"],
    ]],
    ...(usage ? [["plaintext-monitor", "mcp", { "app.kubernetes.io/name": "mcp" }, [
      ["token-proxy", 3111, "/__mesh_check__", "reset"],
    ]]] : []),
  ];
  return cases.map(([name, identity, labels, requests]) => ({
    name, identity, labels,
    plaintext: name.startsWith("plaintext-"),
    requests: requests.map(([service, port, path, expected]) => ({
      url: `http://${service}.${outputs.namespace}.svc.cluster.local:${port}${path}`, expected,
    })),
  }));
}

// Runs inside the test pod without secrets or a Kubernetes API token.
export async function checkRequests(requests) {
  const { connect } = await import("node:net");
  const results = [];
  for (const { url, expected } of requests) {
    const transportRefusal = expected === "reset" || expected === "denied";
    try {
      if (transportRefusal) {
        // A TCP handshake must succeed: a NetworkPolicy timeout isn't mTLS evidence.
        const target = new URL(url);
        await new Promise((resolve, reject) => {
          const socket = connect({ host: target.hostname, port: Number(target.port) });
          socket.setTimeout(5000, () => socket.destroy(new Error("TCP handshake timed out")));
          socket.once("error", reject);
          socket.once("connect", () => { socket.destroy(); resolve(); });
        });
      }
      let response;
      try {
        response = await fetch(url, { signal: AbortSignal.timeout(10_000), redirect: "manual" });
      } catch (error) {
        if (!transportRefusal || !["ECONNRESET", "UND_ERR_SOCKET", "EPIPE"].includes(error.cause?.code)) throw error;
        results.push({ url, expected, ok: true, reset: error.cause.code });
        continue;
      }
      const body = await response.text();
      const ok = response.status === expected
        && (expected !== 403 || body.trim() === "RBAC: access denied")
        && (expected !== 404 || body.includes('"not_found"'));
      results.push({ url, expected, status: response.status, ok });
    } catch (error) {
      results.push({ url, expected, ok: false, error: error.message });
    }
  }
  return { ok: results.every((result) => result.ok), results };
}

export function renderMeshCheckJob(outputs, check, suffix) {
  if (!/^[a-z0-9]{1,12}$/.test(suffix)) throw new Error("Invalid mesh-check run suffix.");
  const agent = check.labels["pghacker.io/role"] === "agent";
  const job = {
    apiVersion: "batch/v1", kind: "Job",
    metadata: {
      name: `mesh-check-${check.name}-${suffix}`, namespace: outputs.namespace,
      labels: { "pghacker.io/mesh-check-run": suffix },
    },
    spec: {
      backoffLimit: 0, activeDeadlineSeconds: 180, ttlSecondsAfterFinished: 300,
      template: {
        metadata: {
          // A separate part-of value permits the deliberate non-mesh negative test.
          labels: {
            ...check.labels, "app.kubernetes.io/part-of": "pghacker-mesh-check",
            "pghacker.io/mesh-check-run": suffix,
          },
        },
        spec: {
          restartPolicy: "Never",
          serviceAccountName: outputs.identities[check.identity].service_account,
          automountServiceAccountToken: false,
          nodeSelector: { "pghacker.io/pool": agent ? "swarm" : "services" },
          ...(agent ? { tolerations: [{ key: "pghacker.io/pool", operator: "Equal", value: "swarm", effect: "NoSchedule" }] } : {}),
          securityContext: { runAsNonRoot: true, runAsUser: 10002, runAsGroup: 10002, seccompProfile: { type: "RuntimeDefault" } },
          containers: [{
            name: "check", image: outputs.image,
            args: ["node", "--input-type=module", "-e",
              `const report = await (${checkRequests.toString()})(${JSON.stringify(check.requests)}); console.log(JSON.stringify(report)); process.exitCode = report.ok ? 0 : 1;`],
            // These labels match Service selectors. Never become a ready endpoint.
            readinessProbe: { exec: { command: ["node", "-e", "process.exit(1)"] }, periodSeconds: 1 },
            resources: { requests: { cpu: "50m", memory: "64Mi" }, limits: { memory: "128Mi" } },
            securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } },
          }],
        },
      },
    },
  };
  enrollInMesh(job, meshRevision(outputs));
  if (check.plaintext) job.spec.template.metadata.annotations["sidecar.istio.io/inject"] = "false";
  return job;
}

/** Server-reported connection security is evidence of negotiated mTLS, not merely configured TLS. */
export function hasMutualTlsMetric(text, namespace, account) {
  return text.split("\n").some((line) => line.startsWith("istio_requests_total{")
    && line.includes('reporter="destination"')
    && line.includes('connection_security_policy="mutual_tls"')
    && line.includes(`source_principal="spiffe://cluster.local/ns/${namespace}/sa/${account}"`)
    && Number(line.slice(line.lastIndexOf("}") + 1).trim()) > 0);
}
