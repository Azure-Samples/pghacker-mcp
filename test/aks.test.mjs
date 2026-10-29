// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { parse, parseAllDocuments } from "yaml";
import { PORTS, USERS, VAULT_SECRETS } from "../scripts/lib/azure.mjs";
import {
  IMAGE_ROOT, JOBS, POOL_LABEL, fillTemplate, internetBlock, invokeArgs, invokeResult, jobEnd, jsonFromOutput,
  kubectlCommand, podSpec, renderJob, renderManifests, revisionOf, rolloutState, shellQuote, toYaml,
} from "../scripts/lib/aks.mjs";
import { APP_ROOT } from "../scripts/lib/config.mjs";
import { MESH_ACCESS } from "../scripts/lib/mesh.mjs";
import { meshCheckCases, renderMeshCheckJob } from "../scripts/lib/mesh-check.mjs";

/** Terraform's kubernetes output (deploy/terraform/outputs.tf), for an example deployment. */
const OUTPUTS = {
  subscription_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  tenant_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
  resource_group_name: "pghacker-example",
  cluster_name: "pghacker-example-aks",
  node_resource_group: "pghacker-example-nodes",
  namespace: "pghacker",
  key_vault_name: "pghacker-example-kv",
  image: "examplepghacr12345.azurecr.io/pghacker-mcp@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  seed_image: null,
  pool_label: "pghacker.io/pool",
  swarm_taint: { key: "pghacker.io/pool", value: "swarm", effect: "NoSchedule" },
  mesh: { revision: "asm-1-30" },
  identities: {
    proxy: { client_id: "11111111-0000-4000-8000-000000000001", service_account: "token-proxy", secrets: [] },
    indexer: { client_id: "11111111-0000-4000-8000-000000000002", service_account: "indexer", secrets: ["pgh-indexer-password"] },
    mcp: { client_id: "11111111-0000-4000-8000-000000000003", service_account: "mcp", secrets: ["pgh-reader-password", "usage-id-key"] },
    swarm: { client_id: "11111111-0000-4000-8000-000000000004", service_account: "swarm", secrets: ["pgh-swarm-password"] },
    edge: {
      client_id: "11111111-0000-4000-8000-000000000005",
      service_account: "edge",
      secrets: ["github-client-id", "github-client-secret", "oauth2-cookie-secret"],
    },
    home: { client_id: "11111111-0000-4000-8000-000000000006", service_account: "home", secrets: ["usage-id-key"] },
    ops: {
      client_id: "11111111-0000-4000-8000-000000000007",
      service_account: "ops",
      secrets: ["db-admin-password", "pgh-indexer-password", "pgh-reader-password", "pgh-swarm-password"],
    },
  },
  networks: {
    vnet: "10.66.0.0/22", pod: "10.244.0.0/16", service: "10.0.0.0/16", edge: "10.66.0.160/28", private_endpoints: "10.66.1.0/27",
  },
  edge: {
    subnet: "edge",
    ip: "10.66.0.164",
    private_link_service: "pghacker-example-edge-pls",
    request_message: "Front Door pghacker-example-afd for pghacker-mcp",
  },
  app_settings: {
    PGHACKER_DEPLOYMENT: "azure",
    PGHACKER_DB_HOST: "pghacker-example-hdb.mock-cluster.westus3.horizondb.azure.com",
    PGHACKER_DB_PORT: "5432",
    PGHACKER_DB_NAME: "pghacker",
    PGHACKER_DB_SSLMODE: "require",
    PGHACKER_DB_ADMIN_USER: "pghacker_admin",
    PGHACKER_FOUNDRY_ENDPOINT: "https://pghacker-example-foundry.openai.azure.com",
    PGHACKER_FOUNDRY_SOL: "gpt-6.1-sol",
    PGHACKER_FOUNDRY_LUNA: "gpt-6-luna",
    PGHACKER_FOUNDRY_EMBEDDING: "text-embedding-3-small",
    PGHACKER_PUBLIC_URL: "https://pghacker-example.z01.azurefd.net",
    PGHACKER_PORTAL_URL: "https://pghacker-example.z01.azurefd.net",
    PGHACKER_PUBLIC_HOSTS: "pghacker-example.z01.azurefd.net",
    PGHACKER_FRONT_DOOR_ID: "4f6f1d1e-8b3a-4c55-9a7e-0b5d2c9e7a11",
    PGHACKER_RATE_PER_MINUTE: "60",
    PGHACKER_RATE_BURST: "30",
    PGHACKER_MCP_CALLS_PER_CLIENT: "10",
    PGHACKER_INTERNAL_RATE_PER_MINUTE: "300",
    PGHACKER_INTERNAL_RATE_BURST: "60",
    PGHACKER_MCP_POOL: "16",
    PGHACKER_QUERY_POOL: "8",
    PGHACKER_CRAWL_START_SESSIONS: "8",
    PGHACKER_CRAWL_MAX_SESSIONS: "16",
    PGHACKER_GITHUB_ORG: "contoso",
    PGHACKER_GITHUB_TEAM: "pghacker-operators",
    PGHACKER_PORTAL_ROLE: "user",
    APPLICATIONINSIGHTS_CONNECTION_STRING:
      "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;"
      + "LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/",
    PGHACKER_USAGE_DETAILS: "false",
    PGHACKER_FEEDBACK_RATE: "0.1",
    PGHACKER_FEEDBACK_COMMENTS: "false",
    PGHACKER_FEEDBACK_QUESTION_GAP: "180",
    PGHACKER_FEEDBACK_SESSION_GAP: "600",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "3",
    PGHACKER_FEEDBACK_CLIENTS: "copilot,codex,Visual Studio Code,claude,cursor",
    PGHACKER_FEEDBACK_DEBUG: "false",
  },
};

