# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

locals {
  rg_name = azurerm_resource_group.this.name
  rg_id   = azurerm_resource_group.this.id
  # Built from variables, so known while planning: Front Door's route checks
  # its endpoint's ID then (frontdoor.tf).
  rg_id_static = "/subscriptions/${var.subscription_id}/resourceGroups/${var.resource_group_name}"

  # The appliers, who get Key Vault Secrets Officer: deployer_object_ids, or
  # whoever is applying.
  deployer_object_ids = toset(
    length(var.deployer_object_ids) > 0
    ? [for id in var.deployer_object_ids : lower(id)]
    : [lower(data.azurerm_client_config.current.object_id)]
  )

  law_name                = "${var.name_prefix}-logs"
  environment_name        = "${var.name_prefix}-env"
  identity_name           = "${var.name_prefix}-id"
  vnet_name               = "${var.name_prefix}-vnet"
  horizondb_cluster_name  = coalesce(var.horizondb_cluster_name, "${var.name_prefix}-hdb")
  foundry_account_name    = coalesce(var.foundry_account_name, "${var.name_prefix}-foundry")
  front_door_profile_name = "${var.name_prefix}-afd"

  # The app's own host name. It comes from the environment, so Front Door's
  # origin and the app's settings know it before the app exists.
  app_fqdn        = "${var.app_name}.${azurerm_container_app_environment.this.default_domain}"
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
  # by hand; Terraform only refers to them.
  github_secrets = ["github-client-id", "github-client-secret"]

  vault_uri = trimsuffix(azurerm_key_vault.this.vault_uri, "/")

  # Container Apps secrets, named after the vault's: each a versionless
  # reference, read through the managed identity, never a value.
  job_secrets = merge(
    { "db-admin-password" = azurerm_key_vault_secret.db_admin_password.versionless_id },
    { for role, name in local.db_role_secrets : name => azurerm_key_vault_secret.db_role[role].versionless_id },
  )
  app_secrets = merge(
    local.job_secrets,
    { "oauth2-cookie-secret" = azurerm_key_vault_secret.oauth2_cookie.versionless_id },
    { for name in local.github_secrets : name => "${local.vault_uri}/secrets/${name}" },
    var.usage_enabled ? { "usage-id-key" = azurerm_key_vault_secret.usage_id_key[0].versionless_id } : {},
  )

  # --- The container's environment -------------------------------------------

  # Variables read from secrets: variable name => secret name.
  job_secret_env = {
    PGHACKER_DB_ADMIN_PASSWORD = "db-admin-password"
    PGH_INDEXER_PASSWORD       = "pgh-indexer-password"
    PGH_READER_PASSWORD        = "pgh-reader-password"
    PGH_SWARM_PASSWORD         = "pgh-swarm-password"
  }
  app_secret_env = merge(
    local.job_secret_env,
    {
      OAUTH2_PROXY_CLIENT_ID     = "github-client-id"
      OAUTH2_PROXY_CLIENT_SECRET = "github-client-secret"
      OAUTH2_PROXY_COOKIE_SECRET = "oauth2-cookie-secret"
    },
    var.usage_enabled ? { PGHACKER_USAGE_ID_KEY = "usage-id-key" } : {},
  )

  # Plain settings for the app and the jobs. The model names come from the
  # deployments, so that the jobs and the app wait for them. AZURE_CLIENT_ID
  # names the identity whose tokens the Foundry proxy requests.
  job_env = {
    PGHACKER_DEPLOYMENT        = "azure"
    AZURE_CLIENT_ID            = azurerm_user_assigned_identity.app.client_id
    PGHACKER_DB_HOST           = azapi_resource.horizondb.output.fqdn
    PGHACKER_DB_PORT           = "5432"
    PGHACKER_DB_NAME           = var.database_name
    PGHACKER_DB_SSLMODE        = "require"
    PGHACKER_DB_ADMIN_USER     = var.horizondb_admin_login
    PGHACKER_FOUNDRY_ENDPOINT  = local.foundry_endpoint
    PGHACKER_FOUNDRY_SOL       = azurerm_cognitive_deployment.this[var.foundry_sol_deployment].name
    PGHACKER_FOUNDRY_LUNA      = azurerm_cognitive_deployment.this[var.foundry_luna_deployment].name
    PGHACKER_FOUNDRY_EMBEDDING = azurerm_cognitive_deployment.this[var.foundry_embedding_deployment].name
  }

  # The app's: the jobs' plus its public face, sign-in, crawler, usage
  # telemetry and feedback settings, then app_env, whose validation keeps it
  # from replacing any of these. The connection string isn't a secret: with local
  # authentication off, its instrumentation key only names the resource.
  app_env = merge(
    local.job_env,
    {
      PGHACKER_PUBLIC_URL           = local.public_url
      PGHACKER_PORTAL_URL           = local.portal_url
      PGHACKER_PUBLIC_HOSTS         = join(",", distinct([local.public_host, local.front_door_host, local.app_fqdn]))
      PGHACKER_FRONT_DOOR_ID        = azurerm_cdn_frontdoor_profile.this.resource_guid
      PGHACKER_RATE_PER_MINUTE      = tostring(var.mcp_rate_per_minute)
      PGHACKER_RATE_BURST           = tostring(var.mcp_rate_burst)
      PGHACKER_CRAWL_START_SESSIONS = tostring(var.crawl_start_sessions)
      PGHACKER_CRAWL_MAX_SESSIONS   = tostring(var.crawl_max_sessions)
      PGHACKER_GITHUB_ORG           = var.github_org
      PGHACKER_GITHUB_TEAM          = var.github_team
      PGHACKER_PORTAL_ROLE          = var.portal_role
    },
    length(var.github_users) > 0 ? { PGHACKER_GITHUB_USERS = join(",", var.github_users) } : {},
    var.usage_enabled ? { APPLICATIONINSIGHTS_CONNECTION_STRING = nonsensitive(azurerm_application_insights.usage[0].connection_string) } : {},
    # Feedback records with usage telemetry, so it goes with it.
    var.usage_enabled ? {
      PGHACKER_FEEDBACK_RATE          = tostring(var.feedback_rate_per_hour)
      PGHACKER_FEEDBACK_COMMENTS      = tostring(var.feedback_comments_enabled)
      PGHACKER_FEEDBACK_QUESTION_GAP  = tostring(var.feedback_question_gap_seconds)
      PGHACKER_FEEDBACK_SESSION_GAP   = tostring(var.feedback_session_gap_seconds)
      PGHACKER_FEEDBACK_MIN_QUESTIONS = tostring(var.feedback_min_questions)
      PGHACKER_FEEDBACK_CLIENTS       = join(",", var.feedback_clients)
    } : {},
    var.app_env,
  )
}
