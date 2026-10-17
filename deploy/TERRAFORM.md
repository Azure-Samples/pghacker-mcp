# pghacker-mcp's Terraform

[`terraform/`](terraform/) deploys pghacker-mcp to Azure: one container app (oauth2-proxy, the home page, the PilotSwarm portal and worker, pghacker-indexer and pghacker-mcp under a supervisor) in a VNet-integrated Container Apps environment, HorizonDB (PostgreSQL 17) and Azure AI Foundry reached only through private endpoints, Key Vault for every secret, Application Insights for usage telemetry, and Azure Front Door Standard as the public entry: `/` is the public home page, `/mcp` the anonymous MCP endpoint, and `/portal/` the portal behind GitHub sign-in, on Front Door's own host name alone (a custom domain serves only the first two).

| Path | Contents |
|---|---|
| `terraform/*.tf` | The configuration: `main.tf` (resource group, registry, identity, vault, generated secrets), `network.tf`, `horizondb.tf`, `foundry.tf`, `containerapps.tf`, `frontdoor.tf`, `usage.tf` (usage telemetry) |
| `terraform/tests/` | Offline `terraform test` suites; every provider is mocked (`tests/mocks/`) |
| `terraform/.terraform.lock.hcl` | Provider versions and hashes for linux_amd64 and darwin_arm64 |
| `environments/example.tfvars` | Synthetic template to copy and customize; no real deployment identifiers |
| `environments/*.tfvars`, `*.backend.hcl` | Local environment settings and state backends; ignored except for the template |
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

State is a blob in a storage account that only Entra ID can reach, in its own resource group, so destroying the deployment can't delete it. `scripts/bootstrap-state.sh` creates what's missing and prints the backend file (`--help` lists its options; `--dry-run` only reads). Choose your own resource group, globally unique storage account name, and region:

```sh
deploy/scripts/bootstrap-state.sh --resource-group "<state-resource-group>" \
  --storage-account "<globally-unique-state-account>" --location "<azure-region>" \
  --subscription "$(az account show --query id --output tsv)" \
  --key pghacker-mcp/local.tfstate > deploy/environments/local.backend.hcl
cd deploy/terraform
terraform init -backend-config=../environments/local.backend.hcl
```

## Before the first apply

Copy `environments/example.tfvars` to an ignored file such as `environments/local.tfvars`, and replace the synthetic values. The example names are not a deployment and must not be used unchanged. Real environment settings, backend files, state, and plans must never be committed.

Terraform registers no resource providers. Once per subscription, someone with rights on it registers `Microsoft.App`, `Microsoft.OperationalInsights`, `Microsoft.ContainerRegistry`, `Microsoft.KeyVault`, `Microsoft.ManagedIdentity`, `Microsoft.HorizonDB`, `Microsoft.Network`, `Microsoft.CognitiveServices`, `Microsoft.Cdn` and `Microsoft.Storage` (`az provider register --namespace <namespace> --wait`).

The first apply runs with `create_app = false`: everything but the app, including the probe job and Front Door, whose host name gives the GitHub OAuth App its callback URL (`terraform output -raw oauth_callback_url`). With the GitHub secrets stored (below), set `create_app = true` and apply again; from then on it stays true, since false destroys the app.

## Key Vault secrets

The app and the jobs read every secret through Key Vault references resolved by the user-assigned identity; no secret value is set on Container Apps. Foundry's key authentication is off, as the subscription's policy requires: the identity is a *Cognitive Services OpenAI User* of the account, and the container's Foundry proxy requests its tokens (`AZURE_CLIENT_ID`). Application Insights' local authentication is off too: the identity is a *Monitoring Metrics Publisher* of it, and the container's usage telemetry proxy requests its tokens, so `APPLICATIONINSIGHTS_CONNECTION_STRING` is a plain setting.

