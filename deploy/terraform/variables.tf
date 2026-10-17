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
  description = "The region for every resource, such as westus3. It must offer HorizonDB, Container Apps and GlobalStandard deployments of the Foundry models."
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
  description = "The prefix of the derived names: <name_prefix>-logs, -env, -id, -vnet, -hdb (HorizonDB), -pg17-<hash> (its parameter groups), -foundry (unless foundry_account_name is set) and -afd (Front Door)."
  type        = string
  default     = "pghacker"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,18}[a-z0-9]$", var.name_prefix)) && !strcontains(var.name_prefix, "--")
    error_message = "name_prefix must be 3 to 20 lowercase letters, digits and single hyphens, starting with a letter and ending with a letter or digit."
  }
}

variable "app_name" {
  description = "The container app's name, which also begins its host name, <app_name>.<environment's default domain>. The jobs are <app_name>-probe and <app_name>-restore."
  type        = string
  default     = "pghacker"

  validation {
    # Job names, at most 8 characters longer, are limited to 32.
    condition     = can(regex("^[a-z][a-z0-9-]{0,22}[a-z0-9]$", var.app_name)) && !strcontains(var.app_name, "--")
    error_message = "app_name must be 2 to 24 lowercase letters, digits and single hyphens, starting with a letter and ending with a letter or digit."
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
  description = "The registry's SKU."
  type        = string
  default     = "Basic"

  validation {
    condition     = contains(["Basic", "Standard", "Premium"], var.acr_sku)
    error_message = "acr_sku must be Basic, Standard or Premium."
  }
}

variable "image" {
  description = "The app's image, built from the repository's Dockerfile by ../acr-build.yaml, which the app and the probe job run: <acr_name>.azurecr.io/pghacker-mcp@sha256:<digest>, or a tag used for one build only. Container Apps makes a new revision only when this reference changes."
  type        = string

  validation {
    condition     = can(regex("^[^\\s/]+(/[^\\s/:@]+)+(:\\w[\\w.-]{0,127}|@sha256:[0-9a-f]{64})$", var.image))
    error_message = "image must be a registry path with a tag or a digest, such as <acr_name>.azurecr.io/pghacker-mcp@sha256:<digest>."
  }

  validation {
    condition     = startswith(lower(var.image), "${lower(var.acr_name)}.azurecr.io/")
    error_message = "image must be in acr_name's registry (<acr_name>.azurecr.io/...), the only one the managed identity can pull from."
  }
}

variable "seed_image" {
  description = "The corpus seed image, the app's image plus the corpus dump, which the restore job runs; in acr_name's registry too. Null creates no restore job."
  type        = string
  default     = null

  validation {
    condition     = var.seed_image == null || can(regex("^[^\\s/]+(/[^\\s/:@]+)+(:\\w[\\w.-]{0,127}|@sha256:[0-9a-f]{64})$", var.seed_image))
    error_message = "seed_image must be a registry path with a tag or a digest, such as <acr_name>.azurecr.io/pghacker-seed:<corpus ID>, or null."
  }

  validation {
    condition     = var.seed_image == null || try(startswith(lower(var.seed_image), "${lower(var.acr_name)}.azurecr.io/"), false)
    error_message = "seed_image must be in acr_name's registry (<acr_name>.azurecr.io/...), the only one the managed identity can pull from."
  }
}

# --- Key Vault and logs -------------------------------------------------------

variable "key_vault_name" {
  description = "The Key Vault's name, which is globally unique. It holds the generated passwords, the Foundry key and the GitHub OAuth App's credentials."
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
  description = "How many days Log Analytics keeps the app's and the jobs' logs."
  type        = number
  default     = 30

  validation {
    condition     = var.log_retention_days >= 30 && var.log_retention_days <= 730 && floor(var.log_retention_days) == var.log_retention_days
    error_message = "log_retention_days must be a whole number from 30 to 730."
  }
}

# --- Who applies --------------------------------------------------------------

variable "deployer_object_ids" {
  description = "Object IDs of the users and pipeline identities that apply. Each gets Key Vault Secrets Officer on the vault: terraform plan reads the secrets it generated, and the operator stores the GitHub OAuth App's. Empty means whoever is applying; list everyone once a second identity applies, since an apply removes the role from anyone left out."
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
  description = "The VNet's IPv4 address space, holding the Container Apps subnet and the private endpoints' subnet. It mustn't overlap networks it may be peered with."
  type        = string
  default     = "10.66.0.0/22"

  validation {
    condition     = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.vnet_address_space)) && try(cidrsubnet(var.vnet_address_space, 0, 0) == var.vnet_address_space, false)
    error_message = "vnet_address_space must be an IPv4 network in CIDR notation, such as 10.66.0.0/22."
  }

  validation {
    # Two networks overlap when they agree on the shorter of their prefixes.
    condition = try(alltrue([
      for reserved in ["100.100.0.0/16", "169.254.0.0/16", "172.30.0.0/15", "192.0.2.0/24"] :
      cidrsubnet("${cidrhost(var.vnet_address_space, 0)}/${min(tonumber(split("/", var.vnet_address_space)[1]), tonumber(split("/", reserved)[1]))}", 0, 0)
      != cidrsubnet("${cidrhost(reserved, 0)}/${min(tonumber(split("/", var.vnet_address_space)[1]), tonumber(split("/", reserved)[1]))}", 0, 0)
    ]), true)
    error_message = "vnet_address_space must not overlap the ranges Container Apps reserves: 100.100.0.0/16, 169.254.0.0/16, 172.30.0.0/15 and 192.0.2.0/24."
  }
}

