# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

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
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/nodes-services" }
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

# The cluster's OIDC issuer, which the workload identities' federated
# credentials name.
mock_resource "azurerm_kubernetes_cluster" {
  defaults = {
    id              = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ContainerService/managedClusters/pghacker-example-aks"
    oidc_issuer_url = "https://westus3.oic.prod-aks.azure.com/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb/dddddddd-dddd-4ddd-8ddd-dddddddddddd/"
  }
}

mock_resource "azurerm_kubernetes_cluster_node_pool" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ContainerService/managedClusters/pghacker-example-aks/agentPools/services" }
}

mock_resource "azurerm_monitor_data_collection_rule" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Insights/dataCollectionRules/MSCI-westus3-pghacker-example-aks" }
}

mock_resource "azurerm_public_ip" {
  defaults = {
    id         = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/publicIPAddresses/pghacker-example-fw-ip"
    ip_address = "20.14.0.10"
  }
}

mock_resource "azurerm_firewall_policy" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/firewallPolicies/pghacker-example-fw-policy" }
}

mock_resource "azurerm_firewall_policy_rule_collection_group" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/firewallPolicies/pghacker-example-fw-policy/ruleCollectionGroups/egress" }
}

# The firewall's private address, the first usable one in AzureFirewallSubnet,
# which the node subnets' route sends everything to.
mock_resource "azurerm_firewall" {
  defaults = {
    id               = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/azureFirewalls/pghacker-example-fw"
    ip_configuration = { private_ip_address = "10.66.0.4" }
  }
}

mock_resource "azurerm_route_table" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/routeTables/pghacker-example-egress" }
}

mock_resource "azurerm_network_security_group" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/networkSecurityGroups/pghacker-example-private-endpoints" }
}

mock_resource "azurerm_federated_identity_credential" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-id/federatedIdentityCredentials/pghacker-mock" }
}

mock_resource "azurerm_monitor_action_group" {
  defaults = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Insights/actionGroups/pghacker-example-egress" }
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

mock_resource "azurerm_application_insights" {
  defaults = {
    id                  = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Insights/components/pghacker-example-usage"
    app_id              = "7a1b2c3d-0000-4000-8000-000000000003"
    instrumentation_key = "11111111-2222-4333-8444-555555555555"
    connection_string   = "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=7a1b2c3d-0000-4000-8000-000000000003"
  }
}
