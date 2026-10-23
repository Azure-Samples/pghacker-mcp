# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Azure AI Foundry: an AIServices account with the app's model deployments,
# reachable only through its private endpoint, from the services pool alone
# (network.tf's NSG). Key (local) authentication is disabled by design: the
# token proxy (scripts/token-proxy.mjs) adds its workload identity's Entra
# token to the requests of PilotSwarm, the embedders and the crawl scheduler,
# which hold no credential.

locals {
  foundry_private_endpoint_name = "${local.foundry_account_name}-pe"

  # Inside the VNet, the account's Cognitive Services, Azure OpenAI and AI
  # Services host names resolve through these zones to the private endpoint.
  foundry_dns_zones = toset([
    "privatelink.cognitiveservices.azure.com",
    "privatelink.openai.azure.com",
    "privatelink.services.ai.azure.com",
  ])
}

# Deployments and the private endpoint go through Azure
# Resource Manager, which the account's network rules don't restrict.
resource "azurerm_cognitive_account" "foundry" {
  name                          = local.foundry_account_name
  resource_group_name           = local.rg_name
  location                      = var.location
  kind                          = "AIServices"
  sku_name                      = "S0"
  custom_subdomain_name         = local.foundry_account_name
  local_auth_enabled            = false
  public_network_access_enabled = false
  tags                          = var.tags

  # Data loss prevention: the account reaches no outside host for a caller,
  # such as an image or file by URL, On Your Data's search service or a
  # remote MCP server, since no host is allowed (fqdns). PilotSwarm's agents
  # reach the models through the token proxy, which refuses such requests
  # first (scripts/lib/foundry.mjs); the app uses none of them.
  outbound_network_access_restricted = true

  # Set in full, bypass included: Azure reports a default bypass that would
  # otherwise show as a change in every plan.
  network_acls {
    default_action = "Deny"
    bypass         = "None"
  }
}

# Azure refuses concurrent deployment operations on an account. Instances of
# one resource can't depend on each other, but azurerm takes a lock on the
# account for each deployment's create, update and delete, so they run one
# at a time. Versions are pinned: the account upgrades a deployment only when
# its version retires, which then shows in the plan as a replacement; set the
# new version in foundry_deployments to match.
resource "azurerm_cognitive_deployment" "this" {
  for_each = var.foundry_deployments

  name                   = each.key
  cognitive_account_id   = azurerm_cognitive_account.foundry.id
  version_upgrade_option = "OnceCurrentVersionExpired"

  model {
    format  = "OpenAI"
    name    = each.value.model
    version = each.value.version
  }

  sku {
    name     = each.value.sku
    capacity = each.value.capacity
  }
}

# Names missing from the zones, such as other accounts' private endpoints,
# fall back to public DNS (NxDomainRedirect).
resource "azurerm_private_dns_zone" "foundry" {
  for_each = local.foundry_dns_zones

  name                = each.key
  resource_group_name = local.rg_name
  tags                = var.tags
}

resource "azurerm_private_dns_zone_virtual_network_link" "foundry" {
  for_each = local.foundry_dns_zones

  name                 = local.vnet_name
  private_dns_zone_id  = azurerm_private_dns_zone.foundry[each.key].id
  virtual_network_id   = azurerm_virtual_network.this.id
  registration_enabled = false
  resolution_policy    = "NxDomainRedirect"
  tags                 = var.tags
}

# One DNS zone group with all three zones, so each of the account's host
# names gets its record.
resource "azurerm_private_endpoint" "foundry" {
  name                          = local.foundry_private_endpoint_name
  location                      = var.location
  resource_group_name           = local.rg_name
  subnet_id                     = azurerm_subnet.private_endpoints.id
  custom_network_interface_name = "${local.foundry_private_endpoint_name}-nic"
  tags                          = var.tags

  private_service_connection {
    name                           = local.foundry_private_endpoint_name
    private_connection_resource_id = azurerm_cognitive_account.foundry.id
    subresource_names              = ["account"]
    is_manual_connection           = false
  }

  private_dns_zone_group {
    name                 = "foundry"
    private_dns_zone_ids = [for zone in local.foundry_dns_zones : azurerm_private_dns_zone.foundry[zone].id]
  }

  # The connection changes the account, which refuses that while a
  # deployment operation runs.
  depends_on = [azurerm_cognitive_deployment.this]
}

# As for HorizonDB: with public access off, an unapproved connection would
# leave the app without models, so this stops the apply instead.
data "azapi_resource" "foundry_private_endpoint_connection" {
  type                   = "Microsoft.CognitiveServices/accounts@2024-10-01"
  resource_id            = azurerm_cognitive_account.foundry.id
  response_export_values = { connections = "properties.privateEndpointConnections" }

  depends_on = [azurerm_private_endpoint.foundry]

  lifecycle {
    postcondition {
      condition = try(anytrue([
        for connection in self.output.connections :
        lower(try(connection.properties.privateEndpoint.id, "")) == lower(azurerm_private_endpoint.foundry.id)
        && try(connection.properties.privateLinkServiceConnectionState.status, "") == "Approved"
      ]), false)
      error_message = "The Foundry account hasn't approved the connection of the private endpoint ${local.foundry_private_endpoint_name}, so the app couldn't reach its models. Approve it under the account's Networking, Private endpoint connections, and apply again."
    }
  }
}

# The token proxy's identity calls the models: inference only, on this
# account.
resource "azurerm_role_assignment" "proxy_foundry_openai_user" {
  scope                            = azurerm_cognitive_account.foundry.id
  role_definition_name             = "Cognitive Services OpenAI User"
  principal_id                     = azurerm_user_assigned_identity.workload["proxy"].principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}
