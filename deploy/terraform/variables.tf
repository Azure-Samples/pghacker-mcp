# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# The settings of one pghacker-mcp deployment. Copy ../environments/example.tfvars
# to a local ignored file; ../TERRAFORM.md tabulates the variables.

# --- Subscription, region and names ---------------------------------------

variable "subscription_id" {
  description = "The Azure subscription to deploy to."
  type        = string

  validation {
    condition     = can(regex("^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$", var.subscription_id))
    error_message = "subscription_id must be a subscription ID, a GUID."
  }
}

variable "tenant_id" {
  description = "The subscription's Entra tenant, which the providers get their Azure CLI tokens from. Null uses the CLI's default tenant."
  type        = string
  default     = null

  validation {
    condition     = var.tenant_id == null || can(regex("^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$", var.tenant_id))
    error_message = "tenant_id must be a tenant ID, a GUID, or null."
  }
}

variable "location" {
  description = "The region for every resource, such as westus3. It must offer HorizonDB, AKS, Azure Firewall, Front Door Premium's Private Link origins and GlobalStandard deployments of the Foundry models."
  type        = string

  validation {
    condition     = can(regex("^[a-z][a-z0-9]+$", var.location))
    error_message = "location must be a region's name, such as westus3, not its display name."
  }
}

variable "resource_group_name" {
  description = "The resource group that this configuration creates and deploys everything into. Terraform's state lives in another one (../scripts/bootstrap-state.sh)."
  type        = string

  validation {
    condition     = can(regex("^[-\\w.()]{0,89}[-\\w()]$", var.resource_group_name))
    error_message = "resource_group_name must be 1 to 90 letters, digits, underscores, hyphens, periods and parentheses, not ending with a period."
  }
}

variable "name_prefix" {
  description = "The prefix of the derived names: <name_prefix>-logs, -aks (with its identities, -aks-id and -aks-kubelet-id), -fw, -vnet, -egress (the route table), -hdb (HorizonDB), -pg17-<hash> (its parameter groups), -foundry (unless foundry_account_name is set), -afd (Front Door), -edge-pls and -usage, and the workload identities, <name_prefix>-<component>-id. AKS's node resource group is <resource_group_name>-nodes."
  type        = string
  default     = "pghacker"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,18}[a-z0-9]$", var.name_prefix)) && !strcontains(var.name_prefix, "--")
    error_message = "name_prefix must be 3 to 20 lowercase letters, digits and single hyphens, starting with a letter and ending with a letter or digit."
  }
}

variable "tags" {
  description = "Tags for every resource that takes them."
  type        = map(string)
  default     = {}
  nullable    = false
}

# --- Registry and images ----------------------------------------------------

variable "acr_name" {
  description = "The container registry's name, which is globally unique. Its login server is <acr_name>.azurecr.io."
  type        = string

  validation {
    condition     = can(regex("^[a-zA-Z0-9]{5,50}$", var.acr_name))
    error_message = "acr_name must be 5 to 50 letters and digits."
  }
}

variable "acr_sku" {
  description = "The registry's SKU. Premium gives it a dedicated data endpoint, <acr_name>.<location>.data.azurecr.io, so the firewall lets the nodes pull from this registry alone rather than from all of Azure Storage."
  type        = string
  default     = "Premium"

  validation {
    condition     = var.acr_sku == "Premium"
    error_message = "acr_sku must be Premium: the firewall admits the registry's dedicated data endpoint only, which Premium has."
  }
}

variable "image" {
  description = "The app's image, built from the repository's Dockerfile by ../acr-build.yaml, which every component and job runs (../scripts/aks.mjs apply): <acr_name>.azurecr.io/pghacker-mcp@sha256:<digest>, or a tag used for one build only."
  type        = string

  validation {
    condition     = can(regex("^[^\\s/]+(/[^\\s/:@]+)+(:\\w[\\w.-]{0,127}|@sha256:[0-9a-f]{64})$", var.image))
    error_message = "image must be a registry path with a tag or a digest, such as <acr_name>.azurecr.io/pghacker-mcp@sha256:<digest>."
  }

  validation {
    condition     = startswith(lower(var.image), "${lower(var.acr_name)}.azurecr.io/")
    error_message = "image must be in acr_name's registry (<acr_name>.azurecr.io/...), the only one the nodes can pull from."
  }
}

variable "seed_image" {
  description = "The corpus seed image, the app's image plus the corpus dump, which the restore job runs (../scripts/aks.mjs job restore); in acr_name's registry too. Null leaves the restore job out."
  type        = string
  default     = null

  validation {
    condition     = var.seed_image == null || can(regex("^[^\\s/]+(/[^\\s/:@]+)+(:\\w[\\w.-]{0,127}|@sha256:[0-9a-f]{64})$", var.seed_image))
    error_message = "seed_image must be a registry path with a tag or a digest, such as <acr_name>.azurecr.io/pghacker-seed:<corpus ID>, or null."
  }

  validation {
    condition     = var.seed_image == null || try(startswith(lower(var.seed_image), "${lower(var.acr_name)}.azurecr.io/"), false)
    error_message = "seed_image must be in acr_name's registry (<acr_name>.azurecr.io/...), the only one the nodes can pull from."
  }
}

# --- Key Vault and logs -------------------------------------------------------

variable "key_vault_name" {
  description = "The Key Vault's name, which is globally unique. It holds the generated passwords and keys and the GitHub OAuth App's credentials."
  type        = string

  validation {
    condition     = can(regex("^[a-zA-Z][a-zA-Z0-9-]{1,22}[a-zA-Z0-9]$", var.key_vault_name)) && !strcontains(var.key_vault_name, "--")
    error_message = "key_vault_name must be 3 to 24 letters, digits and single hyphens, starting with a letter and ending with a letter or digit."
  }
}

