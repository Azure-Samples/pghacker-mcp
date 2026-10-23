# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The AKS cluster, as Waldemort runs PilotSwarm, without its extras: OIDC and
# workload identity, the Key Vault secrets provider (the Secrets Store CSI
# driver), Container Insights, and image pulls by a kubelet identity of its
# own, made here with its role before the cluster. Its
# network is Azure CNI Overlay with Cilium, which enforces the manifests'
# NetworkPolicies, and its outbound traffic goes through Azure Firewall
# (userDefinedRouting). Entra ID authorizes every request to its API server;
# it has no local accounts.
#
# Three pools, each in its own subnet:
#   system    the cluster's add-ons only (CriticalAddonsOnly)
#   services  oauth2-proxy, the home page, pghacker-mcp, pghacker-indexer,
#             the token proxy and the ops jobs
#   swarm     PilotSwarm: the portal and the worker, which runs every agent
#             session and the crawl scheduler. Tainted, so nothing else lands
#             there, and given no internet by the firewall.
# The manifests (deploy/k8s) pin each component with the pools' label,
# pghacker.io/pool.

locals {
  aks_name = "${var.name_prefix}-aks"
  # Where AKS puts the nodes, the load balancers and the edge's Private Link
  # Service, whose ID Front Door's origin names.
  aks_node_resource_group = "${substr(var.resource_group_name, 0, 84)}-nodes"

  pool_label = "pghacker.io/pool"
  swarm_taint = {
    key    = local.pool_label
    value  = "swarm"
    effect = "NoSchedule"
  }
}

# The control plane's identity, which manages the nodes' network: the
# subnets, the route table and the edge's load balancer and Private Link
# Service.
resource "azurerm_user_assigned_identity" "aks" {
  name                = "${local.aks_name}-id"
  resource_group_name = local.rg_name
  location            = var.location
  tags                = var.tags
}

