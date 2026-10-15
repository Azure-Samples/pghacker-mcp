# The Container Apps environment, the app and its manual jobs. They share the
# identity, the registry, the Key Vault references and the database and
# Foundry settings (locals.tf); the restore job runs the seed image.

resource "azurerm_container_app_environment" "this" {
  name                       = local.environment_name
  resource_group_name        = local.rg_name
  location                   = var.location
  logs_destination           = "log-analytics"
  log_analytics_workspace_id = azurerm_log_analytics_workspace.this.id
  tags                       = var.tags

  # In the VNet, to reach the private endpoints, with public ingress for
  # Front Door.
  infrastructure_subnet_id       = azurerm_subnet.container_apps.id
  internal_load_balancer_enabled = false

  # A workload-profiles environment with only the Consumption profile: billed
  # per use, and a dedicated profile can be added later.
  workload_profile {
    name                  = "Consumption"
    workload_profile_type = "Consumption"
  }
}

# --- Front Door's backend addresses -------------------------------------------

# Container Apps' ingress rules take neither service tags nor IPv6, so the
# app admits the tag's IPv4 ranges one rule each. The tag is read at every
# plan; its ranges change now and then.
data "azurerm_network_service_tags" "front_door_backend" {
  count = var.restrict_origin_to_front_door ? 1 : 0

  location = var.location
  service  = "AzureFrontDoor.Backend"

  lifecycle {
    postcondition {
      condition     = length(self.ipv4_cidrs) > 0
      error_message = "The AzureFrontDoor.Backend service tag lists no IPv4 ranges, and an ingress without allow rules would admit everyone. Plan again later."
    }
  }
}

locals {
  front_door_backend_cidrs = var.restrict_origin_to_front_door ? sort(distinct(data.azurerm_network_service_tags.front_door_backend[0].ipv4_cidrs)) : []
}

# --- Jobs -------------------------------------------------------------------------

# Started by hand. The probe checks the database and Foundry from inside the
# VNet, so it exists from the first apply, before the app; the restore job
# loads the corpus from seed_image into an empty database.
locals {
  jobs = merge(
    {
      probe = {
        name    = "${var.app_name}-probe"
        image   = var.image
        command = ["node", "scripts/probe.mjs"]
        timeout = 3600
      }
    },
    var.seed_image == null ? {} : {
      restore = {
        name    = "${var.app_name}-restore"
        image   = var.seed_image
        command = ["node", "scripts/corpus-restore.mjs"]
        timeout = 7200
      }
    },
  )
}

resource "azurerm_container_app_job" "this" {
  for_each = local.jobs

  name                         = each.value.name
  resource_group_name          = local.rg_name
  location                     = var.location
  container_app_environment_id = azurerm_container_app_environment.this.id
  workload_profile_name        = "Consumption"
  replica_timeout_in_seconds   = each.value.timeout
  replica_retry_limit          = 0
  tags                         = var.tags

  manual_trigger_config {
    parallelism              = 1
    replica_completion_count = 1
  }

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.app.id]
  }

  registry {
    server   = azurerm_container_registry.this.login_server
    identity = azurerm_user_assigned_identity.app.id
  }

  dynamic "secret" {
    for_each = local.job_secrets
    content {
      name                = secret.key
      key_vault_secret_id = secret.value
      identity            = azurerm_user_assigned_identity.app.id
    }
  }

  template {
    container {
      name    = each.key
      image   = each.value.image
      cpu     = var.job_cpu
      memory  = var.job_memory
      command = each.value.command

      dynamic "env" {
        for_each = local.job_env
        content {
          name  = env.key
          value = env.value
        }
      }

      dynamic "env" {
        for_each = local.job_secret_env
        content {
          name        = env.key
          secret_name = env.value
        }
      }
    }
  }

  # The private paths to the database and the models, approved and resolvable,
  # with the extensions allowed and the identity's roles in effect.
  depends_on = [
    azurerm_private_endpoint.horizondb,
    data.azapi_resource.horizondb_private_endpoint_connection,
    azurerm_private_dns_zone_virtual_network_link.horizondb,
    azapi_resource_action.attach_parameter_group,
    azurerm_private_endpoint.foundry,
    data.azapi_resource.foundry_private_endpoint_connection,
    azurerm_private_dns_zone_virtual_network_link.foundry,
    time_sleep.rbac_propagation,
  ]
}

# --- The app ----------------------------------------------------------------------

