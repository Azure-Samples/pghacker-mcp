#!/usr/bin/env bash
# Starts one of the deployment's jobs, waits for it to finish, and prints its
# one-line JSON report, which Log Analytics receives a few minutes later.
#   deploy/scripts/run-job.sh probe | restore | history
# history builds the commit history (scripts/history-build.mjs) in the probe
# job's container, given 4 vCPU and 8 GiB.
# Run after `terraform init` in deploy/terraform (it reads the outputs).
set -euo pipefail

kind="${1:-}"
case "$kind" in
  probe | restore | history) ;;
  *) echo "usage: $0 probe|restore|history" >&2; exit 2 ;;
esac
job_kind="$kind"
overrides=()
if [ "$kind" = history ]; then
  job_kind=probe
  overrides=(--cpu 4 --memory 8Gi --command node --args scripts/history-build.mjs)
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