variable "container_apps_subnet_prefix" {
  description = "The Container Apps environment's subnet, inside vnet_address_space: /27 or larger. It can't change once the environment exists."
  type        = string
  default     = "10.66.0.0/24"

  validation {
    condition     = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.container_apps_subnet_prefix)) && try(cidrsubnet(var.container_apps_subnet_prefix, 0, 0) == var.container_apps_subnet_prefix && tonumber(split("/", var.container_apps_subnet_prefix)[1]) <= 27, false)
    error_message = "container_apps_subnet_prefix must be an IPv4 network in CIDR notation, /27 or larger, such as 10.66.0.0/24."
  }

  validation {
    condition = try(
      tonumber(split("/", var.container_apps_subnet_prefix)[1]) >= tonumber(split("/", var.vnet_address_space)[1])
      && cidrsubnet("${cidrhost(var.container_apps_subnet_prefix, 0)}/${split("/", var.vnet_address_space)[1]}", 0, 0) == cidrsubnet(var.vnet_address_space, 0, 0),
      true
    )
    error_message = "container_apps_subnet_prefix must lie inside vnet_address_space."
  }
}

variable "private_endpoint_subnet_prefix" {
  description = "The subnet of HorizonDB's and Foundry's private endpoints, inside vnet_address_space and apart from container_apps_subnet_prefix: /28 or larger."
  type        = string
  default     = "10.66.1.0/27"

  validation {
    condition     = can(regex("^([0-9]{1,3}\\.){3}[0-9]{1,3}/[0-9]{1,2}$", var.private_endpoint_subnet_prefix)) && try(cidrsubnet(var.private_endpoint_subnet_prefix, 0, 0) == var.private_endpoint_subnet_prefix && tonumber(split("/", var.private_endpoint_subnet_prefix)[1]) <= 28, false)
    error_message = "private_endpoint_subnet_prefix must be an IPv4 network in CIDR notation, /28 or larger, such as 10.66.1.0/27."
  }

  validation {
    condition = try(
      tonumber(split("/", var.private_endpoint_subnet_prefix)[1]) >= tonumber(split("/", var.vnet_address_space)[1])
      && cidrsubnet("${cidrhost(var.private_endpoint_subnet_prefix, 0)}/${split("/", var.vnet_address_space)[1]}", 0, 0) == cidrsubnet(var.vnet_address_space, 0, 0),
      true
    )
    error_message = "private_endpoint_subnet_prefix must lie inside vnet_address_space."
  }

  validation {
    condition = try(
      cidrsubnet("${cidrhost(var.private_endpoint_subnet_prefix, 0)}/${min(tonumber(split("/", var.private_endpoint_subnet_prefix)[1]), tonumber(split("/", var.container_apps_subnet_prefix)[1]))}", 0, 0)
      != cidrsubnet("${cidrhost(var.container_apps_subnet_prefix, 0)}/${min(tonumber(split("/", var.private_endpoint_subnet_prefix)[1]), tonumber(split("/", var.container_apps_subnet_prefix)[1]))}", 0, 0),
      true
    )
    error_message = "private_endpoint_subnet_prefix must not overlap container_apps_subnet_prefix."
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
  description = "The Foundry model deployments by deployment name: each an OpenAI-format model, its version, and its GlobalStandard capacity in thousands of tokens per minute, which the subscription's regional quota bounds. Terraform creates them one at a time. A new capacity applies in place; a new model or version replaces the deployment."
  type = map(object({
    model    = string
    version  = string
    capacity = number
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

# --- Container Apps -------------------------------------------------------------

variable "create_app" {
  description = "Create the container app. False, the default, deploys everything else: the probe job can check the database, and the operator can store the GitHub OAuth App's credentials, whose callback is on Front Door's host name. Once the app exists, false destroys it."
  type        = bool
  default     = false
  nullable    = false
}

variable "cpu" {
  description = "vCPUs for the app's container."
  type        = number
  default     = 4

  validation {
    condition     = var.cpu >= 0.25 && var.cpu <= 4 && floor(var.cpu * 4) == var.cpu * 4
    error_message = "cpu must be a multiple of 0.25 from 0.25 to 4."
  }
}

variable "memory" {
  description = "Memory for the app's container: twice cpu, in Gi, as the Consumption profile requires."
  type        = string
  default     = "8Gi"

  validation {
    condition     = endswith(var.memory, "Gi") && try(tonumber(trimsuffix(var.memory, "Gi")) == var.cpu * 2, false)
    error_message = "memory must be twice cpu, in Gi: 8Gi for 4 vCPUs, 4Gi for 2 and so on."
  }
}

variable "job_cpu" {
  description = "vCPUs for the probe and restore jobs' containers."
  type        = number
  default     = 2

  validation {
    condition     = var.job_cpu >= 0.25 && var.job_cpu <= 4 && floor(var.job_cpu * 4) == var.job_cpu * 4
    error_message = "job_cpu must be a multiple of 0.25 from 0.25 to 4."
  }
}

variable "job_memory" {
  description = "Memory for the jobs' containers: twice job_cpu, in Gi."
  type        = string
  default     = "4Gi"

  validation {
    condition     = endswith(var.job_memory, "Gi") && try(tonumber(trimsuffix(var.job_memory, "Gi")) == var.job_cpu * 2, false)
    error_message = "job_memory must be twice job_cpu, in Gi: 4Gi for 2 vCPUs, 8Gi for 4 and so on."
  }
}

variable "restrict_origin_to_front_door" {
  description = "Admit only Front Door's backend addresses to the app's own host name: one ingress allow rule per IPv4 range of the AzureFrontDoor.Backend service tag, read while planning (Container Apps takes neither service tags nor IPv6). Apply again now and then to follow changes to the ranges. False lets anyone reach the app directly, bypassing Front Door."
  type        = bool
  default     = true
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
  description = "The most extraction sessions the crawler runs at once, sized to the app's memory (PGHACKER_CRAWL_MAX_SESSIONS). The app accepts up to 64."
  type        = number
  default     = 16

  validation {
    condition     = var.crawl_max_sessions >= 1 && var.crawl_max_sessions <= 64 && floor(var.crawl_max_sessions) == var.crawl_max_sessions
    error_message = "crawl_max_sessions must be a whole number from 1 to 64."
  }
}

variable "github_org" {
  description = "The GitHub organization whose members may sign in to the portal (PGHACKER_GITHUB_ORG)."
  type        = string
  default     = "example-org"

  validation {
    condition     = can(regex("^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$", var.github_org)) && !strcontains(var.github_org, "--")
    error_message = "github_org must be a GitHub organization's login: 1 to 39 letters, digits and single hyphens, not starting or ending with a hyphen."
  }
}

variable "github_team" {
  description = "The slug of the github_org team whose members may sign in to the portal (PGHACKER_GITHUB_TEAM)."
  type        = string
  default     = "example-team"

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9_-]{0,99}$", var.github_team))
    error_message = "github_team must be a team's slug: lowercase letters, digits, hyphens and underscores, such as example-team."
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
  description = "More environment variables for the app (not the jobs), such as RUST_LOG, merged after the ones this configuration sets. It can't replace those, nor set oauth2-proxy's own (OAUTH2_PROXY_*)."
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
    error_message = "app_env can't set the variables this configuration derives (the database, Foundry, Front Door, GitHub sign-in, rate-limit, crawl, home page footer, usage telemetry and feedback settings, and the secrets) or oauth2-proxy's OAUTH2_PROXY_* settings; use the matching variables instead."
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
  description = "Record usage telemetry (PRIVACY.md): an Application Insights resource on the logs' workspace, the retention of its AppEvents table, the key of the users' keyed hashes in Key Vault (usage-id-key) and APPLICATIONINSIGHTS_CONNECTION_STRING for the app. False creates none of these, and the app records nothing."
  type        = bool
  default     = true
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
  description = "Whether usage telemetry records what users type into tools: their arguments' values, error messages, and sqlparser's errors for the statements query lets through by their tokens (PGHACKER_USAGE_DETAILS; PRIVACY.md). Off by default: a call then keeps which tool, its arguments' names and size, the values of its fixed-choice arguments (enums and booleans), its outcome and, for query, PostgreSQL's SQLSTATE. Feedback comments don't depend on it: feedback_comments_enabled alone decides."
  type        = bool
  default     = false
  nullable    = false
}

variable "feedback_rate_per_hour" {
  description = "The chance, from 0 to 1, that an address's hour of use of the public MCP endpoint brings one request for feedback, \"Was the last answer helpful?\", which an interactive client's own form shows as the user's next question starts (PGHACKER_FEEDBACK_RATE; PRIVACY.md). Only the apps in feedback_clients that declare form elicitation, under MCP 2026-07-28, are asked, after feedback_min_questions questions and only in an active session. It needs usage_enabled, whose telemetry records the answers; 0 turns feedback off."
  type        = number
  default     = 0.1
  nullable    = false

  validation {
    condition     = var.feedback_rate_per_hour >= 0 && var.feedback_rate_per_hour <= 1
    error_message = "feedback_rate_per_hour must be from 0 to 1, such as 0.1; 0 turns feedback off."
  }
}

variable "feedback_comments_enabled" {
  description = "Whether the feedback form also takes an optional comment of up to 500 characters, recorded with the rating (PGHACKER_FEEDBACK_COMMENTS). Off by default: a comment is free text, which can hold personal data; update PRIVACY.md before turning it on."
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
