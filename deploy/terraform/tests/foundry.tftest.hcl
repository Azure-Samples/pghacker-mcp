# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Azure AI Foundry: the private account, its deployments, its private
# endpoint and DNS. Every provider is mocked, so runs apply to synthetic
# state, never to Azure.

variables {
  subscription_id     = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
  tenant_id           = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
  location            = "westus3"
  resource_group_name = "pghacker-example"
  name_prefix         = "pghacker-example"
  acr_name            = "examplepghacr12345"
  key_vault_name      = "pghacker-example-kv"
  image               = "examplepghacr12345.azurecr.io/pghacker-mcp@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  github_org          = "contoso"
  github_team         = "pghacker-operators"
  deployer_object_ids = ["cccccccc-cccc-4ccc-8ccc-cccccccccccc"]
}

mock_provider "azurerm" {
  source = "./tests/mocks/azurerm"
}
mock_provider "azapi" {
  source = "./tests/mocks/azapi"
}
mock_provider "random" {
  source = "./tests/mocks/random"
}
mock_provider "time" {}

# Azure Firewall takes only a subnet named AzureFirewallSubnet.
override_resource {
  target = azurerm_subnet.firewall
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/AzureFirewallSubnet" }
}

override_resource {
  target = azurerm_subnet.private_endpoints
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/private-endpoints" }
}

# The IDs that the mocked endpoint connections approve.
override_resource {
  target = azurerm_private_endpoint.horizondb
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-hdb-pe" }
}
override_resource {
  target = azurerm_private_endpoint.foundry
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-foundry-pe" }
}

# The token proxy's identity, apart from the others.
override_resource {
  target = azurerm_user_assigned_identity.workload["proxy"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-proxy-id"
    client_id    = "11111111-0000-4000-8000-000000000001"
    principal_id = "22222222-0000-4000-8000-000000000001"
  }
}

