# pghacker-mcp's Terraform

[`terraform/`](terraform/) deploys pghacker-mcp's infrastructure to Azure: an AKS cluster with a services pool and a tainted PilotSwarm pool, each in its own subnet, whose outbound traffic goes through Azure Firewall; a workload identity per component; HorizonDB (PostgreSQL 17) and Azure AI Foundry reached only through private endpoints; Key Vault for every secret; Application Insights for usage telemetry; and Azure Front Door Premium as the public entry, which reaches the cluster over Private Link alone: `/` is the public home page, `/mcp` the anonymous MCP endpoint, and `/portal/` the portal behind GitHub sign-in, on Front Door's own host name alone (a custom domain serves only the first two). The workloads themselves are Kubernetes manifests (`k8s/`), which `scripts/aks.mjs` renders from the `kubernetes` output and applies ([README.md](README.md)).

| Path | Contents |
|---|---|
| `terraform/*.tf` | The configuration: `main.tf` (resource group, registry, vault, generated secrets), `network.tf` (the VNet, its subnets and route, the private endpoints' NSG), `firewall.tf`, `aks.tf`, `identity.tf` (the workload identities and their secrets), `horizondb.tf`, `foundry.tf`, `frontdoor.tf`, `usage.tf` (usage telemetry) |
| `terraform/tests/` | Offline `terraform test` suites; every provider is mocked (`tests/mocks/`) |
| `terraform/.terraform.lock.hcl` | Provider versions and hashes for linux_amd64 and darwin_arm64 |
| `k8s/` | The manifests' templates; `scripts/lib/aks.mjs` fills them in and adds the service accounts, secret classes and model catalog |
| `scripts/aks.mjs` | Applies the manifests, runs the ops jobs and `kubectl` in the private cluster (`az aks command invoke`), and approves Front Door's connection |
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

The tests apply to synthetic state and never reach Azure. They cover the cluster and its pools, the subnets, the route through the firewall and its rules for each pool, the private endpoints' NSG and DNS, the workload identities and the secrets each may read, the parameter group, Foundry's network settings and deployments, the `kubernetes` output and the app's settings in it, Front Door's Private Link origin and routes, the staged first apply, and the variables' validations. `npm test` checks the manifests that the output renders (`test/aks.test.mjs`).

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

Terraform registers no resource providers. Once per subscription, someone with rights on it registers `Microsoft.ContainerService`, `Microsoft.OperationalInsights`, `Microsoft.Insights`, `Microsoft.OperationsManagement`, `Microsoft.ContainerRegistry`, `Microsoft.KeyVault`, `Microsoft.ManagedIdentity`, `Microsoft.HorizonDB`, `Microsoft.Network`, `Microsoft.CognitiveServices`, `Microsoft.Cdn` and `Microsoft.Storage` (`az provider register --namespace <namespace> --wait`).

The first applies run with `create_origin = false`: everything but Front Door's origin and routes, whose Private Link Service AKS creates only once the manifests' edge Service exists. Front Door's host name is known already, which gives the GitHub OAuth App its callback URL (`terraform output -raw oauth_callback_url`). Once `../scripts/aks.mjs apply` has deployed the app, set `create_origin = true`, apply, and approve Front Door's connection (`aks.mjs approve-edge`); from then on it stays true, since false takes the site down.

## Key Vault secrets

No secret is in the manifests or in Terraform's outputs. Each component's workload identity is granted *Key Vault Secrets User* on its own secrets, one by one (`local.workloads`), and the Secrets Store CSI driver syncs them into the Kubernetes Secret `<identity>-secrets`, which the component's environment reads. Foundry's key authentication is disabled by design: the token proxy's identity is a *Cognitive Services OpenAI User* of the account, and requests tokens for it. Its outbound access is restricted with no allowed host (`outbound_network_access_restricted`, data loss prevention), so it fetches no URL for a caller. Application Insights' local authentication is off too: the same identity is a *Monitoring Metrics Publisher* of it, so `APPLICATIONINSIGHTS_CONNECTION_STRING` is a plain setting.

| Secret | Made by | Read as | By (identity) |
|---|---|---|---|
| `db-admin-password` | Terraform, 40 characters; also set on the cluster | `PGHACKER_DB_ADMIN_PASSWORD` | The jobs (`ops`) alone |
| `pgh-indexer-password`, `pgh-reader-password`, `pgh-swarm-password` | Terraform, 40 characters | `PGH_INDEXER_PASSWORD`, `PGH_READER_PASSWORD`, `PGH_SWARM_PASSWORD` | Each its role's component (`indexer`, `mcp`, `swarm`: the portal, the worker and the egress check); the jobs, which set them on the roles |
| `oauth2-cookie-secret` | Terraform, 32 alphanumerics | `OAUTH2_PROXY_COOKIE_SECRET` | oauth2-proxy (`edge`) |
| `github-client-id`, `github-client-secret` | Terraform, as placeholders; then the operator, from the GitHub OAuth App | `OAUTH2_PROXY_CLIENT_ID`, `OAUTH2_PROXY_CLIENT_SECRET` | oauth2-proxy (`edge`) |
| `usage-id-key` | Terraform, 64 alphanumerics, with `usage_enabled` | `PGHACKER_USAGE_ID_KEY` | pghacker-mcp (`mcp`) and the home page (`home`) |

Terraform creates the GitHub OAuth App's secrets with the value `unset` and the tag `placeholder = true`, so that oauth2-proxy's identity can be granted them before they exist, and ignores their value from then on. Register the GitHub OAuth App with `public_url` as its homepage and `oauth_callback_url` as its callback, then store its credentials over the placeholders (`../scripts/aks.mjs apply` refuses to deploy while either is still one):

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
| `location` | required | Region for everything; needs HorizonDB, AKS, Azure Firewall, Front Door Premium's Private Link origins and the models |
| `resource_group_name` | required | Resource group Terraform creates for the deployment |
| `name_prefix` | `"pghacker"` | Prefix of derived names (`-logs`, `-aks`, `-fw`, `-vnet`, `-egress`, `-hdb`, `-pg17-<hash>`, `-foundry`, `-afd`, `-edge-pls`, `-usage`, and `-<component>-id` for the workload identities); AKS's nodes are in `<resource_group_name>-nodes` |
| `tags` | `{}` | Tags for every resource that takes them |
| `acr_name` | required | Registry name, globally unique |
| `acr_sku` | `"Premium"` | Registry SKU; only Premium, whose dedicated data endpoint the firewall admits |
| `image` | required | App image in `acr_name`'s registry, by digest or a one-build tag, which every component and job runs |
| `seed_image` | `null` | Corpus seed image, which the restore job runs; with null, it refuses to run |
| `key_vault_name` | required | Vault name, globally unique |
| `key_vault_purge_protection_enabled` | `false` | Purge protection; irreversible |
| `log_retention_days` | `30` | Log Analytics retention, 30 to 730 |
| `usage_enabled` | `false` | Usage telemetry ([PRIVACY.md](../PRIVACY.md)): Application Insights on the logs' workspace, `usage-id-key`, and `APPLICATIONINSIGHTS_CONNECTION_STRING` for the app; false creates none of them, and the app records nothing. `AppEvents` keeps the events it already holds until their retention ends. Set `privacy_url` before turning it on |
| `usage_daily_cap_gb` | `5` | The most usage telemetry Application Insights takes a day; it drops the rest of the day's events |
| `usage_retention_days`, `usage_total_retention_days` | `730`, `1095` | How long usage events stay queryable (30 to 730) and are kept (up to 4383) in the `AppEvents` table |
| `usage_details_enabled` | `false` | Whether usage telemetry records what users type into tools: arguments' values, error messages and sqlparser's errors (`PGHACKER_USAGE_DETAILS`). Without it, a call keeps its tool, its arguments' names and size, the values of its fixed-choice arguments (enums and booleans), its outcome and, for `query`, PostgreSQL's SQLSTATE; its events say `common.details` `off`. Feedback comments follow `feedback_comments_enabled` alone |
| `feedback_rate_per_hour` | `0.1` | The chance, 0 to 1, that an address's hour of use of `/mcp` brings one "Was the last answer helpful?" through an interactive client's own form, as its user's next question starts (`PGHACKER_FEEDBACK_RATE`; [feedback](../docs/reference.md#feedback)); only with `usage_enabled`, and 0 asks nothing |
| `feedback_comments_enabled` | `false` | Whether that form also takes an optional comment of up to 500 characters (`PGHACKER_FEEDBACK_COMMENTS`); update your privacy statement (`privacy_url`) before turning it on |
| `feedback_question_gap_seconds` | `180` | A pause between a client's tool calls longer than this starts a new question, whose first call the form can come before (`PGHACKER_FEEDBACK_QUESTION_GAP`), 1 to 3600 |
| `feedback_session_gap_seconds` | `600` | A pause shorter than this keeps a client's session active; the form comes only in an active session (`PGHACKER_FEEDBACK_SESSION_GAP`), more than `feedback_question_gap_seconds` |
| `feedback_min_questions` | `3` | The questions a client asks before the form can come, 0 to 100 (`PGHACKER_FEEDBACK_MIN_QUESTIONS`) |
| `feedback_clients` | `copilot`, `codex`, `Visual Studio Code`, `claude`, `cursor` | The interactive apps that can be asked: any whose `clientInfo.name` contains one of these, ignoring case (`PGHACKER_FEEDBACK_CLIENTS`); scripts and agents are never asked |
| `feedback_debug_enabled` | `false` | Debug mode, for trying the form by hand (`PGHACKER_FEEDBACK_DEBUG`): those apps are asked at every question after their first, without the warm-up, the chance, the hour's one ask, the active-session limit or the pause after a decline. It asks every user of `/mcp` that often, so turn it off after testing; it needs `feedback_rate_per_hour` above 0 |
| `deployer_object_ids` | `[]` | Everyone who applies and deploys, given Key Vault Secrets Officer and the cluster's Azure Kubernetes Service RBAC Cluster Admin, which `kubectl` runs with; empty means whoever is applying. Running a command in the private cluster (`az aks command invoke`) also takes the run command's actions, which Owner and Contributor have |
| `vnet_address_space` | `"10.66.0.0/22"` | VNet range |
| `subnet_prefixes` | `firewall` `10.66.0.0/26`, `system` `10.66.0.64/27`, `services` `10.66.0.96/27`, `swarm` `10.66.0.128/27`, `edge` `10.66.0.160/28`, `private_endpoints` `10.66.1.0/27` | The subnets, inside `vnet_address_space` and apart: Azure Firewall's (/26 or larger), each pool's, the edge's internal load balancer and Private Link Service, and the private endpoints'. The firewall's rules and the private endpoints' NSG tell the pools apart by them |
| `pod_cidr`, `service_cidr` | `"10.244.0.0/16"`, `"10.0.0.0/16"` | The pods' overlay addresses and the Services', apart from the VNet and each other |
| `kubernetes_version` | `null` | The cluster's version; null takes AKS's default. Patch releases and node images follow automatically |
| `aks_sku_tier` | `"Standard"` | The cluster's tier: `Standard`, with the API server's uptime SLA, `Free` or `Premium`. The API server is private either way: a private endpoint in the system pool's subnet, without a public address or name |
| `system_pool` | `Standard_D2ds_v5`, 1 to 3 | The add-ons' pool: `vm_size`, `min_count`, `max_count` |
| `services_pool` | `Standard_D4ds_v5`, 2 | The services' pool: `vm_size`, `node_count` |
| `swarm_pool` | `Standard_D4ds_v5`, 1 | PilotSwarm's pool, which has no internet: `vm_size`, `node_count`. The worker's memory limit (`k8s/worker.yaml`, 8 GiB) must fit a node with the portal's pods |
| `alert_emails` | `[]` | Who the denied-egress alert emails when the firewall denies anything from the PilotSwarm pool; empty creates the alert without an action group |
| `horizondb_cluster_name` | `null` | Cluster name; null uses `<name_prefix>-hdb` |
| `horizondb_vcores` | `4` | vCores per replica, at least 2 |
| `horizondb_replica_count` | `1` | Replicas at creation; a later change fails the plan |
| `horizondb_admin_login` | `"pghacker_admin"` | Administrator login (`PGHACKER_DB_ADMIN_USER`) |
| `database_name` | `"pghacker"` | App database, created by the app's bootstrap (`PGHACKER_DB_NAME`) |
| `horizondb_max_connections` | `null` | `max_connections` in the parameter group; null keeps HorizonDB's default |
| `extra_firewall_ips` | `[]` | Emergency operator IPv4 addresses for the public endpoint; none by default |
| `foundry_account_name` | `null` | Foundry account and custom subdomain, globally unique; null uses `<name_prefix>-foundry` |
| `foundry_deployments` | `gpt-6-luna` (2026-09-22, 1000), `text-embedding-3-small` (1, 800), `gpt-6.1-sol` (2026-09-29, 300) | Deployments by name: `{ model, version, capacity, sku }`, capacity in thousands of tokens per minute. `sku` is `GlobalStandard`, the default, or `DataZoneStandard`, which keeps requests in the account's data zone and has a quota of its own: use it when another deployment in the subscription holds the GlobalStandard quota |
| `foundry_sol_deployment` | `"gpt-6.1-sol"` | Deployment used as Sol, the portal's default model (`PGHACKER_FOUNDRY_SOL`) |
| `foundry_luna_deployment` | `"gpt-6-luna"` | Deployment used as Luna, the crawler's model (`PGHACKER_FOUNDRY_LUNA`) |
| `foundry_embedding_deployment` | `"text-embedding-3-small"` | Embedding deployment (`PGHACKER_FOUNDRY_EMBEDDING`) |
| `create_origin` | `false` | Connect Front Door to the edge's Private Link Service: false until `../scripts/aks.mjs apply` has created it, then true for good |
| `mcp_rate_per_minute`, `mcp_rate_burst` | `60`, `30` | Per-client limits of the public MCP endpoint (`PGHACKER_RATE_PER_MINUTE`, `PGHACKER_RATE_BURST`) |
| `mcp_calls_per_client` | `10` | Calls a client address may run at once on the public MCP endpoint, 1 to 1000 (`PGHACKER_MCP_CALLS_PER_CLIENT`); it answers more at once 429. The endpoint runs at most twice `mcp_pool_size` and `mcp_query_pool_size` together in all |
| `mcp_internal_rate_per_minute`, `mcp_internal_rate_burst` | `300`, `60` | Limits of pghacker-mcp's internal listener, which the agents share, since they all connect from the worker's pod (`PGHACKER_INTERNAL_RATE_PER_MINUTE`, `PGHACKER_INTERNAL_RATE_BURST`). A call over them gets 429 with Retry-After |
| `mcp_pool_size`, `mcp_query_pool_size` | `16`, `8` | pghacker-mcp's `pgh_reader` connections: every tool but `query`, and `query` (`PGHACKER_MCP_POOL`, `PGHACKER_QUERY_POOL`). Size them to the database: about twice its vCores in all, a quarter of them for `query`, such as `64` and `16` for 32 vCores. At most 128 together, under `pgh_reader`'s connection limit of 160 |
| `crawl_start_sessions`, `crawl_max_sessions` | `8`, `16` | Extraction sessions at start and at most, up to 64 (`PGHACKER_CRAWL_START_SESSIONS`, `PGHACKER_CRAWL_MAX_SESSIONS`) |
| `github_org`, `github_team` | required | The GitHub organization and the slug of its team whose members may sign in to the portal (`PGHACKER_GITHUB_ORG`, `PGHACKER_GITHUB_TEAM`). There is no default: their members get agents that run shell commands on your subscription. Empty values and the placeholders `example-org` and `example-team` are refused |
| `github_users` | `[]` | More GitHub logins allowed in (`PGHACKER_GITHUB_USERS`, omitted when empty) |
| `portal_role` | `"user"` | Portal role of everyone signed in, `user` or `admin` (`PGHACKER_PORTAL_ROLE`) |
| `privacy_url` | `null` | The privacy statement that the home page's footer links to, as "Privacy": an https URL of your own, such as `https://example.com/privacy` (`PGHACKER_PRIVACY_URL`, omitted when null). Null gives the home page no privacy link |
| `copyright_url`, `copyright_text` | `null`, `null` | The home page footer's copyright link, to the right of the privacy link: an https URL of your own, such as `https://example.com/`, and what the link says, such as `© 2026 Example`, 1 to 100 characters (`PGHACKER_COPYRIGHT_URL`, `PGHACKER_COPYRIGHT_TEXT`, omitted when null). Set both or neither; without them the home page has no copyright link. The year isn't kept current: change it when it changes |
| `app_env` | `{}` | More app environment variables, such as `RUST_LOG`, `PGHACKER_CRAWL`, `PGHACKER_CODE_HISTORY` and the extraction order's; can't replace the settings above or set `OAUTH2_PROXY_*`. Each component gets only those it uses (`scripts/lib/azure.mjs`) |
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
| `edge` | The edge's internal address and its Private Link Service, Front Door's origin, with its ID and resource group |
| `subscription_id`, `resource_group_name` | Where everything is |
| `acr_name`, `acr_login_server` | Registry |
| `key_vault_name`, `key_vault_uri` | Vault |
| `horizondb_cluster_name`, `horizondb_fqdn`, `horizondb_parameter_group`, `horizondb_firewall_rules` | Database |
| `foundry_account_name`, `foundry_endpoint` | Foundry |
| `aks_cluster_name`, `aks_node_resource_group` | The cluster, which `az aks command invoke` takes, and where AKS keeps its nodes, load balancers, the edge's Private Link Service and the API server's private endpoint and DNS zone |
| `firewall_public_ip` | The address the cluster's allowed outbound traffic leaves from |
| `kubernetes` | What `../scripts/aks.mjs` renders the manifests from: the cluster, the namespace, the images, each workload identity's client ID, service account and secrets, the networks, the edge, and the app's settings, which hold no secret |
| `log_analytics_workspace_id` | The logs: the containers' (`ContainerLogV2`) and the firewall's (`AZFWApplicationRule`, `AZFWNetworkRule`) |
| `usage_app_insights_name`, `usage_app_id` | Usage telemetry's Application Insights resource and its application ID, for `az monitor app-insights query --apps`; null with `usage_enabled = false` |

## Secrets in state

State holds the generated secrets (in the `random_password` resources, the vault secrets, and HorizonDB's create and password requests), but not the GitHub OAuth App's, whose value it never reads after the placeholder, and no Foundry key: with key authentication off, the provider doesn't read the keys. Anyone who can read the state container can read the secrets: give container access to appliers only, and keep plan files, which hold them too, out of version control.
