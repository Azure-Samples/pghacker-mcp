# The VNet of the Container Apps environment and of the private endpoints
# through which it reaches HorizonDB (horizondb.tf) and Foundry (foundry.tf).
# Neither service joins a VNet; private endpoints are their only private path.

resource "azurerm_virtual_network" "this" {
  name                = local.vnet_name
  location            = var.location
  resource_group_name = local.rg_name
  address_space       = [var.vnet_address_space]
  tags                = var.tags
}

# The environment's subnet, delegated to Container Apps. It can't be resized
# once the environment exists.
resource "azurerm_subnet" "container_apps" {
  name                 = "container-apps"
  resource_group_name  = local.rg_name
  virtual_network_name = azurerm_virtual_network.this.name
  address_prefixes     = [var.container_apps_subnet_prefix]

  delegation {
    name = "container-apps"

    service_delegation {
      name    = "Microsoft.App/environments"
      actions = ["Microsoft.Network/virtualNetworks/subnets/join/action"]
    }
  }
}

# The private endpoints' subnet. Network security groups, if any, belong to
# the subscription's policy; this keeps them enforced on the endpoints.
resource "azurerm_subnet" "private_endpoints" {
  name                              = "private-endpoints"
  resource_group_name               = local.rg_name
  virtual_network_name              = azurerm_virtual_network.this.name
  address_prefixes                  = [var.private_endpoint_subnet_prefix]
  private_endpoint_network_policies = "NetworkSecurityGroupEnabled"
}
