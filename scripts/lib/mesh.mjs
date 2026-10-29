// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

export const MESH_NAMESPACE = "aks-istio-system";

export function meshRevision(outputs) {
  const revision = outputs.mesh?.revision;
  if (!/^asm-1-[3-9][0-9]$/.test(revision ?? "")) {
    throw new Error("Terraform's mesh.revision must be an explicit asm-1-30 or later revision.");
  }
  return revision;
}

/** Every internal destination/port and its callers; NetworkPolicies still distinguish swarm pods. */
export const MESH_ACCESS = [
  { service: "home", port: 3104, callers: ["edge"] },
  { service: "mcp", port: 3102, callers: ["edge"] },
  { service: "mcp", port: 3103, callers: ["swarm"] },
  { service: "portal", port: 3001, callers: ["edge"] },
  { service: "indexer", port: 3101, callers: ["swarm"] },
  { service: "token-proxy", port: 3110, callers: ["swarm", "indexer", "mcp", "ops"] },
  { service: "token-proxy", port: 3111, callers: ["mcp", "home"] },
];

export function meshPolicies(outputs) {
  const namespace = outputs.namespace;
  const principal = (identity) => {
    const account = outputs.identities[identity]?.service_account;
    if (!account) throw new Error(`Missing mesh caller identity ${identity}.`);
    return `cluster.local/ns/${namespace}/sa/${account}`;
  };
  const services = [...new Set(MESH_ACCESS.map((entry) => entry.service))];
  return services.flatMap((service) => [
    {
      apiVersion: "security.istio.io/v1", kind: "AuthorizationPolicy",
      metadata: { name: `allow-${service}`, namespace },
      spec: {
        selector: { matchLabels: { "app.kubernetes.io/name": service } },
        action: "ALLOW",
        rules: MESH_ACCESS.filter((entry) => entry.service === service).map(({ port, callers }) => ({
          from: [{ source: { principals: callers.map(principal) } }],
          to: [{ operation: { ports: [String(port)] } }],
        })),
      },
    },
    {
      apiVersion: "networking.istio.io/v1", kind: "DestinationRule",
      metadata: { name: `mtls-${service}`, namespace },
      spec: {
        host: `${service}.${namespace}.svc.cluster.local`,
        trafficPolicy: { tls: { mode: "ISTIO_MUTUAL" } },
      },
    },
  ]);
}

/** A missing injector must not turn a new application pod into a plaintext server. */
export function meshAdmission(outputs) {
  const name = `pghacker-mesh-${outputs.namespace}`;
  return [
    {
      apiVersion: "admissionregistration.k8s.io/v1", kind: "ValidatingAdmissionPolicy",
      metadata: { name },
      spec: {
        failurePolicy: "Fail",
        matchConstraints: {
          resourceRules: [{ apiGroups: [""], apiVersions: ["v1"], operations: ["CREATE"], resources: ["pods"] }],
          objectSelector: { matchLabels: { "app.kubernetes.io/part-of": "pghacker-mcp" } },
        },
        validations: [{
          expression: "has(object.spec.initContainers) && object.spec.initContainers.exists(c, c.name == 'istio-proxy' && has(c.restartPolicy) && c.restartPolicy == 'Always') && object.spec.initContainers.exists(c, c.name == 'istio-validation') && !object.spec.initContainers.exists(c, c.name == 'istio-init')",
          message: "Application pods require an injected native Istio sidecar and CNI traffic redirection.",
        }],
      },
    },
    {
      apiVersion: "admissionregistration.k8s.io/v1", kind: "ValidatingAdmissionPolicyBinding",
      metadata: { name },
      spec: {
        policyName: name, validationActions: ["Deny"],
        matchResources: { namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": outputs.namespace } } },
      },
    },
  ];
}

/** Native sidecars start before applications and terminate with completed Jobs. */
export function enrollInMesh(object, revision) {
  if (!["Deployment", "Job"].includes(object.kind)) return object;
  const metadata = object.spec.template.metadata;
  metadata.labels = { ...metadata.labels, "istio.io/rev": revision };
  metadata.annotations = {
    ...metadata.annotations,
    "sidecar.istio.io/inject": "true",
    "sidecar.istio.io/nativeSidecar": "true",
    "sidecar.istio.io/rewriteAppHTTPProbers": "true",
    "proxy.istio.io/config": JSON.stringify({ holdApplicationUntilProxyStarts: true }),
  };
  return object;
}

export function checkMeshProfile(cluster, outputs) {
  const revision = meshRevision(outputs);
  const profile = cluster.serviceMeshProfile;
  if (profile?.mode !== "Istio" || !profile.istio?.revisions?.includes(revision)) {
    throw new Error(`The cluster must have managed Istio ${revision} enabled; apply Terraform first.`);
  }
  if (profile.istio.components?.proxyRedirectionMechanism !== "CNIChaining") {
    throw new Error("The managed mesh must use CNIChaining; privileged workload init containers are not allowed.");
  }
  if (!cluster.agentPoolProfiles?.length) throw new Error("Cannot verify native-sidecar support without node pool versions.");
  for (const version of [cluster.currentKubernetesVersion ?? cluster.kubernetesVersion,
    ...(cluster.agentPoolProfiles ?? []).map((pool) => pool.currentOrchestratorVersion ?? pool.orchestratorVersion)]) {
    if (!/^1\.(3[3-9]|[4-9][0-9])(?:\.|$)/.test(version ?? "")) {
      throw new Error(`Native mesh sidecars require Kubernetes 1.33+ on the control plane and every pool, found ${version}.`);
    }
  }
}

/** Fail closed if admission skipped injection or injected a privileged/legacy proxy. */
export function checkMeshedPod(pod, { ready = false } = {}) {
  const spec = pod.spec ?? {};
  const proxy = spec.initContainers?.find((container) => container.name === "istio-proxy");
  if (!proxy || proxy.restartPolicy !== "Always"
    || !spec.initContainers.some((c) => c.name === "istio-validation")
    || spec.initContainers.some((c) => c.name === "istio-init")) {
    throw new Error(`${pod.metadata?.name}: expected an injected native Istio sidecar and CNI, not a legacy/absent proxy.`);
  }
  if (ready && !pod.status?.initContainerStatuses?.find((c) => c.name === "istio-proxy")?.ready) {
    throw new Error(`${pod.metadata?.name}: the Istio sidecar is not ready.`);
  }
}
