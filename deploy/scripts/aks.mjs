#!/usr/bin/env node
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Deploys pghacker-mcp to the AKS cluster that deploy/terraform creates, from
// its `kubernetes` output (deploy/README.md):
//   node deploy/scripts/aks.mjs render [--stage infra|app|all]   the manifests, to stdout
//   node deploy/scripts/aks.mjs apply --infra    everything but the app's workloads: the namespace,
//                                                identities, policies, Services (the edge's
//                                                Private Link Service) and the token proxy, which
//                                                the jobs reach the models through; then the
//                                                bootstrap job
//   node deploy/scripts/aks.mjs apply            the same, then the workloads, waiting for them
//   node deploy/scripts/aks.mjs job <name> [-- args]   bootstrap, probe, restore, history or
//                                                egress-check: runs it, prints its JSON report
//   node deploy/scripts/aks.mjs kubectl -- <args>   kubectl in the cluster, in the app's namespace
//   node deploy/scripts/aks.mjs approve-edge     approves Front Door's connection to the edge
// The cluster is private: its API server has no public address. kubectl runs in
// the cluster instead, through Azure Resource Manager (az aks command invoke),
// with the signed-in user's Entra ID token, so nobody needs a network path to
// the API server. It needs the Azure CLI, signed in as one of
// deployer_object_ids with a role that may run commands on the cluster (Owner
// or Contributor), and Terraform initialized in deploy/terraform. Deploys
// never reset data: the bootstrap only creates what's missing and migrates.
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import {
  JOBS, invokeArgs, invokeResult, jobEnd, jsonFromOutput, kubectlCommand, renderJob, renderManifests, rolloutState,
  toYaml,
} from "../../scripts/lib/aks.mjs";
import { checkMeshedPod, checkMeshProfile, meshRevision } from "../../scripts/lib/mesh.mjs";
import {
  MESH_JOB_SNAPSHOT, MESH_POD_SNAPSHOT, MESH_SERVER_SNAPSHOT, hasMutualTlsMetric, meshCheckCases, meshCheckSnapshot,
  renderMeshCheckJob, tcpDenialEvidence,
} from "../../scripts/lib/mesh-check.mjs";
import { randomBytes } from "node:crypto";

const TERRAFORM_DIR = path.join(import.meta.dirname, "..", "terraform");
const log = (message) => console.error(`[aks] ${message}`);

const USAGE = `usage: node deploy/scripts/aks.mjs <command>
  render [--stage infra|app|all]    print the manifests
  apply [--infra]                   apply the manifests (with --infra, all but the app's workloads)
                                    and run the bootstrap job
  job <${Object.keys(JOBS).join("|")}> [-- args]
                                    run an ops job and print its report
  kubectl -- <args>                 run kubectl in the cluster, in the app's namespace
  approve-edge                      approve Front Door's connection to the edge's Private Link Service
  check-mesh                        run temporary positive/negative identity and TLS probes, then remove them`;

/** Runs a command; its output is returned when `capture`, else shown. Throws when it fails. */
function run(command, args, { capture = false, input, allowFailure = false } = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    input,
    stdio: [input === undefined ? "inherit" : "pipe", capture ? "pipe" : "inherit", capture ? "pipe" : "inherit"],
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) throw new Error(`${command}: ${result.error.message}`);
  if (result.status !== 0 && !allowFailure) {
    const detail = capture ? `: ${(result.stderr || result.stdout || "").trim().slice(-2000)}` : "";
    throw new Error(`${command} ${args.slice(0, 3).join(" ")} failed (exit ${result.status})${detail}`);
  }
  return result;
}

function outputs() {
  const result = run("terraform", [`-chdir=${TERRAFORM_DIR}`, "output", "-json", "kubernetes"], { capture: true });
  return JSON.parse(result.stdout);
}

/**
 * Runs `command` in the cluster (az aks command invoke) with `files` attached,
 * and resolves with its exit code and output. Throws when it fails, unless
 * `allowFailure`. Azure gives the command's pod 60 seconds to start; when it
 * doesn't, it's tried again.
 */