const SEED_IMAGE = `examplepghacr12345.azurecr.io/pghacker-seed@sha256:${"f".repeat(64)}`;
const NAMESPACE = OUTPUTS.namespace;
const service = (name, port) => `http://${name}.${NAMESPACE}.svc.cluster.local:${port}`;

/** The pods that run PilotSwarm's agents, and those on the PilotSwarm pool. */
const AGENTS = ["egress-check", "worker"];
const SWARM = ["egress-check", "portal", "worker"];

/** Every object: the manifests and every job, the restore with a seed image. */
function everything(outputs = OUTPUTS) {
  const seeded = { ...outputs, seed_image: SEED_IMAGE };
  return [...renderManifests(outputs), ...Object.keys(JOBS).map((name) => renderJob(seeded, name))];
}

/** Each Deployment and Job, with its pod spec and its pods' labels. */
function workloads(objects = everything()) {
  return objects.filter(podSpec).map((object) => ({
    name: object.metadata.name, kind: object.kind, spec: podSpec(object), labels: object.spec.template.metadata.labels ?? {},
  }));
}

function workload(objects, name) {
  return workloads(objects).find((entry) => entry.name === name);
}

/** A container's plain environment, as a map. */
function plainEnv(container) {
  return Object.fromEntries((container.env ?? []).filter((entry) => !entry.valueFrom).map((entry) => [entry.name, entry.value]));
}

/** Whether a pod selector selects pods with `labels`: {} selects all. */
function selects(selector, labels) {
  return Object.entries(selector?.matchLabels ?? {}).every(([key, value]) => labels[key] === value);
}

function policiesOf(policies, labels) {
  return policies.filter((policy) => selects(policy.spec.podSelector, labels));
}

/** A NetworkPolicy peer, as text. */
function peer(entry) {
  if (entry.ipBlock) return entry.ipBlock.cidr === "0.0.0.0/0" ? "internet" : `cidr:${entry.ipBlock.cidr}`;
  const pods = Object.entries(entry.podSelector?.matchLabels ?? {}).map(([key, value]) => `${key}=${value}`).join(",");
  if (entry.namespaceSelector) return `ns:${entry.namespaceSelector.matchLabels["kubernetes.io/metadata.name"]}/${pods}`;
  return pods;
}

/** Where pods with `labels` may connect, as peer:port, from every policy that selects them. */
function egressOf(policies, labels) {
  return policiesOf(policies, labels).flatMap((policy) => (policy.spec.egress ?? []).flatMap((rule) =>
    rule.to.flatMap((to) => rule.ports.map((port) => `${peer(to)}:${port.port}`))));
}

/** Who may connect to pods with `labels`, as peer:port. */
function ingressOf(policies, labels) {
  return policiesOf(policies, labels).flatMap((policy) => (policy.spec.ingress ?? []).flatMap((rule) =>
    rule.from.flatMap((from) => rule.ports.map((port) => `${peer(from)}:${port.port}`))));
}

test("the PilotSwarm pool runs the portal and the agents alone; every other pod runs on the services pool", () => {
  const all = workloads();
  assert.deepEqual(all.map((entry) => entry.name).sort(), [
    "bootstrap", "edge", "egress-check", "history", "home", "indexer", "mcp", "portal", "probe", "restore", "token-proxy", "worker",
  ]);
  const { key, value, effect } = OUTPUTS.swarm_taint;
  for (const { name, spec } of all) {
    const swarm = SWARM.includes(name);
    assert.deepEqual(spec.nodeSelector, { [POOL_LABEL]: swarm ? "swarm" : "services" }, name);
    // Only the PilotSwarm pool's pods tolerate its taint, so nothing else lands there.
    assert.deepEqual(spec.tolerations ?? [], swarm ? [{ key, operator: "Equal", value, effect }] : [], name);
  }
});

test("every deployment and job enrolls in the explicit mesh revision without relaxing Pod Security", () => {
  const objects = everything();
  assert.equal(objects.find((o) => o.kind === "Namespace").metadata.labels["istio.io/rev"], OUTPUTS.mesh.revision);
  for (const object of objects.filter(podSpec)) {
    const { labels, annotations } = object.spec.template.metadata;
    assert.equal(labels["istio.io/rev"], OUTPUTS.mesh.revision, object.metadata.name);
    assert.equal(annotations["sidecar.istio.io/inject"], "true");
    assert.equal(annotations["sidecar.istio.io/nativeSidecar"], "true");
    assert.equal(annotations["sidecar.istio.io/rewriteAppHTTPProbers"], "true");
    assert.equal(JSON.parse(annotations["proxy.istio.io/config"]).holdApplicationUntilProxyStarts, true);
  }
  for (const revision of [undefined, "", "asm-1-29", "latest", "asm-1-30; echo unsafe"]) {
    assert.throws(() => renderManifests({ ...OUTPUTS, mesh: { revision } }), /mesh.revision/);
  }
  const updated = renderManifests({ ...OUTPUTS, mesh: { revision: "asm-1-31" } });
  assert.equal(updated.find((o) => o.kind === "Deployment").spec.template.metadata.labels["istio.io/rev"], "asm-1-31");
  for (const service of objects.filter((o) => o.kind === "Service" && o.metadata.name !== "edge")) {
    for (const port of service.spec.ports) {
      const proxied = service.metadata.name === "home" || service.metadata.name === "portal"
        || (service.metadata.name === "mcp" && port.port === 3102);
      assert.equal(port.appProtocol, proxied ? "tcp" : "http");
      if (proxied) assert.ok(port.name.startsWith("tcp-"));
    }
  }
});

