#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Sets up the remote state that Terraform's azurerm backend uses for
# pghacker-mcp, in a resource group of its own, apart from the deployment:
#
# - the resource group;
# - a StorageV2 account reachable with Entra ID only: shared keys and public
#   blobs off, HTTPS and TLS 1.2 or later, no cross-tenant replication; with
#   blob versioning and 30 days of soft delete, so an earlier state can be
#   restored;
# - a private container;
# - Storage Blob Data Contributor on that container for whoever runs
#   Terraform (by default, the signed-in account).
#
# Running it again is safe: it creates only what's missing, and puts the
# account's security settings back if they've drifted. Progress goes to
# stderr; stdout gets the backend settings. For your environment, from the
# repository root:
#
#   deploy/scripts/bootstrap-state.sh --resource-group "<state-resource-group>" \
#     --storage-account "<globally-unique-state-account>" --location "<azure-region>" \
#     --subscription "$(az account show --query id --output tsv)" \
#     --key pghacker-mcp/local.tfstate > deploy/environments/local.backend.hcl
#
# --dry-run runs only the commands that read, and prints the ones that would
# change something.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: bootstrap-state.sh --resource-group <name> --storage-account <name> [options]

  --resource-group <name>    Resource group for the state, created if missing.
  --storage-account <name>   Storage account for the state: 3 to 24 lowercase
                             letters and digits, unique across Azure.
  --location <region>        Region for whichever of the two is created; not
                             needed when both exist.
  --subscription <ID>        Subscription. Default: the Azure CLI's current one.
  --container <name>         Blob container. Default: tfstate.
  --key <blob>               State blob, written into the backend settings.
                             Default: pghacker-mcp/prod.tfstate.
  --assignee <object ID>     Who gets Storage Blob Data Contributor on the
                             container. Default: the signed-in user or
                             service principal.
  --assignee-type <type>     User, Group or ServicePrincipal. Default: the
                             signed-in account's type, or User with --assignee.
  --dry-run                  Only read; print the commands that would write.
  -h, --help                 Show this help.
EOF
}

log() { printf 'bootstrap-state: %s\n' "$*" >&2; }
die() {
  log "$*"
  exit 1
}

resource_group=""
storage_account=""
location=""
subscription=""
container="tfstate"
key="pghacker-mcp/prod.tfstate"
assignee=""
assignee_type=""
dry_run=false

while [ $# -gt 0 ]; do
  case "$1" in
    --resource-group) resource_group="${2:?--resource-group needs a value}"; shift 2 ;;
    --storage-account) storage_account="${2:?--storage-account needs a value}"; shift 2 ;;
    --location) location="${2:?--location needs a value}"; shift 2 ;;
    --subscription) subscription="${2:?--subscription needs a value}"; shift 2 ;;
    --container) container="${2:?--container needs a value}"; shift 2 ;;
    --key) key="${2:?--key needs a value}"; shift 2 ;;
    --assignee) assignee="${2:?--assignee needs a value}"; shift 2 ;;
    --assignee-type) assignee_type="${2:?--assignee-type needs a value}"; shift 2 ;;
    --dry-run) dry_run=true; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage; die "unknown argument: $1" ;;
  esac
done

[ -n "$resource_group" ] || { usage; die "--resource-group is required"; }
[ -n "$storage_account" ] || { usage; die "--storage-account is required"; }
case "$storage_account" in
  *[!a-z0-9]*) die "--storage-account takes lowercase letters and digits only: $storage_account" ;;
esac
[ "${#storage_account}" -ge 3 ] && [ "${#storage_account}" -le 24 ] ||
  die "--storage-account must be 3 to 24 characters long: $storage_account"
case "$container" in
  *[!a-z0-9-]* | -* | *- | *--*) die "--container takes lowercase letters, digits and single hyphens: $container" ;;
esac
[ "${#container}" -ge 3 ] && [ "${#container}" -le 63 ] ||
  die "--container must be 3 to 63 characters long: $container"
case "$assignee_type" in
  "" | User | Group | ServicePrincipal) ;;
  *) die "--assignee-type must be User, Group or ServicePrincipal: $assignee_type" ;;
esac

# Runs a command that writes, or with --dry-run only prints it.
change() {
  if $dry_run; then
    local line="would run:"
    local arg
    for arg in "$@"; do
      line="$line $(printf '%q' "$arg")"
    done
    log "$line"
  else
    "$@"
  fi
}

command -v az >/dev/null 2>&1 || die "the Azure CLI (az) isn't installed"
if [ -z "$subscription" ]; then
  subscription=$(az account show --query id --output tsv 2>/dev/null) ||
    die "the Azure CLI isn't signed in: run az login"
fi
tenant=$(az account show --subscription "$subscription" --query tenantId --output tsv) ||
  die "the Azure CLI can't use subscription $subscription"
if $dry_run; then
  log "subscription $subscription, tenant $tenant (dry run)"
else
  log "subscription $subscription, tenant $tenant"
fi

tags=(purpose=terraform-state app=pghacker-mcp)

# --- Resource group ------------------------------------------------------------

if [ "$(az group exists --name "$resource_group" --subscription "$subscription")" = "true" ]; then
  log "resource group $resource_group exists"
else
  [ -n "$location" ] || die "resource group $resource_group doesn't exist; pass --location to create it"
  log "creating resource group $resource_group in $location"
  change az group create --name "$resource_group" --location "$location" \
    --subscription "$subscription" --tags "${tags[@]}" --only-show-errors --output none
fi

# --- Storage account -------------------------------------------------------------

