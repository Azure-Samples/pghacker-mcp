# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Azure Firewall: every node subnet's way out (network.tf's route table). Its
# policy tells the pools apart by their subnets:
#   - every pool: what AKS and its add-ons need (the AzureKubernetesService
#     FQDN tag, this registry, this vault, Azure Monitor), and the hosts in
#     extra_node_fqdns, none by default;
#   - the services pool: the named hosts that the indexer, oauth2-proxy, the
#     home page and the token proxy reach;
#   - the PilotSwarm pool: nothing more. Its agents' pods are kept off even
#     the platform's endpoints by their NetworkPolicy (deploy/k8s).
# Anything else is denied, and logged to Log Analytics, where an alert watches
# for denials from the PilotSwarm pool.

locals {
  firewall_name = "${var.name_prefix}-fw"

  node_subnet_prefixes = [for pool in local.node_pools : var.subnet_prefixes[pool]]

  # Container Insights' endpoints (the AzureMonitor tag covers its addresses).
  monitor_fqdns = [
    "dc.services.visualstudio.com",
    "*.ods.opinsights.azure.com",
    "*.oms.opinsights.azure.com",
    "*.monitoring.azure.com",
    "global.handler.control.monitor.azure.com",
    "${var.location}.handler.control.monitor.azure.com",
    "*.ingest.monitor.azure.com",
  ]

  # The services pool's hosts: the archive and the code's history (the
  # indexer, the probe and history jobs), GitHub (oauth2-proxy's sign-in and
  # membership checks, the home page's stars), Entra ID (the token proxy's
  # workload identity exchanges its Kubernetes token there; AKS's tag admits
  # it for every pool, but the token proxy depends on it, so it's named
  # here too) and, with usage telemetry, Application Insights' ingestion (the
  # token proxy).
  services_fqdns = merge(
    {
      postgresql = ["www.postgresql.org", "git.postgresql.org"]
      github     = ["github.com", "api.github.com"]
      entra      = ["login.microsoftonline.com"]
    },
    var.usage_enabled ? { usage = ["*.in.applicationinsights.azure.com"] } : {},
  )
}

resource "azurerm_public_ip" "firewall" {
  name                = "${local.firewall_name}-ip"
  location            = var.location
  resource_group_name = local.rg_name
  allocation_method   = "Static"
  sku                 = "Standard"
  tags                = var.tags
}

resource "azurerm_firewall_policy" "this" {
  name                     = "${local.firewall_name}-policy"
  location                 = var.location
  resource_group_name      = local.rg_name
  sku                      = "Standard"
  threat_intelligence_mode = "Deny"
  tags                     = var.tags
}

resource "azurerm_firewall_policy_rule_collection_group" "egress" {
  name               = "egress"
  firewall_policy_id = azurerm_firewall_policy.this.id
  priority           = 200

  network_rule_collection {
    name     = "platform-network"
    priority = 100
    action   = "Allow"

    rule {
      name                  = "azure-monitor"
      protocols             = ["TCP"]
      source_addresses      = local.node_subnet_prefixes
      destination_addresses = ["AzureMonitor"]
      destination_ports     = ["443"]
    }
  }

  application_rule_collection {
    name     = "platform"
    priority = 200
    action   = "Allow"

    rule {
      name                  = "aks"
      source_addresses      = local.node_subnet_prefixes
      destination_fqdn_tags = ["AzureKubernetesService"]
      protocols {
        type = "Https"
        port = 443
      }
      protocols {
        type = "Http"
        port = 80
      }
    }

    # The kubelet pulls the app's image; Premium's dedicated data endpoint
    # keeps it to this registry's storage.
    rule {
      name              = "registry"
      source_addresses  = local.node_subnet_prefixes
      destination_fqdns = [azurerm_container_registry.this.login_server, "${azurerm_container_registry.this.name}.${var.location}.data.azurecr.io"]
      protocols {
        type = "Https"
        port = 443
      }
    }

    # The Secrets Store CSI driver reads the pods' secrets.
    rule {
      name              = "key-vault"
      source_addresses  = local.node_subnet_prefixes
      destination_fqdns = ["${azurerm_key_vault.this.name}.vault.azure.net"]
      protocols {
        type = "Https"
        port = 443
      }
    }

    rule {
      name              = "container-insights"
      source_addresses  = local.node_subnet_prefixes
      destination_fqdns = local.monitor_fqdns
      protocols {
        type = "Https"
        port = 443
      }
    }

    # Agents that the subscription installs on every node, such as an Azure
    # Policy's, may need hosts of their own.
    dynamic "rule" {
      for_each = length(var.extra_node_fqdns) > 0 ? [var.extra_node_fqdns] : []
      content {
        name              = "extra-node-fqdns"
        source_addresses  = local.node_subnet_prefixes
        destination_fqdns = rule.value
        protocols {
          type = "Https"
          port = 443
        }
      }
    }
  }

  application_rule_collection {
    name     = "services"
    priority = 300
    action   = "Allow"

    dynamic "rule" {
      for_each = local.services_fqdns
      content {
        name              = rule.key
        source_addresses  = [var.subnet_prefixes.services]
        destination_fqdns = rule.value
        protocols {
          type = "Https"
          port = 443
        }
      }
    }
  }
}