test("mesh policy requires strict TLS and exact authenticated callers on each destination port", () => {
  const infra = renderManifests(OUTPUTS, { stage: "infra" });
  const strict = infra.find((o) => o.kind === "PeerAuthentication" && o.metadata.name === "strict-mtls");
  assert.deepEqual(strict.spec, { mtls: { mode: "STRICT" } });
  const edge = infra.find((o) => o.kind === "PeerAuthentication" && o.metadata.name === "edge-ingress");
  assert.deepEqual(edge.spec, {
    selector: { matchLabels: { "app.kubernetes.io/name": "edge" } },
    mtls: { mode: "STRICT" }, portLevelMtls: { 8080: { mode: "DISABLE" } },
  });
  assert.equal(infra.filter((o) => o.kind === "PeerAuthentication").length, 2);
  const policies = infra.filter((o) => o.kind === "AuthorizationPolicy");
  assert.deepEqual(policies.find((o) => o.metadata.name === "default-deny").spec, {});
  const expected = [
    ["home", 3104, ["edge"]], ["mcp", 3102, ["edge"]], ["mcp", 3103, ["swarm"]],
    ["portal", 3001, ["edge"]], ["indexer", 3101, ["swarm"]],
    ["token-proxy", 3110, ["swarm", "indexer", "mcp", "ops"]], ["token-proxy", 3111, ["mcp", "home"]],
  ];
  assert.deepEqual(MESH_ACCESS.map(({ service, port, callers }) => [service, port, callers]), expected);
  const actual = policies.filter((o) => o.metadata.name.startsWith("allow-")).flatMap((policy) =>
    policy.spec.rules.map((rule) => [
      policy.spec.selector.matchLabels["app.kubernetes.io/name"], Number(rule.to[0].operation.ports[0]),
      rule.from[0].source.principals.map((principal) => {
        assert.ok(principal.startsWith(`cluster.local/ns/${NAMESPACE}/sa/`));
        return principal.split("/").at(-1);
      }),
    ]));
  assert.deepEqual(actual.sort(), expected.sort());
  const publicIngress = policies.find((o) => o.metadata.name === "edge-ingress");
  assert.deepEqual(publicIngress.spec.rules, [{
    from: [{ source: { ipBlocks: [OUTPUTS.networks.edge] } }], to: [{ operation: { ports: ["8080"] } }],
  }]);
  assert.equal(policies.length, 7);
  const destinations = infra.filter((o) => o.kind === "DestinationRule");
  assert.equal(destinations.length, 5);
  for (const rule of destinations) {
    assert.match(rule.spec.host, new RegExp(`^[a-z-]+\\.${NAMESPACE}\\.svc\\.cluster\\.local$`));
    assert.deepEqual(rule.spec.trafficPolicy, { tls: { mode: "ISTIO_MUTUAL" } });
  }
  const policy = infra.find((o) => o.kind === "ValidatingAdmissionPolicy");
  assert.equal(policy.spec.failurePolicy, "Fail");
  assert.equal(policy.spec.validations.length, 1);
  const binding = infra.find((o) => o.kind === "ValidatingAdmissionPolicyBinding");
  assert.equal(binding.spec.policyName, policy.metadata.name);
  assert.deepEqual(binding.spec.validationActions, ["Deny"]);
  assert.equal(binding.spec.matchResources.namespaceSelector.matchLabels["kubernetes.io/metadata.name"], NAMESPACE);
});

test("security checks test network-permitted paths with different identities and with no proxy", () => {
  const cases = meshCheckCases(OUTPUTS);
  const agent = cases.find((check) => check.name === "agent");
  const wrong = cases.find((check) => check.name === "wrong-agent");
  const plaintext = cases.find((check) => check.name === "plaintext-agent");
  assert.deepEqual(agent.labels, wrong.labels);
  assert.deepEqual(agent.labels, plaintext.labels);
  assert.equal(agent.identity, plaintext.identity);
  assert.notEqual(agent.identity, wrong.identity);
  for (const check of cases) {
    const job = renderMeshCheckJob(OUTPUTS, check, "abcd1234");
    const spec = podSpec(job);
    assert.equal(spec.serviceAccountName, OUTPUTS.identities[check.identity].service_account);
    assert.equal(spec.automountServiceAccountToken, false);
    assert.equal(spec.volumes, undefined);
    assert.equal(spec.containers[0].env, undefined);
    assert.equal(spec.containers[0].readinessProbe.exec.command.at(-1), "process.exit(1)");
    assert.equal(job.spec.template.metadata.annotations["sidecar.istio.io/inject"], check.plaintext ? "false" : "true");
    assert.ok(job.spec.activeDeadlineSeconds > 0);
    assert.notEqual(job.spec.template.metadata.labels["app.kubernetes.io/part-of"], "pghacker-mcp");
    assert.equal(job.metadata.labels["pghacker.io/mesh-check-run"], "abcd1234");
    assert.equal(job.spec.template.metadata.labels["pghacker.io/mesh-check-run"], "abcd1234");
  }
  const noUsage = { ...OUTPUTS, app_settings: {} };
  assert.ok(meshCheckCases(noUsage).every((check) => check.requests.every(({ url }) => !url.includes(":3111"))));
  assert.throws(() => renderMeshCheckJob(OUTPUTS, agent, "invalid/name"), /suffix/);
});

test("only the token proxy uses its workload identity's token, and no pod gets a Kubernetes API token", () => {
  const objects = everything();
  const accounts = new Map(objects.filter((o) => o.kind === "ServiceAccount").map((o) => [o.metadata.name, o]));
  assert.deepEqual([...accounts.keys()].sort(), Object.values(OUTPUTS.identities).map((identity) => identity.service_account).sort());
  for (const identity of Object.values(OUTPUTS.identities)) {
    const account = accounts.get(identity.service_account);
    // The federated credential's subject (deploy/terraform/identity.tf).
    assert.equal(account.metadata.namespace, NAMESPACE);
    assert.equal(account.metadata.annotations["azure.workload.identity/client-id"], identity.client_id);
    assert.equal(account.automountServiceAccountToken, false);
  }
  for (const { name, spec, labels } of workloads(objects)) {
    assert.equal(spec.automountServiceAccountToken, false, name);
    assert.ok(accounts.has(spec.serviceAccountName), name);
    assert.equal(labels["azure.workload.identity/use"], name === "token-proxy" ? "true" : undefined, name);
    assert.ok(!spec.containers.some((container) => (container.env ?? []).some((entry) => entry.name.startsWith("AZURE_FEDERATED"))), name);
  }
  // The portal and the agents are the swarm identity, which reads only its role's password.
  for (const name of SWARM) assert.equal(workload(objects, name).spec.serviceAccountName, "swarm", name);
  assert.deepEqual(OUTPUTS.identities.swarm.secrets, ["pgh-swarm-password"]);
});

