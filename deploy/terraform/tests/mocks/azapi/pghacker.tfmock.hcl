# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Every azapi resource shares these defaults: the HorizonDB cluster reads
# fqdn, the Front Door endpoint host_name.
mock_resource "azapi_resource" {
  defaults = {
    id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.HorizonDB/clusters/pghacker-example-hdb"
    output = {
      fqdn      = "pghacker-example-hdb.mock-cluster.westus3.horizondb.azure.com"
      state     = "Succeeded"
      host_name = "pghacker-example.mock.azurefd.net"
    }
  }
}

# Reads of the cluster (its replicas and connections), of the Foundry account
# (its connections) and of the edge's Private Link Service (its connections,
# from Front Door). Both private endpoints, whose IDs the tests fix, are
# approved, and so is every connection to the edge.
mock_data "azapi_resource" {
  defaults = {
    exists = true
    output = {
      replicas = 1
      connections = [
        {
          properties = {
            privateEndpoint                   = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-hdb-pe" }
            privateLinkServiceConnectionState = { status = "Approved" }
          }
        },
        {
          properties = {
            privateEndpoint                   = { id = "/subscriptions/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/resourceGroups/pghacker-example/providers/Microsoft.Network/privateEndpoints/pghacker-example-foundry-pe" }
            privateLinkServiceConnectionState = { status = "Approved" }
          }
        },
      ]
    }
  }
}