security_flags=(
  --allow-blob-public-access false
  --allow-shared-key-access false
  --min-tls-version TLS1_2
  --https-only true
  --allow-cross-tenant-replication false
)
expected_security="false false TLS1_2 true false"

account_id=$(az storage account show --name "$storage_account" --resource-group "$resource_group" \
  --subscription "$subscription" --query id --output tsv 2>/dev/null || true)
if [ -n "$account_id" ]; then
  security=$(az storage account show --ids "$account_id" --query \
    "join(' ', [to_string(allowBlobPublicAccess), to_string(allowSharedKeyAccess), to_string(minimumTlsVersion), to_string(enableHttpsTrafficOnly), to_string(allowCrossTenantReplication)])" \
    --output tsv)
  if [ "$security" = "$expected_security" ]; then
    log "storage account $storage_account exists, with the expected security settings"
  else
    log "storage account $storage_account exists; restoring its security settings (found: $security)"
    change az storage account update --ids "$account_id" "${security_flags[@]}" --only-show-errors --output none
  fi
else
  available=$(az storage account check-name --name "$storage_account" --subscription "$subscription" \
    --query nameAvailable --output tsv)
  [ "$available" = "true" ] ||
    die "the storage account name $storage_account is taken, possibly in another resource group or subscription; choose another"
  [ -n "$location" ] || die "storage account $storage_account doesn't exist; pass --location to create it"
  log "creating storage account $storage_account in $location"
  change az storage account create --name "$storage_account" --resource-group "$resource_group" \
    --subscription "$subscription" --location "$location" --kind StorageV2 --sku Standard_LRS \
    "${security_flags[@]}" --tags "${tags[@]}" --only-show-errors --output none
  account_id="/subscriptions/$subscription/resourceGroups/$resource_group/providers/Microsoft.Storage/storageAccounts/$storage_account"
fi

# Versions and soft delete are what make a damaged or deleted state
# recoverable.
recovery=$(az storage account blob-service-properties show --account-name "$storage_account" \
  --resource-group "$resource_group" --subscription "$subscription" --query \
  "join(' ', [to_string(isVersioningEnabled), to_string(deleteRetentionPolicy.enabled), to_string(containerDeleteRetentionPolicy.enabled)])" \
  --output tsv 2>/dev/null || true)
if [ "$recovery" = "true true true" ]; then
  log "blob versioning and soft delete are on"
else
  log "turning on blob versioning and 30 days of soft delete for blobs and containers"
  change az storage account blob-service-properties update --account-name "$storage_account" \
    --resource-group "$resource_group" --subscription "$subscription" \
    --enable-versioning true --enable-delete-retention true --delete-retention-days 30 \
    --enable-container-delete-retention true --container-delete-retention-days 30 \
    --only-show-errors --output none
fi

# --- Container -------------------------------------------------------------------

# Through Azure Resource Manager, since nobody has a data-plane role yet.
container_exists=$(az storage container-rm exists --storage-account "$storage_account" \
  --resource-group "$resource_group" --subscription "$subscription" --name "$container" \
  --query exists --output tsv 2>/dev/null || true)
if [ "$container_exists" = "true" ]; then
  log "container $container exists"
else
  log "creating container $container"
  change az storage container-rm create --storage-account "$storage_account" \
    --resource-group "$resource_group" --subscription "$subscription" --name "$container" \
    --public-access off --only-show-errors --output none
fi

# --- Access for Terraform ----------------------------------------------------------

if [ -z "$assignee" ]; then
  account_type=$(az account show --subscription "$subscription" --query user.type --output tsv)
  case "$account_type" in
    user)
      assignee=$(az ad signed-in-user show --query id --output tsv) ||
        die "can't read the signed-in user's object ID; pass --assignee"
      assignee_type=${assignee_type:-User}
      ;;
    servicePrincipal)
      app_id=$(az account show --subscription "$subscription" --query user.name --output tsv)
      assignee=$(az ad sp show --id "$app_id" --query id --output tsv 2>/dev/null) ||
        die "can't find the object ID of the signed-in service principal $app_id; pass --assignee"
      assignee_type=${assignee_type:-ServicePrincipal}
      ;;
    *) die "can't tell who is signed in (account type '$account_type'); pass --assignee" ;;
  esac
fi
assignee_type=${assignee_type:-User}

scope="$account_id/blobServices/default/containers/$container"
role="Storage Blob Data Contributor"
assignment=$(az role assignment list --assignee "$assignee" --role "$role" --scope "$scope" \
  --subscription "$subscription" --query "[0].id" --output tsv 2>/dev/null || true)
if [ -n "$assignment" ]; then
  log "$assignee already has $role on the container"
else
  log "granting $assignee ($assignee_type) $role on the container"
  change az role assignment create --assignee-object-id "$assignee" \
    --assignee-principal-type "$assignee_type" --role "$role" --scope "$scope" \
    --subscription "$subscription" --description "Terraform state for pghacker-mcp" \
    --only-show-errors --output none
fi

# The environment's name, from the key: prod for pghacker-mcp/prod.tfstate.
env_name=$(basename "$key" .tfstate)

cat <<EOF
# State in the $resource_group resource group, made by
# ../scripts/bootstrap-state.sh. From ../terraform:
#   terraform init -backend-config=../environments/$env_name.backend.hcl
use_azuread_auth     = true
use_cli              = true
tenant_id            = "$tenant"
storage_account_name = "$storage_account"
container_name       = "$container"
key                  = "$key"
EOF

log "done. A new role assignment can take a few minutes to take effect; until it does, terraform init fails with 403 AuthorizationPermissionMismatch."