# Distinct zone IDs, so that the zone group's IDs can be told apart.
override_resource {
  target = azurerm_private_dns_zone.foundry["privatelink.cognitiveservices.azure.com"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateDnsZones/privatelink.cognitiveservices.azure.com" }
}
override_resource {
  target = azurerm_private_dns_zone.foundry["privatelink.openai.azure.com"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateDnsZones/privatelink.openai.azure.com" }
}
override_resource {
  target = azurerm_private_dns_zone.foundry["privatelink.services.ai.azure.com"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateDnsZones/privatelink.services.ai.azure.com" }
}

run "foundry_is_private" {
  assert {
    condition = (
      azurerm_cognitive_account.foundry.name == "pghacker-example-foundry"
      && azurerm_cognitive_account.foundry.kind == "AIServices"
      && azurerm_cognitive_account.foundry.sku_name == "S0"
      && azurerm_cognitive_account.foundry.custom_subdomain_name == "pghacker-example-foundry"
    )
    error_message = "The Foundry account must be an S0 AIServices account whose custom subdomain is its name."
  }

  assert {
    condition = (
      !azurerm_cognitive_account.foundry.public_network_access_enabled
      && !azurerm_cognitive_account.foundry.local_auth_enabled
      && azurerm_cognitive_account.foundry.network_acls[0].default_action == "Deny"
      && azurerm_cognitive_account.foundry.network_acls[0].bypass == "None"
    )
    error_message = "The account must refuse public network access and deny by default, and refuse keys, as the subscription's policy requires."
  }

  assert {
    condition = (
      azurerm_cognitive_account.foundry.outbound_network_access_restricted
      && length(coalesce(azurerm_cognitive_account.foundry.fqdns, [])) == 0
    )
    error_message = "The account must reach no outside host for a caller (data loss prevention): no image or file by URL, no On Your Data source, no remote MCP server."
  }

  assert {
    condition = {
      for name, deployment in azurerm_cognitive_deployment.this :
      name => "${deployment.name} ${deployment.model[0].format}/${deployment.model[0].name}@${deployment.model[0].version} ${deployment.sku[0].name}x${deployment.sku[0].capacity}"
      } == {
      "gpt-6-luna"             = "gpt-6-luna OpenAI/gpt-6-luna@2026-09-22 GlobalStandardx1000"
      "text-embedding-3-small" = "text-embedding-3-small OpenAI/text-embedding-3-small@1 GlobalStandardx800"
      "gpt-6.1-sol"            = "gpt-6.1-sol OpenAI/gpt-6.1-sol@2026-09-29 GlobalStandardx300"
    }
    error_message = "The deployments must be gpt-6-luna (1000K TPM), text-embedding-3-small (800K) and gpt-6.1-sol (300K), GlobalStandard, each named after its model."
  }

  assert {
    condition = alltrue([
      for deployment in azurerm_cognitive_deployment.this :
      deployment.cognitive_account_id == azurerm_cognitive_account.foundry.id && deployment.version_upgrade_option == "OnceCurrentVersionExpired"
    ])
    error_message = "Every deployment must belong to the account and keep its pinned version until that retires."
  }

  assert {
    condition = (
      azurerm_private_endpoint.foundry.subnet_id == azurerm_subnet.private_endpoints.id
      && azurerm_private_endpoint.foundry.private_service_connection[0].private_connection_resource_id == azurerm_cognitive_account.foundry.id
      && azurerm_private_endpoint.foundry.private_service_connection[0].subresource_names == tolist(["account"])
      && !azurerm_private_endpoint.foundry.private_service_connection[0].is_manual_connection
    )
    error_message = "The account's private endpoint must connect its account subresource in the private-endpoints subnet."
  }

  assert {
    condition     = toset([for zone in azurerm_private_dns_zone.foundry : zone.name]) == toset(["privatelink.cognitiveservices.azure.com", "privatelink.openai.azure.com", "privatelink.services.ai.azure.com"])
    error_message = "Foundry needs the cognitiveservices, openai and services.ai private DNS zones."
  }

  assert {
    condition = (
      length(toset([for zone in azurerm_private_dns_zone.foundry : zone.id])) == 3
      && length(azurerm_private_endpoint.foundry.private_dns_zone_group) == 1
      && length(azurerm_private_endpoint.foundry.private_dns_zone_group[0].private_dns_zone_ids) == 3
      && toset(azurerm_private_endpoint.foundry.private_dns_zone_group[0].private_dns_zone_ids) == toset([for zone in azurerm_private_dns_zone.foundry : zone.id])
    )
    error_message = "One DNS zone group must register the endpoint in all three zones."
  }

  assert {
    condition = alltrue([
      for name, link in azurerm_private_dns_zone_virtual_network_link.foundry :
      link.private_dns_zone_id == azurerm_private_dns_zone.foundry[name].id
      && link.virtual_network_id == azurerm_virtual_network.this.id
      && link.resolution_policy == "NxDomainRedirect"
      && !link.registration_enabled
    ]) && length(azurerm_private_dns_zone_virtual_network_link.foundry) == 3
    error_message = "Each Foundry zone must be linked to the VNet, falling back to public DNS for names it lacks."
  }

  assert {
    condition = (
      azurerm_role_assignment.proxy_foundry_openai_user.scope == azurerm_cognitive_account.foundry.id
      && azurerm_role_assignment.proxy_foundry_openai_user.role_definition_name == "Cognitive Services OpenAI User"
      && azurerm_role_assignment.proxy_foundry_openai_user.principal_id == "22222222-0000-4000-8000-000000000001"
    )
    error_message = "The token proxy's identity must be a Cognitive Services OpenAI User of the account, which its tokens need."
  }

  assert {
    condition = (
      output.foundry_endpoint == "https://pghacker-example-foundry.openai.azure.com"
      && output.foundry_account_name == "pghacker-example-foundry"
      && local.app_settings.PGHACKER_FOUNDRY_ENDPOINT == output.foundry_endpoint
      && local.app_settings.PGHACKER_FOUNDRY_SOL == "gpt-6.1-sol"
      && local.app_settings.PGHACKER_FOUNDRY_LUNA == "gpt-6-luna"
      && local.app_settings.PGHACKER_FOUNDRY_EMBEDDING == "text-embedding-3-small"
    )
    error_message = "The app must get the account's Azure OpenAI endpoint, without a trailing slash, and the deployments playing each role."
  }
}

run "unapproved_foundry_endpoint_is_rejected" {
  command = plan

  override_data {
    target = data.azapi_resource.foundry_private_endpoint_connection
    values = {
      output = {
        connections = [{
          properties = {
            privateEndpoint                   = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-foundry-pe" }
            privateLinkServiceConnectionState = { status = "Pending" }
          }
        }]
      }
    }
  }

  expect_failures = [data.azapi_resource.foundry_private_endpoint_connection]
}

run "roles_follow_other_deployments" {
  command   = plan
  state_key = "other-deployments"

  variables {
    foundry_account_name = "pghacker-models"
    foundry_deployments = {
      chat  = { model = "gpt-6-sol", version = "2026-09-22", capacity = 100, sku = "DataZoneStandard" }
      crawl = { model = "gpt-5.6-luna", version = "2026-07-09", capacity = 200 }
      embed = { model = "text-embedding-3-small", version = "1", capacity = 50, sku = "DataZoneStandard" }
    }
    foundry_sol_deployment       = "chat"
    foundry_luna_deployment      = "crawl"
    foundry_embedding_deployment = "embed"
  }

  assert {
    condition = (
      local.app_settings.PGHACKER_FOUNDRY_SOL == "chat"
      && local.app_settings.PGHACKER_FOUNDRY_LUNA == "crawl"
      && local.app_settings.PGHACKER_FOUNDRY_EMBEDDING == "embed"
      && local.app_settings.PGHACKER_FOUNDRY_ENDPOINT == "https://pghacker-models.openai.azure.com"
      && azurerm_cognitive_account.foundry.custom_subdomain_name == "pghacker-models"
      && azurerm_private_endpoint.foundry.name == "pghacker-models-pe"
    )
    error_message = "The roles must name the chosen deployments, and foundry_account_name the account, its subdomain and its endpoint."
  }

  assert {
    condition = tomap({ for name, deployment in azurerm_cognitive_deployment.this : name => deployment.sku[0].name }) == tomap({
      chat  = "DataZoneStandard"
      crawl = "GlobalStandard"
      embed = "DataZoneStandard"
    })
    error_message = "Each deployment must take the deployment type it names, GlobalStandard unless it names one."
  }
}

run "roles_must_name_a_deployment" {
  command = plan

  variables {
    foundry_luna_deployment = "gpt-5.6-luna"
  }

  expect_failures = [var.foundry_luna_deployment]
}

run "deployment_types_are_known" {
  command = plan

  variables {
    foundry_deployments = {
      "gpt-6-luna"             = { model = "gpt-6-luna", version = "2026-09-22", capacity = 100, sku = "Standard" }
      "text-embedding-3-small" = { model = "text-embedding-3-small", version = "1", capacity = 800 }
      "gpt-6.1-sol"            = { model = "gpt-6.1-sol", version = "2026-09-29", capacity = 300 }
    }
  }

  expect_failures = [var.foundry_deployments]
}

run "capacities_are_whole_thousands" {
  command = plan

  variables {
    foundry_deployments = {
      "gpt-6-luna"             = { model = "gpt-6-luna", version = "2026-09-22", capacity = 0.5 }
      "text-embedding-3-small" = { model = "text-embedding-3-small", version = "1", capacity = 800 }
      "gpt-6.1-sol"            = { model = "gpt-6.1-sol", version = "2026-09-29", capacity = 300 }
    }
  }

  expect_failures = [var.foundry_deployments]
}
