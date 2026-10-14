#!/usr/bin/env bash
# Starts one of the deployment's jobs, waits for it to finish, and prints its
# one-line JSON report, which Log Analytics receives a few minutes later.
#   deploy/scripts/run-job.sh probe | restore | history | bench IMAGE [OPTIONS]
# history builds the commit history (scripts/history-build.mjs) in the probe
# job's container, given 4 vCPU and 8 GiB. bench runs the engine benchmark
# (scripts/engine-bench.mjs) the same way, from IMAGE, built by
# ../bench.Dockerfile; OPTIONS go to pghacker-replay engines.
# Run after `terraform init` in deploy/terraform (it reads the outputs). history
# and bench also need jq.
set -euo pipefail

kind="${1:-}"
case "$kind" in
  probe | restore | history) ;;
  bench) [ -n "${2:-}" ] || { echo "usage: $0 bench IMAGE [OPTIONS]" >&2; exit 2; } ;;
  *) echo "usage: $0 probe|restore|history|bench IMAGE [OPTIONS]" >&2; exit 2 ;;
esac
job_kind="$kind"
image=""
command='["node", "scripts/history-build.mjs"]'
if [ "$kind" = history ]; then
  job_kind=probe
elif [ "$kind" = bench ]; then
  job_kind=probe
  image="$2"
  shift 2
  command='["node", "scripts/engine-bench.mjs"]'
  for arg in ${1+"$@"}; do
    command=$(jq -c --arg arg "$arg" '. + [$arg]' <<<"$command")
  done
fi
cd "$(dirname "$0")/../terraform"
subscription=$(terraform output -raw subscription_id)
rg=$(terraform output -raw resource_group_name)
job=$(terraform output -raw "${job_kind}_job_name")
workspace=$(terraform output -raw log_analytics_workspace_id)
if [ -z "$job" ] || [ "$job" = "null" ]; then
  echo "There's no $kind job: set seed_image and apply first." >&2
  exit 1
fi

overrides=()
if [ "$kind" = history ] || [ "$kind" = bench ]; then
  # An execution's template replaces the job's whole template, and the Azure
  # CLI ignores --cpu, --memory, --command and --args without --image. So the
  # execution gets a copy of the job's container, with its environment and
  # secrets, that runs the build or the benchmark instead of the probe.
  template=$(mktemp)
  trap 'rm -f "$template"' EXIT
  az containerapp job show --subscription "$subscription" --name "$job" --resource-group "$rg" \
    --query properties.template --output json |
    jq --arg image "$image" --argjson command "$command" '{containers: [.containers[0] | {name,
      image: (if $image == "" then .image else $image end), env,
      command: $command, resources: {cpu: 4, memory: "8Gi"}}]}' > "$template"
  overrides=(--yaml "$template")
fi

execution=$(az containerapp job start --subscription "$subscription" --name "$job" --resource-group "$rg" \
  ${overrides[@]+"${overrides[@]}"} --query name --output tsv)
echo "Started $execution; waiting for it to finish." >&2
while :; do
  state=$(az containerapp job execution show --subscription "$subscription" --name "$job" --resource-group "$rg" \
    --job-execution-name "$execution" --query properties.status --output tsv)
  case "$state" in
    Running | Processing | "") sleep 20 ;;
    *) echo "$execution: $state" >&2; break ;;
  esac
done

query="ContainerAppConsoleLogs_CL
  | where ContainerGroupName_s startswith '$execution'
  | where Log_s startswith '{' and Log_s contains '\"ok\"'
  | order by TimeGenerated desc | take 1 | project Log_s"
for _ in $(seq 1 30); do
  report=$(az monitor log-analytics query --workspace "$workspace" --analytics-query "$query" \
    --query '[0].Log_s' --output tsv 2>/dev/null || true)
  if [ -n "$report" ]; then
    echo "$report"
    [ "$state" = "Succeeded" ] && exit 0 || exit 1
  fi
  sleep 20
done
echo "The report hasn't reached Log Analytics yet; query it later:" >&2
echo "az monitor log-analytics query -w $workspace --analytics-query \"$query\"" >&2
exit 1