async function invoke(out, command, { files = [], allowFailure = false } = {}) {
  for (let attempt = 1; ; attempt++) {
    const result = run("az", invokeArgs(out, command, { files }), { capture: true, allowFailure: true });
    if (result.status === 0) {
      const outcome = invokeResult(result.stdout);
      if (outcome.status !== 0 && !allowFailure) {
        throw new Error(`${command} failed in the cluster (exit ${outcome.status}): ${outcome.output.trim().slice(-2000)}`);
      }
      return outcome;
    }
    const detail = (result.stderr || result.stdout || "").trim();
    if (attempt >= 3 || !/KubernetesPerformanceError|did not start within|retry may help/i.test(detail)) {
      throw new Error(`az aks command invoke failed: ${detail.slice(-2000)}`);
    }
    log(`The command's pod didn't start in time; trying again (${attempt + 1} of 3).`);
    await delay(15_000);
  }
}

function kubectl(out, args, options) {
  return invoke(out, kubectlCommand(out, args), options);
}

/** Read-only prerequisite checks; never silently deploy without the security layer. */
async function checkMesh(out) {
  // Older az aks show models discard proxyRedirectionMechanism even when
  // ARM returns it. Read the same API as Terraform's CNI update instead.
  const cluster = JSON.parse(run("az", [
    "rest", "--method", "get", "--subscription", out.subscription_id,
    "--url", `https://management.azure.com/subscriptions/${out.subscription_id}/resourceGroups/${encodeURIComponent(out.resource_group_name)}/providers/Microsoft.ContainerService/managedClusters/${encodeURIComponent(out.cluster_name)}?api-version=2026-03-01`,
    "--output", "json",
  ], { capture: true }).stdout).properties;
  checkMeshProfile(cluster, out);
  await kubectl(out, [
    "wait", "--for=condition=Established", "--timeout=120s",
    "crd/peerauthentications.security.istio.io", "crd/authorizationpolicies.security.istio.io",
    "crd/destinationrules.networking.istio.io",
  ]);
  const revision = meshRevision(out);
  const checks = await invoke(out, [
    `kubectl -n aks-istio-system rollout status deployment/istiod-${revision} --timeout=120s`,
    "kubectl -n aks-istio-system rollout status daemonset/azure-service-mesh-istio-cni-addon-node --timeout=120s",
    `kubectl -n aks-istio-system get deployment istiod-${revision} -o json`,
  ].join(" && "));
  const control = jsonFromOutput(checks.output);
  if (!control.spec.template.spec.containers.some((container) =>
    container.env?.some((env) => env.name === "ENABLE_NATIVE_SIDECARS" && env.value === "true"))) {
    throw new Error("The managed mesh must enable native sidecars so jobs can terminate.");
  }
}

/**
 * Runs `script` in the cluster with `objects`, as manifests.yaml in its
 * directory, written through a file that only this user can read.
 */
