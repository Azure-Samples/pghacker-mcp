# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

locals {
  rg_name = azurerm_resource_group.this.name
  rg_id   = azurerm_resource_group.this.id
  # Built from variables, so known while planning: Front Door's route checks
  # its endpoint's ID then (frontdoor.tf).
  rg_id_static = "/subscriptions/${var.subscription_id}/resourceGroups/${var.resource_group_name}"

  # The appliers, who get Key Vault Secrets Officer and the cluster's RBAC
  # Cluster Admin: deployer_object_ids, or whoever is applying.
  deployer_object_ids = toset(
    length(var.deployer_object_ids) > 0
    ? [for id in var.deployer_object_ids : lower(id)]
    : [lower(data.azurerm_client_config.current.object_id)]
  )

  law_name                = "${var.name_prefix}-logs"
  vnet_name               = "${var.name_prefix}-vnet"
  horizondb_cluster_name  = coalesce(var.horizondb_cluster_name, "${var.name_prefix}-hdb")
  foundry_account_name    = coalesce(var.foundry_account_name, "${var.name_prefix}-foundry")
  front_door_profile_name = "${var.name_prefix}-afd"

  # The app's Kubernetes namespace, which the workload identities' federated
  # credentials name (identity.tf).
  k8s_namespace = "pghacker"

  # The edge: the internal load balancer's address in its subnet (the first
  # Azure doesn't reserve), and the Private Link Service that AKS creates for
  # it in the node resource group, which Front Door's origin connects to.
  edge_ip              = cidrhost(var.subnet_prefixes.edge, 4)
  edge_pls_name        = "${var.name_prefix}-edge-pls"
  edge_pls_id          = "/subscriptions/${var.subscription_id}/resourceGroups/${local.aks_node_resource_group}/providers/Microsoft.Network/privateLinkServices/${local.edge_pls_name}"
  edge_request_message = "Front Door ${local.front_door_profile_name} for pghacker-mcp"

  front_door_host = azapi_resource.front_door_endpoint.output.host_name
  public_host     = coalesce(var.front_door_custom_domain, local.front_door_host)
  public_url      = "https://${local.public_host}"
  # The portal and its sign-in stay on Front Door's own host name: a custom
  # domain serves only the public paths (frontdoor.tf).
  portal_url = "https://${local.front_door_host}"

  # The account's Azure OpenAI endpoint, which its custom subdomain names.
  foundry_endpoint = "https://${azurerm_cognitive_account.foundry.custom_subdomain_name}.openai.azure.com"

  # --- Secrets ---------------------------------------------------------------

  # The passwords of the database roles that the app's bootstrap creates, by
  # role: pgh_indexer writes the corpus, pgh_reader serves the public MCP
  # endpoint and pgh_swarm runs PilotSwarm.
  db_role_secrets = {
    pgh_indexer = "pgh-indexer-password"
    pgh_reader  = "pgh-reader-password"
    pgh_swarm   = "pgh-swarm-password"
  }

  # The GitHub OAuth App's credentials, which the operator stores in the vault
  # (main.tf creates them as placeholders).
  github_secrets = ["github-client-id", "github-client-secret"]

  # Each vault secret's resource ID, the scope of the role assignments that
  # grant it (identity.tf).
  vault_secret_ids = merge(
    { "db-admin-password" = azurerm_key_vault_secret.db_admin_password.resource_versionless_id },
    { for role, name in local.db_role_secrets : name => azurerm_key_vault_secret.db_role[role].resource_versionless_id },
    { "oauth2-cookie-secret" = azurerm_key_vault_secret.oauth2_cookie.resource_versionless_id },
    { for name in local.github_secrets : name => azurerm_key_vault_secret.github[name].resource_versionless_id },
    var.usage_enabled ? { "usage-id-key" = azurerm_key_vault_secret.usage_id_key[0].resource_versionless_id } : {},
  )

  # --- Workloads -----------------------------------------------------------------

  # The components' workload identities: the Kubernetes service account that
  # each is federated with, and the vault secrets that it may read, which its
  # SecretProviderClass syncs (scripts/lib/azure.mjs, componentSecrets, keeps
  # the same list). The token proxy reads none: its identity calls Foundry
  # and Application Insights. Only ops, the jobs, reads the administrator's
  # password.
  workloads = {
    proxy = {
      service_account = "token-proxy"
      secrets         = []
    }
    indexer = {
      service_account = "indexer"
      secrets         = ["pgh-indexer-password"]
    }
    mcp = {
      service_account = "mcp"
      secrets         = concat(["pgh-reader-password"], var.usage_enabled ? ["usage-id-key"] : [])
    }
    swarm = {
      service_account = "swarm"
      secrets         = ["pgh-swarm-password"]
    }
    edge = {
      service_account = "edge"
      secrets         = ["github-client-id", "github-client-secret", "oauth2-cookie-secret"]
    }
    home = {
      service_account = "home"
      secrets         = var.usage_enabled ? ["usage-id-key"] : []
    }
    ops = {
      service_account = "ops"
      secrets         = ["db-admin-password", "pgh-indexer-password", "pgh-reader-password", "pgh-swarm-password"]
    }
  }

  # One grant per identity and secret.
  workload_secret_grants = {
    for grant in flatten([
      for identity, workload in local.workloads : [
        for secret in workload.secrets : { identity = identity, secret = secret }
      ]
    ]) : "${grant.identity}/${grant.secret}" => grant
  }

  # --- The app's settings ------------------------------------------------------

  # What the components' environments are made from (scripts/lib/azure.mjs):
  # the database, Foundry, the public face (the home page's footer links, if
  # privacy_url and copyright_url give any), sign-in, the crawler, usage
  # telemetry and feedback, then app_env, whose validation keeps it from
  # replacing any of these. No secret: the connection string's
  # instrumentation key only names the resource, with local authentication
  # off.
  app_settings = merge(
    {
      PGHACKER_DEPLOYMENT               = "azure"
      PGHACKER_DB_HOST                  = azapi_resource.horizondb.output.fqdn
      PGHACKER_DB_PORT                  = "5432"
      PGHACKER_DB_NAME                  = var.database_name
      PGHACKER_DB_SSLMODE               = "require"
      PGHACKER_DB_ADMIN_USER            = var.horizondb_admin_login
      PGHACKER_FOUNDRY_ENDPOINT         = local.foundry_endpoint
      PGHACKER_FOUNDRY_SOL              = azurerm_cognitive_deployment.this[var.foundry_sol_deployment].name
      PGHACKER_FOUNDRY_LUNA             = azurerm_cognitive_deployment.this[var.foundry_luna_deployment].name
      PGHACKER_FOUNDRY_EMBEDDING        = azurerm_cognitive_deployment.this[var.foundry_embedding_deployment].name
      PGHACKER_PUBLIC_URL               = local.public_url
      PGHACKER_PORTAL_URL               = local.portal_url
      PGHACKER_PUBLIC_HOSTS             = join(",", distinct([local.public_host, local.front_door_host]))
      PGHACKER_FRONT_DOOR_ID            = azurerm_cdn_frontdoor_profile.this.resource_guid
      PGHACKER_RATE_PER_MINUTE          = tostring(var.mcp_rate_per_minute)
      PGHACKER_RATE_BURST               = tostring(var.mcp_rate_burst)
      PGHACKER_MCP_CALLS_PER_CLIENT     = tostring(var.mcp_calls_per_client)
      PGHACKER_INTERNAL_RATE_PER_MINUTE = tostring(var.mcp_internal_rate_per_minute)
      PGHACKER_INTERNAL_RATE_BURST      = tostring(var.mcp_internal_rate_burst)
      PGHACKER_MCP_POOL                 = tostring(var.mcp_pool_size)
      PGHACKER_QUERY_POOL               = tostring(var.mcp_query_pool_size)
      PGHACKER_CRAWL_START_SESSIONS     = tostring(var.crawl_start_sessions)
      PGHACKER_CRAWL_MAX_SESSIONS       = tostring(var.crawl_max_sessions)
      PGHACKER_GITHUB_ORG               = var.github_org
      PGHACKER_GITHUB_TEAM              = var.github_team
      PGHACKER_PORTAL_ROLE              = var.portal_role
    },
    length(var.github_users) > 0 ? { PGHACKER_GITHUB_USERS = join(",", var.github_users) } : {},
    var.privacy_url != null ? { PGHACKER_PRIVACY_URL = var.privacy_url } : {},
    var.copyright_url != null ? {
      PGHACKER_COPYRIGHT_URL  = var.copyright_url
      PGHACKER_COPYRIGHT_TEXT = var.copyright_text
    } : {},
    var.usage_enabled ? {
      APPLICATIONINSIGHTS_CONNECTION_STRING = nonsensitive(azurerm_application_insights.usage[0].connection_string)
      PGHACKER_USAGE_DETAILS                = tostring(var.usage_details_enabled)
    } : {},
    # Feedback records with usage telemetry, so it goes with it.
    var.usage_enabled ? {
      PGHACKER_FEEDBACK_RATE          = tostring(var.feedback_rate_per_hour)
      PGHACKER_FEEDBACK_COMMENTS      = tostring(var.feedback_comments_enabled)
      PGHACKER_FEEDBACK_QUESTION_GAP  = tostring(var.feedback_question_gap_seconds)
      PGHACKER_FEEDBACK_SESSION_GAP   = tostring(var.feedback_session_gap_seconds)
      PGHACKER_FEEDBACK_MIN_QUESTIONS = tostring(var.feedback_min_questions)
      PGHACKER_FEEDBACK_CLIENTS       = join(",", var.feedback_clients)
      PGHACKER_FEEDBACK_DEBUG         = tostring(var.feedback_debug_enabled)
    } : {},
    var.app_env,
  )
}
