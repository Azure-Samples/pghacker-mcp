# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Usage telemetry: Application Insights on the logs' workspace, which only
# the app's identity may send to, the retention of its AppEvents table, the
# key of the users' keyed hashes, and what the app gets; and none of it with
# usage_enabled = false. Every provider is mocked.

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
  create_app          = true
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

# The IDs that the mocked endpoint connections approve.
override_resource {
  target = azurerm_private_endpoint.horizondb
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-hdb-pe" }
}
override_resource {
  target = azurerm_private_endpoint.foundry
  values = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-foundry-pe" }
}
override_resource {
  target = azurerm_key_vault_secret.usage_id_key[0]
  values = { versionless_id = "https://pghacker-example-kv.vault.azure.net/secrets/usage-id-key" }
}

run "usage_is_recorded_by_default" {
  assert {
    condition = (
      length(azurerm_application_insights.usage) == 1
      && azurerm_application_insights.usage[0].name == "pghacker-example-usage"
      && azurerm_application_insights.usage[0].workspace_id == azurerm_log_analytics_workspace.this.id
      && azurerm_application_insights.usage[0].application_type == "web"
      && !azurerm_application_insights.usage[0].local_authentication_enabled
      && azurerm_application_insights.usage[0].ip_masking_enabled
      && azurerm_application_insights.usage[0].daily_data_cap_in_gb == 5
    )
    error_message = "Usage must go to an Application Insights resource on the logs' workspace that takes only Entra-authenticated telemetry, masks addresses and caps a day at 5 GB."
  }

  assert {
    condition = (
      azurerm_role_assignment.app_usage_publisher[0].scope == azurerm_application_insights.usage[0].id
      && azurerm_role_assignment.app_usage_publisher[0].role_definition_name == "Monitoring Metrics Publisher"
      && azurerm_role_assignment.app_usage_publisher[0].principal_id == azurerm_user_assigned_identity.app.principal_id
    )
    error_message = "The app's identity, and only through this role, must send to the resource."
  }

  assert {
    condition = (
      azapi_resource_action.usage_table[0].type == "Microsoft.OperationalInsights/workspaces/tables@2025-02-01"
      && azapi_resource_action.usage_table[0].resource_id == "${azurerm_log_analytics_workspace.this.id}/tables/AppEvents"
      && azapi_resource_action.usage_table[0].method == "PATCH"
      && azapi_resource_action.usage_table[0].body.properties.retentionInDays == 730
      && azapi_resource_action.usage_table[0].body.properties.totalRetentionInDays == 1095
    )
    error_message = "The AppEvents table must stay queryable two years and be kept three."
  }

  assert {
    condition = (
      azurerm_key_vault_secret.usage_id_key[0].name == "usage-id-key"
      && random_password.usage_id_key[0].length == 64
      && !random_password.usage_id_key[0].special
    )
    error_message = "The users' key must be 64 alphanumerics, stored as usage-id-key."
  }

  assert {
    condition = (
      {
        for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.value if env.secret_name == null
      }["APPLICATIONINSIGHTS_CONNECTION_STRING"] == "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=7a1b2c3d-0000-4000-8000-000000000003"
      && {
        for env in azurerm_container_app.app[0].template[0].container[0].env : env.name => env.secret_name if env.secret_name != null
      }["PGHACKER_USAGE_ID_KEY"] == "usage-id-key"
      && !anytrue(flatten([
        for job in azurerm_container_app_job.this : [
          for env in job.template[0].container[0].env : contains(["APPLICATIONINSIGHTS_CONNECTION_STRING", "PGHACKER_USAGE_ID_KEY"], env.name)
        ]
      ]))
    )
    error_message = "The app, and not the jobs, must get the connection string and the users' key."
  }

  assert {
    condition     = output.usage_app_insights_name == "pghacker-example-usage" && output.usage_app_id == "7a1b2c3d-0000-4000-8000-000000000003"
    error_message = "The outputs must name the resource and its application ID."
  }
}

run "usage_can_be_turned_off" {
  variables {
    usage_enabled = false
  }

  assert {
    condition = (
      length(azurerm_application_insights.usage) == 0
      && length(azurerm_role_assignment.app_usage_publisher) == 0
      && length(azapi_resource_action.usage_table) == 0
      && length(azurerm_key_vault_secret.usage_id_key) == 0
      && output.usage_app_insights_name == null
      && output.usage_app_id == null
    )
    error_message = "usage_enabled = false must create none of the usage telemetry."
  }

  assert {
    condition = (
      !anytrue([for env in azurerm_container_app.app[0].template[0].container[0].env : contains(["APPLICATIONINSIGHTS_CONNECTION_STRING", "PGHACKER_USAGE_ID_KEY"], env.name)])
      && !anytrue([for secret in azurerm_container_app.app[0].secret : secret.name == "usage-id-key"])
    )
    error_message = "Without usage, the app must get neither the connection string nor the users' key, so it records nothing."
  }
}

run "total_retention_must_cover_the_queryable_days" {
  command = plan

  variables {
    usage_total_retention_days = 365
  }

  expect_failures = [var.usage_total_retention_days]
}

run "queryable_retention_is_at_most_two_years" {
  command = plan

  variables {
    usage_retention_days       = 731
    usage_total_retention_days = 1095
  }

  expect_failures = [var.usage_retention_days]
}

run "app_env_cannot_replace_the_usage_settings" {
  command = plan

  variables {
    app_env = { APPLICATIONINSIGHTS_CONNECTION_STRING = "InstrumentationKey=elsewhere" }
  }

  expect_failures = [var.app_env]
}
