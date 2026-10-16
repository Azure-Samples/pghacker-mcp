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
location            = "westus3"                              # Check HorizonDB and model availability.
resource_group_name = "pghacker-example"
name_prefix         = "pghacker-example"
app_name            = "pghacker-example"

# Replace both names with globally unique values you own.
acr_name       = "examplepghacr12345"
key_vault_name = "pghacker-example-kv"

# Bootstrap placeholder: build the image in your registry, then pin its digest.
# The image's registry prefix must match acr_name.
image = "examplepghacr12345.azurecr.io/pghacker-mcp:bootstrap"

# Optional: an image containing a corpus dump. Null creates no restore job.
seed_image = null

# Keep false during the first apply. Once GitHub OAuth credentials are stored
# in Key Vault, set true and apply. Do not set false again: that destroys the app.
create_app = false

# Empty grants the applying identity access to Key Vault. If multiple identities
# apply, list all their object IDs; an apply revokes access for anyone omitted.
deployer_object_ids = []

# Choose nonoverlapping ranges for your own network.
vnet_address_space             = "10.80.0.0/22"
container_apps_subnet_prefix   = "10.80.0.0/24"
private_endpoint_subnet_prefix = "10.80.1.0/27"

horizondb_vcores        = 4
horizondb_replica_count = 1
horizondb_admin_login   = "pghacker_admin"
database_name           = "pghacker"
extra_firewall_ips      = []

# Model names/versions must be available in your region. These example
# capacities are in thousands of tokens per minute; size them to your quota.
foundry_deployments = {
  "gpt-6.1-sol"            = { model = "gpt-6.1-sol", version = "2026-09-29", capacity = 10 }
  "gpt-6-luna"             = { model = "gpt-6-luna", version = "2026-09-22", capacity = 10 }
  "text-embedding-3-small" = { model = "text-embedding-3-small", version = "1", capacity = 10 }
}
foundry_sol_deployment       = "gpt-6.1-sol"
foundry_luna_deployment      = "gpt-6-luna"
foundry_embedding_deployment = "text-embedding-3-small"

cpu        = 4
memory     = "8Gi"
job_cpu    = 2
job_memory = "4Gi"

front_door_endpoint_name = "pghacker-example"
# Optional: your own hostname, such as "mcp.example.com" or "example.com".
# Null keeps the default azurefd.net URL. See deploy/README.md, "Custom domains".
front_door_custom_domain      = null
restrict_origin_to_front_door = true
mcp_rate_per_minute           = 60
mcp_rate_burst                = 30
crawl_start_sessions          = 2
crawl_max_sessions            = 4

# Usage telemetry (PRIVACY.md); false records nothing.
usage_enabled = true
# Feedback (PRIVACY.md): the chance that an address's hour of use brings one
# "Was the last answer helpful?" to an interactive client, as its user's next
# question starts; 0 asks nothing. feedback_question_gap_seconds (180),
# feedback_session_gap_seconds (600), feedback_min_questions (5) and
# feedback_clients set when, and whom.
feedback_rate_per_hour = 0.1

# Replace these with your GitHub organization and team slug.
# Explicit github_users can admit additional logins or work around an OAuth App
# that your organization has not approved. Keep the list private and intentional.
github_org   = "example-org"
github_team  = "example-team"
github_users = []
portal_role  = "user"

tags = {
  app         = "pghacker-mcp"
  environment = "example"
}
