# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Usage telemetry (PRIVACY.md): an Application Insights resource on
# the logs' workspace, to which pghacker-mcp and the home page send an event
# for each request. The events land in the workspace's AppEvents table, kept
# longer than the logs.
#
# The resource takes only Entra-authenticated telemetry: the token proxy
# (scripts/token-proxy.mjs, scripts/lib/monitor.mjs) signs the events with
# its workload identity, a Monitoring Metrics Publisher of it, so the
# instrumentation key in APPLICATIONINSIGHTS_CONNECTION_STRING only names the
# resource. Application
# Insights derives each event's country, region and city from the client's
# address, then discards the address: IP masking stays on.
#
# usage_enabled = false creates none of this, and the app records nothing.

locals {
  usage_name = "${var.name_prefix}-usage"
}

resource "azurerm_application_insights" "usage" {
  count = var.usage_enabled ? 1 : 0

  name                         = local.usage_name
  resource_group_name          = local.rg_name
  location                     = var.location
  workspace_id                 = azurerm_log_analytics_workspace.this.id
  application_type             = "web"
  local_authentication_enabled = false
  ip_masking_enabled           = true
  daily_data_cap_in_gb         = var.usage_daily_cap_gb
  tags                         = var.tags
}

# The token proxy's identity sends the events.
resource "azurerm_role_assignment" "proxy_usage_publisher" {
  count = var.usage_enabled ? 1 : 0

  scope                            = azurerm_application_insights.usage[0].id
  role_definition_name             = "Monitoring Metrics Publisher"
  principal_id                     = azurerm_user_assigned_identity.workload["proxy"].principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

# The events' table: queryable for usage_retention_days, and kept for
# usage_total_retention_days (beyond the first, in long-term retention, which
# search jobs read). A PATCH sets these two settings alone. It's sent again
# when they change; turning usage off leaves the table as it is.
resource "azapi_resource_action" "usage_table" {
  count = var.usage_enabled ? 1 : 0

  type        = "Microsoft.OperationalInsights/workspaces/tables@2025-02-01"
  resource_id = "${azurerm_log_analytics_workspace.this.id}/tables/AppEvents"
  method      = "PATCH"

  body = {
    properties = {
      retentionInDays      = var.usage_retention_days
      totalRetentionInDays = var.usage_total_retention_days
    }
  }

  depends_on = [azurerm_application_insights.usage]
}

# The key of the users' keyed hashes (PGHACKER_USAGE_ID_KEY). A new key gives
# every user a new ID, so retention would count them all as new: it changes
# only if usage is turned off and on again.
resource "random_password" "usage_id_key" {
  count = var.usage_enabled ? 1 : 0

  length  = 64
  special = false
}

resource "azurerm_key_vault_secret" "usage_id_key" {
  count = var.usage_enabled ? 1 : 0

  name         = "usage-id-key"
  key_vault_id = azurerm_key_vault.this.id
  value        = random_password.usage_id_key[0].result
  content_type = "text/plain"
  tags         = var.tags

  depends_on = [time_sleep.rbac_propagation]
}