| Secret | Made by | Read as | By |
|---|---|---|---|
| `db-admin-password` | Terraform, 40 characters; also set on the cluster | `PGHACKER_DB_ADMIN_PASSWORD` | App, jobs |
| `pgh-indexer-password`, `pgh-reader-password`, `pgh-swarm-password` | Terraform, 40 characters | `PGH_INDEXER_PASSWORD`, `PGH_READER_PASSWORD`, `PGH_SWARM_PASSWORD` | App, jobs |
| `oauth2-cookie-secret` | Terraform, 32 alphanumerics | `OAUTH2_PROXY_COOKIE_SECRET` | App |
| `github-client-id`, `github-client-secret` | The operator, from the GitHub OAuth App | `OAUTH2_PROXY_CLIENT_ID`, `OAUTH2_PROXY_CLIENT_SECRET` | App |
| `usage-id-key` | Terraform, 64 alphanumerics, with `usage_enabled` | `PGHACKER_USAGE_ID_KEY` | App: pghacker-mcp and the home page |

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
| `name_prefix` | `"pghacker"` | Prefix of derived names (`-logs`, `-env`, `-id`, `-vnet`, `-hdb`, `-pg17-<hash>`, `-foundry`, `-afd`, `-usage`) |
| `app_name` | `"pghacker"` | Container app's name; jobs are `<app_name>-probe` and `-restore` |
| `tags` | `{}` | Tags for every resource that takes them |
| `acr_name` | required | Registry name, globally unique |
| `acr_sku` | `"Basic"` | Registry SKU |
| `image` | required | App image in `acr_name`'s registry, by digest or a one-build tag; the probe job runs it too |
| `seed_image` | `null` | Corpus seed image for the restore job; null creates no restore job |
| `key_vault_name` | required | Vault name, globally unique |
| `key_vault_purge_protection_enabled` | `false` | Purge protection; irreversible |
| `log_retention_days` | `30` | Log Analytics retention, 30 to 730 |
| `usage_enabled` | `true` | Usage telemetry ([PRIVACY.md](../PRIVACY.md)): Application Insights on the logs' workspace, `usage-id-key`, and `APPLICATIONINSIGHTS_CONNECTION_STRING` for the app; false creates none of them, and the app records nothing. `AppEvents` keeps the events it already holds until their retention ends |
| `usage_daily_cap_gb` | `5` | The most usage telemetry Application Insights takes a day; it drops the rest of the day's events |
| `usage_retention_days`, `usage_total_retention_days` | `730`, `1095` | How long usage events stay queryable (30 to 730) and are kept (up to 4383) in the `AppEvents` table |
| `usage_details_enabled` | `false` | Whether usage telemetry records what users type into tools: arguments' values, error messages and sqlparser's errors (`PGHACKER_USAGE_DETAILS`; [PRIVACY.md](../PRIVACY.md)). Without it, a call keeps its tool, its arguments' names and size, the values of its fixed-choice arguments (enums and booleans), its outcome and, for `query`, PostgreSQL's SQLSTATE; its events say `common.details` `off`. Feedback comments follow `feedback_comments_enabled` alone |
| `feedback_rate_per_hour` | `0.1` | The chance, 0 to 1, that an address's hour of use of `/mcp` brings one "Was the last answer helpful?" through an interactive client's own form, as its user's next question starts (`PGHACKER_FEEDBACK_RATE`; [PRIVACY.md](../PRIVACY.md), [feedback](../docs/reference.md#feedback)); only with `usage_enabled`, and 0 asks nothing |
| `feedback_comments_enabled` | `false` | Whether that form also takes an optional comment of up to 500 characters (`PGHACKER_FEEDBACK_COMMENTS`); update PRIVACY.md before turning it on |
| `feedback_question_gap_seconds` | `180` | A pause between a client's tool calls longer than this starts a new question, whose first call the form can come before (`PGHACKER_FEEDBACK_QUESTION_GAP`), 1 to 3600 |
| `feedback_session_gap_seconds` | `600` | A pause shorter than this keeps a client's session active; the form comes only in an active session (`PGHACKER_FEEDBACK_SESSION_GAP`), more than `feedback_question_gap_seconds` |
| `feedback_min_questions` | `3` | The questions a client asks before the form can come, 0 to 100 (`PGHACKER_FEEDBACK_MIN_QUESTIONS`) |
| `feedback_clients` | `copilot`, `codex`, `Visual Studio Code`, `claude`, `cursor` | The interactive apps that can be asked: any whose `clientInfo.name` contains one of these, ignoring case (`PGHACKER_FEEDBACK_CLIENTS`); scripts and agents are never asked |
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
| `github_org`, `github_team` | `"example-org"`, `"example-team"` | Placeholder access settings; replace with your organization and team (`PGHACKER_GITHUB_ORG`, `PGHACKER_GITHUB_TEAM`) |
| `github_users` | `[]` | More GitHub logins allowed in (`PGHACKER_GITHUB_USERS`, omitted when empty) |
| `portal_role` | `"user"` | Portal role of everyone signed in, `user` or `admin` (`PGHACKER_PORTAL_ROLE`) |
| `privacy_url` | `null` | The privacy statement that the home page's footer links to, as "Privacy": an https URL of your own, such as `https://example.com/privacy` (`PGHACKER_PRIVACY_URL`, omitted when null). Null gives the home page no privacy link |
| `copyright_url`, `copyright_text` | `null`, `null` | The home page footer's copyright link, to the right of the privacy link: an https URL of your own, such as `https://example.com/`, and what the link says, such as `© 2026 Example`, 1 to 100 characters (`PGHACKER_COPYRIGHT_URL`, `PGHACKER_COPYRIGHT_TEXT`, omitted when null). Set both or neither; without them the home page has no copyright link. The year isn't kept current: change it when it changes |
| `app_env` | `{}` | More app environment variables, such as `RUST_LOG`, `PGHACKER_CRAWL`, `PGHACKER_CODE_HISTORY` and the extraction order's; can't replace the settings above or set `OAUTH2_PROXY_*`. Each process gets only those it uses |
| `front_door_endpoint_name` | `"pghacker"` | Front Door endpoint, `<name>-<hash>.<zone>.azurefd.net`; changing it changes the public host name |
| `front_door_custom_domain` | `null` | Optional hostname you own, such as `mcp.example.com` or `example.com`; managed TLS, a route of its own for the home page and `/mcp` only, and the canonical public and MCP URLs. The portal and its OAuth callback stay on Front Door's host name. Set it only in ignored environment settings; DNS stays with your provider. See [Custom domains](README.md#custom-domains). |

