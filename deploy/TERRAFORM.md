# pghacker-mcp's Terraform

[`terraform/`](terraform/) deploys pghacker-mcp to Azure: one container app (oauth2-proxy, the PilotSwarm portal and worker, pghacker-indexer and pghacker-mcp under a supervisor) in a VNet-integrated Container Apps environment, HorizonDB (PostgreSQL 17) and Azure AI Foundry reached only through private endpoints, Key Vault for every secret, and Azure Front Door Standard as the public entry: `/mcp` is the anonymous MCP endpoint, everything else is the portal behind GitHub sign-in.

| Path | Contents |
|---|---|
| `terraform/*.tf` | The configuration: `main.tf` (resource group, registry, identity, vault, generated secrets), `network.tf`, `horizondb.tf`, `foundry.tf`, `containerapps.tf`, `frontdoor.tf` |
| `terraform/tests/` | Offline `terraform test` suites; every provider is mocked (`tests/mocks/`) |
| `terraform/.terraform.lock.hcl` | Provider versions and hashes for linux_amd64 and darwin_arm64 |
| `environments/prod.tfvars`, `prod.backend.hcl` | Production's settings and state backend; no secrets |
| `scripts/bootstrap-state.sh` | Creates the state's resource group, storage account and container |
| `acr-build.yaml` | Builds the repository's `Dockerfile` with ACR Tasks |

## Offline checks

In `terraform/`, with a data directory of its own so a deployment's backend isn't touched:

```sh
export TF_DATA_DIR="$(mktemp -d)"
terraform init -backend=false -input=false -lockfile=readonly
terraform fmt -recursive -check
terraform validate
terraform test
```

The tests apply to synthetic state and never reach Azure. They cover the parameter group, the private endpoints and DNS, Foundry's network settings and deployments, the Key Vault references and environment of the app and the jobs, the ingress restriction to Front Door, the Front Door route, the staged first apply, and the variables' validations.

## State

State is a blob in a storage account that only Entra ID can reach, in its own resource group (`pghacker-example-state`), so destroying the deployment can't delete it. `scripts/bootstrap-state.sh` creates what's missing and prints the backend file (`--help` lists its options; `--dry-run` only reads):

```sh
deploy/scripts/bootstrap-state.sh --resource-group pghacker-example-state \
  --storage-account examplepghtf12345 --location westus3 \
  --subscription aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa \
  --key pghacker-mcp/prod.tfstate > deploy/environments/prod.backend.hcl
cd deploy/terraform
terraform init -backend-config=../environments/prod.backend.hcl
```

## Before the first apply

Terraform registers no resource providers. Once per subscription, someone with rights on it registers `Microsoft.App`, `Microsoft.OperationalInsights`, `Microsoft.ContainerRegistry`, `Microsoft.KeyVault`, `Microsoft.ManagedIdentity`, `Microsoft.HorizonDB`, `Microsoft.Network`, `Microsoft.CognitiveServices`, `Microsoft.Cdn` and `Microsoft.Storage` (`az provider register --namespace <namespace> --wait`).

The first apply runs with `create_app = false`: everything but the app, including the probe job and Front Door, whose host name gives the GitHub OAuth App its callback URL (`terraform output -raw oauth_callback_url`). With the GitHub secrets stored (below), set `create_app = true` and apply again; from then on it stays true, since false destroys the app.

## Key Vault secrets

The app and the jobs read every secret through Key Vault references resolved by the user-assigned identity; no secret value is set on Container Apps.

