# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The AKS cluster and its pools, the network and the firewall that keep the
# PilotSwarm pool off the internet, the workload identities and their
# secrets, the kubernetes output that ../scripts/aks.mjs renders the
# manifests from, and the app's settings in it. Every provider is mocked, so
# runs apply to synthetic state, never to Azure.
#
# Terraform 1.16 crashes while reporting a failed assertion that refers to a
# whole value with sensitive parts, such as a resource's secret blocks; the
# assertions refer to single attributes where they can.

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
  # The app's whole contract, usage telemetry's settings among them; it's off
  # by default (usage.tftest.hcl).
  usage_enabled = true
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

# Each subnet at its own ID, as in Azure.
override_resource {
  target = azurerm_subnet.firewall
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/AzureFirewallSubnet" }
}
override_resource {
  target = azurerm_subnet.nodes["system"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/nodes-system" }
}
override_resource {
  target = azurerm_subnet.nodes["services"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/nodes-services" }
}
override_resource {
  target = azurerm_subnet.nodes["swarm"]
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/nodes-swarm" }
}
override_resource {
  target = azurerm_subnet.edge
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/virtualNetworks/pghacker-example-vnet/subnets/edge" }
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

# Each identity with IDs of its own.
override_resource {
  target = azurerm_user_assigned_identity.aks
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-aks-id"
    client_id    = "11111111-0000-4000-8000-000000000000"
    principal_id = "22222222-0000-4000-8000-000000000000"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.kubelet
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-aks-kubelet-id"
    client_id    = "11111111-0000-4000-8000-0000000000a1"
    principal_id = "22222222-0000-4000-8000-0000000000a1"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["proxy"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-proxy-id"
    client_id    = "11111111-0000-4000-8000-000000000001"
    principal_id = "22222222-0000-4000-8000-000000000001"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["indexer"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-indexer-id"
    client_id    = "11111111-0000-4000-8000-000000000002"
    principal_id = "22222222-0000-4000-8000-000000000002"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["mcp"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-mcp-id"
    client_id    = "11111111-0000-4000-8000-000000000003"
    principal_id = "22222222-0000-4000-8000-000000000003"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["swarm"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-swarm-id"
    client_id    = "11111111-0000-4000-8000-000000000004"
    principal_id = "22222222-0000-4000-8000-000000000004"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["edge"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-edge-id"
    client_id    = "11111111-0000-4000-8000-000000000005"
    principal_id = "22222222-0000-4000-8000-000000000005"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["home"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-home-id"
    client_id    = "11111111-0000-4000-8000-000000000006"
    principal_id = "22222222-0000-4000-8000-000000000006"
  }
}
override_resource {
  target = azurerm_user_assigned_identity.workload["ops"]
  values = {
    id           = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.ManagedIdentity/userAssignedIdentities/pghacker-example-ops-id"
    client_id    = "11111111-0000-4000-8000-000000000007"
    principal_id = "22222222-0000-4000-8000-000000000007"
  }
}

# Each secret at its own URI and resource ID, the scope of its grants.
override_resource {
  target = azurerm_key_vault_secret.db_admin_password
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/db-admin-password"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/db-admin-password"
  }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_indexer"]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-indexer-password"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/pgh-indexer-password"
  }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_reader"]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-reader-password"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/pgh-reader-password"
  }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_swarm"]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-swarm-password"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/pgh-swarm-password"
  }
}
override_resource {
  target = azurerm_key_vault_secret.oauth2_cookie
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/oauth2-cookie-secret"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/oauth2-cookie-secret"
  }
}
override_resource {
  target = azurerm_key_vault_secret.usage_id_key[0]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/usage-id-key"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/usage-id-key"
  }
}
override_resource {
  target = azurerm_key_vault_secret.github["github-client-id"]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/github-client-id"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/github-client-id"
  }
}
override_resource {
  target = azurerm_key_vault_secret.github["github-client-secret"]
  values = {
    versionless_id          = "https://pghacker-example-kv.vault.azure.net/secrets/github-client-secret"
    resource_versionless_id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/github-client-secret"
  }
}

run "cluster" {
  assert {
    condition = (
      azurerm_kubernetes_cluster.this.name == "pghacker-example-aks"
      && azurerm_kubernetes_cluster.this.node_resource_group == "pghacker-example-nodes"
      && azurerm_kubernetes_cluster.this.sku_tier == "Standard"
      && azurerm_kubernetes_cluster.this.automatic_upgrade_channel == "patch"
      && azurerm_kubernetes_cluster.this.node_os_upgrade_channel == "NodeImage"
    )
    error_message = "The cluster must be pghacker-example-aks, Standard, following patch releases and node images, with its nodes in pghacker-example-nodes."
  }

  assert {
    condition = (
      azurerm_kubernetes_cluster.this.oidc_issuer_enabled
      && azurerm_kubernetes_cluster.this.workload_identity_enabled
      && azurerm_kubernetes_cluster.this.local_account_disabled
      && azurerm_kubernetes_cluster.this.role_based_access_control_enabled
      && azurerm_kubernetes_cluster.this.azure_active_directory_role_based_access_control[0].azure_rbac_enabled
      && azurerm_kubernetes_cluster.this.azure_active_directory_role_based_access_control[0].tenant_id == "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
      && length(azurerm_kubernetes_cluster.this.api_server_access_profile) == 0
    )
    error_message = "The cluster must issue OIDC tokens for workload identity, and authorize its API with Entra ID and Azure RBAC alone, without local accounts."
  }

  assert {
    condition = (
      azurerm_kubernetes_cluster.this.network_profile[0].network_plugin == "azure"
      && azurerm_kubernetes_cluster.this.network_profile[0].network_plugin_mode == "overlay"
      && azurerm_kubernetes_cluster.this.network_profile[0].network_data_plane == "cilium"
      && azurerm_kubernetes_cluster.this.network_profile[0].network_policy == "cilium"
      && azurerm_kubernetes_cluster.this.network_profile[0].outbound_type == "userDefinedRouting"
      && azurerm_kubernetes_cluster.this.network_profile[0].pod_cidr == "10.244.0.0/16"
      && azurerm_kubernetes_cluster.this.network_profile[0].service_cidr == "10.0.0.0/16"
      && azurerm_kubernetes_cluster.this.network_profile[0].dns_service_ip == "10.0.0.10"
    )
    error_message = "The cluster must use Azure CNI Overlay with Cilium, which enforces the NetworkPolicies, and send its outbound traffic by its route table, to the firewall."
  }

  assert {
    condition = (
      azurerm_kubernetes_cluster.this.key_vault_secrets_provider[0].secret_rotation_enabled
      && azurerm_kubernetes_cluster.this.oms_agent[0].log_analytics_workspace_id == azurerm_log_analytics_workspace.this.id
      && azurerm_kubernetes_cluster.this.oms_agent[0].msi_auth_for_monitoring_enabled
      && azurerm_kubernetes_cluster.this.identity[0].type == "UserAssigned"
      && azurerm_kubernetes_cluster.this.identity[0].identity_ids == toset([azurerm_user_assigned_identity.aks.id])
    )
    error_message = "The cluster must have the Key Vault secrets provider, rotating, Container Insights on the workspace, and its own user-assigned identity."
  }

  assert {
    condition = (
      azurerm_kubernetes_cluster.this.default_node_pool[0].name == "system"
      && azurerm_kubernetes_cluster.this.default_node_pool[0].only_critical_addons_enabled
      && azurerm_kubernetes_cluster.this.default_node_pool[0].vnet_subnet_id == azurerm_subnet.nodes["system"].id
      && azurerm_kubernetes_cluster.this.default_node_pool[0].node_labels == tomap({ "pghacker.io/pool" = "system" })
      && azurerm_kubernetes_cluster_node_pool.services.kubernetes_cluster_id == azurerm_kubernetes_cluster.this.id
      && azurerm_kubernetes_cluster_node_pool.services.vnet_subnet_id == azurerm_subnet.nodes["services"].id
      && azurerm_kubernetes_cluster_node_pool.services.node_labels == tomap({ "pghacker.io/pool" = "services" })
      && try(length(azurerm_kubernetes_cluster_node_pool.services.node_taints), 0) == 0
      && azurerm_kubernetes_cluster_node_pool.swarm.kubernetes_cluster_id == azurerm_kubernetes_cluster.this.id
      && azurerm_kubernetes_cluster_node_pool.swarm.vnet_subnet_id == azurerm_subnet.nodes["swarm"].id
      && azurerm_kubernetes_cluster_node_pool.swarm.node_labels == tomap({ "pghacker.io/pool" = "swarm" })
      && azurerm_kubernetes_cluster_node_pool.swarm.node_taints == tolist(["pghacker.io/pool=swarm:NoSchedule"])
    )
    error_message = "The system pool must run the add-ons alone; the services and PilotSwarm pools must each be in their own subnet, labeled pghacker.io/pool, and only PilotSwarm's tainted."
  }

  assert {
    condition = (
      azurerm_role_assignment.kubelet_acr_pull.scope == azurerm_container_registry.this.id
      && azurerm_role_assignment.kubelet_acr_pull.role_definition_name == "AcrPull"
      && azurerm_role_assignment.kubelet_acr_pull.principal_id == azurerm_user_assigned_identity.kubelet.principal_id
      && azurerm_kubernetes_cluster.this.kubelet_identity[0].user_assigned_identity_id == azurerm_user_assigned_identity.kubelet.id
      && azurerm_kubernetes_cluster.this.kubelet_identity[0].object_id == azurerm_user_assigned_identity.kubelet.principal_id
      && azurerm_role_assignment.aks_kubelet_operator.scope == azurerm_user_assigned_identity.kubelet.id
      && azurerm_role_assignment.aks_kubelet_operator.role_definition_name == "Managed Identity Operator"
      && azurerm_role_assignment.aks_kubelet_operator.principal_id == azurerm_user_assigned_identity.aks.principal_id
      && tolist([for assignment in azurerm_role_assignment.deployer_aks_admin : assignment.role_definition_name]) == tolist(["Azure Kubernetes Service RBAC Cluster Admin"])
      && tolist([for assignment in azurerm_role_assignment.deployer_aks_admin : assignment.scope]) == tolist([azurerm_kubernetes_cluster.this.id])
      && azurerm_role_assignment.aks_network.scope == azurerm_virtual_network.this.id
      && azurerm_role_assignment.aks_network.principal_id == azurerm_user_assigned_identity.aks.principal_id
      && azurerm_role_assignment.aks_route_table.scope == azurerm_route_table.egress.id
      && azurerm_role_assignment.aks_route_table.principal_id == azurerm_user_assigned_identity.aks.principal_id
    )
    error_message = "The kubelet's own identity must pull from the registry, the appliers administer the cluster through Entra ID, and the control plane manage the VNet, the route table and the kubelet's identity."
  }

  assert {
    condition     = azurerm_container_registry.this.sku == "Premium" && azurerm_container_registry.this.data_endpoint_enabled && !azurerm_container_registry.this.admin_enabled
    error_message = "The registry must be Premium, with the dedicated data endpoint that the firewall admits, and no admin user."
  }
}

run "network" {
  assert {
    condition = (
      azurerm_virtual_network.this.address_space == toset(["10.66.0.0/22"])
      && azurerm_subnet.firewall.name == "AzureFirewallSubnet"
      && azurerm_subnet.firewall.address_prefixes == tolist(["10.66.0.0/26"])
      && tomap({ for pool, subnet in azurerm_subnet.nodes : pool => subnet.address_prefixes[0] }) == tomap({
        system   = "10.66.0.64/27"
        services = "10.66.0.96/27"
        swarm    = "10.66.0.128/27"
      })
      && alltrue([for subnet in azurerm_subnet.nodes : !subnet.default_outbound_access_enabled])
      && azurerm_subnet.edge.address_prefixes == tolist(["10.66.0.160/28"])
      && !azurerm_subnet.edge.private_link_service_network_policies_enabled
      && azurerm_subnet.private_endpoints.address_prefixes == tolist(["10.66.1.0/27"])
      && azurerm_subnet.private_endpoints.private_endpoint_network_policies == "NetworkSecurityGroupEnabled"
    )
    error_message = "The VNet must hold the firewall's subnet, a subnet per pool without default outbound access, the edge's (Private Link) and the private endpoints' (with their NSG)."
  }

  assert {
    condition = (
      one(azurerm_route_table.egress.route).address_prefix == "0.0.0.0/0"
      && one(azurerm_route_table.egress.route).next_hop_type == "VirtualAppliance"
      && one(azurerm_route_table.egress.route).next_hop_in_ip_address == azurerm_firewall.this.ip_configuration[0].private_ip_address
      && !azurerm_route_table.egress.bgp_route_propagation_enabled
      && toset([for association in azurerm_subnet_route_table_association.nodes : association.subnet_id]) == toset([for subnet in azurerm_subnet.nodes : subnet.id])
      && alltrue([for association in azurerm_subnet_route_table_association.nodes : association.route_table_id == azurerm_route_table.egress.id])
    )
    error_message = "Every node subnet must send everything outside the VNet to the firewall."
  }

  assert {
    condition = (
      azurerm_network_security_rule.swarm.access == "Deny"
      && azurerm_network_security_rule.swarm.protocol == "*"
      && azurerm_network_security_rule.swarm.source_address_prefix == "10.66.0.128/27"
      && azurerm_network_security_rule.swarm.destination_address_prefix == "10.66.1.0/27"
      && azurerm_network_security_rule.swarm.destination_port_ranges == toset(["0-5431", "5433-65535"])
      && azurerm_network_security_rule.swarm.priority == 100
      && azurerm_network_security_rule.swarm.priority < min(azurerm_network_security_rule.database.priority, azurerm_network_security_rule.foundry.priority, azurerm_network_security_rule.deny_vnet.priority)
    )
    error_message = "The PilotSwarm pool's subnet must be denied all but the database at the NSG's top priority, 100, above any rule added later."
  }

  assert {
    condition = (
      azurerm_network_security_rule.database.access == "Allow"
      && azurerm_network_security_rule.database.source_address_prefixes == toset(["10.66.0.96/27", "10.66.0.128/27"])
      && azurerm_network_security_rule.database.destination_port_range == "5432"
      && azurerm_network_security_rule.foundry.access == "Allow"
      && azurerm_network_security_rule.foundry.source_address_prefix == "10.66.0.96/27"
      && azurerm_network_security_rule.foundry.destination_port_range == "443"
      && azurerm_network_security_rule.deny_vnet.access == "Deny"
      && azurerm_network_security_rule.deny_vnet.source_address_prefix == "VirtualNetwork"
      && azurerm_network_security_rule.deny_vnet.priority > max(azurerm_network_security_rule.database.priority, azurerm_network_security_rule.foundry.priority)
      && azurerm_subnet_network_security_group_association.private_endpoints.subnet_id == azurerm_subnet.private_endpoints.id
      && azurerm_subnet_network_security_group_association.private_endpoints.network_security_group_id == azurerm_network_security_group.private_endpoints.id
    )
    error_message = "The private endpoints must take the database from the services and PilotSwarm pools, Foundry from the services pool alone, and nothing else from the VNet."
  }
}

run "firewall" {
  assert {
    condition = (
      azurerm_firewall.this.firewall_policy_id == azurerm_firewall_policy.this.id
      && azurerm_firewall.this.ip_configuration[0].subnet_id == azurerm_subnet.firewall.id
      && azurerm_firewall.this.ip_configuration[0].public_ip_address_id == azurerm_public_ip.firewall.id
      && azurerm_firewall_policy.this.threat_intelligence_mode == "Deny"
      && azurerm_firewall_policy_rule_collection_group.egress.firewall_policy_id == azurerm_firewall_policy.this.id
    )
    error_message = "The firewall must be in AzureFirewallSubnet with its policy, which denies known-malicious addresses."
  }

  # The PilotSwarm pool's subnet gets what AKS needs and nothing else.
  assert {
    condition = (
      toset(flatten([
        for collection in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection : [
          for rule in collection.rule : rule.name if contains(rule.source_addresses, "10.66.0.128/27")
        ]
      ])) == toset(["aks", "registry", "key-vault", "container-insights"])
      && toset(flatten([
        for collection in azurerm_firewall_policy_rule_collection_group.egress.network_rule_collection : [
          for rule in collection.rule : rule.name if contains(rule.source_addresses, "10.66.0.128/27")
        ]
      ])) == toset(["azure-monitor"])
    )
    error_message = "The PilotSwarm pool's subnet must get only what AKS needs (its FQDN tag, the registry, the vault, Azure Monitor), and no app host."
  }

  assert {
    condition = (
      tolist([for rule in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[0].rule : rule.name]) == tolist(["aks", "registry", "key-vault", "container-insights"])
      && azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[0].rule[0].destination_fqdn_tags == tolist(["AzureKubernetesService"])
      && azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[0].rule[1].destination_fqdns == tolist(["examplepghacr12345.azurecr.io", "examplepghacr12345.westus3.data.azurecr.io"])
      && azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[0].rule[2].destination_fqdns == tolist(["pghacker-example-kv.vault.azure.net"])
      && alltrue([
        for rule in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[0].rule :
        toset(rule.source_addresses) == toset(["10.66.0.64/27", "10.66.0.96/27", "10.66.0.128/27"])
      ])
    )
    error_message = "Every pool must reach what AKS needs: its FQDN tag, this registry and its data endpoint, this vault and Container Insights."
  }

  assert {
    condition = (
      azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[1].name == "services"
      && tomap({
        for rule in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[1].rule : rule.name => rule.destination_fqdns
        }) == tomap({
        postgresql = tolist(["www.postgresql.org", "git.postgresql.org"])
        github     = tolist(["github.com", "api.github.com"])
        entra      = tolist(["login.microsoftonline.com"])
        usage      = tolist(["*.in.applicationinsights.azure.com"])
      })
      && alltrue([
        for rule in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[1].rule :
        rule.source_addresses == tolist(["10.66.0.96/27"])
      ])
    )
    error_message = "The services pool alone must reach the archive, PostgreSQL's git, GitHub, Entra ID and, with usage telemetry, Application Insights' ingestion."
  }

  assert {
    condition = !anytrue(flatten([
      for collection in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection : [
        for rule in collection.rule : try(contains(rule.destination_fqdns, "*"), false)
      ]
    ]))
    error_message = "No rule may admit every host name."
  }

  assert {
    condition = (
      azurerm_monitor_diagnostic_setting.firewall.target_resource_id == azurerm_firewall.this.id
      && azurerm_monitor_diagnostic_setting.firewall.log_analytics_workspace_id == azurerm_log_analytics_workspace.this.id
      && azurerm_monitor_diagnostic_setting.firewall.log_analytics_destination_type == "Dedicated"
      && strcontains(azurerm_monitor_scheduled_query_rules_alert_v2.denied_egress.criteria[0].query, "ipv4_is_in_range(SourceIp, \"10.66.0.128/27\")")
      && azurerm_monitor_scheduled_query_rules_alert_v2.denied_egress.scopes == tolist([azurerm_log_analytics_workspace.this.id])
      && azurerm_monitor_scheduled_query_rules_alert_v2.denied_egress.criteria[0].threshold == 0
      && length(azurerm_monitor_action_group.egress) == 0
      && length(azurerm_monitor_scheduled_query_rules_alert_v2.denied_egress.action) == 0
    )
    error_message = "The firewall's verdicts must go to the workspace, where an alert counts denials from the PilotSwarm pool's subnet; without alert_emails, it emails no one."
  }
}

run "without_usage_the_firewall_names_no_ingestion" {
  command = plan

  variables {
    usage_enabled = false
  }

  assert {
    condition = toset([
      for rule in azurerm_firewall_policy_rule_collection_group.egress.application_rule_collection[1].rule : rule.name
    ]) == toset(["postgresql", "github", "entra"])
    error_message = "Without usage telemetry, the services pool must not reach Application Insights."
  }
}

run "denied_egress_emails_the_operators" {
  state_key = "alerts"

  variables {
    alert_emails = ["ops@example.com", "oncall@example.com"]
  }

  assert {
    condition = (
      tolist([for receiver in azurerm_monitor_action_group.egress[0].email_receiver : receiver.email_address]) == tolist(["ops@example.com", "oncall@example.com"])
      && azurerm_monitor_scheduled_query_rules_alert_v2.denied_egress.action[0].action_groups == tolist([azurerm_monitor_action_group.egress[0].id])
    )
    error_message = "alert_emails must get the denied-egress alert's emails."
  }
}

run "identities" {
  assert {
    condition = (
      toset(keys(azurerm_user_assigned_identity.workload)) == toset(["proxy", "indexer", "mcp", "swarm", "edge", "home", "ops"])
      && alltrue([
        for name, credential in azurerm_federated_identity_credential.workload :
        credential.user_assigned_identity_id == azurerm_user_assigned_identity.workload[name].id
        && credential.issuer == azurerm_kubernetes_cluster.this.oidc_issuer_url
        && credential.audience == tolist(["api://AzureADTokenExchange"])
      ])
      && tomap({ for name, credential in azurerm_federated_identity_credential.workload : name => credential.subject }) == tomap({
        proxy   = "system:serviceaccount:pghacker:token-proxy"
        indexer = "system:serviceaccount:pghacker:indexer"
        mcp     = "system:serviceaccount:pghacker:mcp"
        swarm   = "system:serviceaccount:pghacker:swarm"
        edge    = "system:serviceaccount:pghacker:edge"
        home    = "system:serviceaccount:pghacker:home"
        ops     = "system:serviceaccount:pghacker:ops"
      })
    )
    error_message = "Each component must have its own identity, federated with its own service account in the pghacker namespace, through the cluster's OIDC issuer."
  }

  assert {
    condition = toset(keys(azurerm_role_assignment.workload_secret)) == toset([
      "indexer/pgh-indexer-password",
      "mcp/pgh-reader-password",
      "mcp/usage-id-key",
      "swarm/pgh-swarm-password",
      "edge/github-client-id",
      "edge/github-client-secret",
      "edge/oauth2-cookie-secret",
      "home/usage-id-key",
      "ops/db-admin-password",
      "ops/pgh-indexer-password",
      "ops/pgh-reader-password",
      "ops/pgh-swarm-password",
    ])
    error_message = "Each identity must read its own secrets alone: PilotSwarm (swarm) only pgh_swarm's password, and only the jobs (ops) the administrator's."
  }

  assert {
    condition = alltrue([
      for key, grant in azurerm_role_assignment.workload_secret :
      grant.role_definition_name == "Key Vault Secrets User"
      && grant.principal_id == azurerm_user_assigned_identity.workload[split("/", key)[0]].principal_id
      && grant.scope == "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.KeyVault/vaults/pghacker-example-kv/secrets/${split("/", key)[1]}"
    ])
    error_message = "Each grant must be Key Vault Secrets User on its one secret, to its identity, never on the vault."
  }

  assert {
    condition = (
      azurerm_role_assignment.proxy_foundry_openai_user.principal_id == azurerm_user_assigned_identity.workload["proxy"].principal_id
      && azurerm_role_assignment.proxy_usage_publisher[0].principal_id == azurerm_user_assigned_identity.workload["proxy"].principal_id
      && length([
        for assignment in concat(
          values(azurerm_role_assignment.workload_secret),
          [azurerm_role_assignment.proxy_foundry_openai_user, azurerm_role_assignment.kubelet_acr_pull, azurerm_role_assignment.aks_kubelet_operator],
          azurerm_role_assignment.proxy_usage_publisher,
        ) : assignment if assignment.principal_id == azurerm_user_assigned_identity.workload["swarm"].principal_id
      ]) == 1
    )
    error_message = "The token proxy's identity alone must call Foundry and Application Insights; PilotSwarm's must hold its one secret and nothing more."
  }
}

run "kubernetes_output" {
  assert {
    condition = (
      output.kubernetes.subscription_id == "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
      && output.kubernetes.tenant_id == "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
      && output.kubernetes.resource_group_name == "pghacker-example"
      && output.kubernetes.cluster_name == "pghacker-example-aks"
      && output.kubernetes.node_resource_group == "pghacker-example-nodes"
      && output.kubernetes.namespace == "pghacker"
      && output.kubernetes.key_vault_name == "pghacker-example-kv"
      && output.kubernetes.image == var.image
      && output.kubernetes.seed_image == null
      && output.kubernetes.pool_label == "pghacker.io/pool"
      && output.kubernetes.swarm_taint == { key = "pghacker.io/pool", value = "swarm", effect = "NoSchedule" }
    )
    error_message = "The kubernetes output must name the cluster, the namespace, the vault, the images, and the pools' label and taint that deploy/k8s uses."
  }

  assert {
    condition = alltrue([
      for name, identity in output.kubernetes.identities :
      identity.client_id == azurerm_user_assigned_identity.workload[name].client_id
      && identity.service_account == trimprefix(azurerm_federated_identity_credential.workload[name].subject, "system:serviceaccount:pghacker:")
      && toset(identity.secrets) == toset([for key, grant in azurerm_role_assignment.workload_secret : split("/", key)[1] if split("/", key)[0] == name])
    ])
    error_message = "Each identity in the output must have its client ID, its federated service account and the secrets it's granted."
  }

  assert {
    condition = (
      output.kubernetes.networks == {
        vnet              = "10.66.0.0/22"
        pod               = "10.244.0.0/16"
        service           = "10.0.0.0/16"
        edge              = "10.66.0.160/28"
        private_endpoints = "10.66.1.0/27"
      }
      && output.kubernetes.edge == {
        subnet               = "edge"
        ip                   = "10.66.0.164"
        private_link_service = "pghacker-example-edge-pls"
        request_message      = "Front Door pghacker-example-afd for pghacker-mcp"
      }
      && output.edge.private_link_service_id == "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example-nodes/providers/Microsoft.Network/privateLinkServices/pghacker-example-edge-pls"
    )
    error_message = "The output must give the networks that the NetworkPolicies name, and the edge's address, subnet and Private Link Service."
  }

  assert {
    condition = toset(keys(output.kubernetes.app_settings)) == toset([
      "PGHACKER_DEPLOYMENT", "PGHACKER_DB_HOST", "PGHACKER_DB_PORT", "PGHACKER_DB_NAME", "PGHACKER_DB_SSLMODE", "PGHACKER_DB_ADMIN_USER",
      "PGHACKER_FOUNDRY_ENDPOINT", "PGHACKER_FOUNDRY_SOL", "PGHACKER_FOUNDRY_LUNA", "PGHACKER_FOUNDRY_EMBEDDING",
      "PGHACKER_PUBLIC_URL", "PGHACKER_PORTAL_URL", "PGHACKER_PUBLIC_HOSTS", "PGHACKER_FRONT_DOOR_ID",
      "PGHACKER_RATE_PER_MINUTE", "PGHACKER_RATE_BURST", "PGHACKER_INTERNAL_RATE_PER_MINUTE", "PGHACKER_INTERNAL_RATE_BURST",
      "PGHACKER_MCP_POOL", "PGHACKER_QUERY_POOL", "PGHACKER_CRAWL_START_SESSIONS", "PGHACKER_CRAWL_MAX_SESSIONS",
      "PGHACKER_GITHUB_ORG", "PGHACKER_GITHUB_TEAM", "PGHACKER_PORTAL_ROLE",
      "APPLICATIONINSIGHTS_CONNECTION_STRING", "PGHACKER_USAGE_DETAILS",
      "PGHACKER_FEEDBACK_RATE", "PGHACKER_FEEDBACK_COMMENTS", "PGHACKER_FEEDBACK_QUESTION_GAP", "PGHACKER_FEEDBACK_SESSION_GAP",
      "PGHACKER_FEEDBACK_MIN_QUESTIONS", "PGHACKER_FEEDBACK_CLIENTS", "PGHACKER_FEEDBACK_DEBUG",
    ])
    error_message = "The app's settings must be exactly the contract's: no password, key, client ID or AZURE_ setting."
  }

  assert {
    condition = (
      output.kubernetes.app_settings.PGHACKER_DEPLOYMENT == "azure"
      && output.kubernetes.app_settings.PGHACKER_DB_HOST == "pghacker-example-hdb.mock-cluster.westus3.horizondb.azure.com"
      && output.kubernetes.app_settings.PGHACKER_DB_PORT == "5432"
      && output.kubernetes.app_settings.PGHACKER_DB_NAME == "pghacker"
      && output.kubernetes.app_settings.PGHACKER_DB_SSLMODE == "require"
      && output.kubernetes.app_settings.PGHACKER_DB_ADMIN_USER == "pghacker_admin"
      && output.kubernetes.app_settings.PGHACKER_PUBLIC_URL == "https://pghacker-example.mock.azurefd.net"
      && output.kubernetes.app_settings.PGHACKER_PORTAL_URL == "https://pghacker-example.mock.azurefd.net"
      && output.kubernetes.app_settings.PGHACKER_PUBLIC_HOSTS == "pghacker-example.mock.azurefd.net"
      && output.kubernetes.app_settings.PGHACKER_FRONT_DOOR_ID == "4f6f1d1e-8b3a-4c55-9a7e-0b5d2c9e7a11"
      && output.kubernetes.app_settings.PGHACKER_RATE_PER_MINUTE == "60"
      && output.kubernetes.app_settings.PGHACKER_RATE_BURST == "30"
      && output.kubernetes.app_settings.PGHACKER_INTERNAL_RATE_PER_MINUTE == "300"
      && output.kubernetes.app_settings.PGHACKER_INTERNAL_RATE_BURST == "60"
      && output.kubernetes.app_settings.PGHACKER_MCP_POOL == "16"
      && output.kubernetes.app_settings.PGHACKER_QUERY_POOL == "8"
      && output.kubernetes.app_settings.PGHACKER_CRAWL_START_SESSIONS == "8"
      && output.kubernetes.app_settings.PGHACKER_CRAWL_MAX_SESSIONS == "16"
      && output.kubernetes.app_settings.PGHACKER_GITHUB_ORG == "contoso"
      && output.kubernetes.app_settings.PGHACKER_GITHUB_TEAM == "pghacker-operators"
      && output.kubernetes.app_settings.PGHACKER_PORTAL_ROLE == "user"
    )
    error_message = "The app's settings must have the deployment's values."
  }

  assert {
    condition = !anytrue([
      for value in values(output.kubernetes.app_settings) : strcontains(value, nonsensitive(random_password.db_admin.result))
    ])
    error_message = "No setting may hold a generated secret."
  }
}

# Front Door's origin waits for the edge's Private Link Service, which the
# manifests create; the GitHub OAuth App's secrets are placeholders until the
# operator stores them.
run "first_apply_waits_for_the_edge" {
  assert {
    condition = (
      length(azurerm_cdn_frontdoor_origin.this) == 0
      && length(azurerm_cdn_frontdoor_route.this) == 0
      && length(azurerm_cdn_frontdoor_route.public) == 0
      && azurerm_cdn_frontdoor_origin_group.this.name == "app"
    )
    error_message = "Without create_origin, Front Door must have no origin or route: the edge's Private Link Service doesn't exist before the manifests."
  }

  assert {
    condition = (
      toset([for secret in azurerm_key_vault_secret.github : secret.name]) == toset(["github-client-id", "github-client-secret"])
      && alltrue([for secret in azurerm_key_vault_secret.github : secret.tags["placeholder"] == "true" && nonsensitive(secret.value) == "unset"])
    )
    error_message = "Terraform must create the GitHub OAuth App's secrets as tagged placeholders, so the edge's identity is granted them before the operator stores their values."
  }

  assert {
    condition = (
      random_password.oauth2_cookie.length == 32 && !random_password.oauth2_cookie.special
      && random_password.oauth2_cookie.upper && random_password.oauth2_cookie.lower && random_password.oauth2_cookie.numeric
      && random_password.db_admin.length >= 32 && !random_password.db_admin.special
      && alltrue([for password in random_password.db_role : password.length >= 32 && !password.special])
      && toset([for secret in azurerm_key_vault_secret.db_role : secret.name]) == toset(["pgh-indexer-password", "pgh-reader-password", "pgh-swarm-password"])
      && azurerm_key_vault_secret.oauth2_cookie.name == "oauth2-cookie-secret"
    )
    error_message = "The cookie secret must be exactly 32 alphanumerics, and the database passwords at least 32, stored under their contract names."
  }
}

run "api_server_ranges_admit_the_firewall" {
  state_key = "api-ranges"

  variables {
    api_server_authorized_ip_ranges = ["203.0.113.0/24"]
  }

  assert {
    condition     = azurerm_kubernetes_cluster.this.api_server_access_profile[0].authorized_ip_ranges == toset(["203.0.113.0/24", "20.14.0.10/32"])
    error_message = "The API server's authorized ranges must add the firewall's address, through which the nodes reach it."
  }
}

run "app_env_and_github_users_reach_the_settings" {
  command = plan

  variables {
    app_env      = { RUST_LOG = "info,pghacker=debug" }
    github_users = ["example-user", "octocat"]
  }

  assert {
    condition     = local.app_settings.RUST_LOG == "info,pghacker=debug" && local.app_settings.PGHACKER_GITHUB_USERS == "example-user,octocat"
    error_message = "app_env and github_users must reach the app's settings."
  }
}

run "privacy_url_reaches_the_settings" {
  command = plan

  variables {
    privacy_url = "https://example.com/privacy"
  }

  assert {
    condition     = local.app_settings.PGHACKER_PRIVACY_URL == "https://example.com/privacy"
    error_message = "privacy_url must reach the app's settings as PGHACKER_PRIVACY_URL, for the home page's footer."
  }
}

run "copyright_link_reaches_the_settings" {
  command = plan

  variables {
    copyright_url  = "https://example.com/"
    copyright_text = "© 2026 Example"
  }

  assert {
    condition     = local.app_settings.PGHACKER_COPYRIGHT_URL == "https://example.com/" && local.app_settings.PGHACKER_COPYRIGHT_TEXT == "© 2026 Example"
    error_message = "copyright_url and copyright_text must reach the app's settings as PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT, for the home page's footer."
  }
}

run "acr_sku_must_be_premium" {
  command = plan

  variables {
    acr_sku = "Standard"
  }

  expect_failures = [var.acr_sku]
}

run "subnets_must_not_overlap" {
  command = plan

  variables {
    subnet_prefixes = {
      firewall          = "10.66.0.0/26"
      system            = "10.66.0.64/27"
      services          = "10.66.0.96/27"
      swarm             = "10.66.0.96/27"
      edge              = "10.66.0.160/28"
      private_endpoints = "10.66.1.0/27"
    }
  }

  expect_failures = [var.subnet_prefixes]
}

run "subnets_must_be_in_the_vnet" {
  command = plan

  variables {
    subnet_prefixes = {
      firewall          = "10.66.0.0/26"
      system            = "10.66.0.64/27"
      services          = "10.66.0.96/27"
      swarm             = "10.70.0.128/27"
      edge              = "10.66.0.160/28"
      private_endpoints = "10.66.1.0/27"
    }
  }

  expect_failures = [var.subnet_prefixes]
}

run "the_firewall_subnet_is_a_26_or_larger" {
  command = plan

  variables {
    subnet_prefixes = {
      firewall          = "10.66.0.0/27"
      system            = "10.66.0.64/27"
      services          = "10.66.0.96/27"
      swarm             = "10.66.0.128/27"
      edge              = "10.66.0.160/28"
      private_endpoints = "10.66.1.0/27"
    }
  }

  expect_failures = [var.subnet_prefixes]
}

run "pods_are_apart_from_the_vnet" {
  command = plan

  variables {
    pod_cidr = "10.66.0.0/16"
  }

  expect_failures = [var.pod_cidr]
}

run "services_are_apart_from_the_pods" {
  command = plan

  variables {
    service_cidr = "10.244.0.0/16"
  }

  expect_failures = [var.service_cidr]
}

run "a_pool_has_a_node" {
  command = plan

  variables {
    swarm_pool = { node_count = 0 }
  }

  expect_failures = [var.swarm_pool]
}

run "alert_emails_are_addresses" {
  command = plan

  variables {
    alert_emails = ["ops"]
  }

  expect_failures = [var.alert_emails]
}

run "app_env_cannot_replace_the_contract" {
  command = plan

  variables {
    app_env = { PGHACKER_DB_HOST = "elsewhere.example.com" }
  }

  expect_failures = [var.app_env]
}

run "mcp_pools_stay_under_the_reader_limit" {
  command = plan

  variables {
    mcp_pool_size       = 120
    mcp_query_pool_size = 16
  }

  expect_failures = [var.mcp_pool_size]
}

run "mcp_pools_sized_to_the_database_plan" {
  command = plan

  variables {
    mcp_pool_size       = 64
    mcp_query_pool_size = 16
  }

  assert {
    condition     = local.app_settings.PGHACKER_MCP_POOL == "64" && local.app_settings.PGHACKER_QUERY_POOL == "16"
    error_message = "The app's settings should hold the pool sizes."
  }
}

run "the_agents_rate_limit_reaches_the_settings" {
  command = plan

  variables {
    mcp_internal_rate_per_minute = 120
    mcp_internal_rate_burst      = 20
  }

  assert {
    condition     = local.app_settings.PGHACKER_INTERNAL_RATE_PER_MINUTE == "120" && local.app_settings.PGHACKER_INTERNAL_RATE_BURST == "20"
    error_message = "The app's settings should hold the agents' rate limit on pghacker-mcp's internal listener."
  }
}

run "the_agents_rate_limit_is_at_least_one_a_minute" {
  command = plan

  variables {
    mcp_internal_rate_per_minute = 0
  }

  expect_failures = [var.mcp_internal_rate_per_minute]
}

run "app_env_cannot_lift_the_agents_rate_limit" {
  command = plan

  variables {
    app_env = { PGHACKER_INTERNAL_RATE_PER_MINUTE = "1000000" }
  }

  expect_failures = [var.app_env]
}

run "privacy_url_must_be_https" {
  command = plan

  variables {
    privacy_url = "http://example.com/privacy"
  }

  expect_failures = [var.privacy_url]
}

run "privacy_url_cannot_carry_credentials" {
  command = plan

  variables {
    privacy_url = "https://user:secret@example.com/privacy"
  }

  expect_failures = [var.privacy_url]
}

run "privacy_url_cannot_hold_markup" {
  command = plan

  variables {
    privacy_url = "https://example.com/privacy\"><script>"
  }

  expect_failures = [var.privacy_url]
}

run "app_env_cannot_set_the_privacy_url" {
  command = plan

  variables {
    app_env = { PGHACKER_PRIVACY_URL = "https://example.com/privacy" }
  }

  expect_failures = [var.app_env]
}

run "copyright_url_needs_its_text" {
  command = plan

  variables {
    copyright_url = "https://example.com/"
  }

  expect_failures = [var.copyright_url]
}

run "copyright_text_needs_its_url" {
  command = plan

  variables {
    copyright_text = "© 2026 Example"
  }

  expect_failures = [var.copyright_url]
}

run "copyright_url_must_be_https" {
  command = plan

  variables {
    copyright_url  = "http://example.com/"
    copyright_text = "© 2026 Example"
  }

  expect_failures = [var.copyright_url]
}

run "copyright_text_cannot_hold_a_line_break" {
  command = plan

  variables {
    copyright_url  = "https://example.com/"
    copyright_text = "© 2026\nExample"
  }

  expect_failures = [var.copyright_text]
}

run "copyright_text_is_at_most_a_hundred_characters" {
  command = plan

  variables {
    copyright_url  = "https://example.com/"
    copyright_text = join("", [for i in range(101) : "x"])
  }

  expect_failures = [var.copyright_text]
}

run "app_env_cannot_set_the_copyright_link" {
  command = plan

  variables {
    app_env = { PGHACKER_COPYRIGHT_TEXT = "© 2026 Example" }
  }

  expect_failures = [var.app_env]
}

run "app_env_cannot_configure_oauth2_proxy" {
  command = plan

  variables {
    app_env = { OAUTH2_PROXY_SKIP_AUTH_ROUTES = "^/.*" }
  }

  expect_failures = [var.app_env]
}

run "image_must_be_in_the_registry" {
  command = plan

  variables {
    image = "docker.io/library/nginx:1.27"
  }

  expect_failures = [var.image]
}

run "seed_image_must_be_in_the_registry" {
  command = plan

  variables {
    seed_image = "ghcr.io/example/pghacker-seed:latest"
  }

  expect_failures = [var.seed_image]
}

run "crawl_start_cannot_exceed_its_maximum" {
  command = plan

  variables {
    crawl_start_sessions = 20
  }

  expect_failures = [var.crawl_start_sessions]
}

# The app refuses more than 64 sessions.
run "crawl_sessions_are_capped" {
  command = plan

  variables {
    crawl_max_sessions = 65
  }

  expect_failures = [var.crawl_max_sessions]
}

# The portal's organization and team are required: the old placeholders name
# a real organization, and empty values are refused.
run "github_org_placeholder_is_rejected" {
  command = plan

  variables {
    github_org = "Example-Org"
  }

  expect_failures = [var.github_org]
}

run "github_team_placeholder_is_rejected" {
  command = plan

  variables {
    github_team = "example-team"
  }

  expect_failures = [var.github_team]
}

run "empty_github_org_is_rejected" {
  command = plan

  variables {
    github_org = ""
  }

  expect_failures = [var.github_org]
}

run "empty_github_team_is_rejected" {
  command = plan

  variables {
    github_team = ""
  }

  expect_failures = [var.github_team]
}
