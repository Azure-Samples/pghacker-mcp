// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The AKS deployment's manifests, from Terraform's `kubernetes` output
// (deploy/terraform/outputs.tf): the templates in deploy/k8s, whose ${NAME}
// placeholders take their values here, each as JSON, which YAML reads as it
// is; and the objects that follow from the output's data: a ServiceAccount per
// workload identity, federated with it (deploy/terraform/identity.tf); a
// SecretProviderClass per identity with secrets, which syncs them from Key
// Vault into the Kubernetes Secret its pods' environments name
// (scripts/lib/azure.mjs, componentEnvironments); and the model catalog.
// deploy/scripts/aks.mjs applies them, through the commands at the end of
// this file. Nothing here holds a secret: the output has none, and pods get
// theirs from Key Vault.

import { readFileSync } from "node:fs";
import path from "node:path";
import { parseAllDocuments, stringify } from "yaml";
import {
  PORTS, SERVICES, VAULT_SECRETS, componentEnvironments, componentSecrets, foundryCatalog, oauth2ProxyArgs,
  readAzureConfig, secretName, serviceOrigin,
} from "./azure.mjs";
import { APP_ROOT } from "./config.mjs";

/** Where the templates are. */
export const MANIFESTS_DIR = path.join(APP_ROOT, "deploy", "k8s");

/** The app's directory in the image (the Dockerfile's WORKDIR), which the pods' paths start from. */
export const IMAGE_ROOT = "/app";

/** The templates, in the order they're applied. */
const TEMPLATES = [
  "namespace.yaml", "network-policies.yaml", "token-proxy.yaml", "indexer.yaml", "mcp.yaml", "home.yaml",
  "edge.yaml", "portal.yaml", "worker.yaml",
];

/** The pools' label and the PilotSwarm pool's taint, as the templates name them. */
export const POOL_LABEL = "pghacker.io/pool";
const SWARM_TAINT = { key: POOL_LABEL, value: "swarm", effect: "NoSchedule" };

/** The ops jobs (deploy/k8s/jobs.yaml) and what each runs. */
export const JOBS = {
  bootstrap: ["/usr/local/bin/pghacker-indexer", "bootstrap"],
  probe: ["node", "scripts/probe.mjs"],
  restore: ["node", "scripts/corpus-restore.mjs"],
  history: ["node", "scripts/history-build.mjs"],
  "egress-check": ["node", "scripts/egress-check.mjs"],
};

/**
 * What `apply --infra` applies: everything but the app's workloads, so that
 * the edge's Private Link Service exists, and the jobs can run, before the
 * app does; and the token proxy, through which the jobs reach the models.
 */
const INFRA_KINDS = new Set([
  "Namespace", "ServiceAccount", "SecretProviderClass", "ConfigMap", "NetworkPolicy", "Service",
]);
const INFRA_WORKLOADS = new Set([SERVICES.tokenProxy]);

function isInfra(object) {
  return INFRA_KINDS.has(object.kind) || INFRA_WORKLOADS.has(object.metadata.name);
}

/**
 * The private, link-local and Azure platform ranges, which no pod reaches as
 * "the internet": the VNet, the pods and Services, the instance metadata
 * endpoint (169.254.169.254) and the platform's virtual address.
 */
const NOT_INTERNET = [
  "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "169.254.0.0/16", "168.63.129.16/32",
];

const LABELS = { "app.kubernetes.io/part-of": "pghacker-mcp" };

function ipv4(address) {
  return address.split(".").reduce((n, part) => n * 256 + Number(part), 0);
}

/** Whether IPv4 network `outer` holds `inner`. */
function contains(outer, inner) {
  const [a, aBits] = outer.split("/");
  const [b, bBits] = inner.split("/");
  if (Number(bBits) < Number(aBits)) return false;
  const size = 2 ** (32 - Number(aBits));
  return Math.floor(ipv4(a) / size) === Math.floor(ipv4(b) / size);
}

/** 0.0.0.0/0 less every range that isn't the internet, as a NetworkPolicy's ipBlock. */
export function internetBlock(networks = {}) {
  const extra = [networks.vnet, networks.pod, networks.service]
    .filter(Boolean)
    .filter((network) => !NOT_INTERNET.some((range) => contains(range, network)));
  return { cidr: "0.0.0.0/0", except: [...NOT_INTERNET, ...extra] };
}

/** A short name for the image's build, which usage telemetry tags its events with. */
export function revisionOf(image) {
  const digest = /@sha256:([0-9a-f]{12})/.exec(image);
  if (digest) return `sha256-${digest[1]}`;
  const tag = /:([\w][\w.-]{0,127})$/.exec(image);
  return tag ? tag[1] : undefined;
}