| Secret | Made by | Read as | By |
|---|---|---|---|
| `db-admin-password` | Terraform, 40 characters; also set on the cluster | `PGHACKER_DB_ADMIN_PASSWORD` | App, jobs |
| `pgh-indexer-password`, `pgh-reader-password`, `pgh-swarm-password` | Terraform, 40 characters | `PGH_INDEXER_PASSWORD`, `PGH_READER_PASSWORD`, `PGH_SWARM_PASSWORD` | App, jobs |
| `foundry-key` | Terraform, from the Foundry account's primary key | `AZURE_OAI_KEY` | App, jobs |
| `oauth2-cookie-secret` | Terraform, 32 alphanumerics | `OAUTH2_PROXY_COOKIE_SECRET` | App |
| `github-client-id`, `github-client-secret` | The operator, from the GitHub OAuth App | `OAUTH2_PROXY_CLIENT_ID`, `OAUTH2_PROXY_CLIENT_SECRET` | App |

Register the GitHub OAuth App with `public_url` as its homepage and `oauth_callback_url` as its callback, then store its credentials (applying with `create_app = true` stops until both exist):

```sh
vault=$(terraform output -raw key_vault_name)
az keyvault secret set --vault-name "$vault" --name github-client-id --value "<client ID>" --output none
read -rsp 'Client secret: ' secret && echo
az keyvault secret set --vault-name "$vault" --name github-client-secret --value "$secret" --output none && unset secret
```

## Variables

[`terraform/variables.tf`](terraform/variables.tf) describes and validates each one. Environment variable names in parentheses are what the app receives.

| Variable | Default | Description |
|---|---|---|
| `subscription_id` | required | Subscription to deploy to |
| `tenant_id` | `null` | The subscription's tenant, for the Azure CLI's tokens; null uses the CLI's default |
| `location` | required | Region for everything; needs HorizonDB, Container Apps and the models |
| `resource_group_name` | required | Resource group Terraform creates for the deployment |
| `name_prefix` | `"pghacker"` | Prefix of derived names (`-logs`, `-env`, `-id`, `-vnet`, `-hdb`, `-pg17-<hash>`, `-foundry`, `-afd`) |
| `app_name` | `"pghacker"` | Container app's name; jobs are `<app_name>-probe` and `-restore` |
| `tags` | `{}` | Tags for every resource that takes them |
| `acr_name` | required | Registry name, globally unique |
| `acr_sku` | `"Basic"` | Registry SKU |
| `image` | required | App image in `acr_name`'s registry, by digest or a one-build tag; the probe job runs it too |
| `seed_image` | `null` | Corpus seed image for the restore job; null creates no restore job |
| `key_vault_name` | required | Vault name, globally unique |
| `key_vault_purge_protection_enabled` | `false` | Purge protection; irreversible |
| `log_retention_days` | `30` | Log Analytics retention, 30 to 730 |
| `deployer_object_ids` | `[]` | Everyone who applies, given Key Vault Secrets Officer; empty means whoever is applying |
| `vnet_address_space` | `"10.66.0.0/22"` | VNet range |
| `container_apps_subnet_prefix` | `"10.66.0.0/24"` | Container Apps subnet, /27 or larger; fixed once the environment exists |
| `private_endpoint_subnet_prefix` | `"10.66.1.0/27"` | Private endpoints' subnet, /28 or larger |
| `horizondb_cluster_name` | `null` | Cluster name; null uses `<name_prefix>-hdb` |
| `horizondb_vcores` | `4` | vCores per replica, at least 2 |
| `horizondb_replica_count` | `1` | Replicas at creation; a later change fails the plan |
| `horizondb_admin_login` | `"pghacker_admin"` | Administrator login (`PGHACKER_DB_ADMIN_USER`) |
| `database_name` | `"pghacker"` | App database, created by the app's bootstrap (`PGHACKER_DB_NAME`) |
| `horizondb_max_connections` | `null` | `max_connections` in the parameter group; null keeps HorizonDB's default |
| `extra_firewall_ips` | `[]` | Emergency operator IPv4 addresses for the public endpoint; none by default |
| `foundry_account_name` | `null` | Foundry account and custom subdomain, globally unique; null uses `<name_prefix>-foundry` |
| `foundry_deployments` | `gpt-6-luna` (2026-09-22, 1000), `text-embedding-3-small` (1, 800), `gpt-6.1-sol` (2026-09-29, 300) | Deployments by name: `{ model, version, capacity }`, GlobalStandard, capacity in thousands of tokens per minute |
| `foundry_sol_deployment` | `"gpt-6.1-sol"` | Deployment used as Sol, the portal's default model (`PGHACKER_FOUNDRY_SOL`) |
| `foundry_luna_deployment` | `"gpt-6-luna"` | Deployment used as Luna, the crawler's model (`PGHACKER_FOUNDRY_LUNA`) |
| `foundry_embedding_deployment` | `"text-embedding-3-small"` | Embedding deployment (`PGHACKER_FOUNDRY_EMBEDDING`) |
| `create_app` | `false` | Create the app; false for the first apply only |
| `cpu`, `memory` | `4`, `"8Gi"` | App container; memory is twice cpu |
| `job_cpu`, `job_memory` | `2`, `"4Gi"` | Jobs' containers; memory is twice cpu |
| `restrict_origin_to_front_door` | `true` | Admit only `AzureFrontDoor.Backend`'s IPv4 ranges to the app's own host name |
| `mcp_rate_per_minute`, `mcp_rate_burst` | `60`, `30` | Per-client limits of the public MCP endpoint (`PGHACKER_RATE_PER_MINUTE`, `PGHACKER_RATE_BURST`) |
| `crawl_start_sessions`, `crawl_max_sessions` | `8`, `16` | Extraction sessions at start and at most, up to 64 (`PGHACKER_CRAWL_START_SESSIONS`, `PGHACKER_CRAWL_MAX_SESSIONS`) |
| `github_org`, `github_team` | `"example-org"`, `"example-team"` | Who may sign in to the portal (`PGHACKER_GITHUB_ORG`, `PGHACKER_GITHUB_TEAM`) |
| `github_users` | `[]` | More GitHub logins allowed in (`PGHACKER_GITHUB_USERS`, omitted when empty) |
| `portal_role` | `"user"` | Portal role of everyone signed in, `user` or `admin` (`PGHACKER_PORTAL_ROLE`) |
| `app_env` | `{}` | More app environment variables, such as `RUST_LOG`; can't replace the settings above or set `OAUTH2_PROXY_*` |
| `front_door_endpoint_name` | `"pghacker"` | Front Door endpoint, `<name>-<hash>.<zone>.azurefd.net`; changing it changes the public host name |