resource "azurerm_role_assignment" "aks_network" {
  scope                            = azurerm_virtual_network.this.id
  role_definition_name             = "Network Contributor"
  principal_id                     = azurerm_user_assigned_identity.aks.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

resource "azurerm_role_assignment" "aks_route_table" {
  scope                            = azurerm_route_table.egress.id
  role_definition_name             = "Network Contributor"
  principal_id                     = azurerm_user_assigned_identity.aks.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

# The kubelet's identity, which pulls the images: made here, as Waldemort
# does, so that it can pull from the registry (AcrPull, below) from the first
# node on, and no pod holds a registry credential. The control plane puts it
# on the nodes, which takes Managed Identity Operator on it.
resource "azurerm_user_assigned_identity" "kubelet" {
  name                = "${local.aks_name}-kubelet-id"
  resource_group_name = local.rg_name
  location            = var.location
  tags                = var.tags
}

resource "azurerm_role_assignment" "aks_kubelet_operator" {
  scope                            = azurerm_user_assigned_identity.kubelet.id
  role_definition_name             = "Managed Identity Operator"
  principal_id                     = azurerm_user_assigned_identity.aks.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

resource "azurerm_role_assignment" "kubelet_acr_pull" {
  scope                            = azurerm_container_registry.this.id
  role_definition_name             = "AcrPull"
  principal_id                     = azurerm_user_assigned_identity.kubelet.principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}

resource "azurerm_kubernetes_cluster" "this" {
  name                      = local.aks_name
  location                  = var.location
  resource_group_name       = local.rg_name
  dns_prefix                = local.aks_name
  node_resource_group       = local.aks_node_resource_group
  kubernetes_version        = var.kubernetes_version
  sku_tier                  = var.aks_sku_tier
  automatic_upgrade_channel = "patch"
  node_os_upgrade_channel   = "NodeImage"
  oidc_issuer_enabled       = true
  workload_identity_enabled = true
  local_account_disabled    = true
  tags                      = var.tags

  role_based_access_control_enabled = true
  azure_active_directory_role_based_access_control {
    azure_rbac_enabled = true
    tenant_id          = data.azurerm_client_config.current.tenant_id
  }

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.aks.id]
  }

  kubelet_identity {
    client_id                 = azurerm_user_assigned_identity.kubelet.client_id
    object_id                 = azurerm_user_assigned_identity.kubelet.principal_id
    user_assigned_identity_id = azurerm_user_assigned_identity.kubelet.id
  }

  default_node_pool {
    name                         = "system"
    vm_size                      = var.system_pool.vm_size
    auto_scaling_enabled         = true
    min_count                    = var.system_pool.min_count
    max_count                    = var.system_pool.max_count
    vnet_subnet_id               = azurerm_subnet.nodes["system"].id
    os_sku                       = "AzureLinux"
    only_critical_addons_enabled = true
    temporary_name_for_rotation  = "systemtmp"
    node_labels                  = { (local.pool_label) = "system" }

    upgrade_settings {
      max_surge = "10%"
    }
  }

  network_profile {
    network_plugin      = "azure"
    network_plugin_mode = "overlay"
    network_data_plane  = "cilium"
    network_policy      = "cilium"
    outbound_type       = "userDefinedRouting"
    load_balancer_sku   = "standard"
    pod_cidr            = var.pod_cidr
    service_cidr        = var.service_cidr
    dns_service_ip      = cidrhost(var.service_cidr, 10)
  }

  node_provisioning_profile {
    mode = "Manual"
  }

  key_vault_secrets_provider {
    secret_rotation_enabled  = true
    secret_rotation_interval = "2m"
  }

  oms_agent {
    log_analytics_workspace_id      = azurerm_log_analytics_workspace.this.id
    msi_auth_for_monitoring_enabled = true
  }

  dynamic "api_server_access_profile" {
    for_each = length(var.api_server_authorized_ip_ranges) > 0 ? [1] : []
    content {
      # The firewall's address too: the nodes reach the API server through it.
      authorized_ip_ranges = concat(var.api_server_authorized_ip_ranges, ["${azurerm_public_ip.firewall.ip_address}/32"])
    }
  }

  lifecycle {
    # The autoscaler sets the system pool's size. An Azure Policy assignment
    # may install the Azure Policy add-on (deployIfNotExists): it's left as
    # found, rather than removed by every apply and installed again.
    ignore_changes = [default_node_pool[0].node_count, azure_policy_enabled]
  }

  # The nodes need their way out, through the firewall, before they start,
  # and their identity its pull; the control plane needs its roles on the
  # network and on that identity.
  depends_on = [
    azurerm_subnet_route_table_association.nodes,
    azurerm_firewall.this,
    azurerm_firewall_policy_rule_collection_group.egress,
    azurerm_role_assignment.kubelet_acr_pull,
    time_sleep.aks_rbac_propagation,
  ]
}

resource "azurerm_kubernetes_cluster_node_pool" "services" {
  name                        = "services"
  kubernetes_cluster_id       = azurerm_kubernetes_cluster.this.id
  mode                        = "User"
  vm_size                     = var.services_pool.vm_size
  node_count                  = var.services_pool.node_count
  vnet_subnet_id              = azurerm_subnet.nodes["services"].id
  os_sku                      = "AzureLinux"
  temporary_name_for_rotation = "servicestmp"
  node_labels                 = { (local.pool_label) = "services" }
  tags                        = var.tags

  upgrade_settings {
    max_surge = "1"
  }
}

resource "azurerm_kubernetes_cluster_node_pool" "swarm" {
  name                        = "swarm"
  kubernetes_cluster_id       = azurerm_kubernetes_cluster.this.id
  mode                        = "User"
  vm_size                     = var.swarm_pool.vm_size
  node_count                  = var.swarm_pool.node_count
  vnet_subnet_id              = azurerm_subnet.nodes["swarm"].id
  os_sku                      = "AzureLinux"
  temporary_name_for_rotation = "swarmtmp"
  node_labels                 = { (local.pool_label) = "swarm" }
  node_taints                 = ["${local.swarm_taint.key}=${local.swarm_taint.value}:${local.swarm_taint.effect}"]
  tags                        = var.tags

  upgrade_settings {
    max_surge = "1"
  }
}

# The appliers deploy the manifests (../scripts/aks.mjs) with Entra ID.
resource "azurerm_role_assignment" "deployer_aks_admin" {
  for_each = local.deployer_object_ids

  scope                = azurerm_kubernetes_cluster.this.id
  role_definition_name = "Azure Kubernetes Service RBAC Cluster Admin"
  principal_id         = each.value
}