variable "key_vault_purge_protection_enabled" {
  description = "Turn on the vault's purge protection. It can't be turned off again, and it keeps a deleted vault's name taken for its 90-day soft-delete retention."
  type        = bool
  default     = false
  nullable    = false
}

variable "log_retention_days" {
  description = "How many days Log Analytics keeps the containers', the jobs' and the firewall's logs."
  type        = number
  default     = 30

  validation {
    condition     = var.log_retention_days >= 30 && var.log_retention_days <= 730 && floor(var.log_retention_days) == var.log_retention_days
    error_message = "log_retention_days must be a whole number from 30 to 730."
  }
}

# --- Who applies --------------------------------------------------------------

variable "deployer_object_ids" {
  description = "Object IDs of the users and pipeline identities that apply and deploy. Each gets Key Vault Secrets Officer on the vault (terraform plan reads the secrets it generated, and the operator stores the GitHub OAuth App's) and Azure Kubernetes Service RBAC Cluster Admin on the cluster (../scripts/aks.mjs applies the manifests). Empty means whoever is applying; list everyone once a second identity applies, since an apply removes the roles from anyone left out."
  type        = list(string)
  default     = []
  nullable    = false

  validation {
    condition     = alltrue([for id in var.deployer_object_ids : can(regex("^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$", id))])
    error_message = "deployer_object_ids must hold object IDs, which are GUIDs."
  }
}

# --- Network ------------------------------------------------------------------

variable "vnet_address_space" {
  description = "The VNet's IPv4 address space, holding the firewall's, the node pools', the edge's and the private endpoints' subnets (subnet_prefixes). It mustn't overlap pod_cidr, service_cidr, or networks it may be peered with."
  type        = string
  default     = "10.66.0.0/22"

  validation {
    condition     = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.vnet_address_space)) && try(cidrsubnet(var.vnet_address_space, 0, 0) == var.vnet_address_space, false)
    error_message = "vnet_address_space must be an IPv4 network in CIDR notation, such as 10.66.0.0/22."
  }
}

variable "subnet_prefixes" {
  description = "The VNet's subnets, inside vnet_address_space and apart from each other: firewall (AzureFirewallSubnet, /26 or larger), the node pools' system, services and swarm (/28 or larger; with Azure CNI Overlay they hold the nodes alone), edge (the internal load balancer and its Private Link Service, /28 or larger) and private_endpoints (HorizonDB's and Foundry's, /28 or larger). The firewall's rules and the private endpoints' NSG tell the pools apart by these."
  type = object({
    firewall          = string
    system            = string
    services          = string
    swarm             = string
    edge              = string
    private_endpoints = string
  })
  default = {
    firewall          = "10.66.0.0/26"
    system            = "10.66.0.64/27"
    services          = "10.66.0.96/27"
    swarm             = "10.66.0.128/27"
    edge              = "10.66.0.160/28"
    private_endpoints = "10.66.1.0/27"
  }
  nullable = false

  validation {
    condition = alltrue([
      for prefix in values(var.subnet_prefixes) :
      can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", prefix)) && try(cidrsubnet(prefix, 0, 0) == prefix && tonumber(split("/", prefix)[1]) <= 28, false)
    ])
    error_message = "subnet_prefixes must be IPv4 networks in CIDR notation, /28 or larger, such as 10.66.0.64/27."
  }

  validation {
    condition     = try(tonumber(split("/", var.subnet_prefixes.firewall)[1]) <= 26, false)
    error_message = "subnet_prefixes.firewall must be /26 or larger, as Azure Firewall requires."
  }

  validation {
    condition = try(alltrue([
      for prefix in values(var.subnet_prefixes) :
      tonumber(split("/", prefix)[1]) >= tonumber(split("/", var.vnet_address_space)[1])
      && cidrsubnet("${cidrhost(prefix, 0)}/${split("/", var.vnet_address_space)[1]}", 0, 0) == cidrsubnet(var.vnet_address_space, 0, 0)
    ]), true)
    error_message = "subnet_prefixes must lie inside vnet_address_space."
  }

  validation {
    # Two networks overlap when they agree on the shorter of their prefixes.
    # Each pair once, by position (>= compares numbers only).
    condition = try(alltrue(flatten([
      for i, x in values(var.subnet_prefixes) : [
        for j, y in values(var.subnet_prefixes) : i >= j || (
          cidrsubnet("${cidrhost(x, 0)}/${min(tonumber(split("/", x)[1]), tonumber(split("/", y)[1]))}", 0, 0)
          != cidrsubnet("${cidrhost(y, 0)}/${min(tonumber(split("/", x)[1]), tonumber(split("/", y)[1]))}", 0, 0)
        )
      ]
    ])), true)
    error_message = "subnet_prefixes must not overlap each other."
  }
}

variable "pod_cidr" {
  description = "The pods' overlay addresses (Azure CNI Overlay), outside vnet_address_space: they never appear on the VNet, where a pod's traffic leaves with its node's address."
  type        = string
  default     = "10.244.0.0/16"

  validation {
    condition = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.pod_cidr)) && try(cidrsubnet(var.pod_cidr, 0, 0) == var.pod_cidr, false) && try(
      cidrsubnet("${cidrhost(var.pod_cidr, 0)}/${min(tonumber(split("/", var.pod_cidr)[1]), tonumber(split("/", var.vnet_address_space)[1]))}", 0, 0)
      != cidrsubnet("${cidrhost(var.vnet_address_space, 0)}/${min(tonumber(split("/", var.pod_cidr)[1]), tonumber(split("/", var.vnet_address_space)[1]))}", 0, 0),
      true
    )
    error_message = "pod_cidr must be an IPv4 network in CIDR notation, such as 10.244.0.0/16, apart from vnet_address_space."
  }
}