/** Each vault secret's environment variable. */
const ENV_OF_SECRET = Object.fromEntries(Object.entries(VAULT_SECRETS).map(([name, secret]) => [secret, name]));

/**
 * Checks that the output is the one this renderer knows: before its settings
 * are read, since without them readAzureConfig would read this process's
 * environment.
 */
function checkOutput(outputs) {
  for (const key of ["namespace", "image", "key_vault_name", "tenant_id", "identities", "networks", "edge", "app_settings"]) {
    if (outputs?.[key] === undefined || outputs[key] === null) throw new Error(`Terraform's kubernetes output lacks ${key}.`);
  }
  if (outputs.pool_label !== POOL_LABEL) {
    throw new Error(`The node pools' label is ${outputs.pool_label}, not ${POOL_LABEL}, which deploy/k8s names.`);
  }
  const taint = outputs.swarm_taint ?? {};
  if (taint.key !== SWARM_TAINT.key || taint.value !== SWARM_TAINT.value || taint.effect !== SWARM_TAINT.effect) {
    throw new Error(`The PilotSwarm pool's taint isn't ${SWARM_TAINT.key}=${SWARM_TAINT.value}:${SWARM_TAINT.effect}, which deploy/k8s tolerates.`);
  }
}

/** Checks that each identity's secrets, as Terraform grants them, are those its components' environments name. */
function checkSecrets(outputs, config) {
  const expected = componentSecrets(config);
  const names = new Set([...Object.keys(expected), ...Object.keys(outputs.identities)]);
  for (const name of names) {
    const identity = outputs.identities[name];
    if (!identity) throw new Error(`Terraform has no workload identity ${name}.`);
    if (!(name in expected)) throw new Error(`The workload identity ${name} has no component here.`);
    const granted = identity.secrets.map((secret) => {
      if (!ENV_OF_SECRET[secret]) throw new Error(`The vault secret ${secret} has no environment variable (VAULT_SECRETS).`);
      return ENV_OF_SECRET[secret];
    });
    const want = [...expected[name]].sort();
    if (JSON.stringify([...granted].sort()) !== JSON.stringify(want)) {
      throw new Error(`The identity ${name} is granted ${granted.join(", ") || "nothing"}, but its components use ${want.join(", ") || "nothing"}.`);
    }
  }
}

/** The settings and the templates' values. */
function prepare(outputs) {
  checkOutput(outputs);
  const config = readAzureConfig(outputs.app_settings, { secrets: false, root: IMAGE_ROOT });
  checkSecrets(outputs, config);
  const namespace = outputs.namespace;
  const origin = (service, port) => serviceOrigin(service, port, namespace);
  const env = componentEnvironments(config, { namespace, revision: revisionOf(outputs.image) });
  const values = {
    NAMESPACE: namespace,
    IMAGE: outputs.image,
    SEED_IMAGE: outputs.seed_image ?? outputs.image,
    EDGE_CIDR: outputs.networks.edge,
    PRIVATE_ENDPOINTS_CIDR: outputs.networks.private_endpoints,
    INTERNET: internetBlock(outputs.networks),
    EDGE_SUBNET: outputs.edge.subnet,
    EDGE_IP: outputs.edge.ip,
    EDGE_PLS_NAME: outputs.edge.private_link_service,
    EDGE_ARGS: [
      "/usr/local/bin/oauth2-proxy",
      ...oauth2ProxyArgs(config, {
        listen: `0.0.0.0:${PORTS.ingress}`,
        origins: {
          mcp: origin(SERVICES.mcp, PORTS.mcp),
          portal: origin(SERVICES.portal, PORTS.portal),
          home: origin(SERVICES.home, PORTS.home),
        },
      }),
    ],
    WORKER_ARGS: ["node", "scripts/worker.mjs", ...(config.crawl.enabled ? ["--crawl"] : [])],
    ENV_EDGE: env.edge,
    ENV_HOME: env.home,
    ENV_MCP: env.mcp,
    ENV_INDEXER: env.indexer,
    ENV_TOKEN_PROXY: env.tokenProxy,
    ENV_PORTAL: env.portal,
    ENV_WORKER: env.worker,
    ENV_OPS: env.ops,
    ENV_EGRESS_CHECK: env.egressCheck,
  };
  return { config, namespace, values, catalog: foundryCatalog(config.foundry, origin(SERVICES.tokenProxy, PORTS.foundry)) };
}

/**
 * A template's documents: each ${NAME} outside comments replaced with
 * JSON.stringify(values.NAME), which must exist.
 */