async function withManifests(out, objects, script) {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-aks-"));
  try {
    const file = path.join(dir, "manifests.yaml");
    writeFileSync(file, toYaml(objects), { mode: 0o600 });
    const { output } = await invoke(out, script, { files: [file] });
    process.stderr.write(output);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** kubectl apply of `objects`. */
function apply(out, objects) {
  return withManifests(out, objects, kubectlCommand(out, ["apply", "--filename", "manifests.yaml"]));
}

/** Refuses to deploy the edge while the GitHub OAuth App's credentials are Terraform's placeholders. */
function checkGitHubSecrets(out) {
  for (const name of ["github-client-id", "github-client-secret"]) {
    const result = run("az", [
      "keyvault", "secret", "show", "--vault-name", out.key_vault_name, "--name", name,
      "--query", "tags.placeholder", "--output", "tsv",
    ], { capture: true });
    if (result.stdout.trim() === "true") {
      throw new Error(`The Key Vault secret ${name} is still Terraform's placeholder: register the GitHub OAuth App and store its credentials (deploy/README.md, "First deployment"), then apply again.`);
    }
  }
}

/** Runs the job `name` to its end; prints its logs' JSON report; resolves with whether it succeeded. */
async function runJob(out, name, args = []) {
  return runRenderedJob(out, renderJob(out, name, { args }));
}

async function runRenderedJob(out, job, { plaintext = false } = {}) {
  const name = job.metadata.name;
  // One command removes the job's last run and starts this one.
  await withManifests(out, [job], [
    kubectlCommand(out, ["delete", "job", name, "--ignore-not-found", "--wait=true"]),
    kubectlCommand(out, ["apply", "--filename", "manifests.yaml"]),
  ].join(" && "));
  log(`Job ${name} started; waiting for it to finish.`);
  const deadline = Date.now() + (job.spec.activeDeadlineSeconds + 600) * 1000;
  let state = null;
  let status = {};
  while (Date.now() < deadline) {
    const current = jsonFromOutput((await kubectl(out, ["get", "job", name, "--output", "json"])).output);
    status = current.status ?? {};
    state = jobEnd(current);
    if (state) break;
    await delay(10_000);
  }
  const succeeded = state === "Complete" && (status.succeeded ?? 0) > 0;
  const pods = jsonFromOutput((await kubectl(out, ["get", "pods", "-l", `job-name=${name}`, "-o", "json"])).output);
  if (!pods.items?.length) throw new Error(`Job ${name} has no pod to verify for mesh injection.`);
  for (const pod of pods.items) {
    if (plaintext) {
      if ([...(pod.spec.initContainers ?? []), ...pod.spec.containers].some((c) => c.name === "istio-proxy")) {
        throw new Error(`Plaintext test ${name} unexpectedly has a proxy.`);
      }
    } else checkMeshedPod(pod);
  }
  const logs = (await kubectl(out, ["logs", `job/${name}`, "--tail=-1"], { allowFailure: true })).output;
  const report = logs.trim().split("\n").reverse().find((line) => line.startsWith("{"));
  if (report) console.log(report);
  else process.stderr.write(logs);
  log(`Job ${name} ${succeeded ? "succeeded" : state ? "failed" : "didn't finish in time"}.`);
  return succeeded;
}

async function tcpDenials(out) {
  const services = ["home", "mcp", "portal"];
  const output = (await kubectl(out, [
    "get", "pods", "-l", `app.kubernetes.io/name in (${services.join(",")})`,
    "--field-selector=status.phase=Running", "-o", `jsonpath=${MESH_SERVER_SNAPSHOT}`,
  ])).output;
  const pods = output.trim().split(/\r?\n/).filter(Boolean).map((line) => {
    const [name, service, container, extra] = line.split("\t");
    if (!name || !services.includes(service) || !container || extra !== undefined) {
      throw new Error(`Invalid mesh server snapshot: ${line.slice(0, 200)}`);
    }
    return { name, service, container };
  });
  for (const service of services) {
    if (!pods.some((pod) => pod.service === service)) throw new Error(`No running ${service} pod for TCP authorization evidence.`);
  }
  const commands = pods.map(({ name, service, container }) => {
    const program = `const meta=${JSON.stringify({ name, service })};` + String.raw`
      const response=await fetch("http://127.0.0.1:15000/stats?filter=tcp.rbac.denied");
      if(!response.ok)throw Error("Envoy stats: HTTP "+response.status);
      const text=await response.text();
      const matches=[...text.matchAll(/^tcp\.[^\n]*rbac\.denied:\s+(\d+)$/gm)];
      if(!matches.length)throw Error("No TCP RBAC counters for "+meta.name);
      console.log(JSON.stringify({...meta,denied:matches.reduce((n,m)=>n+Number(m[1]),0)}));`;
    return kubectlCommand(out, ["exec", name, "-c", container, "--", "node", "--input-type=module", "-e", program]);
  });
  const records = (await invoke(out, commands.join(" && "))).output.trim().split(/\r?\n/).filter(Boolean).map((line) => JSON.parse(line));
  if (records.length !== pods.length || new Set(records.map((record) => record.name)).size !== pods.length) {
    throw new Error("Incomplete TCP authorization counters from the mesh servers.");
  }
  const counts = Object.fromEntries(services.map((service) => [service, 0]));
  for (const record of records) {
    if (!pods.some((pod) => pod.name === record.name && pod.service === record.service)
      || !Number.isSafeInteger(record.denied) || record.denied < 0) {
      throw new Error("Invalid TCP authorization counter from a mesh server.");
    }
    counts[record.service] += record.denied;
  }
  return counts;
}

/** Deliberate security probes, opt-in and separate from the production rollout. */
async function checkCommunication(out) {
  await checkMesh(out);
  const suffix = randomBytes(4).toString("hex");
  const cases = meshCheckCases(out);
  const jobs = cases.map((check) => renderMeshCheckJob(out, check, suffix));
  const reports = [];
  const before = await tcpDenials(out);
  try {
    await apply(out, jobs);
    // Run the independent identities together: ARM's command pod is expensive
    // to start, and sequential per-job polling needlessly prolongs the check.
    const waited = await kubectl(out, [
      "wait", "--for=condition=Complete", "--timeout=240s", ...jobs.map((job) => `job/${job.metadata.name}`),
    ], { allowFailure: true });
    const selector = `pghacker.io/mesh-check-run=${suffix}`;
    const listed = meshCheckSnapshot((await invoke(out, [
      "echo MESH_JOBS_BEGIN",
      kubectlCommand(out, ["get", "jobs", "-l", selector, "-o", `jsonpath=${MESH_JOB_SNAPSHOT}`]),
      "echo MESH_PODS_BEGIN",
      kubectlCommand(out, ["get", "pods", "-l", selector, "-o", `jsonpath=${MESH_POD_SNAPSHOT}`]),
      "echo MESH_SNAPSHOT_END",
    ].join(" && "))).output);
    for (const [index, check] of cases.entries()) {
      const name = jobs[index].metadata.name;
      const job = listed.find((object) => object.kind === "Job" && object.metadata.name === name);
      const pods = listed.filter((object) => object.kind === "Pod" && object.metadata.labels?.["job-name"] === name);
      if (!job || !pods.length) throw new Error(`Mesh check ${check.name} has no job/pod to verify.`);
      for (const pod of pods) {
        if (check.plaintext) {
          if ([...(pod.spec.initContainers ?? []), ...pod.spec.containers].some((c) => c.name === "istio-proxy")) {
            throw new Error(`Plaintext test ${check.name} unexpectedly has a proxy.`);
          }
        } else checkMeshedPod(pod);
      }
      reports.push({ name: check.name, ok: jobEnd(job) === "Complete" && (job.status?.succeeded ?? 0) > 0 });
    }
    const logs = await invoke(out, jobs.map((job) =>
      kubectlCommand(out, ["logs", `job/${job.metadata.name}`, "-c", "check", "--tail=-1"])).join(" && "));
    process.stderr.write(logs.output);
    if (waited.status !== 0 || reports.some((report) => !report.ok)) {
      throw new Error(`Mesh checks failed or timed out: ${reports.filter((report) => !report.ok).map((report) => report.name).join(", ")}.`);
    }
  } finally {
    await kubectl(out, ["delete", "job", ...jobs.map((job) => job.metadata.name), "--ignore-not-found", "--wait=true"]);
  }
  const tcpAuthorizationDenied = tcpDenialEvidence(before, await tcpDenials(out), cases);
  // Read server-side negotiated connection metrics from both proxy replicas.
  const pods = jsonFromOutput((await kubectl(out, [
    "get", "pods", "-l", "app.kubernetes.io/name=token-proxy", "-o", "json",
  ])).output).items.filter((pod) => !pod.metadata.deletionTimestamp);
  let metrics = "";
  for (const pod of pods) {
    checkMeshedPod(pod, { ready: true });
    metrics += (await kubectl(out, [
      "exec", pod.metadata.name, "-c", "token-proxy", "--", "node", "-e",
      "fetch('http://127.0.0.1:15090/stats/prometheus').then(r=>{if(!r.ok)throw Error(r.status);return r.text()}).then(t=>console.log(t.split('\\n').filter(l=>l.startsWith('istio_requests_total{')).join('\\n'))).catch(e=>{console.error(e);process.exitCode=1})",
    ])).output + "\n";
  }
  for (const identity of ["swarm", "indexer", "mcp", "ops",
    ...(out.app_settings.APPLICATIONINSIGHTS_CONNECTION_STRING ? ["home"] : [])]) {
    const account = out.identities[identity].service_account;
    if (!hasMutualTlsMetric(metrics, out.namespace, account)) {
      throw new Error(`No destination-reported negotiated mTLS evidence for ${account} at the token proxy.`);
    }
  }
  console.log(JSON.stringify({ ok: true, checks: reports, mutualTls: true, tcpAuthorizationDenied }));
}

/** Waits up to 15 minutes for each Deployment's rollout, as kubectl rollout status would. */
async function waitForRollouts(out, objects) {
  const names = objects.filter((o) => o.kind === "Deployment").map((o) => o.metadata.name);
  if (names.length === 0) return;
  const deadline = Date.now() + 15 * 60_000;
  let last = "";
  for (;;) {
    const listed = jsonFromOutput((await kubectl(out, ["get", "deployment", ...names, "--output", "json"])).output);
    const states = (listed.kind === "List" ? listed.items : [listed]).map(rolloutState);
    const failed = states.find((state) => state.failed);
    if (failed) throw new Error(`The rollout failed: ${failed.failed}.`);
    const waiting = states.filter((state) => !state.done).map((state) => state.wait);
    if (waiting.length === 0) {
      for (const name of names) {
        const pods = jsonFromOutput((await kubectl(out, [
          "get", "pods", "-l", `app.kubernetes.io/name=${name}`, "-o", "json",
        ])).output);
        const active = pods.items.filter((pod) => !pod.metadata.deletionTimestamp);
        if (!active.length) throw new Error(`Deployment ${name} has no active meshed pods.`);
        for (const pod of active) checkMeshedPod(pod, { ready: true });
      }
      log(`Rolled out: ${names.join(", ")}.`);
      return;
    }
    if (Date.now() > deadline) throw new Error(`The rollout didn't finish within 15 minutes: ${waiting.join("; ")}.`);
    const now = waiting.join("; ");
    if (now !== last) log(`Waiting for the rollout: ${now}.`);
    last = now;
    await delay(10_000);
  }
}

/**
 * Approves the pending connections to the edge's Private Link Service whose
 * request is Front Door's (the origin's request message). Anyone may request
 * a connection; only Front Door's is approved.
 */
function approveEdge(out) {
  const target = ["--subscription", out.subscription_id, "--resource-group", out.node_resource_group];
  const service = JSON.parse(run("az", [
    "network", "private-link-service", "show", ...target, "--name", out.edge.private_link_service, "--output", "json",
  ], { capture: true }).stdout);
  const connections = service.privateEndpointConnections ?? [];
  const pending = connections.filter((c) => c.privateLinkServiceConnectionState?.status === "Pending");
  const ours = pending.filter((c) => c.privateLinkServiceConnectionState?.description === out.edge.request_message);
  for (const connection of ours) {
    run("az", [
      "network", "private-link-service", "connection", "update", ...target,
      "--service-name", out.edge.private_link_service, "--name", connection.name,
      "--connection-status", "Approved", "--description", "Approved: Front Door's origin",
    ], { capture: true });
    log(`Approved ${connection.name}.`);
  }
  for (const connection of pending.filter((c) => !ours.includes(c))) {
    log(`Left pending ${connection.name}: its request (${connection.privateLinkServiceConnectionState?.description}) isn't Front Door's.`);
  }
  if (ours.length === 0) {
    const approved = connections.filter((c) => c.privateLinkServiceConnectionState?.status === "Approved").length;
    log(approved ? `No pending request from Front Door; ${approved} connection(s) approved already.` : "No request from Front Door yet: apply Terraform with create_origin = true first.");
  }
}

const [command, ...rest] = process.argv.slice(2);
try {
  switch (command) {
    case "render": {
      const at = rest.indexOf("--stage");
      const stage = at >= 0 ? rest[at + 1] : "all";
      process.stdout.write(toYaml(renderManifests(outputs(), { stage })));
      break;
    }
    case "apply": {
      const infraOnly = rest.includes("--infra");
      const out = outputs();
      await checkMesh(out);
      if (!infraOnly) checkGitHubSecrets(out);
      const infra = renderManifests(out, { stage: "infra" });
      await apply(out, infra);
      await waitForRollouts(out, infra);
      if (!(await runJob(out, "bootstrap"))) throw new Error("The bootstrap failed: the workloads were left as they were.");
      if (!infraOnly) {
        const workloads = renderManifests(out, { stage: "app" });
        await apply(out, workloads);
        await waitForRollouts(out, workloads);
      }
      log(infraOnly ? "Applied everything but the workloads." : "Deployed.");
      break;
    }
    case "job": {
      const [name, separator, ...args] = rest;
      if (!name) throw new Error(USAGE);
      if (separator !== undefined && separator !== "--") throw new Error(USAGE);
      const out = outputs();
      await checkMesh(out);
      process.exitCode = (await runJob(out, name, args)) ? 0 : 1;
      break;
    }
    case "check-mesh":
      await checkCommunication(outputs());
      break;
    case "kubectl": {
      const [separator, ...args] = rest;
      if (separator !== "--" || args.length === 0) throw new Error(USAGE);
      const { status, output } = await kubectl(outputs(), args, { allowFailure: true });
      process.stdout.write(output);
      process.exitCode = status;
      break;
    }
    case "approve-edge":
      approveEdge(outputs());
      break;
    default:
      console.error(USAGE);
      process.exitCode = 2;
  }
} catch (error) {
  console.error(`[aks] ${error.message}`);
  process.exitCode = 1;
}
