# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The AKS cluster, as Waldemort runs PilotSwarm, without its extras: OIDC and
# workload identity, the Key Vault secrets provider (the Secrets Store CSI
# driver), Container Insights (the app's namespace alone, below), and image
# pulls by a kubelet identity of its own, made here with its role before the
# cluster. Its network is Azure CNI Overlay with Cilium, which enforces the
# manifests' NetworkPolicies, and its outbound traffic goes through Azure
# Firewall (userDefinedRouting). Its API server is private, and Entra ID
# authorizes every request to it; it has no local accounts.
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

  # A private cluster: the API server's address is a private endpoint in the
  # system pool's subnet, whose name a private DNS zone that AKS keeps holds;
  # it has no public address or name. The nodes reach it inside the VNet, not
  # through the firewall. Operators reach it through Azure Resource Manager:
  # the run command (az aks command invoke), which ../scripts/aks.mjs uses,
  # runs kubectl in a pod in the cluster with their Entra ID token.
  private_cluster_enabled             = true
  private_cluster_public_fqdn_enabled = false
  run_command_enabled                 = true

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

  service_mesh_profile {
    mode                             = "Istio"
    revisions                        = [var.istio_revision]
    internal_ingress_gateway_enabled = false
    external_ingress_gateway_enabled = false
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

# AzureRM owns the mesh mode/revision but doesn't expose CNIChaining.
# AzAPI merges only this otherwise-unmanaged property into the cluster.
resource "azapi_update_resource" "mesh_cni" {
  type        = "Microsoft.ContainerService/managedClusters@2026-03-01"
  resource_id = azurerm_kubernetes_cluster.this.id
  body = {
    properties = {
      serviceMeshProfile = {
        istio = {
          components = {
            proxyRedirectionMechanism = "CNIChaining"
          }
        }
      }
    }
  }

  retry = {
    error_message_regex  = ["AKSOperationPreempted", "(?i)(operation.*in.progress|in.progress.*operation)"]
    interval_seconds     = 15
    max_interval_seconds = 60
  }

  # Cluster updates cannot overlap pool creation. Azure Policy can also
  # briefly update a new cluster, hence the bounded conflict retry above.
  depends_on = [
    azurerm_kubernetes_cluster_node_pool.services,
    azurerm_kubernetes_cluster_node_pool.swarm,
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

# --- Container Insights ---------------------------------------------------------

# What Container Insights' agent (oms_agent, above) collects. With managed
# identity authentication, it collects nothing until a data collection rule is
# associated with the cluster, which az aks enable-addons makes but the
# oms_agent block doesn't. This one collects the app's namespace, every minute:
#   ContainerLogV2    its containers' stdout and stderr
#   KubePodInventory  its pods and containers, with their restart counts
#   KubeEvents        their Warning events: back-offs, failed probes, evictions
# The namespace setting covers neither the containers' logs nor the agent's
# own pods and their events, so a transformation keeps each table to the
# namespace: kubectl's output from the run command (../scripts/aks.mjs), which
# its pods print in the aks-command namespace, stays out of the workspace.
locals {
  container_insights_streams = ["Microsoft-ContainerLogV2", "Microsoft-KubePodInventory", "Microsoft-KubeEvents"]
}

resource "azurerm_monitor_data_collection_rule" "container_insights" {
  name                = "MSCI-${var.location}-${local.aks_name}"
  resource_group_name = local.rg_name
  location            = var.location
  kind                = "Linux"
  description         = "Container Insights for ${local.aks_name}: the ${local.k8s_namespace} namespace's logs, pods and Warning events."
  tags                = var.tags

  destinations {
    log_analytics {
      name                  = "ciworkspace"
      workspace_resource_id = azurerm_log_analytics_workspace.this.id
    }
  }

  data_sources {
    extension {
      name           = "ContainerInsightsExtension"
      extension_name = "ContainerInsights"
      streams        = local.container_insights_streams
      extension_json = jsonencode({
        dataCollectionSettings = {
          interval               = "1m"
          namespaceFilteringMode = "Include"
          namespaces             = [local.k8s_namespace]
          enableContainerLogV2   = true
        }
      })
    }
  }

  data_flow {
    streams       = ["Microsoft-ContainerLogV2"]
    destinations  = ["ciworkspace"]
    transform_kql = "source | where PodNamespace == '${local.k8s_namespace}'"
  }

  data_flow {
    streams       = ["Microsoft-KubePodInventory"]
    destinations  = ["ciworkspace"]
    transform_kql = "source | where Namespace == '${local.k8s_namespace}'"
  }

  data_flow {
    streams       = ["Microsoft-KubeEvents"]
    destinations  = ["ciworkspace"]
    transform_kql = "source | where Namespace == '${local.k8s_namespace}'"
  }
}

# The agent looks for its rule's association by this name.
resource "azurerm_monitor_data_collection_rule_association" "container_insights" {
  name                    = "ContainerInsightsExtension"
  target_resource_id      = azurerm_kubernetes_cluster.this.id
  data_collection_rule_id = azurerm_monitor_data_collection_rule.container_insights.id
  description             = "Container Insights' data collection rule for ${local.aks_name}."
}
