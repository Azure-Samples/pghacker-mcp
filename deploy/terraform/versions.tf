# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

terraform {
  # 1.9 for variable validations that refer to other variables.
  required_version = ">= 1.9"

  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 5.6"
    }
    azapi = {
      source  = "azure/azapi"
      version = "~> 2.12"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.9"
    }
    time = {
      source  = "hashicorp/time"
      version = "~> 0.14"
    }
  }

  # Filled in by `terraform init -backend-config=../environments/<env>.backend.hcl`
  # (../TERRAFORM.md, "State").
  backend "azurerm" {}
}