variable "service_cidr" {
  description = "The Kubernetes Services' addresses, outside vnet_address_space and pod_cidr. The cluster's DNS service takes its tenth address."
  type        = string
  default     = "10.0.0.0/16"

  validation {
    condition = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.service_cidr)) && try(cidrsubnet(var.service_cidr, 0, 0) == var.service_cidr, false) && try(alltrue([
      for other in [var.vnet_address_space, var.pod_cidr] :
      cidrsubnet("${cidrhost(var.service_cidr, 0)}/${min(tonumber(split("/", var.service_cidr)[1]), tonumber(split("/", other)[1]))}", 0, 0)
      != cidrsubnet("${cidrhost(other, 0)}/${min(tonumber(split("/", var.service_cidr)[1]), tonumber(split("/", other)[1]))}", 0, 0)
    ]), true)
    error_message = "service_cidr must be an IPv4 network in CIDR notation, such as 10.0.0.0/16, apart from vnet_address_space and pod_cidr."
  }
}

# --- HorizonDB ----------------------------------------------------------------

variable "horizondb_cluster_name" {
  description = "The HorizonDB cluster's name. Null uses <name_prefix>-hdb."
  type        = string
  default     = null

  validation {
    condition     = var.horizondb_cluster_name == null || can(regex("^[a-z][a-z0-9-]{1,61}[a-z0-9]$", var.horizondb_cluster_name))
    error_message = "horizondb_cluster_name must be 3 to 63 lowercase letters, digits and hyphens, starting with a letter and ending with a letter or digit."
  }
}

variable "horizondb_vcores" {
  description = "vCores per HorizonDB replica."
  type        = number
  default     = 4

  validation {
    condition     = var.horizondb_vcores >= 2 && floor(var.horizondb_vcores) == var.horizondb_vcores
    error_message = "horizondb_vcores must be a whole number, at least 2."
  }
}

variable "horizondb_replica_count" {
  description = "HorizonDB replicas when the cluster is created. Changing it later fails the plan: this configuration doesn't manage the cluster's replicas child resources."
  type        = number
  default     = 1

  validation {
    condition     = var.horizondb_replica_count >= 1 && floor(var.horizondb_replica_count) == var.horizondb_replica_count
    error_message = "horizondb_replica_count must be a whole number, at least 1."
  }
}

variable "horizondb_admin_login" {
  description = "The cluster's administrator login (PGHACKER_DB_ADMIN_USER), with which the app's bootstrap creates the database, the extensions and its own roles. Its password is generated into Key Vault as db-admin-password."
  type        = string
  default     = "pghacker_admin"

  validation {
    condition = (
      can(regex("^[a-z][a-z0-9_]{2,62}$", var.horizondb_admin_login))
      && !startswith(var.horizondb_admin_login, "pg_")
      && !contains(["admin", "administrator", "azure_pg_admin", "azure_superuser", "guest", "postgres", "public", "root"], var.horizondb_admin_login)
    )
    error_message = "horizondb_admin_login must be 3 to 63 lowercase letters, digits and underscores, starting with a letter; not a reserved name such as postgres or admin, and not starting with pg_."
  }
}

variable "database_name" {
  description = "The app's database (PGHACKER_DB_NAME). The app's bootstrap creates it; Terraform doesn't."
  type        = string
  default     = "pghacker"

  validation {
    condition     = can(regex("^[a-z_][a-z0-9_]{0,62}$", var.database_name))
    error_message = "database_name must be 1 to 63 lowercase letters, digits and underscores, not starting with a digit."
  }
}

variable "horizondb_max_connections" {
  description = "max_connections for the cluster, added to its parameter group. Null keeps HorizonDB's default."
  type        = number
  default     = null

  validation {
    condition     = var.horizondb_max_connections == null || try(var.horizondb_max_connections >= 25 && var.horizondb_max_connections <= 5000 && floor(var.horizondb_max_connections) == var.horizondb_max_connections, false)
    error_message = "horizondb_max_connections must be a whole number from 25 to 5000, or null."
  }
}

variable "extra_firewall_ips" {
  description = "Operator IPv4 addresses admitted to HorizonDB's public endpoint, for emergencies only: one firewall rule each. Empty, the default, admits nobody; rules don't expire, so remove the addresses and apply again when done. The app and the jobs always use the private endpoint."
  type        = list(string)
  default     = []
  nullable    = false

  validation {
    condition = alltrue([
      for ip in var.extra_firewall_ips :
      can(regex("^[0-9]{1,3}(\\.[0-9]{1,3}){3}$", ip)) && try(cidrhost("${ip}/32", 0) == ip, false)
    ])
    error_message = "extra_firewall_ips must hold IPv4 addresses such as 203.0.113.7."
  }
}

# --- Azure AI Foundry ---------------------------------------------------------

variable "foundry_account_name" {
  description = "The Foundry (AIServices) account's name, which is also its custom subdomain, <name>.openai.azure.com, and so globally unique. Null uses <name_prefix>-foundry."
  type        = string
  default     = null

  validation {
    condition     = var.foundry_account_name == null || can(regex("^[a-z0-9][a-z0-9-]{0,62}[a-z0-9]$", var.foundry_account_name))
    error_message = "foundry_account_name must be 2 to 64 lowercase letters, digits and hyphens, starting and ending with a letter or digit."
  }
}

