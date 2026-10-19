# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The Container Apps environment, the app and its jobs: Key Vault references,
# the environment contract, ingress restrictions and the staged first apply.
# Every provider is mocked, so runs apply to synthetic state, never to Azure.
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

# Each generated secret at its own URI, as in Azure.
override_resource {
  target = azurerm_key_vault_secret.db_admin_password
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/db-admin-password" }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_indexer"]
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-indexer-password" }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_reader"]
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-reader-password" }
}
override_resource {
  target = azurerm_key_vault_secret.db_role["pgh_swarm"]
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/pgh-swarm-password" }
}
override_resource {
  target = azurerm_key_vault_secret.oauth2_cookie
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/oauth2-cookie-secret" }
}
override_resource {
  target = azurerm_key_vault_secret.usage_id_key[0]
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/usage-id-key" }
}

# The defaults: everything but the app, so the probe can run first.
run "first_apply_creates_no_app" {
  assert {
    condition     = length(azurerm_container_app.app) == 0 && length(data.azapi_resource.github_secret) == 0
    error_message = "With create_app false, there must be no app, and no need yet for the GitHub secrets."
  }

  assert {
    condition     = output.probe_job_name == "pghacker-example-probe" && output.restore_job_name == null
    error_message = "The first apply must create the probe job, and no restore job without seed_image."
  }

  assert {
    condition = (
      azurerm_container_app_job.this["probe"].template[0].container[0].image == var.image
      && azurerm_container_app_job.this["probe"].template[0].container[0].command == tolist(["node", "scripts/probe.mjs"])
      && azurerm_container_app_job.this["probe"].template[0].container[0].cpu == 2
      && azurerm_container_app_job.this["probe"].template[0].container[0].memory == "4Gi"
      && azurerm_container_app_job.this["probe"].replica_timeout_in_seconds == 3600
      && azurerm_container_app_job.this["probe"].replica_retry_limit == 0
      && azurerm_container_app_job.this["probe"].manual_trigger_config[0].parallelism == 1
      && azurerm_container_app_job.this["probe"].manual_trigger_config[0].replica_completion_count == 1
      && azurerm_container_app_job.this["probe"].workload_profile_name == "Consumption"
    )
    error_message = "The probe job must run node scripts/probe.mjs from the app's image, once, by hand, on 2 vCPUs and 4Gi, for up to an hour."
  }

  assert {
    condition = (
      azurerm_container_app_environment.this.name == "pghacker-example-env"
      && azurerm_container_app_environment.this.infrastructure_subnet_id == azurerm_subnet.container_apps.id
      && !azurerm_container_app_environment.this.internal_load_balancer_enabled
      && azurerm_container_app_environment.this.log_analytics_workspace_id == azurerm_log_analytics_workspace.this.id
      && azurerm_log_analytics_workspace.this.name == "pghacker-example-logs"
      && [for profile in azurerm_container_app_environment.this.workload_profile : profile.workload_profile_type] == ["Consumption"]
      && azurerm_container_app_job.this["probe"].container_app_environment_id == azurerm_container_app_environment.this.id
    )
    error_message = "The environment must be in the Container Apps subnet, with public ingress, the Consumption profile and the Log Analytics workspace."
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

# The app, once the GitHub secrets exist (mocked), and the restore job.
run "app_and_restore_job" {
  state_key = "full"

  variables {
    create_app = true
    seed_image = "examplepghacr12345.azurecr.io/pghacker-seed:corpus-20261009"
  }

  assert {
    condition     = length(azurerm_container_app.app) == 1 && length(data.azapi_resource.github_secret) == 2
    error_message = "With create_app, the app must exist, after both GitHub secrets were found."
  }

  assert {
    condition = (
      nonsensitive(alltrue([for secret in azurerm_container_app.app[0].secret : secret.value == null && secret.identity == azurerm_user_assigned_identity.app.id]))
      && { for secret in azurerm_container_app.app[0].secret : secret.name => secret.key_vault_secret_id } == {
        for name in ["db-admin-password", "pgh-indexer-password", "pgh-reader-password", "pgh-swarm-password", "oauth2-cookie-secret", "github-client-id", "github-client-secret", "usage-id-key"] :
        name => "https://pghacker-example-kv.vault.azure.net/secrets/${name}"
      }
    )
    error_message = "Every app secret must be a versionless Key Vault reference, through the identity, to the vault secret of the same name, never a value."
  }

  assert {
    condition = alltrue([
      for job in azurerm_container_app_job.this :
      nonsensitive(alltrue([for secret in job.secret : secret.value == null && secret.identity == azurerm_user_assigned_identity.app.id]))
      && { for secret in job.secret : secret.name => secret.key_vault_secret_id } == {
        for name in ["db-admin-password", "pgh-indexer-password", "pgh-reader-password", "pgh-swarm-password"] :
        name => "https://pghacker-example-kv.vault.azure.net/secrets/${name}"
      }
    ])
    error_message = "Every job secret must be a Key Vault reference through the identity, and the jobs must get only the database's secrets."
  }

  assert {
    condition = {
      for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.secret_name if env.secret_name != null
      } == {
      PGHACKER_DB_ADMIN_PASSWORD = "db-admin-password"
      PGH_INDEXER_PASSWORD       = "pgh-indexer-password"
      PGH_READER_PASSWORD        = "pgh-reader-password"
      PGH_SWARM_PASSWORD         = "pgh-swarm-password"
      OAUTH2_PROXY_CLIENT_ID     = "github-client-id"
      OAUTH2_PROXY_CLIENT_SECRET = "github-client-secret"
      OAUTH2_PROXY_COOKIE_SECRET = "oauth2-cookie-secret"
      PGHACKER_USAGE_ID_KEY      = "usage-id-key"
    }
    error_message = "The app's secret variables must read the contract's secrets."
  }

  assert {
    condition = {
      for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null
      } == {
      PGHACKER_DEPLOYMENT             = "azure"
      AZURE_CLIENT_ID                 = "00000000-0000-0000-0000-000000000001"
      PGHACKER_DB_HOST                = "pghacker-example-hdb.mock-cluster.westus3.horizondb.azure.com"
      PGHACKER_DB_PORT                = "5432"
      PGHACKER_DB_NAME                = "pghacker"
      PGHACKER_DB_SSLMODE             = "require"
      PGHACKER_DB_ADMIN_USER          = "pghacker_admin"
      PGHACKER_FOUNDRY_ENDPOINT       = "https://pghacker-example-foundry.openai.azure.com"
      PGHACKER_FOUNDRY_SOL            = "gpt-6.1-sol"
      PGHACKER_FOUNDRY_LUNA           = "gpt-6-luna"
      PGHACKER_FOUNDRY_EMBEDDING      = "text-embedding-3-small"
      PGHACKER_PUBLIC_URL             = "https://pghacker-example.mock.azurefd.net"
      PGHACKER_PORTAL_URL             = "https://pghacker-example.mock.azurefd.net"
      PGHACKER_PUBLIC_HOSTS           = "pghacker-example.mock.azurefd.net,pghacker-example.mock-1a2b3c4d.westus3.azurecontainerapps.io"
      PGHACKER_FRONT_DOOR_ID          = "4f6f1d1e-8b3a-4c55-9a7e-0b5d2c9e7a11"
      PGHACKER_RATE_PER_MINUTE        = "60"
      PGHACKER_RATE_BURST             = "30"
      PGHACKER_CRAWL_START_SESSIONS   = "8"
      PGHACKER_CRAWL_MAX_SESSIONS     = "16"
      PGHACKER_GITHUB_ORG             = "example-org"
      PGHACKER_GITHUB_TEAM            = "example-team"
      PGHACKER_PORTAL_ROLE            = "user"
      PGHACKER_FEEDBACK_RATE          = "0.1"
      PGHACKER_FEEDBACK_COMMENTS      = "false"
      PGHACKER_FEEDBACK_QUESTION_GAP  = "180"
      PGHACKER_FEEDBACK_SESSION_GAP   = "600"
      PGHACKER_FEEDBACK_MIN_QUESTIONS = "3"
      PGHACKER_FEEDBACK_CLIENTS       = "copilot,codex,Visual Studio Code,claude,cursor"
      PGHACKER_FEEDBACK_DEBUG         = "false"
      # The mocked resource's, which only names it: local authentication is off.
      APPLICATIONINSIGHTS_CONNECTION_STRING = "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=7a1b2c3d-0000-4000-8000-000000000003"
      PGHACKER_USAGE_DETAILS                = "false"
    }
    error_message = "The app's plain variables must be exactly the contract's, with the deployment's values; no PGHACKER_GITHUB_USERS without github_users, and none of the home page footer's links (PGHACKER_PRIVACY_URL, PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT) without privacy_url, copyright_url and copyright_text."
  }

  assert {
    condition = alltrue([
      for job in azurerm_container_app_job.this :
      {
        for env in job.template[0].container[0].env : env.name => env.secret_name if env.secret_name != null
        } == {
        PGHACKER_DB_ADMIN_PASSWORD = "db-admin-password"
        PGH_INDEXER_PASSWORD       = "pgh-indexer-password"
        PGH_READER_PASSWORD        = "pgh-reader-password"
        PGH_SWARM_PASSWORD         = "pgh-swarm-password"
      }
      && {
        for env in job.template[0].container[0].env : env.name => env.value if env.secret_name == null
        } == {
        PGHACKER_DEPLOYMENT        = "azure"
        AZURE_CLIENT_ID            = "00000000-0000-0000-0000-000000000001"
        PGHACKER_DB_HOST           = "pghacker-example-hdb.mock-cluster.westus3.horizondb.azure.com"
        PGHACKER_DB_PORT           = "5432"
        PGHACKER_DB_NAME           = "pghacker"
        PGHACKER_DB_SSLMODE        = "require"
        PGHACKER_DB_ADMIN_USER     = "pghacker_admin"
        PGHACKER_FOUNDRY_ENDPOINT  = "https://pghacker-example-foundry.openai.azure.com"
        PGHACKER_FOUNDRY_SOL       = "gpt-6.1-sol"
        PGHACKER_FOUNDRY_LUNA      = "gpt-6-luna"
        PGHACKER_FOUNDRY_EMBEDDING = "text-embedding-3-small"
      }
    ])
    error_message = "The jobs must get the database and Foundry part of the contract, and none of the app's sign-in or public settings."
  }

  assert {
    condition = (
      azurerm_container_app.app[0].name == "pghacker-example"
      && azurerm_container_app.app[0].revision_mode == "Single"
      && azurerm_container_app.app[0].workload_profile_name == "Consumption"
      && azurerm_container_app.app[0].template[0].min_replicas == 1
      && azurerm_container_app.app[0].template[0].max_replicas == 1
      && azurerm_container_app.app[0].template[0].container[0].image == var.image
      && azurerm_container_app.app[0].template[0].container[0].cpu == 4
      && azurerm_container_app.app[0].template[0].container[0].memory == "8Gi"
      && length(coalesce(azurerm_container_app.app[0].template[0].container[0].command, [])) == 0
      && azurerm_container_app.app[0].identity[0].type == "UserAssigned"
      && azurerm_container_app.app[0].identity[0].identity_ids == toset([azurerm_user_assigned_identity.app.id])
      && azurerm_container_app.app[0].registry[0].server == "examplepghacr12345.azurecr.io"
      && azurerm_container_app.app[0].registry[0].identity == azurerm_user_assigned_identity.app.id
    )
    error_message = "The app must be a single-revision singleton of the image's own command, 4 vCPUs and 8Gi, pulling through the identity."
  }

  assert {
    condition = (
      azurerm_container_app.app[0].template[0].container[0].startup_probe[0].port == 8081
      && azurerm_container_app.app[0].template[0].container[0].startup_probe[0].path == "/healthz"
      && azurerm_container_app.app[0].template[0].container[0].startup_probe[0].initial_delay == 5
      && azurerm_container_app.app[0].template[0].container[0].startup_probe[0].interval_seconds == 5
      && azurerm_container_app.app[0].template[0].container[0].startup_probe[0].timeout == 3
      && azurerm_container_app.app[0].template[0].container[0].startup_probe[0].failure_count_threshold == 60
      && azurerm_container_app.app[0].template[0].container[0].readiness_probe[0].port == 8081
      && azurerm_container_app.app[0].template[0].container[0].readiness_probe[0].path == "/readyz"
      && azurerm_container_app.app[0].template[0].container[0].readiness_probe[0].interval_seconds == 10
      && azurerm_container_app.app[0].template[0].container[0].readiness_probe[0].timeout == 5
      && azurerm_container_app.app[0].template[0].container[0].readiness_probe[0].failure_count_threshold == 3
      && azurerm_container_app.app[0].template[0].container[0].liveness_probe[0].port == 8081
      && azurerm_container_app.app[0].template[0].container[0].liveness_probe[0].path == "/healthz"
      && azurerm_container_app.app[0].template[0].container[0].liveness_probe[0].interval_seconds == 15
      && azurerm_container_app.app[0].template[0].container[0].liveness_probe[0].timeout == 5
      && azurerm_container_app.app[0].template[0].container[0].liveness_probe[0].failure_count_threshold == 4
    )
    error_message = "The probes must ask the supervisor's health server on 8081 with the contract's timings."
  }

  assert {
    condition = (
      azurerm_container_app.app[0].ingress[0].external_enabled
      && azurerm_container_app.app[0].ingress[0].target_port == 8080
      && azurerm_container_app.app[0].ingress[0].transport == "auto"
      && !azurerm_container_app.app[0].ingress[0].allow_insecure_connections
      && azurerm_container_app.app[0].ingress[0].fqdn == output.app_fqdn
    )
    error_message = "Ingress must be external HTTPS to port 8080, at the host name Front Door's origin uses."
  }

  assert {
    condition = (
      tolist([for rule in azurerm_container_app.app[0].ingress[0].ip_security_restriction : rule.ip_address_range]) == tolist(["13.107.208.0/24", "13.73.248.16/29", "147.243.0.0/16", "20.21.37.40/29", "20.36.120.104/29"])
      && alltrue([
        for rule in azurerm_container_app.app[0].ingress[0].ip_security_restriction :
        rule.action == "Allow" && rule.description == "AzureFrontDoor.Backend" && rule.name == "afd-${replace(replace(rule.ip_address_range, ".", "-"), "/", "-")}"
      ])
    )
    error_message = "Ingress must allow exactly the AzureFrontDoor.Backend IPv4 ranges, one named rule each, and no IPv6 range."
  }

  assert {
    condition = (
      azurerm_container_app_job.this["restore"].name == "pghacker-example-restore"
      && azurerm_container_app_job.this["restore"].template[0].container[0].image == var.seed_image
      && azurerm_container_app_job.this["restore"].template[0].container[0].command == tolist(["node", "scripts/corpus-restore.mjs"])
      && azurerm_container_app_job.this["restore"].replica_timeout_in_seconds == 7200
      && azurerm_container_app_job.this["restore"].replica_retry_limit == 0
      && azurerm_container_app_job.this["restore"].manual_trigger_config[0].parallelism == 1
      && output.restore_job_name == "pghacker-example-restore"
    )
    error_message = "seed_image must add the restore job: node scripts/corpus-restore.mjs from the seed image, once, for up to two hours."
  }
}

# A second plan over the full deployment's state must still pass every check.
run "full_deployment_replans" {
  command   = plan
  state_key = "full"

  variables {
    create_app = true
    seed_image = "examplepghacr12345.azurecr.io/pghacker-seed:corpus-20261009"
  }

  assert {
    condition     = length(azurerm_container_app.app) == 1 && length(azurerm_container_app_job.this) == 2
    error_message = "The plan must keep the app and both jobs."
  }
}

run "origin_open_without_the_restriction" {
  state_key = "open"

  variables {
    create_app                    = true
    restrict_origin_to_front_door = false
  }

  assert {
    condition     = length(azurerm_container_app.app[0].ingress[0].ip_security_restriction) == 0 && length(data.azurerm_network_service_tags.front_door_backend) == 0
    error_message = "restrict_origin_to_front_door = false must leave ingress without rules and skip the service tag."
  }
}

run "app_env_and_github_users_reach_only_the_app" {
  state_key = "extras"

  variables {
    create_app   = true
    app_env      = { RUST_LOG = "info,pghacker=debug" }
    github_users = ["example-user", "octocat"]
  }

  assert {
    condition = (
      { for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null }["RUST_LOG"] == "info,pghacker=debug"
      && { for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null }["PGHACKER_GITHUB_USERS"] == "example-user,octocat"
      && !anytrue([
        for env in azurerm_container_app_job.this["probe"].template[0].container[0].env :
        contains(["RUST_LOG", "PGHACKER_GITHUB_USERS"], env.name)
      ])
    )
    error_message = "app_env and github_users must reach the app, and only the app."
  }
}

run "missing_github_secrets_stop_the_app" {
  command = plan

  variables {
    create_app = true
  }

  override_data {
    target = data.azapi_resource.github_secret
    values = { exists = false }
  }

  expect_failures = [data.azapi_resource.github_secret]
}

run "app_env_cannot_replace_the_contract" {
  command = plan

  variables {
    app_env = { PGHACKER_DB_HOST = "elsewhere.example.com" }
  }

  expect_failures = [var.app_env]
}

run "privacy_url_reaches_only_the_app" {
  state_key = "privacy"

  variables {
    create_app  = true
    privacy_url = "https://example.com/privacy"
  }

  assert {
    condition = (
      { for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null }["PGHACKER_PRIVACY_URL"] == "https://example.com/privacy"
      && !anytrue(flatten([
        for job in azurerm_container_app_job.this : [
          for env in job.template[0].container[0].env : env.name == "PGHACKER_PRIVACY_URL"
        ]
      ]))
    )
    error_message = "privacy_url must reach the app, and only the app, as PGHACKER_PRIVACY_URL, for the home page's footer."
  }
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

run "copyright_link_reaches_only_the_app" {
  state_key = "copyright"

  variables {
    create_app     = true
    copyright_url  = "https://example.com/"
    copyright_text = "© 2026 Example"
  }

  assert {
    condition = (
      { for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null }["PGHACKER_COPYRIGHT_URL"] == "https://example.com/"
      && { for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null }["PGHACKER_COPYRIGHT_TEXT"] == "© 2026 Example"
      && !anytrue(flatten([
        for job in azurerm_container_app_job.this : [
          for env in job.template[0].container[0].env : startswith(env.name, "PGHACKER_COPYRIGHT_")
        ]
      ]))
    )
    error_message = "copyright_url and copyright_text must reach the app, and only the app, as PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT, for the home page's footer."
  }
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

run "memory_must_be_twice_cpu" {
  command = plan

  variables {
    cpu    = 2
    memory = "8Gi"
  }

  expect_failures = [var.memory]
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
