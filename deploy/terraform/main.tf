# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

data "azurerm_client_config" "current" {
  lifecycle {
    # Checked before anything touches the vault: an apply by someone left out
    # of the list would take away their own Key Vault role.
    postcondition {
      condition     = length(var.deployer_object_ids) == 0 || contains([for id in var.deployer_object_ids : lower(id)], lower(self.object_id))
      error_message = "You (object ID ${self.object_id}) aren't in deployer_object_ids, so this apply would remove your own Key Vault role. Add yourself to the list, or ask someone in it to add you and apply."
    }
  }
}

resource "azurerm_resource_group" "this" {
  name     = var.resource_group_name
  location = var.location
  tags     = var.tags
}

# The containers' and the jobs' logs (Container Insights), and the firewall's.
resource "azurerm_log_analytics_workspace" "this" {
  name                = local.law_name
  resource_group_name = local.rg_name
  location            = var.location
  sku                 = "PerGB2018"
  retention_in_days   = var.log_retention_days
  tags                = var.tags
}

# Holds the app and seed images. The cluster's kubelet pulls (AcrPull,
# aks.tf); the operator pushes with Entra ID through ACR Tasks, so the admin
# user stays off. Premium's dedicated data endpoint lets the firewall admit
# this registry's storage alone.
resource "azurerm_container_registry" "this" {
  name                  = var.acr_name
  resource_group_name   = local.rg_name
  location              = var.location
  sku                   = var.acr_sku
  admin_enabled         = false
  data_endpoint_enabled = true
  tags                  = var.tags
}

# Access is by Azure RBAC, not access policies.
resource "azurerm_key_vault" "this" {
  name                       = var.key_vault_name
  resource_group_name        = local.rg_name
  location                   = var.location
  tenant_id                  = data.azurerm_client_config.current.tenant_id
  sku_name                   = "standard"
  rbac_authorization_enabled = true
  purge_protection_enabled   = var.key_vault_purge_protection_enabled
  tags                       = var.tags
}

# --- Role assignments ---------------------------------------------------------

# The appliers write the generated secrets and store the GitHub OAuth App's;
# terraform plan reads the generated ones back. Owning the subscription
# doesn't grant access to a vault's secrets.
resource "azurerm_role_assignment" "deployer_kv_secrets_officer" {
  for_each = local.deployer_object_ids

  scope                = azurerm_key_vault.this.id
  role_definition_name = "Key Vault Secrets Officer"
  principal_id         = each.value
}

# New role assignments take a minute or so to reach Key Vault, Foundry and
# Application Insights. Writing secrets waits for this, rather than fail with
# 403s, as the cluster waits for its own (aks_rbac_propagation).
resource "time_sleep" "rbac_propagation" {
  create_duration = "60s"

  triggers = {
    deployer_kv   = join(",", [for assignment in azurerm_role_assignment.deployer_kv_secrets_officer : assignment.id])
    proxy_foundry = azurerm_role_assignment.proxy_foundry_openai_user.id
    proxy_usage   = join(",", azurerm_role_assignment.proxy_usage_publisher[*].id)
  }
}

# The control plane's roles on the network and the kubelet's identity, and
# the kubelet's pull, before the cluster uses them.
resource "time_sleep" "aks_rbac_propagation" {
  create_duration = "60s"

  triggers = {
    network          = azurerm_role_assignment.aks_network.id
    route_table      = azurerm_role_assignment.aks_route_table.id
    kubelet_operator = azurerm_role_assignment.aks_kubelet_operator.id
    kubelet_pull     = azurerm_role_assignment.kubelet_acr_pull.id
  }
}

# --- Generated secrets ----------------------------------------------------------

# Every generated secret is also in Terraform's state (../TERRAFORM.md,
# "Secrets in state"). None uses punctuation, so all are safe in URLs and
# connection strings.

# HorizonDB's administrator password (horizondb.tf sets it on the cluster).
resource "random_password" "db_admin" {
  length      = 40
  special     = false
  min_upper   = 2
  min_lower   = 2
  min_numeric = 2
}

# Written after the cluster has the password, so the vault never holds one
# that the cluster would refuse.
resource "azurerm_key_vault_secret" "db_admin_password" {
  name         = "db-admin-password"
  key_vault_id = azurerm_key_vault.this.id
  value        = random_password.db_admin.result
  content_type = "text/plain"
  tags         = var.tags

  depends_on = [
    time_sleep.rbac_propagation,
    azapi_resource_action.set_password,
  ]
}

resource "random_password" "db_role" {
  for_each = local.db_role_secrets

  length      = 40
  special     = false
  min_upper   = 2
  min_lower   = 2
  min_numeric = 2
}

resource "azurerm_key_vault_secret" "db_role" {
  for_each = local.db_role_secrets

  name         = each.value
  key_vault_id = azurerm_key_vault.this.id
  value        = random_password.db_role[each.key].result
  content_type = "text/plain"
  tags         = var.tags

  depends_on = [time_sleep.rbac_propagation]
}

# oauth2-proxy encrypts its session cookie with this, and accepts only 16,
# 24 or 32 bytes: 32 alphanumeric characters.
resource "random_password" "oauth2_cookie" {
  length  = 32
  special = false
  upper   = true
  lower   = true
  numeric = true
}

resource "azurerm_key_vault_secret" "oauth2_cookie" {
  name         = "oauth2-cookie-secret"
  key_vault_id = azurerm_key_vault.this.id
  value        = random_password.oauth2_cookie.result
  content_type = "text/plain"
  tags         = var.tags

  depends_on = [time_sleep.rbac_propagation]
}

# The GitHub OAuth App's credentials, which the operator stores (../TERRAFORM.md,
# "Key Vault secrets"). Terraform creates each with a placeholder, tagged
# placeholder = true, so that the edge's identity can be granted it before
# its value exists; `az keyvault secret set` adds the real version, without
# the tag, and Terraform keeps out of it from then on. ../scripts/aks.mjs
# refuses to deploy while a placeholder is current.
resource "azurerm_key_vault_secret" "github" {
  for_each = toset(local.github_secrets)

  name         = each.key
  key_vault_id = azurerm_key_vault.this.id
  value        = "unset"
  content_type = "text/plain"
  tags         = merge(var.tags, { placeholder = "true" })

  depends_on = [time_sleep.rbac_propagation]

  lifecycle {
    ignore_changes = [value, tags, content_type, not_before_date, expiration_date]
  }
}