variable "foundry_deployments" {
  description = "The Foundry model deployments by deployment name: each an OpenAI-format model, its version, its capacity in thousands of tokens per minute, and its deployment type (sku): GlobalStandard, the default, or DataZoneStandard, which processes requests within the account's data zone, such as the US, and has a quota of its own. The subscription's quota for the model and type bounds the capacity; GlobalStandard's is shared by every account in the subscription, in every region. Terraform creates them one at a time. A new capacity applies in place; a new model, version or type replaces the deployment."
  type = map(object({
    model    = string
    version  = string
    capacity = number
    sku      = optional(string, "GlobalStandard")
  }))
  default = {
    "gpt-6-luna"             = { model = "gpt-6-luna", version = "2026-09-22", capacity = 1000 }
    "text-embedding-3-small" = { model = "text-embedding-3-small", version = "1", capacity = 800 }
    "gpt-6.1-sol"            = { model = "gpt-6.1-sol", version = "2026-09-29", capacity = 300 }
  }
  nullable = false

  validation {
    condition     = alltrue([for name in keys(var.foundry_deployments) : can(regex("^[a-zA-Z0-9][a-zA-Z0-9._-]{1,63}$", name))])
    error_message = "foundry_deployments' names must be 2 to 64 letters, digits, periods, underscores and hyphens, starting with a letter or digit."
  }

  validation {
    condition     = alltrue([for d in values(var.foundry_deployments) : trimspace(d.model) != "" && trimspace(d.version) != ""])
    error_message = "Each of foundry_deployments must name a model and its version."
  }

  validation {
    condition     = alltrue([for d in values(var.foundry_deployments) : d.capacity >= 1 && floor(d.capacity) == d.capacity])
    error_message = "Each of foundry_deployments' capacities must be a whole number of thousands of tokens per minute, at least 1."
  }

  validation {
    condition     = alltrue([for d in values(var.foundry_deployments) : contains(["GlobalStandard", "DataZoneStandard"], d.sku)])
    error_message = "Each of foundry_deployments' sku must be GlobalStandard or DataZoneStandard."
  }
}

variable "foundry_sol_deployment" {
  description = "The deployment, a key of foundry_deployments, that the app uses as Sol, the portal's default model (PGHACKER_FOUNDRY_SOL)."
  type        = string
  default     = "gpt-6.1-sol"

  validation {
    condition     = contains(keys(var.foundry_deployments), var.foundry_sol_deployment)
    error_message = "foundry_sol_deployment must be one of foundry_deployments' names."
  }
}

variable "foundry_luna_deployment" {
  description = "The deployment, a key of foundry_deployments, that the app uses as Luna, the crawler's and extractor's model (PGHACKER_FOUNDRY_LUNA)."
  type        = string
  default     = "gpt-6-luna"

  validation {
    condition     = contains(keys(var.foundry_deployments), var.foundry_luna_deployment)
    error_message = "foundry_luna_deployment must be one of foundry_deployments' names."
  }
}

variable "foundry_embedding_deployment" {
  description = "The deployment, a key of foundry_deployments, that embeds text for semantic search (PGHACKER_FOUNDRY_EMBEDDING). Its model must match the corpus's embeddings."
  type        = string
  default     = "text-embedding-3-small"

  validation {
    condition     = contains(keys(var.foundry_deployments), var.foundry_embedding_deployment)
    error_message = "foundry_embedding_deployment must be one of foundry_deployments' names."
  }
}

# --- AKS --------------------------------------------------------------------------

variable "kubernetes_version" {
  description = "The cluster's Kubernetes version, such as 1.34. Null takes AKS's default; the cluster then follows patch releases (automatic_upgrade_channel = patch)."
  type        = string
  default     = null
}

variable "aks_sku_tier" {
  description = "The cluster's tier: Standard, with the API server's uptime SLA, or Free."
  type        = string
  default     = "Standard"

  validation {
    condition     = contains(["Free", "Standard", "Premium"], var.aks_sku_tier)
    error_message = "aks_sku_tier must be Free, Standard or Premium."
  }
}