test("every pod runs as a non-root user, unprivileged, with a read-only root, the runtime's seccomp profile and a memory limit", () => {
  const objects = everything();
  const labels = objects.find((o) => o.kind === "Namespace").metadata.labels;
  assert.equal(labels["pod-security.kubernetes.io/enforce"], "restricted");
  for (const { name, spec } of workloads(objects)) {
    assert.equal(spec.securityContext.runAsNonRoot, true, name);
    assert.ok(Object.values(USERS).includes(spec.securityContext.runAsUser), name);
    assert.equal(spec.securityContext.seccompProfile.type, "RuntimeDefault", name);
    for (const key of ["hostNetwork", "hostPID", "hostIPC"]) assert.ok(!spec[key], `${name} ${key}`);
    assert.ok(!(spec.volumes ?? []).some((volume) => volume.hostPath), name);
    for (const container of [...spec.containers, ...(spec.initContainers ?? [])]) {
      const context = container.securityContext;
      assert.equal(context.allowPrivilegeEscalation, false, name);
      assert.equal(context.readOnlyRootFilesystem, true, name);
      assert.deepEqual(context.capabilities, { drop: ["ALL"] }, name);
      assert.ok(!context.privileged, name);
      assert.ok(container.resources.limits.memory, name);
      assert.equal(container.image, name === "restore" ? SEED_IMAGE : OUTPUTS.image, name);
      // tini, the image's entrypoint, stays PID 1 and reaps the shells' children.
      assert.equal(container.command, undefined, name);
    }
  }
  // Each runs as its user in the image (USERS).
  const users = Object.fromEntries(workloads(objects).map(({ name, spec }) => [name, spec.securityContext.runAsUser]));
  assert.deepEqual(users, {
    "token-proxy": USERS.services, indexer: USERS.services, mcp: USERS.services, home: USERS.home, edge: USERS.proxy,
    portal: USERS.agent, worker: USERS.agent, "egress-check": USERS.agent,
    bootstrap: USERS.services, probe: USERS.services, restore: USERS.services, history: USERS.services,
  });
});

test("the agents reach the indexer's tools, pghacker-mcp's internal listener, the token proxy and their database, and nothing else", () => {
  const objects = everything();
  const policies = objects.filter((o) => o.kind === "NetworkPolicy");
  const deny = policies.find((policy) => policy.metadata.name === "default-deny");
  assert.deepEqual(deny.spec, { podSelector: {}, policyTypes: ["Ingress", "Egress"] });
  assert.deepEqual(workloads(objects).filter((entry) => entry.labels["pghacker.io/role"] === "agent").map((entry) => entry.name).sort(), AGENTS);
  for (const name of AGENTS) {
    const { labels } = workload(objects, name);
    assert.deepEqual(new Set(egressOf(policies, labels)), new Set([
      "ns:kube-system/k8s-app=kube-dns:53",
      "ns:aks-istio-system/app=istiod:15012",
      `app.kubernetes.io/name=indexer:${PORTS.indexer}`,
      `app.kubernetes.io/name=mcp:${PORTS.mcpInternal}`,
      `app.kubernetes.io/name=token-proxy:${PORTS.foundry}`,
      `cidr:${OUTPUTS.networks.private_endpoints}:5432`,
    ]), name);
    assert.deepEqual(ingressOf(policies, labels), [], name);
  }
  // On the other side, only those ports admit them.
  const admitting = policies.flatMap((policy) => (policy.spec.ingress ?? [])
    .filter((rule) => rule.from.some((from) => from.podSelector?.matchLabels?.["pghacker.io/role"] === "agent"))
    .map((rule) => `${policy.metadata.name}:${rule.ports.map((port) => port.port).join(",")}`));
  assert.deepEqual(admitting.sort(), [`indexer:${PORTS.indexer}`, `mcp:${PORTS.mcpInternal}`, `token-proxy:${PORTS.foundry}`]);
  // Every rule names its peers and ports: none admits everything.
  for (const policy of policies) {
    for (const rule of [...(policy.spec.ingress ?? []), ...(policy.spec.egress ?? [])]) {
      assert.ok((rule.from ?? rule.to).length > 0 && rule.ports.length > 0, policy.metadata.name);
    }
  }
});

test("only the edge takes traffic from outside the cluster, from the Private Link Service's subnet; the portal takes it from the edge alone", () => {
  const objects = everything();
  const policies = objects.filter((o) => o.kind === "NetworkPolicy");
  const fromOutside = policies.flatMap((policy) => (policy.spec.ingress ?? []).flatMap((rule) => rule.from
    .filter((from) => from.ipBlock)
    .map((from) => `${policy.metadata.name}:${from.ipBlock.cidr}:${rule.ports.map((port) => port.port).join(",")}`)));
  assert.deepEqual(fromOutside, [`edge:${OUTPUTS.networks.edge}:${PORTS.ingress}`]);
  const ingress = (name) => ingressOf(policies, workload(objects, name).labels).sort();
  // The portal trusts oauth2-proxy's identity headers, so only oauth2-proxy may reach it.
  assert.deepEqual(ingress("portal"), [`app.kubernetes.io/name=edge:${PORTS.portal}`]);
  assert.deepEqual(ingress("home"), [`app.kubernetes.io/name=edge:${PORTS.home}`]);
  assert.deepEqual(ingress("mcp"), [`app.kubernetes.io/name=edge:${PORTS.mcp}`, `pghacker.io/role=agent:${PORTS.mcpInternal}`]);
  assert.deepEqual(ingress("indexer"), [`pghacker.io/role=agent:${PORTS.indexer}`]);
  for (const name of ["bootstrap", "history", "probe", "restore"]) assert.deepEqual(ingress(name), [], name);
});

