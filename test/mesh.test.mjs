// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import test from "node:test";
import { checkMeshedPod, checkMeshProfile, meshPolicies } from "../scripts/lib/mesh.mjs";
import { checkRequests, hasMutualTlsMetric, meshCheckSnapshot, tcpDenialEvidence } from "../scripts/lib/mesh-check.mjs";

test("mesh prerequisites reject absent mesh, wrong revision, privileged redirection and old pools", () => {
  const outputs = { mesh: { revision: "asm-1-30" } };
  const cluster = {
    currentKubernetesVersion: "1.34.2",
    agentPoolProfiles: [{ currentOrchestratorVersion: "1.34.2" }],
    serviceMeshProfile: {
      mode: "Istio", istio: { revisions: ["asm-1-30"], components: { proxyRedirectionMechanism: "CNIChaining" } },
    },
  };
  assert.doesNotThrow(() => checkMeshProfile(cluster, outputs));
  const fails = (change, message) => {
    const copy = structuredClone(cluster);
    change(copy);
    assert.throws(() => checkMeshProfile(copy, outputs), message);
  };
  fails((c) => { delete c.serviceMeshProfile; }, /apply Terraform/);
  fails((c) => { c.serviceMeshProfile.istio.revisions = ["asm-1-31"]; }, /apply Terraform/);
  fails((c) => { c.serviceMeshProfile.istio.components.proxyRedirectionMechanism = "InitContainers"; }, /CNIChaining/);
  fails((c) => { c.currentKubernetesVersion = "1.32.9"; }, /1.33/);
  fails((c) => { c.agentPoolProfiles[0].currentOrchestratorVersion = "1.32.9"; }, /1.33/);
  fails((c) => { c.agentPoolProfiles = []; }, /node pool versions/);
});

test("injection verification rejects missing, legacy, privileged-redirection and unready proxies", () => {
  const pod = {
    metadata: { name: "test" },
    spec: { initContainers: [{ name: "istio-validation" }, { name: "istio-proxy", restartPolicy: "Always" }] },
    status: { initContainerStatuses: [{ name: "istio-proxy", ready: true }] },
  };
  checkMeshedPod(pod, { ready: true });
  assert.throws(() => checkMeshedPod({ metadata: pod.metadata, spec: {} }), /absent proxy/);
  assert.throws(() => checkMeshedPod({ spec: { initContainers: [{ name: "istio-proxy" }] } }), /absent proxy/);
  assert.throws(() => checkMeshedPod({ spec: { initContainers: [{ name: "istio-proxy", restartPolicy: "Always" }] } }), /absent proxy/);
  assert.throws(() => checkMeshedPod({ spec: { initContainers: [...pod.spec.initContainers, { name: "istio-init" }] } }), /absent proxy/);
  const stopped = structuredClone(pod);
  stopped.status.initContainerStatuses[0].ready = false;
  assert.throws(() => checkMeshedPod(stopped, { ready: true }), /not ready/);
  checkMeshedPod(stopped); // Completed Jobs no longer have a running proxy.
});

test("authorization principals follow the deployment namespace and actual service accounts", () => {
  const identities = Object.fromEntries(["edge", "swarm", "mcp", "home", "indexer", "ops"]
    .map((name) => [name, { service_account: `test-${name}` }]));
  const policies = meshPolicies({ namespace: "another", identities });
  for (const policy of policies.filter((o) => o.kind === "AuthorizationPolicy")) {
    for (const rule of policy.spec.rules) {
      assert.ok(rule.from[0].source.principals.every((p) => p.startsWith("cluster.local/ns/another/sa/test-")));
    }
  }
  delete identities.swarm;
  assert.throws(() => meshPolicies({ namespace: "another", identities }), /Missing mesh caller identity swarm/);
});

test("mTLS evidence requires destination-reported, nonzero counters for the exact identity", () => {
  const metric = 'istio_requests_total{reporter="destination",connection_security_policy="mutual_tls",source_principal="spiffe://cluster.local/ns/pghacker/sa/swarm"} 1';
  assert.ok(hasMutualTlsMetric(metric, "pghacker", "swarm"));
  assert.ok(!hasMutualTlsMetric(metric, "pghacker", "mcp"));
  assert.ok(!hasMutualTlsMetric(metric.replace("mutual_tls", "none"), "pghacker", "swarm"));
  assert.ok(!hasMutualTlsMetric(metric.replace("destination", "source"), "pghacker", "swarm"));
  assert.ok(!hasMutualTlsMetric(metric.replace("} 1", "} 0"), "pghacker", "swarm"));
  assert.ok(!hasMutualTlsMetric(metric.replace("/pghacker/", "/other/"), "pghacker", "swarm"));
});

