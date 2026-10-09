# None of these is secret: passwords and keys stay in Key Vault.

# --- Public endpoints -------------------------------------------------------------

output "public_url" {
  description = "The public URL, on Front Door's host name: PGHACKER_PUBLIC_URL, the GitHub OAuth App's homepage URL and the portal's address."
  value       = local.public_url
}

output "mcp_url" {
  description = "The public, anonymous MCP endpoint."
  value       = "${local.public_url}/mcp"
}

output "oauth_callback_url" {
  description = "The authorization callback URL to register on the GitHub OAuth App, where oauth2-proxy completes sign-in."
  value       = "${local.public_url}/oauth2/callback"
}

output "front_door_host_name" {
  description = "Front Door's host name, <front_door_endpoint_name>-<hash>.<zone>.azurefd.net."
  value       = local.front_door_host
}

output "front_door_id" {
  description = "The Front Door profile's ID (resource_guid), sent in each request's X-Azure-FDID header: PGHACKER_FRONT_DOOR_ID."
  value       = azurerm_cdn_frontdoor_profile.this.resource_guid
}

output "app_fqdn" {
  description = "The container app's own host name, Front Door's origin, which changes when the environment is replaced. With restrict_origin_to_front_door, only Front Door can reach it."
  value       = local.app_fqdn
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
  description = "The Key Vault's name, where the operator stores github-client-id and github-client-secret."
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

output "container_app_environment_name" {
  description = "The Container Apps environment's name."
  value       = azurerm_container_app_environment.this.name
}

output "container_app_environment_default_domain" {
  description = "The Container Apps environment's default domain, which the app's host name ends with."
  value       = azurerm_container_app_environment.this.default_domain
}

output "app_name" {
  description = "The container app's name. It exists only with create_app."
  value       = var.app_name
}

output "probe_job_name" {
  description = "The probe job's name, for az containerapp job start."
  value       = azurerm_container_app_job.this["probe"].name
}

output "restore_job_name" {
  description = "The restore job's name, or null without seed_image."
  value       = try(azurerm_container_app_job.this["restore"].name, null)
}

output "managed_identity_client_id" {
  description = "The client ID of the app's and jobs' user-assigned managed identity."
  value       = azurerm_user_assigned_identity.app.client_id
}

output "log_analytics_workspace_id" {
  description = "The Log Analytics workspace's customer ID, for az monitor log-analytics query, which reads the jobs' reports."
  value       = azurerm_log_analytics_workspace.this.workspace_id
}