test("only services-pool pods that need the internet go out to it, on 443, where the firewall admits its named hosts", () => {
  const objects = everything();
  const policies = objects.filter((o) => o.kind === "NetworkPolicy");
  const out = workloads(objects).filter((entry) => egressOf(policies, entry.labels).some((to) => to.startsWith("internet:")));
  assert.deepEqual(out.map((entry) => entry.name).sort(), [
    "bootstrap", "edge", "history", "home", "indexer", "probe", "restore", "token-proxy",
  ]);
  for (const { name, spec, labels } of out) {
    assert.equal(spec.nodeSelector[POOL_LABEL], "services", name);
    for (const to of egressOf(policies, labels).filter((entry) => entry.startsWith("internet:"))) assert.equal(to, "internet:443", name);
  }
  // The internet leaves out the VNet, the pods, the Services and the metadata endpoint.
  const blocks = policies.flatMap((policy) => (policy.spec.egress ?? []).flatMap((rule) => rule.to.filter((to) => to.ipBlock?.cidr === "0.0.0.0/0")));
  assert.ok(blocks.length > 0);
  for (const block of blocks) assert.deepEqual(block.ipBlock, internetBlock(OUTPUTS.networks));
  // pghacker-mcp and the portal reach no internet host at all.
  for (const name of ["mcp", "portal"]) {
    assert.ok(!egressOf(policies, workload(objects, name).labels).some((to) => to.startsWith("internet")), name);
  }
});

test("each pod's secrets come from the Secret that the class it mounts syncs, and the administrator's password from the jobs' alone", () => {
  const objects = everything();
  const envOf = Object.fromEntries(Object.entries(VAULT_SECRETS).map(([name, secret]) => [secret, name]));
  const classes = new Map(objects.filter((o) => o.kind === "SecretProviderClass").map((o) => [o.metadata.name, o]));
  assert.deepEqual(
    [...classes.keys()].sort(),
    Object.entries(OUTPUTS.identities).filter(([, identity]) => identity.secrets.length > 0).map(([name]) => name).sort(),
  );
  for (const [name, spc] of classes) {
    const identity = OUTPUTS.identities[name];
    const { parameters, secretObjects } = spc.spec;
    assert.equal(spc.spec.provider, "azure");
    assert.deepEqual(
      { ...parameters, objects: undefined },
      { usePodIdentity: "false", clientID: identity.client_id, keyvaultName: OUTPUTS.key_vault_name, tenantId: OUTPUTS.tenant_id, objects: undefined },
      name,
    );
    assert.deepEqual(
      parse(parameters.objects).array.map((entry) => parse(entry)),
      identity.secrets.map((secret) => ({ objectName: secret, objectType: "secret" })),
      name,
    );
    assert.deepEqual(secretObjects, [{
      secretName: `${name}-secrets`,
      type: "Opaque",
      data: identity.secrets.map((secret) => ({ objectName: secret, key: envOf[secret] })),
    }], name);
  }
  for (const { name, kind, spec } of workloads(objects)) {
    const csi = (spec.volumes ?? []).filter((volume) => volume.csi);
    assert.ok(csi.length <= 1, name);
    const spc = csi[0] && classes.get(csi[0].csi.volumeAttributes.secretProviderClass);
    if (csi[0]) {
      assert.ok(spc, name);
      assert.deepEqual({ driver: csi[0].csi.driver, readOnly: csi[0].csi.readOnly }, { driver: "secrets-store.csi.k8s.io", readOnly: true });
      // The class reads the vault as the pod's own identity.
      assert.equal(OUTPUTS.identities[spc.metadata.name].service_account, spec.serviceAccountName, name);
    }
    for (const container of spec.containers) {
      const refs = (container.env ?? []).filter((entry) => entry.valueFrom);
      // The driver syncs the Secret only while a pod mounts the class.
      if (refs.length > 0) assert.ok(csi[0] && container.volumeMounts.some((mount) => mount.name === csi[0].name && mount.readOnly), name);
      for (const entry of refs) {
        const ref = entry.valueFrom.secretKeyRef;
        assert.equal(ref.name, spc.spec.secretObjects[0].secretName, `${name} ${entry.name}`);
        assert.ok(spc.spec.secretObjects[0].data.some((data) => data.key === ref.key), `${name} ${entry.name}`);
        assert.equal(entry.name, ref.key, name);
        if (ref.key === "PGHACKER_DB_ADMIN_PASSWORD") assert.equal(kind, "Job", name);
      }
    }
  }
  const admin = workloads(objects).filter((entry) => entry.spec.containers
    .some((container) => (container.env ?? []).some((env) => env.name === "PGHACKER_DB_ADMIN_PASSWORD")));
  assert.deepEqual(admin.map((entry) => entry.name).sort(), ["bootstrap", "history", "probe", "restore"]);
  // No secret's value is in a manifest: the output has none to give.
  assert.doesNotMatch(toYaml(objects), /password=|:\/\/[^:/\s]+:[^$@\s]+@/i);
});