variable "api_server_authorized_ip_ranges" {
  description = "IPv4 ranges allowed to reach the cluster's API server, such as the operators' addresses. Empty allows any address; Entra ID authorizes every request either way."
  type        = list(string)
  default     = []
  nullable    = false

  validation {
    condition     = alltrue([for range in var.api_server_authorized_ip_ranges : can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", range)) && try(cidrsubnet(range, 0, 0) == range, false)])
    error_message = "api_server_authorized_ip_ranges must hold IPv4 networks in CIDR notation, such as 203.0.113.0/24."
  }
}

variable "system_pool" {
  description = "The system node pool, which runs only the cluster's own add-ons (CriticalAddonsOnly): its VM size and autoscaling bounds."
  type = object({
    vm_size   = optional(string, "Standard_D2ds_v5")
    min_count = optional(number, 1)
    max_count = optional(number, 3)
  })
  default  = {}
  nullable = false

  validation {
    condition     = var.system_pool.min_count >= 1 && var.system_pool.max_count >= var.system_pool.min_count && floor(var.system_pool.min_count) == var.system_pool.min_count && floor(var.system_pool.max_count) == var.system_pool.max_count
    error_message = "system_pool's min_count must be a whole number of at least 1, and max_count at least min_count."
  }
}

variable "services_pool" {
  description = "The services node pool, which runs oauth2-proxy, the home page, pghacker-mcp, pghacker-indexer, the token proxy and the ops jobs: its VM size and node count."
  type = object({
    vm_size    = optional(string, "Standard_D4ds_v5")
    node_count = optional(number, 2)
  })
  default  = {}
  nullable = false

  validation {
    condition     = var.services_pool.node_count >= 1 && floor(var.services_pool.node_count) == var.services_pool.node_count
    error_message = "services_pool.node_count must be a whole number of at least 1."
  }
}

variable "swarm_pool" {
  description = "The PilotSwarm node pool, tainted for PilotSwarm alone: the portal and the worker, which runs every agent session and the crawl scheduler. Its subnet gets no internet through the firewall. Its VM size and node count; the worker holds up to crawl_max_sessions Copilot CLI sessions."
  type = object({
    vm_size    = optional(string, "Standard_D4ds_v5")
    node_count = optional(number, 1)
  })
  default  = {}
  nullable = false

  validation {
    condition     = var.swarm_pool.node_count >= 1 && floor(var.swarm_pool.node_count) == var.swarm_pool.node_count
    error_message = "swarm_pool.node_count must be a whole number of at least 1."
  }
}

variable "alert_emails" {
  description = "Addresses that the denied-egress alert emails: it fires when the firewall denies a connection from the PilotSwarm pool. Empty creates the alert without an action group; it shows in Azure Monitor."
  type        = list(string)
  default     = []
  nullable    = false

  validation {
    condition     = alltrue([for email in var.alert_emails : can(regex("^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$", email))])
    error_message = "alert_emails must hold email addresses."
  }
}

variable "create_origin" {
  description = "Connect Front Door to the cluster: its origin, over Private Link to the edge's Private Link Service, and its routes. False, the default, for the applies before ../scripts/aks.mjs apply has created that service; then true, and approve the connection (../scripts/aks.mjs approve-edge)."
  type        = bool
  default     = false
  nullable    = false
}

# --- The app's settings ---------------------------------------------------------

variable "mcp_rate_per_minute" {
  description = "Requests per minute that each client address may make to the public MCP endpoint (PGHACKER_RATE_PER_MINUTE)."
  type        = number
  default     = 60

  validation {
    condition     = var.mcp_rate_per_minute >= 1 && var.mcp_rate_per_minute <= 1000000 && floor(var.mcp_rate_per_minute) == var.mcp_rate_per_minute
    error_message = "mcp_rate_per_minute must be a whole number from 1 to 1000000."
  }
}

variable "mcp_rate_burst" {
  description = "Requests a client address may make at once to the public MCP endpoint before mcp_rate_per_minute applies (PGHACKER_RATE_BURST)."
  type        = number
  default     = 30

  validation {
    condition     = var.mcp_rate_burst >= 1 && var.mcp_rate_burst <= 1000000 && floor(var.mcp_rate_burst) == var.mcp_rate_burst
    error_message = "mcp_rate_burst must be a whole number from 1 to 1000000."
  }
}

variable "mcp_calls_per_client" {
  description = "Calls a client address may run at once on the public MCP endpoint (PGHACKER_MCP_CALLS_PER_CLIENT); it answers more at once 429. The endpoint runs at most twice mcp_pool_size and mcp_query_pool_size together in all, whatever this allows."
  type        = number
  default     = 10

  validation {
    condition     = var.mcp_calls_per_client >= 1 && var.mcp_calls_per_client <= 1000 && floor(var.mcp_calls_per_client) == var.mcp_calls_per_client
    error_message = "mcp_calls_per_client must be a whole number from 1 to 1000."
  }
}

variable "mcp_internal_rate_per_minute" {
  description = "Requests per minute that the deployment's agents may make to pghacker-mcp's internal listener, all of them together: they connect from one address (PGHACKER_INTERNAL_RATE_PER_MINUTE)."
  type        = number
  default     = 300

  validation {
    condition     = var.mcp_internal_rate_per_minute >= 1 && var.mcp_internal_rate_per_minute <= 1000000 && floor(var.mcp_internal_rate_per_minute) == var.mcp_internal_rate_per_minute
    error_message = "mcp_internal_rate_per_minute must be a whole number from 1 to 1000000."
  }
}

variable "mcp_internal_rate_burst" {
  description = "Requests the agents may make at once to pghacker-mcp's internal listener before mcp_internal_rate_per_minute applies (PGHACKER_INTERNAL_RATE_BURST)."
  type        = number
  default     = 60

  validation {
    condition     = var.mcp_internal_rate_burst >= 1 && var.mcp_internal_rate_burst <= 1000000 && floor(var.mcp_internal_rate_burst) == var.mcp_internal_rate_burst
    error_message = "mcp_internal_rate_burst must be a whole number from 1 to 1000000."
  }
}

variable "mcp_pool_size" {
  description = "pgh_reader connections for every pghacker-mcp tool but query (PGHACKER_MCP_POOL). Size both pools to the database's capacity: about twice its vCores in all."
  type        = number
  default     = 16

  validation {
    condition     = var.mcp_pool_size >= 1 && floor(var.mcp_pool_size) == var.mcp_pool_size && var.mcp_pool_size + var.mcp_query_pool_size <= 128
    error_message = "mcp_pool_size must be a whole number of at least 1, and with mcp_query_pool_size at most 128: pgh_reader's connection limit is 160."
  }
}

variable "mcp_query_pool_size" {
  description = "pgh_reader connections for pghacker-mcp's query tool, which runs users' SQL for up to 10 seconds each (PGHACKER_QUERY_POOL)."
  type        = number
  default     = 8

  validation {
    condition     = var.mcp_query_pool_size >= 1 && floor(var.mcp_query_pool_size) == var.mcp_query_pool_size
    error_message = "mcp_query_pool_size must be a whole number of at least 1."
  }
}

variable "crawl_start_sessions" {
  description = "Extraction sessions the crawler starts with; it adds more while Foundry doesn't throttle it (PGHACKER_CRAWL_START_SESSIONS)."
  type        = number
  default     = 8

  validation {
    condition     = var.crawl_start_sessions >= 1 && floor(var.crawl_start_sessions) == var.crawl_start_sessions && var.crawl_start_sessions <= var.crawl_max_sessions
    error_message = "crawl_start_sessions must be a whole number from 1 to crawl_max_sessions."
  }
}

variable "crawl_max_sessions" {
  description = "The most extraction sessions the crawler runs at once, sized to the worker's memory on the PilotSwarm pool (PGHACKER_CRAWL_MAX_SESSIONS). The app accepts up to 64."
  type        = number
  default     = 16

  validation {
    condition     = var.crawl_max_sessions >= 1 && var.crawl_max_sessions <= 64 && floor(var.crawl_max_sessions) == var.crawl_max_sessions
    error_message = "crawl_max_sessions must be a whole number from 1 to 64."
  }
}

variable "github_org" {
  description = "The GitHub organization whose members may sign in to the portal (PGHACKER_GITHUB_ORG). Required: its members get agents that run shell commands on your subscription."
  type        = string
  nullable    = false

  validation {
    condition     = can(regex("^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$", var.github_org)) && !strcontains(var.github_org, "--")
    error_message = "github_org must be a GitHub organization's login: 1 to 39 letters, digits and single hyphens, not starting or ending with a hyphen."
  }

  validation {
    condition     = lower(var.github_org) != "example-org"
    error_message = "github_org must be your own organization, not the placeholder example-org, which is a real GitHub organization."
  }
}

variable "github_team" {
  description = "The slug of the github_org team whose members may sign in to the portal (PGHACKER_GITHUB_TEAM). Required, with no default."
  type        = string
  nullable    = false

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9_-]{0,99}$", var.github_team))
    error_message = "github_team must be a team's slug: lowercase letters, digits, hyphens and underscores, such as pghacker-operators."
  }

  validation {
    condition     = lower(var.github_team) != "example-team"
    error_message = "github_team must be your own team, not the placeholder example-team."
  }
}