# The GitHub OAuth App's credentials, which the operator stores by hand. Read
# through Azure Resource Manager, which returns a secret's metadata but never
# its value; the app's revision can't start without them.
data "azapi_resource" "github_secret" {
  for_each = var.create_app ? toset(local.github_secrets) : toset([])

  type             = "Microsoft.KeyVault/vaults/secrets@2023-07-01"
  parent_id        = azurerm_key_vault.this.id
  name             = each.key
  ignore_not_found = true

  lifecycle {
    postcondition {
      condition     = self.exists
      error_message = "The Key Vault secret ${each.key} doesn't exist yet. Register the GitHub OAuth App with the callback URL in the oauth_callback_url output, store its client ID and secret as github-client-id and github-client-secret (../TERRAFORM.md, \"Key Vault secrets\"), and apply again."
    }
  }
}

# The supervisor and its services in one container: oauth2-proxy on 8080
# sends /mcp to pghacker-mcp and / to the home page without sign-in, and
# /portal (with the portal's /api and /assets), after GitHub sign-in, to the
# PilotSwarm portal (scripts/lib/routes.mjs). A singleton: one replica,
# always on.
resource "azurerm_container_app" "app" {
  count = var.create_app ? 1 : 0

  name                         = var.app_name
  resource_group_name          = local.rg_name
  container_app_environment_id = azurerm_container_app_environment.this.id
  revision_mode                = "Single"
  workload_profile_name        = "Consumption"
  tags                         = var.tags

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.app.id]
  }

  registry {
    server   = azurerm_container_registry.this.login_server
    identity = azurerm_user_assigned_identity.app.id
  }

  dynamic "secret" {
    for_each = local.app_secrets
    content {
      name                = secret.key
      key_vault_secret_id = secret.value
      identity            = azurerm_user_assigned_identity.app.id
    }
  }

  ingress {
    external_enabled           = true
    target_port                = 8080
    transport                  = "auto"
    allow_insecure_connections = false

    traffic_weight {
      latest_revision = true
      percentage      = 100
    }

    # Named after the range, so that a change to the tag changes only the
    # rules concerned.
    dynamic "ip_security_restriction" {
      for_each = local.front_door_backend_cidrs
      content {
        name             = "afd-${replace(replace(ip_security_restriction.value, ".", "-"), "/", "-")}"
        description      = "AzureFrontDoor.Backend"
        action           = "Allow"
        ip_address_range = ip_security_restriction.value
      }
    }
  }

  template {
    min_replicas = 1
    max_replicas = 1

    container {
      name   = "pghacker"
      image  = var.image
      cpu    = var.cpu
      memory = var.memory
      # The image's own command starts the supervisor.

      dynamic "env" {
        for_each = local.app_env
        content {
          name  = env.key
          value = env.value
        }
      }

      dynamic "env" {
        for_each = local.app_secret_env
        content {
          name        = env.key
          secret_name = env.value
        }
      }

      # The probes ask the supervisor's health server on 8081, apart from
      # ingress: /healthz while the supervisor works, /readyz once the
      # bootstrap is done and every service answers. Starting may take
      # 5 + 60 x 5 = 305 s.
      startup_probe {
        transport               = "HTTP"
        port                    = 8081
        path                    = "/healthz"
        initial_delay           = 5
        interval_seconds        = 5
        timeout                 = 3
        failure_count_threshold = 60
      }

      readiness_probe {
        transport               = "HTTP"
        port                    = 8081
        path                    = "/readyz"
        interval_seconds        = 10
        timeout                 = 5
        failure_count_threshold = 3
        success_count_threshold = 1
      }

      liveness_probe {
        transport               = "HTTP"
        port                    = 8081
        path                    = "/healthz"
        interval_seconds        = 15
        timeout                 = 5
        failure_count_threshold = 4
      }
    }
  }

  # As for the jobs, plus the operator's GitHub secrets.
  depends_on = [
    azurerm_private_endpoint.horizondb,
    data.azapi_resource.horizondb_private_endpoint_connection,
    azurerm_private_dns_zone_virtual_network_link.horizondb,
    azapi_resource_action.attach_parameter_group,
    azurerm_private_endpoint.foundry,
    data.azapi_resource.foundry_private_endpoint_connection,
    azurerm_private_dns_zone_virtual_network_link.foundry,
    time_sleep.rbac_propagation,
    data.azapi_resource.github_secret,
  ]
}

check "app_host_name" {
  assert {
    condition     = !var.create_app || try(azurerm_container_app.app[0].ingress[0].fqdn == local.app_fqdn, true)
    error_message = "The app's host name isn't ${local.app_fqdn}, which Front Door's origin, PGHACKER_PUBLIC_HOSTS and the outputs assume."
  }
}