test("without usage telemetry, the home page has no secret and mounts no class", () => {
  const plain = structuredClone(OUTPUTS);
  delete plain.app_settings.APPLICATIONINSIGHTS_CONNECTION_STRING;
  plain.identities.home.secrets = [];
  plain.identities.mcp.secrets = ["pgh-reader-password"];
  const objects = renderManifests(plain);
  assert.ok(!objects.some((o) => o.kind === "SecretProviderClass" && o.metadata.name === "home"));
  const { spec } = workload(objects, "home");
  assert.ok(!spec.volumes.some((volume) => volume.csi));
  assert.ok(!spec.containers[0].volumeMounts.some((mount) => mount.name === "secrets"));
  assert.ok(!(spec.containers[0].env ?? []).some((entry) => entry.valueFrom));
  // The usage key is Terraform's to grant: a grant the components don't use is refused.
  plain.identities.home.secrets = ["usage-id-key"];
  assert.throws(() => renderManifests(plain), /The identity home is granted PGHACKER_USAGE_ID_KEY, but its components use nothing/);
});

test("the renderer refuses an output that doesn't match the manifests or the components' secrets", () => {
  const changed = (change) => {
    const outputs = structuredClone(OUTPUTS);
    change(outputs);
    return () => renderManifests(outputs);
  };
  assert.throws(changed((o) => o.identities.swarm.secrets.push("db-admin-password")), /The identity swarm is granted/);
  assert.throws(changed((o) => { o.identities.mcp.secrets = ["pgh-reader-password"]; }), /The identity mcp is granted/);
  assert.throws(changed((o) => o.identities.edge.secrets.push("not-a-secret")), /not-a-secret has no environment variable/);
  assert.throws(changed((o) => { delete o.identities.home; }), /no workload identity home/);
  assert.throws(
    changed((o) => { o.identities.extra = { client_id: "x", service_account: "extra", secrets: [] }; }),
    /extra has no component/,
  );
  assert.throws(changed((o) => { o.pool_label = "agentpool"; }), /node pools' label/);
  assert.throws(changed((o) => { o.swarm_taint.effect = "PreferNoSchedule"; }), /taint/);
  assert.throws(changed((o) => { delete o.app_settings; }), /lacks app_settings/);
  assert.throws(changed((o) => { o.app_settings.PGHACKER_PUBLIC_URL = "http://example.org"; }), /https origin/);
  assert.throws(() => renderManifests(OUTPUTS, { stage: "workloads" }), /Unknown stage workloads/);
});

test("a template's placeholders take their values as JSON, outside comments; a missing value is an error", () => {
  const values = { NAME: "a: b # c", LIST: ["x", { y: 1 }], COUNT: 3 };
  assert.deepEqual(
    fillTemplate("# ${MISSING} stays a comment\nname: ${NAME}\nlist: ${LIST}\ncount: ${COUNT}\n", values),
    [{ name: "a: b # c", list: ["x", { y: 1 }], count: 3 }],
  );
  assert.deepEqual(fillTemplate("a: 1\n---\nb: 2\n", {}), [{ a: 1 }, { b: 2 }]);
  assert.throws(() => fillTemplate("name: ${MISSING}\n", values, "t.yaml"), /t\.yaml: no value for \$\{MISSING\}/);
  assert.throws(() => fillTemplate("a: [\n", {}, "bad.yaml"), /^Error: bad\.yaml: /);
  // Rendered, nothing is left to fill, and the stream reads back as the objects.
  const objects = everything();
  const text = toYaml(objects);
  assert.doesNotMatch(text, /\$\{[A-Z0-9_]+\}/);
  assert.deepEqual(parseAllDocuments(text).map((document) => document.toJS()), objects);
  // The pods' paths are the image's, never the machine's that rendered them.
  assert.ok(!text.includes(APP_ROOT));
  const worker = plainEnv(workload(objects, "worker").spec.containers[0]);
  for (const [name, value] of Object.entries(worker)) {
    if (value.startsWith("/")) assert.ok(value.startsWith(`${IMAGE_ROOT}/`) || value.startsWith("/etc/") || value.startsWith("/var/"), `${name}=${value}`);
  }
});

test("the edge runs oauth2-proxy with the Services as its upstreams, behind an internal load balancer that Front Door reaches by Private Link", () => {
  const objects = everything();
  const [container] = workload(objects, "edge").spec.containers;
  assert.equal(container.args[0], "/usr/local/bin/oauth2-proxy");
  for (const expected of [
    `--http-address=0.0.0.0:${PORTS.ingress}`, "--reverse-proxy=false",
    `--upstream=${service("mcp", PORTS.mcp)}/mcp`, `--upstream=${service("portal", PORTS.portal)}/api/`,
    `--upstream=${service("home", PORTS.home)}/`, "--redirect-url=https://pghacker-example.z01.azurefd.net/oauth2/callback",
  ]) {
    assert.ok(container.args.includes(expected), expected);
  }
  assert.ok(!container.args.some((arg) => arg.includes("127.0.0.1")));
  // Its credentials are in its environment, from its Secret, never in its arguments.
  assert.ok(!container.args.some((arg) => /^--(client-id|client-secret|cookie-secret)/.test(arg)));
  const services = objects.filter((o) => o.kind === "Service");
  const edge = services.find((o) => o.metadata.name === "edge");
  assert.equal(edge.spec.type, "LoadBalancer");
  // The PLS's NAT addresses, in the edge subnet, reach the pods as they are, for the edge's policy.
  assert.equal(edge.spec.externalTrafficPolicy, "Local");
  assert.deepEqual(edge.metadata.annotations, {
    "service.beta.kubernetes.io/azure-load-balancer-internal": "true",
    "service.beta.kubernetes.io/azure-load-balancer-internal-subnet": OUTPUTS.edge.subnet,
    "service.beta.kubernetes.io/azure-load-balancer-ipv4": OUTPUTS.edge.ip,
    "service.beta.kubernetes.io/azure-pls-create": "true",
    "service.beta.kubernetes.io/azure-pls-name": OUTPUTS.edge.private_link_service,
    "service.beta.kubernetes.io/azure-pls-ip-configuration-subnet": OUTPUTS.edge.subnet,
    "service.beta.kubernetes.io/azure-pls-ip-configuration-ip-address-count": "1",
    "service.beta.kubernetes.io/azure-pls-proxy-protocol": "false",
  });
  // Every other Service is the cluster's alone.
  for (const other of services.filter((o) => o !== edge)) assert.ok([undefined, "ClusterIP"].includes(other.spec.type), other.metadata.name);
});

test("the infra stage has everything but the app's workloads, so the edge's Private Link Service and the jobs' token proxy come first", () => {
  const all = renderManifests(OUTPUTS);
  const infra = renderManifests(OUTPUTS, { stage: "infra" });
  const app = renderManifests(OUTPUTS, { stage: "app" });
  const id = (object) => `${object.kind}/${object.metadata.name}`;
  assert.equal(new Set(all.map(id)).size, all.length);
  assert.deepEqual([...infra, ...app].map(id).sort(), all.map(id).sort());
  assert.equal(infra[0].kind, "Namespace");
  assert.ok(infra.some((object) => object.kind === "Service" && object.metadata.name === "edge"));
  // The jobs reach the models through the token proxy: the probe runs before the app.
  assert.deepEqual(infra.filter(podSpec).map((object) => object.metadata.name), ["token-proxy"]);
  assert.ok(infra.some((object) => object.kind === "PodDisruptionBudget" && object.metadata.name === "token-proxy"));
  assert.deepEqual([...new Set(app.map((object) => object.kind))].sort(), ["Deployment", "PodDisruptionBudget"]);
  assert.ok(!app.some((object) => object.metadata.name === "token-proxy"));
  for (const object of all.slice(1)) {
    const clusterScoped = ["ValidatingAdmissionPolicy", "ValidatingAdmissionPolicyBinding"].includes(object.kind);
    assert.equal(object.metadata.namespace, clusterScoped ? undefined : NAMESPACE, id(object));
  }
});

test("each job runs its command with the operator's arguments; the restore runs the seed image, and needs one", () => {
  assert.throws(() => renderJob(OUTPUTS, "restore"), /no seed image/);
  assert.throws(() => renderJob(OUTPUTS, "migrate"), /Unknown job migrate/);
  const seeded = { ...OUTPUTS, seed_image: SEED_IMAGE };
  const restore = renderJob(seeded, "restore", { args: ["--replace"] });
  assert.deepEqual([restore.kind, restore.metadata.name], ["Job", "restore"]);
  assert.equal(podSpec(restore).containers[0].image, SEED_IMAGE);
  assert.deepEqual(podSpec(restore).containers[0].args, [...JOBS.restore, "--replace"]);
  for (const name of Object.keys(JOBS)) {
    const job = renderJob(seeded, name);
    const spec = podSpec(job);
    assert.deepEqual(spec.containers[0].args, JOBS[name], name);
    assert.equal(spec.restartPolicy, "Never", name);
    assert.ok(job.spec.activeDeadlineSeconds > 0 && job.spec.ttlSecondsAfterFinished > 0, name);
    assert.equal(spec.serviceAccountName, name === "egress-check" ? "swarm" : "ops", name);
  }
});

test("the worker is replaced, never doubled, and has time to stop the crawl and drain its sessions", () => {
  const objects = everything();
  const worker = objects.find((object) => object.kind === "Deployment" && object.metadata.name === "worker");
  assert.equal(worker.spec.replicas, 1);
  assert.equal(worker.spec.strategy.type, "Recreate");
  const spec = podSpec(worker);
  const env = plainEnv(spec.containers[0]);
  // scripts/worker.mjs gives the crawl scheduler 20 s to stop, then PilotSwarm its drain.
  assert.ok(spec.terminationGracePeriodSeconds * 1000 > 20_000 + Number(env.PILOTSWARM_WORKER_SHUTDOWN_TIMEOUT_MS));
  assert.deepEqual(spec.containers[0].args, ["node", "scripts/worker.mjs", "--crawl"]);
  const quiet = structuredClone(OUTPUTS);
  quiet.app_settings.PGHACKER_CRAWL = "off";
  assert.deepEqual(podSpec(renderManifests(quiet).find((o) => o.metadata.name === "worker")).containers[0].args, ["node", "scripts/worker.mjs"]);
  // The Deployments with two replicas keep one through a node's drain.
  for (const deployment of objects.filter((object) => object.kind === "Deployment" && object.spec.replicas > 1)) {
    const name = deployment.metadata.name;
    const budget = objects.find((object) => object.kind === "PodDisruptionBudget" && object.metadata.name === name);
    assert.ok(budget, name);
    assert.deepEqual(budget.spec.selector.matchLabels, { "app.kubernetes.io/name": name }, name);
  }
});

test("PilotSwarm's model catalog reaches Foundry through the token proxy's Service, where the portal and the worker read it", () => {
  const objects = everything();
  const catalog = objects.find((object) => object.kind === "ConfigMap" && object.metadata.name === "model-catalog");
  const [provider] = JSON.parse(catalog.data["model_providers.json"]).providers;
  assert.equal(provider.baseUrl, `${service("token-proxy", PORTS.foundry)}/openai/v1`);
  for (const name of ["portal", "worker"]) {
    const { spec } = workload(objects, name);
    const volume = spec.volumes.find((entry) => entry.configMap?.name === "model-catalog");
    const mount = spec.containers[0].volumeMounts.find((entry) => entry.name === volume.name);
    assert.equal(plainEnv(spec.containers[0]).PS_MODEL_PROVIDERS_PATH, `${mount.mountPath}/model_providers.json`, name);
    assert.equal(mount.readOnly, true, name);
  }
});

test("the internet, to a NetworkPolicy, is every address but the private, link-local and platform ones, and the cluster's own", () => {
  const base = internetBlock();
  assert.equal(base.cidr, "0.0.0.0/0");
  for (const range of ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "169.254.0.0/16", "168.63.129.16/32"]) {
    assert.ok(base.except.includes(range), range);
  }
  // The example's networks are private already.
  assert.deepEqual(internetBlock(OUTPUTS.networks), base);
  // Any that isn't is left out too.
  assert.deepEqual(
    internetBlock({ vnet: "172.32.0.0/16", pod: "10.244.0.0/16", service: "192.0.2.0/24" }).except,
    [...base.except, "172.32.0.0/16", "192.0.2.0/24"],
  );
});

