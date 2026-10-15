# Values that the configuration reads back from azurerm, shaped like Azure's.
# The provider still validates the IDs that one resource passes to another,
# so every such ID is well formed here; the tests override the ones that
# must differ between resources or instances.

mock_data "azurerm_client_config" {
  defaults = {
    object_id       = "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
    tenant_id       = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    subscription_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
  }
}

# A few of the AzureFrontDoor.Backend tag's ranges, out of order; the real tag
# has about 157 IPv4 and 92 IPv6 ranges.
mock_data "azurerm_network_service_tags" {
  defaults = {
    name             = "AzureFrontDoor.Backend"
    address_prefixes = ["147.243.0.0/16", "13.73.248.16/29", "20.21.37.40/29", "2603:1000:4::5e0/123", "20.36.120.104/29", "2a01:111:2050::/44", "13.107.208.0/24"]
    ipv4_cidrs       = ["147.243.0.0/16", "13.73.248.16/29", "20.21.37.40/29", "20.36.120.104/29", "13.107.208.0/24"]
    ipv6_cidrs       = ["2603:1000:4::5e0/123", "2a01:111:2050::/44"]
  }
}

mock_resource "azurerm_resource_group" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example" }
}

mock_resource "azurerm_log_analytics_workspace" {
  defaults = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.OperationalInsights/workspaces/pghacker-example-logs"
    workspace_id = "6d1c3f2a-9b8e-4f70-a5d2-3c4b5a697e81"
  }
}

mock_resource "azurerm_container_registry" {
  defaults = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ContainerRegistry/registries/examplepghacr12345"
    login_server = "examplepghacr12345.azurecr.io"
  }
}

mock_resource "azurerm_user_assigned_identity" {
  defaults = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-id"
    client_id    = "00000000-0000-0000-0000-000000000001"
    principal_id = "00000000-0000-0000-0000-000000000002"
  }
}

mock_resource "azurerm_key_vault" {
  defaults = {
    id        = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv"
    vault_uri = "https://pghacker-example-kv.vault.azure.net/"
  }
}

mock_resource "azurerm_virtual_network" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet" }
}

mock_resource "azurerm_subnet" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/container-apps" }
}

mock_resource "azurerm_private_dns_zone" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateDnsZones/privatelink.horizondb.azure.com" }
}

mock_resource "azurerm_private_endpoint" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-hdb-pe" }
}

mock_resource "azurerm_key_vault_secret" {
  defaults = {
    id                      = "https://pghacker-example-kv.vault.azure.net/secrets/mock-secret/0123456789abcdef0123456789abcdef"
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/mock-secret"
    resource_id             = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/mock-secret/versions/0123456789abcdef0123456789abcdef"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/mock-secret"
  }
}

mock_resource "azurerm_cognitive_account" {
  defaults = {
    id                 = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.CognitiveServices/accounts/pghacker-example-foundry"
    endpoint           = "https://pghacker-example-foundry.cognitiveservices.azure.com/"
    primary_access_key = "mock-foundry-primary-key"
  }
}

mock_resource "azurerm_container_app_environment" {
  defaults = {
    id             = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.App/managedEnvironments/pghacker-example-env"
    default_domain = "mock-1a2b3c4d.westus3.azurecontainerapps.io"
  }
}

mock_resource "azurerm_container_app" {
  defaults = {
    id      = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.App/containerApps/pghacker-example"
    ingress = { fqdn = "pghacker-example.mock-1a2b3c4d.westus3.azurecontainerapps.io" }
  }
}

mock_resource "azurerm_cdn_frontdoor_origin_group" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Cdn/profiles/pghacker-example-afd/originGroups/app" }
}

mock_resource "azurerm_cdn_frontdoor_origin" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Cdn/profiles/pghacker-example-afd/originGroups/app/origins/app" }
}

mock_resource "azurerm_cdn_frontdoor_profile" {
  defaults = {
    id            = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Cdn/profiles/pghacker-example-afd"
    resource_guid = "4f6f1d1e-8b3a-4c55-9a7e-0b5d2c9e7a11"
  }
}

mock_resource "azurerm_cdn_frontdoor_custom_domain" {
  defaults = {
    id               = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Cdn/profiles/pghacker-example-afd/customDomains/mcp-example-org"
    validation_token = "mock-domain-validation-token"
    expiration_date  = "2026-12-01T00:00:00Z"
  }
}