## Outputs

None is secret. `terraform output -raw <name>` prints one.

| Output | Meaning |
|---|---|
| `public_url`, `mcp_url`, `oauth_callback_url` | Front Door URLs: the portal, the MCP endpoint, the GitHub OAuth App's callback |
| `front_door_host_name`, `front_door_id` | Front Door's host name and profile ID (`X-Azure-FDID`) |
| `app_fqdn` | The app's own host name, Front Door's origin |
| `subscription_id`, `resource_group_name` | Where everything is |
| `acr_name`, `acr_login_server` | Registry |
| `key_vault_name`, `key_vault_uri` | Vault |
| `horizondb_cluster_name`, `horizondb_fqdn`, `horizondb_parameter_group`, `horizondb_firewall_rules` | Database |
| `foundry_account_name`, `foundry_endpoint` | Foundry |
| `container_app_environment_name`, `container_app_environment_default_domain`, `app_name` | Container Apps |
| `probe_job_name`, `restore_job_name` | Jobs (`restore_job_name` is null without `seed_image`) |
| `managed_identity_client_id`, `log_analytics_workspace_id` | Identity and logs |

## Secrets in state

State holds the generated secrets (in the `random_password` resources, the vault secrets, and HorizonDB's create and password requests) and the Foundry key. Anyone who can read the state container can read them: give container access to appliers only, and keep plan files, which hold them too, out of version control.