variable "github_users" {
  description = "More GitHub logins that may sign in to the portal besides the team's members, joined with commas into PGHACKER_GITHUB_USERS. Empty leaves the variable out."
  type        = list(string)
  default     = []
  nullable    = false

  validation {
    condition     = alltrue([for login in var.github_users : can(regex("^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$", login))])
    error_message = "github_users must hold GitHub logins: 1 to 39 letters, digits and hyphens, not starting or ending with a hyphen."
  }
}

variable "portal_role" {
  description = "The PilotSwarm portal role of everyone who signs in (PGHACKER_PORTAL_ROLE): user or admin."
  type        = string
  default     = "user"

  validation {
    condition     = contains(["user", "admin"], var.portal_role)
    error_message = "portal_role must be user or admin."
  }
}

variable "app_env" {
  description = "More app settings, such as RUST_LOG or PGHACKER_CRAWL = \"off\", merged after the ones this configuration sets (the kubernetes output's app_settings). Each component gets only those it uses (scripts/lib/azure.mjs). It can't replace the derived ones, nor set oauth2-proxy's own (OAUTH2_PROXY_*)."
  type        = map(string)
  default     = {}
  nullable    = false

  validation {
    condition     = alltrue([for name in keys(var.app_env) : can(regex("^[A-Z_][A-Z0-9_]*$", name))])
    error_message = "app_env's names must be upper-case letters, digits and underscores, such as RUST_LOG."
  }

  validation {
    condition = !anytrue([for name in keys(var.app_env) : startswith(name, "OAUTH2_PROXY_")]) && length(setintersection(keys(var.app_env), [
      "APPLICATIONINSIGHTS_CONNECTION_STRING",
      "AZURE_CLIENT_ID",
      "PGH_INDEXER_PASSWORD",
      "PGH_READER_PASSWORD",
      "PGH_SWARM_PASSWORD",
      "PGHACKER_COPYRIGHT_TEXT",
      "PGHACKER_COPYRIGHT_URL",
      "PGHACKER_CRAWL_MAX_SESSIONS",
      "PGHACKER_CRAWL_START_SESSIONS",
      "PGHACKER_DB_ADMIN_PASSWORD",
      "PGHACKER_DB_ADMIN_USER",
      "PGHACKER_DB_HOST",
      "PGHACKER_DB_NAME",
      "PGHACKER_DB_PORT",
      "PGHACKER_DB_SSLMODE",
      "PGHACKER_DEPLOYMENT",
      "PGHACKER_FEEDBACK_CLIENTS",
      "PGHACKER_FEEDBACK_COMMENTS",
      "PGHACKER_FEEDBACK_DEBUG",
      "PGHACKER_FEEDBACK_MIN_QUESTIONS",
      "PGHACKER_FEEDBACK_QUESTION_GAP",
      "PGHACKER_FEEDBACK_RATE",
      "PGHACKER_FEEDBACK_SESSION_GAP",
      "PGHACKER_FOUNDRY_EMBEDDING",
      "PGHACKER_FOUNDRY_ENDPOINT",
      "PGHACKER_FOUNDRY_LUNA",
      "PGHACKER_FOUNDRY_SOL",
      "PGHACKER_FRONT_DOOR_ID",
      "PGHACKER_GITHUB_ORG",
      "PGHACKER_GITHUB_TEAM",
      "PGHACKER_GITHUB_USERS",
      "PGHACKER_INTERNAL_RATE_BURST",
      "PGHACKER_INTERNAL_RATE_PER_MINUTE",
      "PGHACKER_MCP_CALLS_PER_CLIENT",
      "PGHACKER_PORTAL_ROLE",
      "PGHACKER_PORTAL_URL",
      "PGHACKER_PRIVACY_URL",
      "PGHACKER_PUBLIC_HOSTS",
      "PGHACKER_PUBLIC_URL",
      "PGHACKER_RATE_BURST",
      "PGHACKER_RATE_PER_MINUTE",
      "PGHACKER_USAGE_DETAILS",
      "PGHACKER_USAGE_ID_KEY",
    ])) == 0
    error_message = "app_env can't set the variables this configuration derives (the database, Foundry, Front Door, GitHub sign-in, rate and call limit, crawl, home page footer, usage telemetry and feedback settings, and the secrets) or oauth2-proxy's OAUTH2_PROXY_* settings; use the matching variables instead."
  }
}

