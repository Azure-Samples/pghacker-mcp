# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# None of these is secret: passwords and keys stay in Key Vault.

# --- Public endpoints -------------------------------------------------------------

output "public_url" {
  description = "The canonical public URL, on the custom domain when configured, otherwise Front Door's host name: PGHACKER_PUBLIC_URL, the public home page and the GitHub OAuth App's homepage URL."
  value       = local.public_url
}

output "portal_url" {
  description = "The PilotSwarm portal, behind GitHub sign-in, on Front Door's host name (PGHACKER_PORTAL_URL), the only one that serves it: a custom domain serves only the home page and /mcp."
  value       = "${local.portal_url}/portal/"
}

output "mcp_url" {
  description = "The public, anonymous MCP endpoint."
  value       = "${local.public_url}/mcp"
}

output "oauth_callback_url" {
  description = "The authorization callback URL to register on the GitHub OAuth App, where oauth2-proxy completes sign-in: on Front Door's host name, the portal's, with or without a custom domain."
  value       = "${local.portal_url}/oauth2/callback"
}

output "front_door_host_name" {
  description = "Front Door's host name, <front_door_endpoint_name>-<hash>.<zone>.azurefd.net."
  value       = local.front_door_host
}

output "front_door_id" {
  description = "The Front Door profile's ID (resource_guid), sent in each request's X-Azure-FDID header: PGHACKER_FRONT_DOOR_ID."
  value       = azurerm_cdn_frontdoor_profile.this.resource_guid
}

output "front_door_custom_domain_validation" {
  description = "The ownership TXT record's full name, value and token expiration to publish with your DNS provider; null without a custom domain."
  value = var.front_door_custom_domain == null ? null : {
    name       = "_dnsauth.${var.front_door_custom_domain}"
    value      = azurerm_cdn_frontdoor_custom_domain.this[0].validation_token
    expiration = azurerm_cdn_frontdoor_custom_domain.this[0].expiration_date
  }
}

output "edge" {
  description = "The edge, Front Door's origin: the internal load balancer's address and its Private Link Service, which AKS creates in the node resource group for the edge Service, and whose connection from Front Door ../scripts/aks.mjs approve-edge approves."
  value = {
    ip                       = local.edge_ip
    private_link_service     = local.edge_pls_name
    private_link_service_id  = local.edge_pls_id
    node_resource_group_name = local.aks_node_resource_group
  }
}

# --- Resources for operator commands -------------------------------------------------

output "subscription_id" {
  description = "The subscription."
  value       = var.subscription_id
}

output "resource_group_name" {
  description = "The resource group."
  value       = local.rg_name
}

output "acr_name" {
  description = "The container registry's name, for az acr run."
  value       = azurerm_container_registry.this.name
}

output "acr_login_server" {
  description = "The container registry's login server, which image and seed_image must start with."
  value       = azurerm_container_registry.this.login_server
}

output "key_vault_name" {
  description = "The Key Vault's name, where the operator stores github-client-id and github-client-secret over Terraform's placeholders."
  value       = azurerm_key_vault.this.name
}

output "key_vault_uri" {
  description = "The Key Vault's URI."
  value       = azurerm_key_vault.this.vault_uri
}

output "horizondb_cluster_name" {
  description = "The HorizonDB cluster's name."
  value       = azapi_resource.horizondb.name
}

output "horizondb_fqdn" {
  description = "The HorizonDB cluster's host name (PGHACKER_DB_HOST), which resolves to the private endpoint inside the VNet."
  value       = azapi_resource.horizondb.output.fqdn
}

output "horizondb_parameter_group" {
  description = "The parameter group attached to the cluster."
  value       = azapi_resource.pg_parameters.name
}

output "horizondb_firewall_rules" {
  description = "The emergency firewall rules from extra_firewall_ips, rule name to IPv4 address. Empty by default."
  value       = local.horizondb_firewall_rules
}

output "foundry_account_name" {
  description = "The Foundry (AIServices) account's name."
  value       = azurerm_cognitive_account.foundry.name
}

output "foundry_endpoint" {
  description = "The Foundry account's Azure OpenAI endpoint (PGHACKER_FOUNDRY_ENDPOINT), reachable only inside the VNet."
  value       = local.foundry_endpoint
}

output "aks_cluster_name" {
  description = "The AKS cluster's name, for az aks get-credentials (../scripts/aks.mjs credentials)."
  value       = azurerm_kubernetes_cluster.this.name
}

output "aks_node_resource_group" {
  description = "The resource group where AKS keeps the nodes, the load balancers and the edge's Private Link Service."
  value       = local.aks_node_resource_group
}

output "firewall_public_ip" {
  description = "Azure Firewall's public address, from which the cluster's allowed outbound traffic leaves: allow it where the app's outside services filter by address."
  value       = azurerm_public_ip.firewall.ip_address
}

output "kubernetes" {
  description = "What ../scripts/aks.mjs renders the manifests from: the cluster, the namespace, the images, each workload identity's client ID, service account and vault secrets, the networks the NetworkPolicies name, the edge, and the app's settings, which hold no secret."
  value = {
    subscription_id     = var.subscription_id
    tenant_id           = data.azurerm_client_config.current.tenant_id
    resource_group_name = local.rg_name
    cluster_name        = azurerm_kubernetes_cluster.this.name
    node_resource_group = local.aks_node_resource_group
    namespace           = local.k8s_namespace
    key_vault_name      = azurerm_key_vault.this.name
    image               = var.image
    seed_image          = var.seed_image
    pool_label          = local.pool_label
    swarm_taint         = local.swarm_taint
    identities = {
      for name, workload in local.workloads : name => {
        client_id       = azurerm_user_assigned_identity.workload[name].client_id
        service_account = workload.service_account
        secrets         = workload.secrets
      }
    }
    networks = {
      vnet              = var.vnet_address_space
      pod               = var.pod_cidr
      service           = var.service_cidr
      edge              = var.subnet_prefixes.edge
      private_endpoints = var.subnet_prefixes.private_endpoints
    }
    edge = {
      subnet               = azurerm_subnet.edge.name
      ip                   = local.edge_ip
      private_link_service = local.edge_pls_name
      request_message      = local.edge_request_message
    }
    app_settings = local.app_settings
  }

  # The private paths to the database and the models, approved, before
  # anything is deployed on them.
  depends_on = [
    data.azapi_resource.horizondb_private_endpoint_connection,
    data.azapi_resource.foundry_private_endpoint_connection,
    azurerm_private_dns_zone_virtual_network_link.horizondb,
    azurerm_private_dns_zone_virtual_network_link.foundry,
    azapi_resource_action.attach_parameter_group,
  ]
}

output "log_analytics_workspace_id" {
  description = "The Log Analytics workspace's customer ID, for az monitor log-analytics query: the containers' logs (ContainerLogV2) and the firewall's (AZFWApplicationRule, AZFWNetworkRule)."
  value       = azurerm_log_analytics_workspace.this.workspace_id
}

output "usage_app_insights_name" {
  description = "The usage telemetry's Application Insights resource, or null with usage_enabled = false."
  value       = try(azurerm_application_insights.usage[0].name, null)
}

output "usage_app_id" {
  description = "Its application ID, for az monitor app-insights query --apps, or null with usage_enabled = false."
  value       = try(azurerm_application_insights.usage[0].app_id, null)
}
