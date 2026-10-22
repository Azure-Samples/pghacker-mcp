# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# HorizonDB (preview), PostgreSQL 17, through azapi. azapi's bundled schemas
# don't cover Microsoft.HorizonDB, so schema validation is off.

locals {
  horizondb_api   = "2026-01-20-preview"
  horizondb_types = "Microsoft.HorizonDB"

  # HorizonDB refuses an operation while another runs on the cluster, such as
  # the restart that attaching a parameter group causes; those are retried.
  horizondb_retry = {
    error_message_regex  = ["RESPONSE 409", "(?i)in progress", "(?i)try again"]
    interval_seconds     = 15
    max_interval_seconds = 120
  }

  # Merged into HorizonDB's defaults, whose azure.extensions allows nothing.
  # The app's bootstrap creates these four extensions; AGE and pg_textsearch
  # must also be preloaded (HorizonDB adds its own libraries to the list).
  pg_parameters = concat(
    [
      { name = "azure.extensions", value = "age,pg_textsearch,pg_trgm,vector" },
      { name = "shared_preload_libraries", value = "age,pg_textsearch" },
    ],
    var.horizondb_max_connections == null ? [] : [
      { name = "max_connections", value = tostring(var.horizondb_max_connections) },
    ],
  )

  # A parameter group can't be changed once created, so its name carries a
  # hash of its content, and different parameters make a different group.
  pg_parameter_group_name = format("%s-pg17-%s", var.name_prefix, substr(sha1(jsonencode({
    location   = var.location
    parameters = local.pg_parameters
  })), 0, 8))

  horizondb_private_endpoint_name = "${local.horizondb_cluster_name}-pe"
}

# create_before_destroy lets the attach action move the cluster to a new
# group before the old one is deleted.
resource "azapi_resource" "pg_parameters" {
  type      = "${local.horizondb_types}/parameterGroups@${local.horizondb_api}"
  name      = local.pg_parameter_group_name
  parent_id = local.rg_id
  location  = var.location
  tags      = var.tags

  body = {
    properties = {
      pgVersion   = 17
      description = "pghacker-mcp (${var.name_prefix})"
      parameters  = local.pg_parameters
    }
  }

  schema_validation_enabled = false

  lifecycle {
    create_before_destroy = true
    # Groups refuse updates, and reading one returns all of its parameters,
    # defaults included. A change of parameters changes the name instead.
    ignore_changes = [body, tags]
  }
}

resource "azapi_resource" "horizondb" {
  type      = "${local.horizondb_types}/clusters@${local.horizondb_api}"
  name      = local.horizondb_cluster_name
  parent_id = local.rg_id
  location  = var.location
  tags      = var.tags

  body = {
    properties = {
      createMode                 = "Create"
      administratorLogin         = var.horizondb_admin_login
      administratorLoginPassword = random_password.db_admin.result
      version                    = "17"
      vCores                     = var.horizondb_vcores
      replicaCount               = var.horizondb_replica_count
      parameterGroup = {
        id               = azapi_resource.pg_parameters.id
        applyImmediately = true
      }
    }
  }

  response_export_values = {
    fqdn  = "properties.fullyQualifiedDomainName"
    state = "properties.state"
  }

  schema_validation_enabled = false

  timeouts {
    create = "60m"
    update = "60m"
    delete = "60m"
  }

  lifecycle {
    # A PUT would resend create-only properties, so later changes go through
    # the PATCH actions below instead.
    ignore_changes = [
      body.properties.parameterGroup,
      body.properties.vCores,
      body.properties.replicaCount,
      body.properties.administratorLoginPassword,
      tags,
    ]
  }
}

# Replica count can only be set at creation here; the plan fails rather than
# silently ignore a change.
data "azapi_resource" "cluster_capacity" {
  type                   = azapi_resource.horizondb.type
  resource_id            = azapi_resource.horizondb.id
  response_export_values = { replicas = "properties.replicaCount" }

  lifecycle {
    postcondition {
      condition     = self.output.replicas == var.horizondb_replica_count
      error_message = "horizondb_replica_count applies only when the cluster is created. Changing an existing cluster's replicas takes HorizonDB's replicas child resources, which this configuration doesn't manage."
    }
  }
}

# A new cluster can come up on the default parameter group even when the
# create request names another, so the group is attached again afterwards,
# and whenever it's replaced. New preload libraries restart the cluster for
# a couple of minutes.
resource "azapi_resource_action" "attach_parameter_group" {
  type        = azapi_resource.horizondb.type
  resource_id = azapi_resource.horizondb.id
  method      = "PATCH"

  body = {
    properties = {
      parameterGroup = {
        id               = azapi_resource.pg_parameters.id
        applyImmediately = true
      }
    }
  }

  locks = [azapi_resource.horizondb.id]
  retry = local.horizondb_retry

  timeouts {
    create = "30m"
    update = "30m"
  }
}