test("compact run-command snapshots retain injection evidence without full pod specs", () => {
  const objects = meshCheckSnapshot([
    "warning before the snapshot",
    "MESH_JOBS_BEGIN",
    "mesh-check-agent-test\tTrue\t1",
    "mesh-check-plaintext-test\t\t",
    "MESH_PODS_BEGIN",
    "agent-pod\tmesh-check-agent-test\tistio-validation istio-proxy\tAlways\tcheck",
    "plaintext-pod\tmesh-check-plaintext-test\t\t\tcheck",
    "MESH_SNAPSHOT_END",
  ].join("\n"));
  assert.equal(objects[0].status.succeeded, 1);
  assert.equal(objects[1].status.succeeded, 0);
  checkMeshedPod(objects[2]);
  assert.equal(objects[3].spec.initContainers.length, 0);
  assert.equal(objects[3].spec.containers[0].name, "check");
  assert.throws(() => meshCheckSnapshot("truncated"), /Missing compact/);
  assert.throws(() => meshCheckSnapshot("MESH_JOBS_BEGIN\nbad\nMESH_PODS_BEGIN\nMESH_SNAPSHOT_END"), /Invalid compact/);
  assert.throws(() => meshCheckSnapshot("MESH_JOBS_BEGIN\njob\tTrue\tx\nMESH_PODS_BEGIN\nMESH_SNAPSHOT_END"), /Invalid success/);
});

test("TCP authorization evidence requires fresh server denials, not just client resets", () => {
  const cases = [{ requests: [
    { url: "http://home.pghacker.svc.cluster.local:3104/healthz", expected: "denied" },
    { url: "http://portal.pghacker.svc.cluster.local:3001/api/health", expected: "denied" },
    { url: "http://portal.pghacker.svc.cluster.local:3001/api/health", expected: "denied" },
    { url: "http://home.pghacker.svc.cluster.local:3104/healthz", expected: "reset" },
  ] }];
  assert.deepEqual(tcpDenialEvidence({ home: 3, portal: 5 }, { home: 4, portal: 7 }, cases), { home: 1, portal: 2 });
  assert.throws(() => tcpDenialEvidence({ home: 3, portal: 5 }, { home: 4, portal: 6 }, cases), /at least 2 new TCP RBAC/);
  assert.throws(() => tcpDenialEvidence({ home: 3, portal: 5 }, { home: 1, portal: 7 }, cases), /observed -2/);
  assert.throws(() => tcpDenialEvidence({}, { home: 4, portal: 7 }, cases), /Missing TCP authorization/);
});

async function listen(server) {
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return `http://127.0.0.1:${server.address().port}/`;
}

test("security diagnostics require application responses, exact RBAC denials and real resets", async () => {
  const server = http.createServer((request, response) => {
    const [status, body] = request.url === "/rbac" ? [403, "RBAC: access denied"]
      : request.url === "/application-denial" ? [403, "unrelated denial"]
        : request.url === "/missing" ? [404, '{"error":{"code":"not_found"}}'] : [200, "ok"];
    response.writeHead(status).end(body);
  });
  const url = await listen(server);
  const reset = net.createServer((socket) => socket.once("data", () => socket.destroy()));
  const resetUrl = await listen(reset);
  try {
    const good = await checkRequests([
      { url, expected: 200 }, { url: `${url}rbac`, expected: 403 },
      { url: `${url}missing`, expected: 404 }, { url: resetUrl, expected: "reset" },
      { url: resetUrl, expected: "denied" },
    ]);
    assert.equal(good.ok, true, JSON.stringify(good));
    const bad = await checkRequests([
      { url: `${url}application-denial`, expected: 403 }, { url, expected: "reset" }, { url, expected: 404 },
      { url: `${url}rbac`, expected: "denied" },
    ]);
    assert.equal(bad.ok, false);
    assert.ok(bad.results.every((result) => !result.ok));
  } finally {
    server.closeAllConnections();
    await Promise.all([new Promise((resolve) => server.close(resolve)), new Promise((resolve) => reset.close(resolve))]);
  }
});
