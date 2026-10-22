# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Workload identities: one user-assigned identity per component, federated
# with the component's Kubernetes service account in the cluster's OIDC
# issuer, and allowed to read its own Key Vault secrets alone, one role
# assignment per secret (local.workloads). The Secrets Store CSI driver
# reads them with the identity's token; only the token proxy's pod gets a
# token itself, for Foundry and Application Insights (foundry.tf, usage.tf).
# The PilotSwarm worker and portal (swarm) read pgh_swarm's password alone;
# only the ops jobs read the administrator's.

resource "azurerm_user_assigned_identity" "workload" {
  for_each = local.workloads

  name                = "${var.name_prefix}-${each.key}-id"
  resource_group_name = local.rg_name
  location            = var.location
  tags                = var.tags
}

resource "azurerm_federated_identity_credential" "workload" {
  for_each = local.workloads

  name                      = "${local.k8s_namespace}-${each.value.service_account}"
  user_assigned_identity_id = azurerm_user_assigned_identity.workload[each.key].id
  issuer                    = azurerm_kubernetes_cluster.this.oidc_issuer_url
  subject                   = "system:serviceaccount:${local.k8s_namespace}:${each.value.service_account}"
  audience                  = ["api://AzureADTokenExchange"]
}

resource "azurerm_role_assignment" "workload_secret" {
  for_each = local.workload_secret_grants

  scope                            = local.vault_secret_ids[each.value.secret]
  role_definition_name             = "Key Vault Secrets User"
  principal_id                     = azurerm_user_assigned_identity.workload[each.value.identity].principal_id
  principal_type                   = "ServicePrincipal"
  skip_service_principal_aad_check = true
}