resource "azapi_resource_action" "configure_cluster" {
  type        = azapi_resource.horizondb.type
  resource_id = azapi_resource.horizondb.id
  method      = "PATCH"

  body = {
    tags = var.tags
    properties = {
      vCores = var.horizondb_vcores
    }
  }

  locks = [azapi_resource.horizondb.id]
  retry = local.horizondb_retry

  timeouts {
    create = "60m"
    update = "60m"
  }

  depends_on = [azapi_resource_action.attach_parameter_group]
}

# Sets random_password.db_admin, on creation and whenever it's replaced
# (rotation). The vault's copy is written after this succeeds (main.tf).
resource "azapi_resource_action" "set_password" {
  type        = azapi_resource.horizondb.type
  resource_id = azapi_resource.horizondb.id
  method      = "PATCH"

  body = {
    properties = {
      administratorLoginPassword = random_password.db_admin.result
    }
  }

  locks = [azapi_resource.horizondb.id]
  retry = local.horizondb_retry

  timeouts {
    create = "60m"
    update = "60m"
  }

  depends_on = [azapi_resource_action.configure_cluster]
}

# --- Private path ---------------------------------------------------------------

# Inside the VNet, the cluster's host name resolves through this zone to the
# private endpoint; the name, and so TLS verification, stay the same. Names
# missing from the zone fall back to public DNS (NxDomainRedirect).
resource "azurerm_private_dns_zone" "horizondb" {
  name                = "privatelink.horizondb.azure.com"
  resource_group_name = local.rg_name
  tags                = var.tags
}

resource "azurerm_private_dns_zone_virtual_network_link" "horizondb" {
  name                 = local.vnet_name
  private_dns_zone_id  = azurerm_private_dns_zone.horizondb.id
  virtual_network_id   = azurerm_virtual_network.this.id
  registration_enabled = false
  resolution_policy    = "NxDomainRedirect"
  tags                 = var.tags
}

# The cluster's DefaultPool, whose read-write and read-only endpoints each get
# an address in the subnet and a record in the zone. Created after the
# cluster's actions, since HorizonDB refuses concurrent operations.
resource "azurerm_private_endpoint" "horizondb" {
  name                          = local.horizondb_private_endpoint_name
  location                      = var.location
  resource_group_name           = local.rg_name
  subnet_id                     = azurerm_subnet.private_endpoints.id
  custom_network_interface_name = "${local.horizondb_private_endpoint_name}-nic"
  tags                          = var.tags

  private_service_connection {
    name                           = local.horizondb_private_endpoint_name
    private_connection_resource_id = azapi_resource.horizondb.id
    subresource_names              = ["DefaultPool"]
    is_manual_connection           = false
  }

  private_dns_zone_group {
    name                 = "horizondb"
    private_dns_zone_ids = [azurerm_private_dns_zone.horizondb.id]
  }

  depends_on = [azapi_resource_action.set_password]
}

# An endpoint carries nothing until the cluster approves its connection. An
# applier who may approve connections to the cluster (its resource group's
# owners and contributors) is approved automatically; otherwise this stops
# the apply, before the kubernetes output that the deployment reads.
data "azapi_resource" "horizondb_private_endpoint_connection" {
  type                   = azapi_resource.horizondb.type
  resource_id            = azapi_resource.horizondb.id
  response_export_values = { connections = "properties.privateEndpointConnections" }

  depends_on = [azurerm_private_endpoint.horizondb]

  lifecycle {
    postcondition {
      condition = try(anytrue([
        for connection in self.output.connections :
        lower(try(connection.properties.privateEndpoint.id, "")) == lower(azurerm_private_endpoint.horizondb.id)
        && try(connection.properties.privateLinkServiceConnectionState.status, "") == "Approved"
      ]), false)
      error_message = "HorizonDB hasn't approved the connection of the private endpoint ${local.horizondb_private_endpoint_name}, so the app couldn't reach the database. Approve it on the cluster (az rest, privateEndpointConnections) and apply again."
    }
  }
}

# --- Emergency public access ------------------------------------------------------

# One rule per extra_firewall_ips address; none by default. HorizonDB derives
# its public network access from the rules.
locals {
  horizondb_firewall_rules = {
    for ip in distinct(var.extra_firewall_ips) :
    "allow-${replace(ip, ".", "-")}" => ip
  }
}

resource "azapi_resource" "horizondb_firewall_rule" {
  for_each = local.horizondb_firewall_rules

  type = "${local.horizondb_types}/clusters/pools/firewallRules@${local.horizondb_api}"
  name = each.key
  # Pools can't be listed, but every cluster has DefaultPool.
  parent_id = "${azapi_resource.horizondb.id}/pools/DefaultPool"

  body = {
    properties = {
      description    = "Operator address (extra_firewall_ips)"
      startIpAddress = each.value
      endIpAddress   = each.value
    }
  }

  schema_validation_enabled = false

  locks = [azapi_resource.horizondb.id]
  retry = local.horizondb_retry

  # HorizonDB would refuse the rules while the endpoint is being created.
  depends_on = [azurerm_private_endpoint.horizondb]

  lifecycle {
    # HorizonDB returns its own rule category in place of the description.
    ignore_changes = [body.properties.description]
  }
}