resource "azurerm_firewall" "this" {
  name                = local.firewall_name
  location            = var.location
  resource_group_name = local.rg_name
  sku_name            = "AZFW_VNet"
  sku_tier            = "Standard"
  firewall_policy_id  = azurerm_firewall_policy.this.id
  tags                = var.tags

  ip_configuration {
    name                 = "egress"
    subnet_id            = azurerm_subnet.firewall.id
    public_ip_address_id = azurerm_public_ip.firewall.id
  }

  # The rules come first: nodes can't start without their egress.
  depends_on = [azurerm_firewall_policy_rule_collection_group.egress]
}

# --- Logs and the denied-egress alert ---------------------------------------------

# Each rule's verdicts in resource-specific tables (AZFWApplicationRule,
# AZFWNetworkRule, AZFWThreatIntel, ...).
resource "azurerm_monitor_diagnostic_setting" "firewall" {
  name                           = "firewall-logs"
  target_resource_id             = azurerm_firewall.this.id
  log_analytics_workspace_id     = azurerm_log_analytics_workspace.this.id
  log_analytics_destination_type = "Dedicated"

  enabled_log {
    category_group = "allLogs"
  }
}

resource "azurerm_monitor_action_group" "egress" {
  count = length(var.alert_emails) > 0 ? 1 : 0

  name                = "${var.name_prefix}-egress"
  resource_group_name = local.rg_name
  short_name          = "pghegress"
  tags                = var.tags

  dynamic "email_receiver" {
    for_each = var.alert_emails
    content {
      name                    = "email-${email_receiver.key}"
      email_address           = email_receiver.value
      use_common_alert_schema = true
    }
  }
}

# The denied-egress alert's query, and its clause for alert_ignored_hosts.
# Only application rules' rows name a host, and no pattern matches an empty
# one (variables.tf), so the clause leaves out no other denial.
locals {
  denied_egress_query = <<-KQL
    union isfuzzy=true AZFWApplicationRule, AZFWNetworkRule, AZFWThreatIntel
    | where Action == "Deny"
    | where ipv4_is_in_range(SourceIp, "${var.subnet_prefixes.swarm}")
  KQL

  denied_egress_ignored = length(var.alert_ignored_hosts) == 0 ? "" : <<-KQL
    | extend Host = iff(Type == "AZFWApplicationRule", tolower(column_ifexists("Fqdn", "")), "")
    | where not(${join(" or ", [for pattern in var.alert_ignored_hosts : "Host matches regex @\"${pattern}\""])})
  KQL
}

# The PilotSwarm pool's nodes may reach only the platform's endpoints, and its
# pods not even those: a denial from its subnet means something there tried
# to get out. The alert counts every denial, by application rule, network
# rule or threat intelligence, except an application rule's denial of a host
# that alert_ignored_hosts matches, which the operators have looked into and
# accept; the firewall still denies and logs it.
resource "azurerm_monitor_scheduled_query_rules_alert_v2" "denied_egress" {
  name                  = "${var.name_prefix}-swarm-denied-egress"
  resource_group_name   = local.rg_name
  location              = var.location
  description           = "Azure Firewall denied a connection from the PilotSwarm pool's subnet (${var.subnet_prefixes.swarm})."
  severity              = 2
  scopes                = [azurerm_log_analytics_workspace.this.id]
  evaluation_frequency  = "PT5M"
  window_duration       = "PT5M"
  skip_query_validation = true
  tags                  = var.tags

  criteria {
    query                   = "${local.denied_egress_query}${local.denied_egress_ignored}"
    time_aggregation_method = "Count"
    operator                = "GreaterThan"
    threshold               = 0
  }

  dynamic "action" {
    for_each = length(var.alert_emails) > 0 ? [1] : []
    content {
      action_groups = [azurerm_monitor_action_group.egress[0].id]
    }
  }

  depends_on = [azurerm_monitor_diagnostic_setting.firewall]
}
