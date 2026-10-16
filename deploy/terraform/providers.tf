# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Both providers sign in with the Azure CLI's account. Neither registers
# resource providers: that needs rights on the whole subscription, so the
# operator registers them once (../TERRAFORM.md, "Before the first apply").
provider "azurerm" {
  subscription_id                 = var.subscription_id
  tenant_id                       = var.tenant_id
  resource_provider_registrations = "none"
  features {}
}

provider "azapi" {
  subscription_id            = var.subscription_id
  tenant_id                  = var.tenant_id
  skip_provider_registration = true
}