test("usage telemetry's revision is the image's digest, or its tag", () => {
  assert.equal(revisionOf(OUTPUTS.image), "sha256-0123456789ab");
  assert.equal(revisionOf("example.azurecr.io/pghacker-mcp:2026.01.15"), "2026.01.15");
  assert.equal(revisionOf("example.azurecr.io/pghacker-mcp"), undefined);
  assert.equal(revisionOf("localhost:5000/pghacker-mcp"), undefined);
  const objects = everything();
  for (const name of ["mcp", "home"]) {
    assert.equal(plainEnv(workload(objects, name).spec.containers[0]).CONTAINER_APP_REVISION, "sha256-0123456789ab", name);
  }
});

test("kubectl runs in the private cluster through az aks command invoke, in the app's namespace, its words quoted", () => {
  assert.equal(shellQuote("deployment/edge"), "deployment/edge");
  assert.equal(shellQuote("--tail=-1"), "--tail=-1");
  assert.equal(shellQuote("a b"), "'a b'");
  assert.equal(shellQuote("it's; rm -rf /"), `'it'"'"'s; rm -rf /'`);
  assert.equal(shellQuote(""), "''");
  assert.equal(
    kubectlCommand(OUTPUTS, ["rollout", "restart", "deployment/edge"]),
    "kubectl --namespace pghacker rollout restart deployment/edge",
  );
  assert.equal(
    kubectlCommand(OUTPUTS, ["get", "pods", "-o", "jsonpath={.items[*].metadata.name}"]),
    "kubectl --namespace pghacker get pods -o 'jsonpath={.items[*].metadata.name}'",
  );
  assert.deepEqual(invokeArgs(OUTPUTS, "kubectl version", { files: ["/tmp/pghacker-aks-1/manifests.yaml"] }), [
    "aks", "command", "invoke", "--subscription", OUTPUTS.subscription_id, "--resource-group", "pghacker-example",
    "--name", "pghacker-example-aks", "--command", "kubectl version", "--file", "/tmp/pghacker-aks-1/manifests.yaml",
    "--output", "json",
  ]);
  assert.deepEqual(invokeArgs(OUTPUTS, "kubectl version").slice(-4), ["--command", "kubectl version", "--output", "json"]);
});