# --- Front Door -------------------------------------------------------------------

variable "front_door_endpoint_name" {
  description = "The Front Door endpoint's name, which begins its host name, <name>-<hash>.<zone>.azurefd.net. It remains reachable with a custom domain too. Changing it replaces the endpoint and its host name."
  type        = string
  default     = "pghacker"
  nullable    = false

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{0,44}[a-z0-9]$", var.front_door_endpoint_name))
    error_message = "front_door_endpoint_name must be 2 to 46 lowercase letters, digits and hyphens, starting and ending with a letter or digit."
  }
}

variable "front_door_custom_domain" {
  description = "Optional custom hostname for Front Door's public paths (the home page and /mcp) and the canonical public and MCP URLs, such as mcp.example.com or example.com. The portal and its OAuth callback stay on the azurefd.net hostname. Null keeps the azurefd.net hostname for everything. Set deployment-specific domains only in ignored environment settings; publish the ownership TXT record and DNS alias with your DNS provider."
  type        = string
  default     = null

  validation {
    condition = var.front_door_custom_domain == null ? true : (
      length(var.front_door_custom_domain) <= 64
      && can(regex("^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\\.)+[a-z]([a-z0-9-]{0,61}[a-z0-9])?$", var.front_door_custom_domain))
    )
    error_message = "front_door_custom_domain must be null or a lowercase DNS hostname of at most 64 characters, with valid labels and no scheme, path, port, wildcard or trailing dot."
  }
}

# --- Home page --------------------------------------------------------------------

variable "privacy_url" {
  description = "The privacy statement that the home page's footer links to, as \"Privacy\" (PGHACKER_PRIVACY_URL): an https URL of your own, such as https://example.com/privacy. Null, the default, gives the home page no privacy link."
  type        = string
  default     = null

  validation {
    condition = var.privacy_url == null ? true : (
      length(var.privacy_url) <= 2048
      && can(regex("^https://[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:[0-9]{1,5})?([/?#][^\\s\"'<>\\\\]*)?$", var.privacy_url))
    )
    error_message = "privacy_url must be null or an https URL, such as https://example.com/privacy, of at most 2048 characters: a host name, an optional port and path, without credentials, spaces, quotes, angle brackets or backslashes."
  }
}

variable "copyright_url" {
  description = "The page that the home page's footer links to as its copyright notice, to the right of the privacy link, with copyright_text as the link's text (PGHACKER_COPYRIGHT_URL): an https URL of your own, such as https://example.com/. Set it with copyright_text, or leave both null, the default, for no copyright link."
  type        = string
  default     = null

  validation {
    condition = var.copyright_url == null ? true : (
      length(var.copyright_url) <= 2048
      && can(regex("^https://[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:[0-9]{1,5})?([/?#][^\\s\"'<>\\\\]*)?$", var.copyright_url))
    )
    error_message = "copyright_url must be null or an https URL, such as https://example.com/, of at most 2048 characters: a host name, an optional port and path, without credentials, spaces, quotes, angle brackets or backslashes."
  }

  validation {
    condition     = (var.copyright_url == null) == (var.copyright_text == null)
    error_message = "copyright_url and copyright_text must be set together, or both left null."
  }
}

variable "copyright_text" {
  description = "What the home page footer's copyright link says (PGHACKER_COPYRIGHT_TEXT), such as \"© 2026 Example\": 1 to 100 characters, without control characters. Set it with copyright_url, or leave both null, the default, for no copyright link. The year isn't kept current: change it when it changes."
  type        = string
  default     = null

  validation {
    condition     = var.copyright_text == null ? true : can(regex("^[^\\p{Cc}]{1,100}$", var.copyright_text)) && trimspace(var.copyright_text) != ""
    error_message = "copyright_text must be null or 1 to 100 characters, not all blank, without control characters, such as \"© 2026 Example\"."
  }
}

# --- Usage telemetry --------------------------------------------------------------

variable "usage_enabled" {
  description = "Record usage telemetry (PRIVACY.md): an Application Insights resource on the logs' workspace, the retention of its AppEvents table, the key of the users' keyed hashes in Key Vault (usage-id-key) and APPLICATIONINSIGHTS_CONNECTION_STRING for the app. False, the default, creates none of these, and the app records nothing. Before turning it on, set privacy_url to your privacy statement, so that the home page's visitors can read what it records."
  type        = bool
  default     = false
  nullable    = false
}

variable "usage_daily_cap_gb" {
  description = "The most usage telemetry Application Insights takes in a day, in GB. Past it, the rest of the day's events are dropped."
  type        = number
  default     = 5
  nullable    = false

  validation {
    condition     = var.usage_daily_cap_gb > 0 && var.usage_daily_cap_gb <= 100
    error_message = "usage_daily_cap_gb must be more than 0 and at most 100."
  }
}

