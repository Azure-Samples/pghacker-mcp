# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The VNet: Azure Firewall's subnet, a subnet per node pool, the edge's (the
# internal load balancer in front of oauth2-proxy and its Private Link
# Service, which Front Door reaches) and the private endpoints' (HorizonDB's
# and Foundry's, which have no other path). Every node subnet sends its
# outbound traffic to the firewall (firewall.tf), whose rules tell the pools
# apart by these subnets: with Azure CNI Overlay, a pod's traffic leaves the
# cluster with its node's address.

locals {
  # The node pools' subnets: system add-ons; the services; PilotSwarm.
  node_pools = toset(["system", "services", "swarm"])
}

resource "azurerm_virtual_network" "this" {
  name                = local.vnet_name
  location            = var.location
  resource_group_name = local.rg_name
  address_space       = [var.vnet_address_space]
  tags                = var.tags
}

# The name Azure Firewall requires.
resource "azurerm_subnet" "firewall" {
  name                 = "AzureFirewallSubnet"
  resource_group_name  = local.rg_name
  virtual_network_name = azurerm_virtual_network.this.name
  address_prefixes     = [var.subnet_prefixes.firewall]
}

# No default outbound access: a pool reaches the internet through the
# firewall or not at all.
resource "azurerm_subnet" "nodes" {
  for_each = local.node_pools

  name                            = "nodes-${each.key}"
  resource_group_name             = local.rg_name
  virtual_network_name            = azurerm_virtual_network.this.name
  address_prefixes                = [var.subnet_prefixes[each.key]]
  default_outbound_access_enabled = false
}

# The edge's internal load balancer and its Private Link Service, which AKS
# creates for the edge Service (deploy/k8s/edge.yaml). The service's NAT
# addresses come from here, which needs Private Link network policies off.
resource "azurerm_subnet" "edge" {
  name                                          = "edge"
  resource_group_name                           = local.rg_name
  virtual_network_name                          = azurerm_virtual_network.this.name
  address_prefixes                              = [var.subnet_prefixes.edge]
  private_link_service_network_policies_enabled = false
  default_outbound_access_enabled               = false
}

# The private endpoints' subnet, whose NSG admits only the pools that use
# each endpoint.
resource "azurerm_subnet" "private_endpoints" {
  name                              = "private-endpoints"
  resource_group_name               = local.rg_name
  virtual_network_name              = azurerm_virtual_network.this.name
  address_prefixes                  = [var.subnet_prefixes.private_endpoints]
  private_endpoint_network_policies = "NetworkSecurityGroupEnabled"
  default_outbound_access_enabled   = false
}

# --- Egress through the firewall ----------------------------------------------

# Everything outside the VNet goes to the firewall; the VNet's own routes
# keep traffic between subnets direct.
resource "azurerm_route_table" "egress" {
  name                          = "${var.name_prefix}-egress"
  location                      = var.location
  resource_group_name           = local.rg_name
  bgp_route_propagation_enabled = false
  tags                          = var.tags

  route = [{
    name                   = "internet-through-firewall"
    address_prefix         = "0.0.0.0/0"
    next_hop_type          = "VirtualAppliance"
    next_hop_in_ip_address = azurerm_firewall.this.ip_configuration[0].private_ip_address
  }]
}

resource "azurerm_subnet_route_table_association" "nodes" {
  for_each = local.node_pools

  subnet_id      = azurerm_subnet.nodes[each.key].id
  route_table_id = azurerm_route_table.egress.id
}

# --- Private endpoints' NSG -----------------------------------------------------

# The database from the services and PilotSwarm pools; Foundry from the
# services pool alone, where the token proxy runs; nothing else from the VNet.
# Pods' traffic arrives with their node's address, from their pool's subnet.
# The PilotSwarm pool's rule, denying it all but the database, takes the top
# priority, so that no rule added to the NSG later, such as a policy's that
# admits HTTPS from the whole VNet, can open Foundry to it.
resource "azurerm_network_security_group" "private_endpoints" {
  name                = "${var.name_prefix}-private-endpoints"
  location            = var.location
  resource_group_name = local.rg_name
  tags                = var.tags
}

resource "azurerm_network_security_rule" "swarm" {
  name                        = "swarm-database-only"
  network_security_group_name = azurerm_network_security_group.private_endpoints.name
  resource_group_name         = local.rg_name
  priority                    = 100
  direction                   = "Inbound"
  access                      = "Deny"
  protocol                    = "*"
  source_address_prefix       = var.subnet_prefixes.swarm
  source_port_range           = "*"
  destination_address_prefix  = var.subnet_prefixes.private_endpoints
  destination_port_ranges     = ["0-5431", "5433-65535"]

  # The database's rule held priority 100 before this one.
  depends_on = [azurerm_network_security_rule.database]
}

resource "azurerm_network_security_rule" "database" {
  name                        = "database-from-services-and-swarm"
  network_security_group_name = azurerm_network_security_group.private_endpoints.name
  resource_group_name         = local.rg_name
  priority                    = 120
  direction                   = "Inbound"
  access                      = "Allow"
  protocol                    = "Tcp"
  source_address_prefixes     = [var.subnet_prefixes.services, var.subnet_prefixes.swarm]
  source_port_range           = "*"
  destination_address_prefix  = var.subnet_prefixes.private_endpoints
  destination_port_range      = "5432"
}

resource "azurerm_network_security_rule" "foundry" {
  name                        = "foundry-from-services"
  network_security_group_name = azurerm_network_security_group.private_endpoints.name
  resource_group_name         = local.rg_name
  priority                    = 110
  direction                   = "Inbound"
  access                      = "Allow"
  protocol                    = "Tcp"
  source_address_prefix       = var.subnet_prefixes.services
  source_port_range           = "*"
  destination_address_prefix  = var.subnet_prefixes.private_endpoints
  destination_port_range      = "443"
}

resource "azurerm_network_security_rule" "deny_vnet" {
  name                        = "deny-other-vnet-traffic"
  network_security_group_name = azurerm_network_security_group.private_endpoints.name
  resource_group_name         = local.rg_name
  priority                    = 4000
  direction                   = "Inbound"
  access                      = "Deny"
  protocol                    = "*"
  source_address_prefix       = "VirtualNetwork"
  source_port_range           = "*"
  destination_address_prefix  = "*"
  destination_port_range      = "*"
}

resource "azurerm_subnet_network_security_group_association" "private_endpoints" {
  subnet_id                 = azurerm_subnet.private_endpoints.id
  network_security_group_id = azurerm_network_security_group.private_endpoints.id
}