export function fillTemplate(text, values, file = "template") {
  const filled = text.split("\n").map((line) => {
    if (/^\s*#/.test(line)) return line;
    return line.replace(/\$\{([A-Z0-9_]+)\}/g, (_, name) => {
      if (values[name] === undefined || values[name] === null) throw new Error(`${file}: no value for \${${name}}.`);
      return JSON.stringify(values[name]);
    });
  }).join("\n");
  const objects = [];
  for (const document of parseAllDocuments(filled)) {
    if (document.errors.length) throw new Error(`${file}: ${document.errors[0].message}`);
    const object = document.toJS();
    if (object !== null && object !== undefined) objects.push(object);
  }
  return objects;
}

function template(file, values) {
  return fillTemplate(readFileSync(path.join(MANIFESTS_DIR, file), "utf8"), values, file);
}

/** The ServiceAccounts, one per workload identity, which its federated credential names. */
function serviceAccounts(outputs) {
  return Object.values(outputs.identities).map((identity) => ({
    apiVersion: "v1",
    kind: "ServiceAccount",
    metadata: {
      name: identity.service_account,
      namespace: outputs.namespace,
      labels: LABELS,
      annotations: { "azure.workload.identity/client-id": identity.client_id },
    },
    automountServiceAccountToken: false,
  }));
}

/**
 * The SecretProviderClasses, one per identity with secrets: the Secrets Store
 * CSI driver reads them from Key Vault with the identity's federated token,
 * when a pod mounts the class, and syncs them into the Kubernetes Secret
 * <identity>-secrets, under their environment variables' names.
 */
function secretProviderClasses(outputs) {
  return Object.entries(outputs.identities)
    .filter(([, identity]) => identity.secrets.length > 0)
    .map(([name, identity]) => ({
      apiVersion: "secrets-store.csi.x-k8s.io/v1",
      kind: "SecretProviderClass",
      metadata: { name, namespace: outputs.namespace, labels: LABELS },
      spec: {
        provider: "azure",
        parameters: {
          usePodIdentity: "false",
          clientID: identity.client_id,
          keyvaultName: outputs.key_vault_name,
          tenantId: outputs.tenant_id,
          objects: stringify({
            array: identity.secrets.map((secret) => stringify({ objectName: secret, objectType: "secret" })),
          }),
        },
        secretObjects: [{
          secretName: secretName(name),
          type: "Opaque",
          data: identity.secrets.map((secret) => ({ objectName: secret, key: ENV_OF_SECRET[secret] })),
        }],
      },
    }));
}

/** PilotSwarm's model catalog, which the portal and the worker mount. */
function modelCatalog(namespace, catalog) {
  return {
    apiVersion: "v1",
    kind: "ConfigMap",
    metadata: { name: "model-catalog", namespace, labels: LABELS },
    data: { "model_providers.json": `${JSON.stringify(catalog, null, 2)}\n` },
  };
}

/** The pod spec of a Deployment or a Job; undefined for other kinds. */
export function podSpec(object) {
  return ["Deployment", "Job"].includes(object.kind) ? object.spec.template.spec : undefined;
}

/**
 * Drops the CSI volumes, and their mounts, of classes that don't exist: an
 * identity without secrets (the home page without usage telemetry) has none.
 */
function dropMissingClasses(objects, classes) {
  for (const object of objects) {
    const spec = podSpec(object);
    if (!spec?.volumes) continue;
    const missing = new Set(spec.volumes
      .filter((volume) => volume.csi && !classes.has(volume.csi.volumeAttributes?.secretProviderClass))
      .map((volume) => volume.name));
    if (missing.size === 0) continue;
    spec.volumes = spec.volumes.filter((volume) => !missing.has(volume.name));
    for (const container of spec.containers) {
      container.volumeMounts = (container.volumeMounts ?? []).filter((mount) => !missing.has(mount.name));
    }
  }
  return objects;
}

/**
 * The deployment's objects, in the order to apply them. `stage` is "infra"
 * (everything but the app's workloads; the token proxy included), "app" (the
 * other workloads) or "all".
 */
export function renderManifests(outputs, { stage = "all" } = {}) {
  if (!["infra", "app", "all"].includes(stage)) throw new Error(`Unknown stage ${stage}: infra, app or all.`);
  const { namespace, values, catalog } = prepare(outputs);
  const classes = secretProviderClasses(outputs);
  const [namespaceObject, ...rest] = TEMPLATES.flatMap((file) => template(file, values));
  const objects = dropMissingClasses(
    [namespaceObject, ...serviceAccounts(outputs), ...classes, modelCatalog(namespace, catalog), ...rest],
    new Set(classes.map((spc) => spc.metadata.name)),
  );
  if (stage === "all") return objects;
  return objects.filter((object) => isInfra(object) === (stage === "infra"));
}

/** One ops job, `name`, running its command with `args` after it. */
export function renderJob(outputs, name, { args = [] } = {}) {
  if (!JOBS[name]) throw new Error(`Unknown job ${name}: ${Object.keys(JOBS).join(", ")}.`);
  if (name === "restore" && !outputs.seed_image) {
    throw new Error("There's no seed image: set seed_image, apply Terraform, then run the restore job.");
  }
  const { values } = prepare(outputs);
  const classes = new Set(secretProviderClasses(outputs).map((spc) => spc.metadata.name));
  const jobs = template("jobs.yaml", { ...values, JOB_ARGS: [...JOBS[name], ...args] });
  const job = jobs.find((object) => object.metadata.name === name);
  if (!job) throw new Error(`deploy/k8s/jobs.yaml has no job ${name}.`);
  return dropMissingClasses([job], classes)[0];
}

/** Objects as one YAML stream, for kubectl apply. */
export function toYaml(objects) {
  return objects.map((object) => stringify(object, { lineWidth: 0 })).join("---\n");
}

// --- Commands in the cluster --------------------------------------------------

// The cluster is private: nothing outside its VNet reaches the API server.
// deploy/scripts/aks.mjs runs kubectl in the cluster instead, through Azure
// Resource Manager (az aks command invoke, AKS's run command): a pod in the
// aks-command namespace runs the command with the caller's Entra ID token, and
// Azure returns its exit code and its output, stdout and stderr together, up to
// 512 kB.

/** `word`, quoted for a POSIX shell where it has to be. */
export function shellQuote(word) {
  const text = String(word);
  return /^[\w@%+=:,./-]+$/.test(text) ? text : `'${text.replaceAll("'", `'"'"'`)}'`;
}

/** kubectl with `args`, in the deployment's namespace, as a shell command. */
export function kubectlCommand(outputs, args) {
  return ["kubectl", "--namespace", outputs.namespace, ...args].map(shellQuote).join(" ");
}

/**
 * The Azure CLI's arguments that run `command` in the cluster, with `files`
 * attached: each lands, by its base name, where the command runs.
 */
export function invokeArgs(outputs, command, { files = [] } = {}) {
  return [
    "aks", "command", "invoke", "--subscription", outputs.subscription_id,
    "--resource-group", outputs.resource_group_name, "--name", outputs.cluster_name,
    "--command", command, ...files.flatMap((file) => ["--file", file]), "--output", "json",
  ];
}

/**
 * The command's exit code and output, from the Azure CLI's JSON; throws when
 * Azure didn't run it. The CLI itself succeeds whatever the command's exit code.
 */
export function invokeResult(json) {
  const result = typeof json === "string" ? JSON.parse(json) : json;
  if (result?.provisioningState !== "Succeeded" || !Number.isInteger(result.exitCode)) {
    const reason = result?.reason ? ` (${result.reason})` : "";
    throw new Error(`The command didn't run in the cluster: ${result?.provisioningState ?? "no result"}${reason}.`);
  }
  return { status: result.exitCode, output: result.logs ?? "" };
}

/** The JSON document in kubectl's output, from the line that opens it to the one that closes it, past any warning. */
export function jsonFromOutput(output) {
  const lines = output.split("\n");
  const start = lines.findIndex((line) => line.startsWith("{"));
  const end = lines.findLastIndex((line) => line.startsWith("}"));
  if (start < 0 || end < start) throw new Error(`kubectl's output isn't JSON: ${output.trim().slice(0, 300)}`);
  return JSON.parse(lines.slice(start, end + 1).join("\n"));
}

/** A Job's end, from its conditions: "Complete", "Failed", or null while it runs. */
export function jobEnd(job) {
  const ended = (job.status?.conditions ?? []).find((c) => ["Complete", "Failed"].includes(c.type) && c.status === "True");
  return ended?.type ?? null;
}

/**
 * Where a Deployment's rollout is, as kubectl rollout status judges it:
 * `done`; `failed`, past its progress deadline; or why to `wait`.
 */
export function rolloutState(deployment) {
  const { metadata = {}, spec = {}, status = {} } = deployment;
  const name = metadata.name;
  if ((status.observedGeneration ?? 0) < (metadata.generation ?? 0)) {
    return { name, done: false, wait: `${name}: its new spec isn't observed yet` };
  }
  const progressing = (status.conditions ?? []).find((c) => c.type === "Progressing");
  if (progressing?.reason === "ProgressDeadlineExceeded") {
    return { name, done: false, failed: `${name} exceeded its progress deadline` };
  }
  const wanted = spec.replicas ?? 1;
  const updated = status.updatedReplicas ?? 0;
  if (updated < wanted) return { name, done: false, wait: `${name}: ${updated} of ${wanted} new replicas updated` };
  const old = (status.replicas ?? 0) - updated;
  if (old > 0) return { name, done: false, wait: `${name}: ${old} old replicas terminating` };
  const available = status.availableReplicas ?? 0;
  if (available < updated) return { name, done: false, wait: `${name}: ${available} of ${updated} updated replicas available` };
  return { name, done: true };
}
