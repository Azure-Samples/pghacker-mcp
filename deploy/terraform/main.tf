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

# The app's and the jobs' console and system logs.
resource "azurerm_log_analytics_workspace" "this" {
  name                = local.law_name
  resource_group_name = local.rg_name
  location            = var.location
  sku                 = "PerGB2018"
  retention_in_days   = var.log_retention_days
  tags                = var.tags
}

# Holds the app and seed images. The managed identity pulls (AcrPull); the
# operator pushes with Entra ID through ACR Tasks, so the admin user stays off.
resource "azurerm_container_registry" "this" {
  name                = var.acr_name
  resource_group_name = local.rg_name
  location            = var.location
  sku                 = var.acr_sku
  admin_enabled       = false
  tags                = var.tags
}

# The identity of the app and the jobs, used only by the platform: it pulls
# the image and resolves the Key Vault references in the apps' secrets.
resource "azurerm_user_assigned_identity" "app" {
  name                = local.identity_name
  resource_group_name = local.rg_name
  location            = var.location
  tags                = var.tags
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

# Resolving the apps' Key Vault references.
resource "azurerm_role_assignment" "app_kv_secrets_user" {
  scope                            = azurerm_key_vault.this.id
  role_definition_name             = "Key Vault Secrets User"
  principal_id                     = azurerm_user_assigned_identity.app.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

resource "azurerm_role_assignment" "app_acr_pull" {
  scope                            = azurerm_container_registry.this.id
  role_definition_name             = "AcrPull"
  principal_id                     = azurerm_user_assigned_identity.app.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

# New role assignments take a minute or so to reach Key Vault, the registry,
# Foundry and Application Insights. Writing secrets and creating the apps
# wait for this, rather than fail with 403s.
resource "time_sleep" "rbac_propagation" {
  create_duration = "60s"

  triggers = {
    deployer_kv = join(",", [for assignment in azurerm_role_assignment.deployer_kv_secrets_officer : assignment.id])
    app_kv      = azurerm_role_assignment.app_kv_secrets_user.id
    app_acr     = azurerm_role_assignment.app_acr_pull.id
    app_foundry = azurerm_role_assignment.app_foundry_openai_user.id
    app_usage   = join(",", azurerm_role_assignment.app_usage_publisher[*].id)
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