test("a command's exit code and output come from Azure's result, and a command that Azure didn't run is an error", () => {
  const ran = { exitCode: 0, logs: "deployment.apps/edge restarted\n", provisioningState: "Succeeded", reason: null };
  assert.deepEqual(invokeResult(JSON.stringify(ran)), { status: 0, output: "deployment.apps/edge restarted\n" });
  assert.deepEqual(invokeResult({ ...ran, exitCode: 1, logs: "Unable to connect to the server: EOF" }),
    { status: 1, output: "Unable to connect to the server: EOF" });
  assert.throws(() => invokeResult({ provisioningState: "Failed", reason: "the pod didn't start" }),
    /didn't run in the cluster: Failed \(the pod didn't start\)/);
  assert.throws(() => invokeResult({ provisioningState: "Succeeded" }), /didn't run in the cluster/);
});

test("kubectl's JSON is read past its warnings, which share the output", () => {
  const job = { kind: "Job", metadata: { name: "probe" }, status: { succeeded: 1 } };
  assert.deepEqual(jsonFromOutput(`Warning: a deprecation\n${JSON.stringify(job, null, 4)}\n`), job);
  assert.deepEqual(jsonFromOutput(JSON.stringify(job, null, 4)), job);
  assert.throws(() => jsonFromOutput('Error from server (NotFound): jobs.batch "probe" not found'), /isn't JSON/);
});

test("a job ends complete or failed, by its conditions; a rollout is done as kubectl rollout status judges", () => {
  assert.equal(jobEnd({ status: { conditions: [{ type: "SuccessCriteriaMet", status: "True" }, { type: "Complete", status: "True" }] } }), "Complete");
  assert.equal(jobEnd({ status: { conditions: [{ type: "FailureTarget", status: "True" }, { type: "Failed", status: "True" }] } }), "Failed");
  assert.equal(jobEnd({ status: { active: 1 } }), null);
  assert.equal(jobEnd({ status: { conditions: [{ type: "Complete", status: "False" }] } }), null);

  const deployment = (status) => ({
    metadata: { name: "edge", generation: 2 }, spec: { replicas: 2 }, status: { observedGeneration: 2, ...status },
  });
  assert.equal(rolloutState(deployment({ replicas: 2, updatedReplicas: 2, availableReplicas: 2 })).done, true);
  assert.match(rolloutState(deployment({ observedGeneration: 1 })).wait, /new spec isn't observed/);
  assert.match(rolloutState(deployment({ replicas: 2, updatedReplicas: 1 })).wait, /1 of 2 new replicas updated/);
  assert.match(rolloutState(deployment({ replicas: 3, updatedReplicas: 2, availableReplicas: 2 })).wait, /1 old replicas terminating/);
  assert.match(rolloutState(deployment({ replicas: 2, updatedReplicas: 2, availableReplicas: 1 })).wait, /1 of 2 updated replicas available/);
  const stuck = rolloutState(deployment({ conditions: [{ type: "Progressing", status: "False", reason: "ProgressDeadlineExceeded" }] }));
  assert.equal(stuck.done, false);
  assert.match(stuck.failed, /edge exceeded its progress deadline/);
});