## Outputs

None is secret. `terraform output -raw <name>` prints one.

| Output | Meaning |
|---|---|
| `public_url`, `mcp_url` | Canonical URLs on the custom domain when set, otherwise Front Door's default hostname: the home page and the MCP endpoint |
| `portal_url`, `oauth_callback_url` | On Front Door's default hostname, with or without a custom domain: the portal and the GitHub OAuth App's callback |
| `front_door_host_name`, `front_door_id` | Front Door's host name and profile ID (`X-Azure-FDID`) |
| `front_door_custom_domain_validation` | Ownership TXT record's full `name`, `value` and token `expiration`; null without a custom domain |
| `app_fqdn` | The app's own host name, Front Door's origin |
| `subscription_id`, `resource_group_name` | Where everything is |
| `acr_name`, `acr_login_server` | Registry |
| `key_vault_name`, `key_vault_uri` | Vault |
| `horizondb_cluster_name`, `horizondb_fqdn`, `horizondb_parameter_group`, `horizondb_firewall_rules` | Database |
| `foundry_account_name`, `foundry_endpoint` | Foundry |
| `container_app_environment_name`, `container_app_environment_default_domain`, `app_name` | Container Apps |
| `probe_job_name`, `restore_job_name` | Jobs (`restore_job_name` is null without `seed_image`) |
| `managed_identity_client_id`, `log_analytics_workspace_id` | Identity and logs |
| `usage_app_insights_name`, `usage_app_id` | Usage telemetry's Application Insights resource and its application ID, for `az monitor app-insights query --apps`; null with `usage_enabled = false` |

## Secrets in state

State holds the generated secrets (in the `random_password` resources, the vault secrets, and HorizonDB's create and password requests), but no Foundry key: with key authentication off, the provider doesn't read the keys. Anyone who can read the state container can read the secrets: give container access to appliers only, and keep plan files, which hold them too, out of version control.
