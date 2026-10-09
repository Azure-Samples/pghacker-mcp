# HorizonDB: the parameter group, the cluster and its private path. Every
# provider is mocked, so runs apply to synthetic state, never to Azure.

variables {
  subscription_id     = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
  tenant_id           = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
  location            = "westus3"
  resource_group_name = "pghacker-example"
  name_prefix         = "pghacker-example"
  app_name            = "pghacker-example"
  acr_name            = "examplepghacr12345"
  key_vault_name      = "pghacker-example-kv"
  image               = "examplepghacr12345.azurecr.io/pghacker-mcp@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
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

run "parameters_and_private_path" {
  assert {
    condition = jsonencode(azapi_resource.pg_parameters.body.properties.parameters) == jsonencode([
      { name = "azure.extensions", value = "age,pg_textsearch,pg_trgm,vector" },
      { name = "shared_preload_libraries", value = "age,pg_textsearch" },
    ])
    error_message = "The parameter group must allow exactly age, pg_textsearch, pg_trgm and vector, and preload exactly age and pg_textsearch."
  }

  assert {
    condition     = azapi_resource.pg_parameters.body.properties.pgVersion == 17 && can(regex("^pghacker-example-pg17-[0-9a-f]{8}$", azapi_resource.pg_parameters.name)) && output.horizondb_parameter_group == azapi_resource.pg_parameters.name
    error_message = "The parameter group must be for PostgreSQL 17 and named after a hash of its content."
  }

  assert {
    condition = (
      azapi_resource.horizondb.name == "pghacker-example-hdb"
      && azapi_resource.horizondb.body.properties.version == "17"
      && azapi_resource.horizondb.body.properties.vCores == 4
      && azapi_resource.horizondb.body.properties.replicaCount == 1
      && azapi_resource.horizondb.body.properties.administratorLogin == "pghacker_admin"
      && azapi_resource.horizondb.body.properties.parameterGroup.id == azapi_resource.pg_parameters.id
      && azapi_resource_action.attach_parameter_group.body.properties.parameterGroup.id == azapi_resource.pg_parameters.id
      && azapi_resource_action.configure_cluster.body.properties.vCores == 4
    )
    error_message = "The cluster must be PostgreSQL 17 with 4 vCores, 1 replica, the pghacker_admin login and the parameter group attached."
  }

  assert {
    condition     = nonsensitive(azurerm_key_vault_secret.db_admin_password.value == random_password.db_admin.result && azapi_resource_action.set_password.body.properties.administratorLoginPassword == random_password.db_admin.result) && azurerm_key_vault_secret.db_admin_password.name == "db-admin-password"
    error_message = "The cluster and the db-admin-password secret must hold the same generated password."
  }

  assert {
    condition = (
      azurerm_private_endpoint.horizondb.subnet_id == azurerm_subnet.private_endpoints.id
      && azurerm_private_endpoint.horizondb.private_service_connection[0].private_connection_resource_id == azapi_resource.horizondb.id
      && azurerm_private_endpoint.horizondb.private_service_connection[0].subresource_names == tolist(["DefaultPool"])
      && !azurerm_private_endpoint.horizondb.private_service_connection[0].is_manual_connection
      && azurerm_private_endpoint.horizondb.private_dns_zone_group[0].private_dns_zone_ids == tolist([azurerm_private_dns_zone.horizondb.id])
    )
    error_message = "The cluster's private endpoint must connect DefaultPool in the private-endpoints subnet and register in HorizonDB's zone."
  }

  assert {
    condition = (
      azurerm_private_dns_zone.horizondb.name == "privatelink.horizondb.azure.com"
      && azurerm_private_dns_zone_virtual_network_link.horizondb.private_dns_zone_id == azurerm_private_dns_zone.horizondb.id
      && azurerm_private_dns_zone_virtual_network_link.horizondb.virtual_network_id == azurerm_virtual_network.this.id
      && azurerm_private_dns_zone_virtual_network_link.horizondb.resolution_policy == "NxDomainRedirect"
      && !azurerm_private_dns_zone_virtual_network_link.horizondb.registration_enabled
    )
    error_message = "privatelink.horizondb.azure.com must be linked to the VNet, falling back to public DNS for names it lacks."
  }

  assert {
    condition = (
      azurerm_subnet.private_endpoints.address_prefixes == tolist(["10.66.1.0/27"])
      && azurerm_subnet.private_endpoints.private_endpoint_network_policies == "NetworkSecurityGroupEnabled"
      && azurerm_subnet.container_apps.address_prefixes == tolist(["10.66.0.0/24"])
      && azurerm_subnet.container_apps.delegation[0].service_delegation[0].name == "Microsoft.App/environments"
      && toset(azurerm_virtual_network.this.address_space) == toset(["10.66.0.0/22"])
    )
    error_message = "The VNet must be 10.66.0.0/22 with the delegated Container Apps subnet and the private endpoints' subnet."
  }

  assert {
    condition     = length(azapi_resource.horizondb_firewall_rule) == 0 && length(output.horizondb_firewall_rules) == 0
    error_message = "By default, no firewall rule may open the cluster's public endpoint."
  }
}

# From empty state: the mocked provider doesn't report that a new name
# replaces the group, so in existing state the plan would show an in-place
# update with ignore_changes keeping the old body.
run "max_connections_makes_a_new_group" {
  command   = plan
  state_key = "max-connections"

  variables {
    horizondb_max_connections = 400
  }

  assert {
    condition = jsonencode(azapi_resource.pg_parameters.body.properties.parameters) == jsonencode([
      { name = "azure.extensions", value = "age,pg_textsearch,pg_trgm,vector" },
      { name = "shared_preload_libraries", value = "age,pg_textsearch" },
      { name = "max_connections", value = "400" },
    ])
    error_message = "horizondb_max_connections must add max_connections to the extensions and preload libraries."
  }

  assert {
    condition     = azapi_resource.pg_parameters.name != run.parameters_and_private_path.horizondb_parameter_group
    error_message = "New parameters must make a new, differently named group."
  }
}

run "replica_count_is_fixed_after_creation" {
  command = plan

  variables {
    horizondb_replica_count = 2
  }

  expect_failures = [data.azapi_resource.cluster_capacity]
}

run "unapproved_endpoint_is_rejected" {
  command = plan

  override_data {
    target = data.azapi_resource.horizondb_private_endpoint_connection
    values = {
      output = {
        connections = [{
          properties = {
            privateEndpoint                   = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-hdb-pe" }
            privateLinkServiceConnectionState = { status = "Pending" }
          }
        }]
      }
    }
  }

  expect_failures = [data.azapi_resource.horizondb_private_endpoint_connection]
}

run "another_endpoints_approval_is_rejected" {
  command = plan

  override_data {
    target = data.azapi_resource.horizondb_private_endpoint_connection
    values = {
      output = {
        connections = [{
          properties = {
            privateEndpoint                   = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/someone-elses-pe" }
            privateLinkServiceConnectionState = { status = "Approved" }
          }
        }]
      }
    }
  }

  expect_failures = [data.azapi_resource.horizondb_private_endpoint_connection]
}

run "endpoint_ids_compare_case_insensitively" {
  command = plan

  override_data {
    target = data.azapi_resource.horizondb_private_endpoint_connection
    values = {
      output = {
        connections = [{
          properties = {
            privateEndpoint                   = { id = "/SUBSCRIPTIONS/AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA/RESOURCEGROUPS/PGHACKER-EXAMPLE/PROVIDERS/MICROSOFT.NETWORK/PRIVATEENDPOINTS/PGHACKER-EXAMPLE-HDB-PE" }
            privateLinkServiceConnectionState = { status = "Approved" }
          }
        }]
      }
    }
  }
}

run "emergency_firewall_rules" {
  variables {
    extra_firewall_ips = ["203.0.113.7", "203.0.113.8", "203.0.113.7"]
  }

  assert {
    condition     = tomap(output.horizondb_firewall_rules) == tomap({ "allow-203-0-113-7" = "203.0.113.7", "allow-203-0-113-8" = "203.0.113.8" })
    error_message = "Each distinct address must get one rule, named after it."
  }

  assert {
    condition = alltrue([
      for name, rule in azapi_resource.horizondb_firewall_rule :
      rule.parent_id == "${azapi_resource.horizondb.id}/pools/DefaultPool"
      && rule.body.properties.startIpAddress == output.horizondb_firewall_rules[name]
      && rule.body.properties.endIpAddress == output.horizondb_firewall_rules[name]
    ])
    error_message = "A rule must admit exactly one address, on DefaultPool."
  }

  assert {
    condition     = length(azurerm_private_endpoint.horizondb.private_service_connection) == 1 && azurerm_private_endpoint.horizondb.subnet_id == azurerm_subnet.private_endpoints.id
    error_message = "Emergency access must leave the private path as it is."
  }
}

run "emergency_firewall_rules_removed" {
  variables {
    extra_firewall_ips = []
  }

  assert {
    condition     = length(azapi_resource.horizondb_firewall_rule) == 0 && length(output.horizondb_firewall_rules) == 0
    error_message = "Emptying extra_firewall_ips must delete every rule."
  }
}

run "invalid_firewall_addresses_are_rejected" {
  command = plan

  variables {
    extra_firewall_ips = ["203.0.113.999", "203.0.113.0/24", "::1"]
  }

  expect_failures = [var.extra_firewall_ips]
}