variable "usage_retention_days" {
  description = "How many days usage events stay queryable in the AppEvents table, 30 to 730."
  type        = number
  default     = 730
  nullable    = false

  validation {
    condition     = var.usage_retention_days >= 30 && var.usage_retention_days <= 730 && floor(var.usage_retention_days) == var.usage_retention_days
    error_message = "usage_retention_days must be a whole number from 30 to 730."
  }
}

variable "usage_total_retention_days" {
  description = "How many days usage events are kept in all, from usage_retention_days to 4383 (12 years). Beyond usage_retention_days they're in long-term retention, which search jobs read."
  type        = number
  default     = 1095
  nullable    = false

  validation {
    condition     = var.usage_total_retention_days >= var.usage_retention_days && var.usage_total_retention_days <= 4383 && floor(var.usage_total_retention_days) == var.usage_total_retention_days
    error_message = "usage_total_retention_days must be a whole number from usage_retention_days to 4383."
  }
}

variable "usage_details_enabled" {
  description = "Whether usage telemetry records what users type into tools: their arguments' values, error messages, and sqlparser's errors for the statements query lets through by their tokens (PGHACKER_USAGE_DETAILS). Off by default: a call then keeps which tool, its arguments' names and size, the values of its fixed-choice arguments (enums and booleans), its outcome and, for query, PostgreSQL's SQLSTATE. Feedback comments don't depend on it: feedback_comments_enabled alone decides."
  type        = bool
  default     = false
  nullable    = false
}

variable "feedback_rate_per_hour" {
  description = "The chance, from 0 to 1, that an address's hour of use of the public MCP endpoint brings one request for feedback, \"Was the last answer helpful?\", which an interactive client's own form shows as the user's next question starts (PGHACKER_FEEDBACK_RATE; docs/reference.md#feedback). Only the apps in feedback_clients that declare form elicitation, under MCP 2026-07-28, are asked, after feedback_min_questions questions and only in an active session. It needs usage_enabled, whose telemetry records the answers; 0 turns feedback off."
  type        = number
  default     = 0.1
  nullable    = false

  validation {
    condition     = var.feedback_rate_per_hour >= 0 && var.feedback_rate_per_hour <= 1
    error_message = "feedback_rate_per_hour must be from 0 to 1, such as 0.1; 0 turns feedback off."
  }
}

variable "feedback_comments_enabled" {
  description = "Whether the feedback form also takes an optional comment of up to 500 characters, recorded with the rating (PGHACKER_FEEDBACK_COMMENTS). Off by default: a comment is free text, which can hold personal data; update your privacy statement (privacy_url) before turning it on."
  type        = bool
  default     = false
  nullable    = false
}

variable "feedback_question_gap_seconds" {
  description = "A pause between a client's tool calls longer than this, in seconds, starts a new question, whose first call feedback may hold for its form (PGHACKER_FEEDBACK_QUESTION_GAP)."
  type        = number
  default     = 180
  nullable    = false

  validation {
    condition     = var.feedback_question_gap_seconds >= 1 && var.feedback_question_gap_seconds <= 3600 && floor(var.feedback_question_gap_seconds) == var.feedback_question_gap_seconds
    error_message = "feedback_question_gap_seconds must be a whole number from 1 to 3600."
  }
}

variable "feedback_session_gap_seconds" {
  description = "A pause between a client's tool calls shorter than this, in seconds, keeps its session active. Feedback asks only as a question starts in an active session (PGHACKER_FEEDBACK_SESSION_GAP)."
  type        = number
  default     = 600
  nullable    = false

  validation {
    condition     = var.feedback_session_gap_seconds > var.feedback_question_gap_seconds && var.feedback_session_gap_seconds <= 86400 && floor(var.feedback_session_gap_seconds) == var.feedback_session_gap_seconds
    error_message = "feedback_session_gap_seconds must be a whole number more than feedback_question_gap_seconds and at most 86400."
  }
}

variable "feedback_min_questions" {
  description = "The questions a client asks before feedback can ask it, 0 to 100 (PGHACKER_FEEDBACK_MIN_QUESTIONS): the first ask can come as the next question starts."
  type        = number
  default     = 3
  nullable    = false

  validation {
    condition     = var.feedback_min_questions >= 0 && var.feedback_min_questions <= 100 && floor(var.feedback_min_questions) == var.feedback_min_questions
    error_message = "feedback_min_questions must be a whole number from 0 to 100."
  }
}

variable "feedback_clients" {
  description = "The interactive apps feedback can ask: any whose MCP requests give a name (clientInfo.name) that contains one of these, ignoring case (PGHACKER_FEEDBACK_CLIENTS). The defaults match Copilot CLI and the GitHub Copilot app, Codex CLI, VS Code and its Insiders build, Claude Code and the Claude app, and Cursor. Scripts, agents and other apps are never asked."
  type        = list(string)
  default     = ["copilot", "codex", "Visual Studio Code", "claude", "cursor"]
  nullable    = false

  validation {
    condition     = length(var.feedback_clients) > 0 && alltrue([for name in var.feedback_clients : trimspace(name) != "" && !strcontains(name, ",")])
    error_message = "feedback_clients must name at least one app, with no blank name and no comma in a name; to ask none, set feedback_rate_per_hour = 0."
  }
}

variable "feedback_debug_enabled" {
  description = "Debug mode, for trying the feedback form by hand (PGHACKER_FEEDBACK_DEBUG): every app in feedback_clients that can show the form is asked at every question after its first, without the warm-up, feedback_rate_per_hour's chance, the hour's one ask, the active-session limit or the pause after a decline, and pghacker-mcp logs why each call it doesn't hold isn't asked. It asks every user of the public endpoint that often, so turn it off after testing. Feedback must be on: feedback_rate_per_hour above 0."
  type        = bool
  default     = false
  nullable    = false
}
