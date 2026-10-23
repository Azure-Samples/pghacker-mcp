# Synthetic example only: none of these IDs, names, or images identify a deployment.
# From the repository root:
#   cp deploy/environments/example.tfvars deploy/environments/local.tfvars
# Replace the values below with your subscription, tenant, region, unique names,
# and the GitHub organization/team you want to authorize. local.tfvars is ignored.
#
# Create a separate state backend with deploy/scripts/bootstrap-state.sh; see
# deploy/README.md and deploy/TERRAFORM.md for the staged deployment commands.
# Never commit real environment settings, backend files, secrets, or state.

subscription_id     = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" # Replace with your subscription ID.
tenant_id           = null                                   # Uses the Azure CLI's signed-in tenant.
location            = "westus3"                              # Check HorizonDB, AKS and model availability.
resource_group_name = "pghacker-example"
name_prefix         = "pghacker-example"

# Replace both names with globally unique values you own.
acr_name       = "examplepghacr12345"
key_vault_name = "pghacker-example-kv"

# Bootstrap placeholder: build the image in your registry, then pin its digest.
# The image's registry prefix must match acr_name.
image = "examplepghacr12345.azurecr.io/pghacker-mcp:bootstrap"

# Optional: an image containing a corpus dump, for the restore job.
seed_image = null

# Keep false until deploy/scripts/aks.mjs apply has created the edge's Private
# Link Service; then set true, apply, and approve Front Door's connection
# (deploy/scripts/aks.mjs approve-edge). See deploy/README.md.
create_origin = false

# Empty grants the applying identity access to Key Vault and the cluster. If
# multiple identities apply, list all their object IDs; an apply revokes
# access for anyone omitted.
deployer_object_ids = []

# Choose nonoverlapping ranges for your own network. The pods' overlay and the
# Services' ranges (pod_cidr, service_cidr) must stay apart from the VNet's.
vnet_address_space = "10.80.0.0/22"
subnet_prefixes = {
  firewall          = "10.80.0.0/26"
  system            = "10.80.0.64/27"
  services          = "10.80.0.96/27"
  swarm             = "10.80.0.128/27"
  edge              = "10.80.0.160/28"
  private_endpoints = "10.80.1.0/27"
}

# The cluster. The API server is open to any address unless you list yours;
# Entra ID authorizes every request either way.
api_server_authorized_ip_ranges = []
system_pool                     = { vm_size = "Standard_D2ds_v5", min_count = 1, max_count = 3 }
services_pool                   = { vm_size = "Standard_D4ds_v5", node_count = 2 }
swarm_pool                      = { vm_size = "Standard_D4ds_v5", node_count = 1 }
# Who the alert on denied egress from the PilotSwarm pool emails.
alert_emails = []

horizondb_vcores        = 4
horizondb_replica_count = 1
horizondb_admin_login   = "pghacker_admin"
database_name           = "pghacker"
extra_firewall_ips      = []

# Model names/versions must be available in your region. These example
# capacities are in thousands of tokens per minute; size them to your quota.
# Each takes GlobalStandard unless it sets sku = "DataZoneStandard", whose
# quota is separate.
foundry_deployments = {
  "gpt-6.1-sol"            = { model = "gpt-6.1-sol", version = "2026-09-29", capacity = 10 }
  "gpt-6-luna"             = { model = "gpt-6-luna", version = "2026-09-22", capacity = 10 }
  "text-embedding-3-small" = { model = "text-embedding-3-small", version = "1", capacity = 10 }
}
foundry_sol_deployment       = "gpt-6.1-sol"
foundry_luna_deployment      = "gpt-6-luna"
foundry_embedding_deployment = "text-embedding-3-small"

front_door_endpoint_name = "pghacker-example"
# Optional: your own hostname, such as "mcp.example.com" or "example.com".
# Null keeps the default azurefd.net URL. See deploy/README.md, "Custom domains".
front_door_custom_domain = null
mcp_rate_per_minute      = 60
mcp_rate_burst           = 30
# The calls a client address may run at once there; more at once get 429.
mcp_calls_per_client = 10
# The agents' own limit on pghacker-mcp's internal listener, which they share.
mcp_internal_rate_per_minute = 300
mcp_internal_rate_burst      = 60
crawl_start_sessions         = 2
crawl_max_sessions           = 4

# The privacy statement that the home page's footer links to, as "Privacy": an
# https URL of your own. Unset, the page has no privacy link.
# privacy_url = "https://example.com/privacy"
# A copyright link, to its right: the URL and what the link says. Set both, or
# neither: the page then has no copyright link. The year isn't kept current.
# copyright_url  = "https://example.com/"
# copyright_text = "© 2026 Example"

# Usage telemetry (PRIVACY.md); false, the default, records nothing. Before
# turning it on, set privacy_url above to your privacy statement.
usage_enabled = false
# Whether it also records what users type into tools: arguments' values, error
# messages and sqlparser's errors. False, the default, keeps the rest: which
# tools, argument names and size, fixed-choice values, outcomes, SQLSTATEs.
usage_details_enabled = false
# Feedback, with usage telemetry only (docs/reference.md#feedback): the chance
# that an address's hour of use brings one "Was the last answer helpful?" to an
# interactive client, as its user's next question starts; 0 asks nothing.
# feedback_question_gap_seconds (180), feedback_session_gap_seconds (600),
# feedback_min_questions (3) and feedback_clients set when, and whom.
# feedback_debug_enabled (false), for trying the form, asks at every question
# after a client's first.
feedback_rate_per_hour = 0.1

# Required: your GitHub organization and the slug of its team whose members may
# sign in to the portal, whose agents run shell commands on your subscription.
# The placeholders below fail validation until you replace them.
# Explicit github_users can admit additional logins or work around an OAuth App
# that your organization has not approved. Keep the list private and intentional.
github_org   = "<your-github-org>"
github_team  = "<your-github-team>"
github_users = []
portal_role  = "user"

tags = {
  app         = "pghacker-mcp"
  environment = "example"
}
