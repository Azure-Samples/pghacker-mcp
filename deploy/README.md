# Deploying pghacker-mcp to Azure

This deploys pghacker-mcp in production mode on Azure Kubernetes Service (AKS). Users index whatever they ask for, and the deployment starts with the local corpus. [TERRAFORM.md](TERRAFORM.md) documents the Terraform itself: variables, outputs, secrets and state. Local development is unchanged: `npm start` runs everything on one machine ([reference](../docs/reference.md)).

![pghacker-mcp on AKS: Front Door Premium reaches the cluster over Private Link; the services pool holds the edge, the home page, pghacker-mcp, the indexer, the token proxy and the jobs; the tainted PilotSwarm pool holds the portal and the worker, which reach only the indexer, pghacker-mcp's internal listener, the token proxy and the database; Azure Firewall admits each subnet's named hosts alone.](aks-architecture.svg)

- **`https://<front door>/mcp`** is the public MCP endpoint. It is anonymous and read-only, rate-limited per client, and serves only requests that came through this Front Door profile (`X-Azure-FDID`). A client is the address it connected from, `X-Azure-SocketIP` (IPv6 by /64). `X-Azure-ClientIP` would follow a client's `X-Forwarded-For`.
- **`https://<front door>/`** is the public home page (`home/`): what pghacker-mcp is, the MCP URL (its own origin's `/mcp`) and how to add it to an agent, and example prompts with their answers. Its top bar links to the GitHub repository, with its stars once the repository is public: the home page's server asks GitHub's API for them, without a token, at most every 10 minutes. Its footer links to your privacy statement if you set `privacy_url`, and to a copyright notice, to its right, if you set `copyright_url` and `copyright_text`; without them it has no such links. Its files are public; any other path needs sign-in. oauth2-proxy answers `/robots.txt` itself (`Disallow: /`).
- **`https://<front door>/portal/`** is the PilotSwarm portal. Users sign in with GitHub, and only members of your configured `github_org`/`github_team` (or `github_users`) get in. Members are PilotSwarm `user`s (`portal_role`).
  - Only Front Door's own host name serves it. A [custom domain](#custom-domains) gets the home page and `/mcp`, and Front Door answers 404 for its other paths. Sign-in therefore happens on one host, at one callback.
  - PilotSwarm 0.8.2 has no base path: its page loads at `/portal/`, but it asks for its files at `/assets/` and its API and WebSockets at `/api/`, so oauth2-proxy sends those to it too, behind sign-in. Its share links (`/?session=...`, `/?canvasShare=...`) reach the home page, which sends them on to `/portal/` on Front Door's host name (`portal_url`), whichever host they name. `scripts/lib/routes.mjs` holds the routes; [microsoft/PilotSwarm#179](https://github.com/microsoft/PilotSwarm/issues/179) asks for a base path, which would put all of the portal under `/portal/`.
  - oauth2-proxy tells the portal who signed in through headers, which PilotSwarm's `proxy` auth provider reads. PilotSwarm 0.8.2's browser app doesn't support that provider, so the image patches support in (`scripts/lib/portal-spa.mjs`).
  - The patch is pinned to the release's bundle and fails the build on any other: re-check it when upgrading PilotSwarm, and drop it once [microsoft/PilotSwarm#154](https://github.com/microsoft/PilotSwarm/issues/154) is fixed.
  - oauth2-proxy trusts no `X-Forwarded-*` header (`--reverse-proxy=false`). Front Door passes a client's on, and with them, oauth2-proxy would match its public routes against a forged `X-Forwarded-Uri` and let anyone past sign-in ([oauth2-proxy/oauth2-proxy#3506](https://github.com/oauth2-proxy/oauth2-proxy/issues/3506)). Its callback is a fixed URL, so it needs none.
- **Models** come from Azure AI Foundry: `gpt-6.1-sol` is the portal's default, `gpt-6-luna` runs extraction and deduplication, and `text-embedding-3-small` does embeddings. Key authentication is disabled by design: the token proxy (`scripts/token-proxy.mjs`) adds its workload identity's Entra token to each request (`scripts/lib/foundry.mjs`), and no pod holds a key. Since the agents reach the models too, the proxy forwards only the operations the app uses (the Responses API, the model list and a deployment's embeddings), removes hosted tools that reach the network (web search, remote MCP servers), and refuses a request that would have Foundry fetch a URL, such as an image's. The account itself may reach no outside host (data loss prevention).
- **Usage telemetry**, with `usage_enabled = true`, goes to Application Insights on the logs' workspace. It's off by default: set `privacy_url` to your privacy statement before you turn it on.
  - It records an event for each request to `/mcp`: the tool, its arguments' names, its outcome and the shape of its answer, and the client. It also records each view of the home page.
  - `usage_details_enabled = true` also records what users type into tools: arguments' values, error messages and sqlparser's errors. Off by default, a call keeps only the values of its fixed-choice arguments (enums and booleans) and, for `query`, PostgreSQL's SQLSTATE.
  - Application Insights derives a country, region and city from each client's address, then discards the address.
  - Now and then, as an interactive client's user starts a new question, `/mcp` asks through the client's own form whether the last answer helped, and records the choice ([feedback](../docs/reference.md#feedback)): `feedback_rate_per_hour` (0.1) sets how often, and 0 turns it off.
  - `usage_enabled = false`, the default, turns it all off ([PRIVACY.md](../PRIVACY.md)).
- **Production mode**:
  - `add_source` takes any list of the postgresql.org archive (chosen months, a range, or the whole archive, plus seed threads).
    - A list added without months, `follow` included, queues the whole archive.
    - PostgreSQL's code isn't a source: the code tools read any branch, tag or commit of the [commit history](#commit-history), which its hourly sync keeps current, and `list_sources.history` reports it.
  - `follow` retrieves a list's current month again every hour.
  - The indexer's job workers have lanes: worker 0 runs the recurring jobs, and the others run anything.
  - The crawl scheduler keeps extraction sessions busy as long as threads wait, as fast as Foundry allows: it starts 8, adds one a minute while saturated, halves on a 429, and stops at `crawl_max_sessions`. It runs deduplication as extracts arrive.
  - There are no caps on scope or spend. Every thread of whatever users add gets extracted by `gpt-6-luna`.

## Disclaimers

MCP is a new and developing standard. As with all new technology standards, you should review the security of any systems that integrate with MCP servers, such as MCP hosts, clients, agents, AI applications, and models and confirm that they comply with system requirements, standards, and expectations. You should follow Microsoft security guidance for MCP servers, including enabling Entra ID authentication, secure token management, and network isolation. Refer to [Microsoft Security Documentation](https://learn.microsoft.com/azure/api-management/secure-mcp-servers) for details.

This MCP server may provide access to underlying resources, such as tools, services, and/or data. Your use of that underlying resource via this MCP server is governed by the underlying resource's license terms.

## The cluster

The cluster has three node pools, each in its own subnet, and every pod runs in the `pghacker` namespace from the one image (`Dockerfile`), with its own command, user, identity and NetworkPolicy (`deploy/k8s`, rendered by `scripts/lib/aks.mjs` from Terraform's `kubernetes` output):

- **system**: the cluster's add-ons alone.
- **services**: oauth2-proxy, the home page, pghacker-mcp, pghacker-indexer, the token proxy and the ops jobs.
- **swarm**: PilotSwarm, tainted so that nothing else lands there: the portal, and the worker, which runs every agent session, the crawl's and the portal's, and the crawl scheduler. Its subnet gets no internet.

| Pod | Pool | Reaches | Secrets |
| --- | --- | --- | --- |
| `edge`: oauth2-proxy, ×2 | services | pghacker-mcp's public listener, the portal, the home page; GitHub | The GitHub OAuth App's ID and secret, and the cookie secret |
| `home`: the home page's server (`scripts/home.mjs`) | services | The token proxy (usage telemetry); GitHub's API, for the stars | The users' key (`usage-id-key`), with usage telemetry |
| `mcp`: pghacker-mcp | services | Its database role, `pgh_reader`; the token proxy (embeddings, usage telemetry) | `pgh_reader`'s password; the users' key |
| `indexer`: pghacker-indexer | services | `pgh_indexer`; the token proxy; the archive and git.postgresql.org | `pgh_indexer`'s password |
| `token-proxy`, ×2 (`scripts/token-proxy.mjs`) | services | Foundry's private endpoint; Entra ID; Application Insights' ingestion | None: its workload identity's tokens |
| `portal`: PilotSwarm's portal, ×2, which runs no agent | swarm | `pgh_swarm`; the token proxy | `pgh_swarm`'s password |
| `worker`: PilotSwarm's worker and the crawl scheduler (`scripts/worker.mjs`) | swarm | The indexer's tools, pghacker-mcp's internal listener, the token proxy and `pgh_swarm`: nothing else | `pgh_swarm`'s password |
| Jobs: `bootstrap`, `probe`, `restore`, `history` | services | The database as its administrator; the token proxy; the firewall's hosts | The administrator's password and the roles' |
| Job: `egress-check` (`scripts/egress-check.mjs`) | swarm | As the worker, whose identity and policy it has | `pgh_swarm`'s password |

- **Identities.** Each component has its own user-assigned identity, federated with its Kubernetes service account. The Secrets Store CSI driver reads a component's Key Vault secrets with it, each granted one by one, and syncs them into the Kubernetes Secret its environment reads. Only the token proxy's pod gets a token of its own. No pod mounts a Kubernetes API token, and only the jobs read the administrator's password.
- **Pods.** The namespace enforces Pod Security's `restricted` profile: each pod runs as its image user (`pgh-agent`, `pgh-svc`, `pgh-proxy`, `pgh-home`), with a read-only root, no Linux capabilities and the runtime's seccomp profile. tini is PID 1 and reaps what the agents' shells leave behind.
- **The network.** Cilium enforces the NetworkPolicies: everything is denied unless a policy allows it. The edge alone takes traffic from outside the cluster, from the Private Link Service's addresses; the portal, which trusts oauth2-proxy's identity headers, takes it from the edge alone. Every node subnet sends what leaves the VNet to Azure Firewall, which admits the services pool's named hosts and every pool's platform endpoints (AKS, the registry, the vault, Azure Monitor), and denies and logs the rest. The private endpoints' NSG takes the database from the services and PilotSwarm pools, and Foundry from the services pool alone.
- **Health.** Each Deployment's probes ask its `/healthz`, oauth2-proxy's `/ping` and PilotSwarm's `/api/health`; the token proxy is ready once it has a token. The worker has none: Kubernetes restarts it when it exits.
- **Artifacts.** Without blob storage, PilotSwarm keeps sessions' artifacts on the worker's disk, where the portal, in pods of its own, can't show them. The app's agents write none.

## First deployment

You need the Azure CLI, signed in (`az login`) with Owner on the subscription, Terraform 1.9+, `kubectl` and `kubelogin` (`az aks install-cli`), and Node 24 with the local app set up (for the corpus) and its development dependencies installed (`npm install`; `deploy/scripts/aks.mjs` uses them). From the repository root, copy the synthetic example:

```sh
cp deploy/environments/example.tfvars deploy/environments/local.tfvars
```

Replace its placeholder subscription ID, names, region, model capacities, and GitHub access settings with your own. `github_org` and `github_team` have no default, and their placeholders fail validation: their members get the portal's agents, which run shell commands on your subscription. Choose globally unique registry and vault names, and leave `create_origin = false` until step 9. Real `.tfvars`, `.tfvars.json`, and `.backend.hcl` files are ignored; do not force-add them or commit secrets, state, or plans.

The remaining commands run from `deploy/terraform` unless they say otherwise. The `local` filenames are examples: use a separate settings file and backend for each environment.

1. **State.**

   ```sh
   ../scripts/bootstrap-state.sh --resource-group "<state-resource-group>" \
     --storage-account "<globally-unique-state-account>" --location "<azure-region>" \
     --subscription "$(az account show --query id --output tsv)" \
     --key pghacker-mcp/local.tfstate > ../environments/local.backend.hcl
   terraform init -backend-config=../environments/local.backend.hcl
   ```

2. **The registry.** This is a targeted apply, so Terraform warns that the plan is incomplete:

   ```sh
   terraform apply -var-file=../environments/local.tfvars -target=azurerm_container_registry.this
   ```

3. **The image.** From the repository root, ACR Tasks builds `Dockerfile` for linux/amd64 (`deploy/acr-build.yaml`):

   ```sh
   acr=$(terraform -chdir=deploy/terraform output -raw acr_name)
   tag=$(git rev-parse --short=12 HEAD)
   az acr run --registry "$acr" --file deploy/acr-build.yaml --set TAG="$tag" .
   az acr repository show --name "$acr" --image "pghacker-mcp:$tag" --query digest --output tsv
   ```

   Set `image = "<your-registry>.azurecr.io/pghacker-mcp@sha256:<digest>"` in `local.tfvars`, using your registry name and the returned digest.

4. **The infrastructure.** This step creates:
   - the VNet, Azure Firewall and its policy, the route that sends every node subnet's traffic through it;
   - the AKS cluster and its three pools, and the workload identities;
   - HorizonDB with its parameter group and private endpoint; Foundry with its three deployments and private endpoint;
   - Key Vault and its secrets, the GitHub OAuth App's as placeholders, and Front Door, without an origin yet.

   ```sh
   terraform apply -var-file=../environments/local.tfvars
   ```

5. **The cluster's objects and the probe.** `credentials` gets a kubeconfig through Entra ID. `apply --infra` applies everything but the app's workloads: the namespace, service accounts, secret classes, NetworkPolicies, Services (the edge's creates its internal load balancer and Private Link Service) and the token proxy. It then runs the `bootstrap` job, which creates the database, the roles, the extensions, the schema and the graph, and applies the migrations.

   ```sh
   node ../scripts/aks.mjs credentials
   node ../scripts/aks.mjs apply --infra
   node ../scripts/aks.mjs job probe | jq .
   ```

   The probe checks, from inside the VNet:
   - the extensions, and that BM25, vector, trigram and Cypher queries run as `pgh_reader`;
   - each Foundry deployment, through the token proxy;
   - the archive, git.postgresql.org and GitHub, and that the firewall refuses any other host.

   Fix what `failures` lists, then run the probe again until `ok` is true.

6. **The corpus.** Dump the local corpus (with `npm start` running locally), build its seed image from the app image, and restore it before the app exists:

   ```sh
   npm run corpus:dump                             # from the repository root: .runtime/corpus/<id>/
   acr=$(terraform -chdir=deploy/terraform output -raw acr_name)
   cd .runtime/corpus/<id>
   az acr build --registry "$acr" --platform linux/amd64 --image pghacker-seed:<id> \
     --build-arg "APP_IMAGE=$acr.azurecr.io/pghacker-mcp@sha256:<digest>" .
   ```

   Set `seed_image = "<your-registry>.azurecr.io/pghacker-seed:<id>"`, apply, and run the restore:

   ```sh
   terraform apply -var-file=../environments/local.tfvars
   node ../scripts/aks.mjs job restore | jq '{ok, corpus, rows}'
   ```

   The restore checks that the dump's checksum, migrations and embedding model match. It refuses a database that already has a corpus unless run with `--replace` (`node ../scripts/aks.mjs job restore -- --replace`), and refuses while the indexer is connected. It loads schema `pghacker` and checks every table's row count. The indexer rebuilds the graph from the tables when it starts.

7. **The GitHub OAuth App.**
   - Register it under your GitHub organization's **Settings > Developer settings > OAuth Apps** (or under your account), with:
     - homepage `terraform output -raw public_url`
     - callback `terraform output -raw oauth_callback_url`: oauth2-proxy's `/oauth2/callback` on Front Door's own host name, which a custom domain doesn't change. Turn off the callback's wildcard matching, which GitHub turns on for an app with a single callback: it would also accept the callback's subdomains and sub-paths, and oauth2-proxy sends exactly this URL.
   - Some organizations restrict OAuth Apps. Until an org owner approves this one, GitHub hides org and team membership from it, and every sign-in fails with a 500 (the logs say `user is missing required organization`).
     - To ask for approval, open `https://github.com/settings/connections/applications/<client ID>` and click **Request** next to your organization.
     - Until then, `github_users` (a list of logins) lets those users in. It doesn't follow the team: once the app is approved, keep only the logins that aren't on the team.
   - Store its credentials over Terraform's placeholders (TERRAFORM.md, "Key Vault secrets"):

   ```sh
   vault=$(terraform output -raw key_vault_name)
   az keyvault secret set --vault-name "$vault" --name github-client-id --value "<client ID>" --output none
   read -rsp 'Client secret: ' secret && echo
   az keyvault secret set --vault-name "$vault" --name github-client-secret --value "$secret" --output none && unset secret
   ```

8. **The app.** `apply` refuses while a GitHub secret is still the placeholder. It applies what `--infra` does, runs the bootstrap again, then the workloads, and waits for their rollouts:

   ```sh
   node ../scripts/aks.mjs apply
   ```

9. **Front Door's origin.** Set `create_origin = true` and apply: Front Door asks to connect to the edge's Private Link Service, and the apply warns that the connection isn't approved (`edge_private_link_approved`). Approve it: `approve-edge` approves only a pending request whose message is this Front Door's. From then on, never set `create_origin` back to `false`, which takes the site down.

   ```sh
   terraform apply -var-file=../environments/local.tfvars
   node ../scripts/aks.mjs approve-edge
   terraform plan -var-file=../environments/local.tfvars   # no warning once approved
   ```

10. **Check it.** Front Door can take a few minutes to route a new origin.

    ```sh
    mcp=$(terraform output -raw mcp_url)
    curl -s "$mcp" -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' | jq '[.result.tools[].name]'
    ```

    From the repository root, check the routes from outside, read-only. On the public URL: the home page and its files without sign-in, its share links sent on to the portal's host, and `/mcp`; with a custom domain, Front Door's 404 for the portal's paths. On the portal's: the portal and its API behind sign-in, whose callback is on that host, even with a forged `X-Forwarded-Uri`:

    ```sh
    npm run check:public -- --url "$(terraform -chdir=deploy/terraform output -raw public_url)" \
      --portal-url "$(terraform -chdir=deploy/terraform output -raw portal_url)"
    ```

    Then check the agents' lockdown from the worker's place: the indexer, pghacker-mcp's internal listener, the token proxy and the database answer, and nothing else does, not the internet, the instance metadata endpoint, Azure's WireServer, the Kubernetes API, the portal or pghacker-mcp's public listener:

    ```sh
    node ../scripts/aks.mjs job egress-check | jq .
    ```

    Then open `public_url`, the home page, without signing in, and `portal_url`: GitHub asks you to sign in, and a team member reaches the portal; anyone else gets 403. In a pghacker Crawler session, `ingest_status` shows the restored corpus and the graph's vertices and edges.

11. **The code.** Unless the corpus brought a commit history, build one ([Commit history](#commit-history)): the code tools have no other source.

## Updates

Build and push a new image (step 3), set its digest as `image`, and apply. Then deploy it: `node ../scripts/aks.mjs apply` runs the `bootstrap` job with the new image, which applies its migrations while the old pods serve ([Schema upgrades](#schema-upgrades)), and only then rolls the Deployments out. A setting changed in Terraform (`app_env` and the other variables in the `kubernetes` output) is deployed the same way: apply, then `aks.mjs apply`; the pods whose environment changed restart.

- The services' Deployments start their new pods before they stop the old ones. The new indexer serves its tools as soon as it starts, but runs jobs only once the old one has stopped. Only the indexer that holds a session advisory lock runs jobs; it checks the lock's connection every 2 s. If that connection is lost, the lock goes with it, so the indexer drops its running jobs and exits, and Kubernetes starts it again to wait for its turn. An indexer that takes the lock and finds jobs left running waits 30 s, until the one that ran them has stopped them, before it queues them again. A worker that can't record a job's outcome, as when the database drops its connection, tries again for 30 s; then the indexer drops its running jobs and exits the same way. If the indexer's MCP server stops, as when it can't listen, the indexer finishes its running jobs, as on a shutdown, and exits with the error for Kubernetes to start it again.
- The worker is replaced, never doubled, since two crawl schedulers would race over the sessions. It gets 60 s to stop: the scheduler first, then PilotSwarm's sessions. A turn still running is dropped; extraction leases lapse after 20 minutes and their threads are extracted again.

Usage telemetry is off unless `usage_enabled = true`; it used to be on by default. A deployment that relied on that default must set `usage_enabled = true`, and `privacy_url`, before its next apply. Otherwise the apply deletes its Application Insights resource and the users' key, `usage-id-key`: `AppEvents` keeps the events it holds until their retention ends, but if telemetry is turned on again, clients get new IDs.

### Schema upgrades

The bootstrap applies the migrations a database lacks, in name order and each in its own transaction, and refuses one that changed after it was applied.

- A migration waits at most 5 s for a lock, unless it sets its own `lock_timeout`, as most do (10 s). Meanwhile, other sessions' requests for the table queue behind it.
- One that fails rolls back alone, and those before it stay applied. The `bootstrap` job tries three more times, each attempt starting with the failed migration; if they all fail, `aks.mjs apply` stops there, and the old pods keep serving.
- The log says when each migration is applied, and how long it took. When one times out waiting for a lock, or deadlocks, its error lists the sessions that hold locks on the schema's tables, oldest transaction first: their pid, role, application, how long they have been connected and in their transaction, their state, their query and their locks. Only this log shows them: the `query` tool can't see other roles' sessions.
- Until every migration is applied, the new pods don't start, and the old ones serve on the partly migrated schema. So each migration must work with the previous image by itself. One that the previous image can't run on, such as `0019`, which changed the type of a column that images before it read, goes last in its deployment: otherwise, while a later migration waits for a lock, the old pods serve on a schema they can't read. Changes that must commit together go in one file.
- A migration can't hold statements that can't run in a transaction, such as `CREATE INDEX CONCURRENTLY`. An index on a populated table is built before the deployment, from a reviewed runbook, as [Graph index preparation](#graph-index-preparation) does for the graph's.

| Migration | Adds |
| --- | --- |
| `0013_duplicate_candidates` | `find_duplicates`' pairs, found as extracts arrive |
| `0014_commit_graph` | The commit history in the graph |
| `0016_retire_snapshots` | Drops the snapshots ([Commit history](#commit-history)) |
| `0017_extraction_lease_versions` | The thread versions each extraction lease read |
| `0018_feature_release_at` | When a feature's release was stated |
| `0019_patch_series_text` | Patches' series versions as text, parsed again ([Patch series](#patch-series)) |
| `0020_retire_version_sources` | Deletes the PostgreSQL versions the snapshot loader left in `sources`, and its jobs ([Commit history](#commit-history)) |
| `0021_shipped_releases` | Features' releases by the names they shipped as ([Shipped release names](#shipped-release-names)) |
| `0022_rethread_mail` | `rethread_mail()` ([Threads](#threads)) |
| `0026_redact_addresses` | Redacts stored mail's email addresses ([Email addresses](#email-addresses)) |
| `0028_extraction_lease_purge` | Drafts deleted with their thread, so that ingest merges a thread that only drafts name; the `purge_leases` job deletes leases a day after they end, with their drafts |

`0016` may have been applied before `0013`–`0015`: some images have `0016` but not `0013`–`0015`. No image that reads the snapshots runs after `0016`; an image that has `0016` but predates `0014` reads the history only and runs on the schema through `0018` ([Rolling back past 0014](#rolling-back-past-0014)). Images before `0019` read patches' series versions as numbers ([Patch series](#patch-series)). `0020` changes nothing that images with `0016` read: none of them stores a PostgreSQL version. Images before `0021` save features' releases as their threads name them ([Shipped release names](#shipped-release-names)). `0022` adds a function only; images before it don't know its job ([Threads](#threads)). Images before `0028` run on its schema, but fail its `purge_leases` job, which retries with backoff: after rolling back to one, cancel it (`UPDATE pghacker.jobs SET state = 'cancelled' WHERE kind = 'purge_leases';`).

### Patch series

An image with migration `0019_patch_series_text` stores a patch's series version as text, as its file name or subject writes it, and takes parts from file names only ([reference](../docs/reference.md#patch-series)).
- The migration replaces the `series_version` column, which only changes the catalog, and queues the `patch_series` job (key `reparse`).
- The job parses every thread's patch names and subjects again, 500 threads a run, and updates the rows that change, whose lines go into the `text_ids` index again. `ingest_status` shows its state; its `result` counts the threads, patches and changes.
- Until it finishes, `search_patches` and `get_commit` show no version for the threads it hasn't reached.
- When the rules change, `RULES` in `crates/pghacker-mail/src/series.rs` goes up, and the leading indexer queues the job once more as it starts (key `rules-<N>`). That run updates only the patches the new rules parse differently, which keep their old values until it reaches them. Rules 2 correct two kinds of version: dates after a version (`insert_on_dup.v3.2013_09_10` gave `3.2013`, now `3`) and slugs taken as labels (`v2-Opclass-bitwise-equality-0001` gave `2-Opclass-bitwise-equality`, now `2`).

An image with older rules, such as rules 1, reads the same column: rolling back to one needs nothing, and it runs a `rules-<N>` job it finds queued with its own rules. After rolling forward, queue the jobs again, as `pgh_indexer`:

```sql
UPDATE pghacker.jobs SET state = 'queued', payload = '{}', attempts = 0, run_after = now(),
       last_error = NULL, result = NULL, finished_at = NULL
WHERE kind = 'patch_series';
```

Images before `0019` read the column as a number. While the old image's pods overlap the new ones during the upgrade, their `search_patches` and `get_commit` fail; their ingest still works. To roll back to one, as `pgh_indexer`, give it an empty number column, and stop the job, which it doesn't know:

```sql
ALTER TABLE pghacker.patches DROP COLUMN series_version;
ALTER TABLE pghacker.patches ADD COLUMN series_version int;
UPDATE pghacker.jobs SET state = 'cancelled'
WHERE kind = 'patch_series' AND state IN ('queued', 'running');
```

It then shows versions only for the mail it stores. `0019` is recorded and doesn't run again: after rolling forward, put the text column back and queue the job:

```sql
ALTER TABLE pghacker.patches DROP COLUMN series_version;
ALTER TABLE pghacker.patches ADD COLUMN series_version text;
UPDATE pghacker.jobs SET state = 'queued', payload = '{}', attempts = 0, run_after = now(),
       last_error = NULL, result = NULL, finished_at = NULL
WHERE kind = 'patch_series';
```

### Shipped release names

An image with migration `0021_shipped_releases` saves a feature's release by the name it shipped as. Four releases were renamed during their development, and their threads use the development names: 6.6 for 7.0, 7.5 for 8.0, 8.5 for 9.0 and 9.7 for 10 ([reference](../docs/reference.md#extraction)).
- The migration renames the features saved before.
- The graph sync then moves their `TARGETS` edges and drops the four development names' `Release` vertices, without a full sync.

Images before `0021` save releases as their threads name them. That includes the old image's pods while they overlap the new ones during the upgrade. Rolling back to one needs nothing. After the upgrade, and after rolling forward from one, rename the releases they saved, as `pgh_indexer`; it changes nothing when there are none:

```sql
UPDATE pghacker.features SET release = pghacker.shipped_release(release)
WHERE release IS DISTINCT FROM pghacker.shipped_release(release);
```

### Threads

An image with migration `0022_rethread_mail` leaves senders' addresses and placeholders out of the IDs that thread mail, and gives a reply on a topic of its own a thread of its own ([threading](../docs/reference.md#threading)). Mail stored before it stays as it was threaded, glued together in places, until the `rethread_mail` job threads it again. The dry run, the apply and the extraction of the threads that change are separate approvals. As `pgh_indexer` from the operator environment:

1. `SELECT pghacker.rethread_mail();` queues a dry run, which changes nothing. It downloads again the months whose stored IDs name a sender or a placeholder, plans, and records its report as the job's result: `SELECT jsonb_pretty(result) FROM pghacker.jobs WHERE kind = 'rethread_mail' AND key = 'dry-run';`. Review its summary and groups: for each, the threads it ends up with, the corrected IDs, and where extracts go.
2. `SELECT pghacker.rethread_mail('<plan>');`, with the report's `plan`, applies the groups that are still as the dry run found them, each in a transaction. A group whose threads or extracts changed since, or that an extraction lease holds, is skipped with its reason: run a dry run again and review what's left. The graph follows, and commits' `DISCUSSED_IN` links are refreshed.
3. The apply's `extract` lists the threads to extract again. The backfill leases those newer than its head soon; lease the older ones by `thread_ids`.

A job that's queued or running is left as it is. To roll back to an image before `0022`, cancel its jobs first:

```sql
UPDATE pghacker.jobs SET state = 'cancelled' WHERE kind = 'rethread_mail' AND state IN ('queued', 'running');
```

### Email addresses

An image with migration `0026_redact_addresses` stores mail with its email addresses redacted: each in subjects, bodies and patches becomes `[email]`, and senders' names lose theirs ([reference](../docs/reference.md#email-addresses)). Mail stored before it is redacted in place by the `redact_addresses` job (key `redact`), which the migration queues; `ingest_status` shows it, and its `result` counts what changed.

- It redacts 500 messages a run: their text, their chunks, which the `embed` job embeds again, and their patches, which the `patch_lines` job indexes again. Senders' names move to their redacted forms, and the graph follows the renamed authors and retitled threads.
- Then it redacts authors' names left over and the extracts' text and quotes. Changed extracts' embeddings are cleared, to be embedded again, and the extracted threads they come from get a new content version, for the backfill to extract again.
- Last, once no patch waits to be indexed again, it deletes the `patch_texts` lines with addresses that no patch uses.

Until it finishes, the tools show addresses in the mail it hasn't reached. Images before `0026` store new mail with addresses and don't know the job: to roll back to one, cancel it first, and queue it again after rolling forward (it skips what's redacted):

```sql
UPDATE pghacker.jobs SET state = 'cancelled' WHERE kind = 'redact_addresses' AND state IN ('queued', 'running');
UPDATE pghacker.jobs SET state = 'queued', payload = '{}', attempts = 0, run_after = now(),
       last_error = NULL, result = NULL, finished_at = NULL
WHERE kind = 'redact_addresses';
```

### Graph index preparation

The graph-index upgrade requires preparation **before deploying the new service
revision**. Fresh bootstrap creates concrete edge-ID indexes atomically with new
labels; migration `0009_graph_indexes` creates mapping endpoint indexes only on a
table checked empty under a write-blocking lock. Existing missing AGE indexes or
populated mappings fail startup with operator guidance instead of launching a
blocking build. Applied migration markers alone do not satisfy readiness.

From an approved private-network operator environment, use the established
`PGHACKER_ADMIN_DATABASE_URL` (graph owner) and, for read-only checks,
`PGHACKER_INDEXER_DATABASE_URL`. Never pass those credentials to the public
reader, portal agents, or serving indexer. No new ownership/grants are needed:
the graph owner must already be able to `SET ROLE pgh_indexer` for mapping DDL.
The script does not bootstrap roles, run migrations/jobs, deploy, or drop indexes.

```sh
node scripts/graph-indexes.mjs --plan --out /protected/operator/graph-indexes.json
node scripts/graph-indexes.mjs --check
# Only after approval of the exact manifest, capacity/window, and statement budget:
node scripts/graph-indexes.mjs --apply \
  --plan-file /protected/operator/graph-indexes.json --build-timeout-ms <approved-ms>
```

Plan output includes existing/parent index definitions and blocked states.
Manifests contain no credentials, are created owner-only without overwriting a
file, and bind the target database/server, relation OIDs, owners, and definitions.
An unexpected label, replaced table, incompatible/invalid index, or new missing
requirement needs a newly reviewed plan. Equivalent valid indexes under another
name are reused; `IF NOT EXISTS` is not treated as proof.

The script uses one persistent autocommit psql session, a separate preparation
advisory lock, and one concurrent build at a time. It checks exact physical ID
duplicates before a selected AGE build; it does not repair them or claim a full
graph-data audit. It sets a two-second lock timeout for its checks, disables
parallel maintenance workers for that session, and requires an explicit build
deadline. The build itself waits for older transactions, such as a running
graph sync, for as long as that deadline allows: a lock timeout would cancel it
and leave an invalid index.
Before applying, approve disk reserve for remaining indexes, measured temporary
build/WAL space and growth, and compare producer/read latency under concurrent
extraction/full sync. Do not change jobs or terminate other sessions to bypass a
writer/snapshot wait without separate permission.

For an approved canary, add `--only pghacker_graph.MODIFIES_id_idx` if that index
is in the manifest. A completed subset reports the remaining requirements and
exits 2 until the full catalog is ready. Resume the same reviewed manifest only
when scope is unchanged; completed equivalent indexes are skipped. Failure exits
nonzero, reports completed work and SQLSTATE, and never automatically drops an
invalid concurrent index. SIGINT/SIGTERM cancels the owned psql operation; inspect
reported active/invalid state before any separately approved cleanup.

Monitor JSON progress and `pg_stat_progress_create_index` phases/lockers,
authorized activity/wait information, disk, WAL/replica lag, and service
latency/timeouts. A concurrent build still scans and waits, and can interfere
with maintenance and long full-sync transactions. Do not infer a total sync
speedup from a selective lookup plan.

Recovery or rollback requires separate approval for the exact added index.
After confirming no active build and no constraint dependency, the appropriate
owner can drop one identified addition with `DROP INDEX CONCURRENTLY` outside a
transaction, then prepare again. Never drop existing AGE endpoint/parent
indexes, reset the graph, or delete the migration ledger row. Additive valid
indexes can remain when application code is rolled back; fence any incremental
consumer before removing its prerequisites.

Before deployment, require successful catalog checks, approved exact graph/data
audits, realistic before/after ID and both endpoint plans, and resource/latency
gates. Production DDL, cleanup, and deployment are distinct approvals; a script
flag is not user authorization. Older corpus seed manifests without the new
migration still fail strict restore compatibility: regenerate them with the
approved new image rather than weakening the check.

The commit history's edge labels, `BACKPATCH_OF` and `COMMITTED` (migration
`0014_commit_graph`), need no preparation: the new image's bootstrap creates
them empty, with their `id`, `start_id` and `end_id` indexes, in the same
transaction, and `--check` then passes.

## Commit history

The code tools read PostgreSQL's whole commit history ([reference](../docs/reference.md#commit-history), [the search design](../docs/search-design.md)), their only source: a deployment has no code to read until one is installed (`list_sources.history` shows it). Installing it, or rebuilding it from scratch, is the `history` job, given up to 4 vCPU, 8 GiB and 16 GiB of scratch disk, which runs `scripts/history-build.mjs`:

```sh
node ../scripts/aks.mjs job history | jq '{ok, error, build, verify: (.verify // {} | {ms, commits, files, mismatch_count, mismatches})}'
```

It fetches every branch and tag of `https://git.postgresql.org/git/postgresql.git` into a bare clone on the job's disk, builds the history into schema `pghacker_history`, and swaps it into `pghacker`. It then rebuilds every file of the branch heads and of 40 random commits and compares them with git, for the rest of the job's hour. `ok` is true when nothing differed.
- The code tools read it at once: `search`, `grep`, `read_file`, `list_files` and `blame` read any commit, and paths below `/branch/<name>/`, `/tag/<name>/` and `/commit/<rev>/`; `get_commit` and `search_commits` answer as `git show` and `git log`.
- The indexer's hourly `code_history_sync` job fetches new commits into its own clone and applies them. The clone is about 850 MB in `PGHACKER_GIT_DIR`, fetched again after a restart.
- The next `resolve_anchors` run resolves mail's file anchors to the history's paths.
- `PGHACKER_CODE_HISTORY` in `app_env` names another schema for the code tools to read; unset, `pghacker`.

Check `tools/list` (First deployment, step 10) and run the evaluation.

**Retired snapshots.** Migration `0016_retire_snapshots` drops the snapshots' tables (`code_snapshots`, `code_files`, `code_lines`) and their change capture, and cancels unfinished `code_snapshot` jobs. It records the graph keys of their files and of the history's, so that the incremental graph sync moves File and Subsystem vertices and `PART_OF` edges onto the history without a full sync: a File is `loaded` when the history has its path. Migration `0020_retire_version_sources` then deletes what their loader left: the 15 `postgres_version` rows in `sources` and its `code_snapshot` jobs, so `sources` lists mailing lists only.

**The graph.** An image with migration `0014_commit_graph` projects the installed history's commits into the graph ([the graph](../docs/reference.md#the-graph)): a `Commit` vertex for each of its commits, and their `MODIFIES`, `DISCUSSED_IN`, `BACKPATCH_OF` and `COMMITTED` edges. That's about seven elements a commit: several hundred thousand for PostgreSQL's history of over 100,000 commits.
- The upgrade asks for one full graph sync, which also re-keys mail's commit citations by their full SHA. So does each install.
- Full syncs, including the weekly reconcile, take longer accordingly: locally, the history's part took 20 s.
- The history's writers record their changes for the incremental sync: an hourly sync's few commits cost a batch of milliseconds.

### Rolling back past 0014

An image that has `0016` but predates `0014` reads the history only.
Give it patches' number column first ([Patch series](#patch-series)).
After rolling back to it, as `pgh_indexer`, cancel the jobs of kinds it
doesn't know, which would fail and retry, drop the graph changes of labels it
doesn't know, and ask for a full sync:

```sql
UPDATE pghacker.jobs SET state = 'cancelled'
WHERE kind IN ('duplicates', 'patch_series', 'rethread_mail') AND state IN ('queued', 'running');
DELETE FROM pghacker.graph_changes WHERE label IN ('BACKPATCH_OF', 'COMMITTED');
SELECT pghacker.graph_request_full('rolled back past 0014');
```

Its full sync drops those labels' counts and bookkeeping but leaves their
edges in AGE, until the next roll-forward's full sync projects them again.
It ignores the other added columns and tables: leave them, and the
migration records, in place.

While rolled back, the image stores new mail with `[email]` in place of
addresses, which no later image puts back. It also sets features' releases
in extraction order without dating them (`release_at`), so keep extraction
off (`PGHACKER_CRAWL=off`). After rolling forward, if it extracted, check
the releases it set or changed against their threads before extraction
resumes, and date those without `release_at` as `0018` did: the latest
`last_at` of the feature's threads. Then rename the development names it
saved ([Shipped release names](#shipped-release-names)).

## Custom domains

`front_door_custom_domain` is optional and defaults to `null`, which keeps the
`azurefd.net` URLs. To use your own domain, set it in your ignored environment
settings, never in the shared configuration or the committed example:

```hcl
front_door_custom_domain = "mcp.example.com"
```

This requests Front Door-managed TLS and makes that hostname the canonical
public and MCP URL. The domain gets a route of its own for the public paths
alone: the home page, its files, `/mcp` and `/robots.txt`. Front Door answers
404 for any other path there. The portal and its sign-in stay on the default
`azurefd.net` hostname, which keeps every path, so the OAuth callback
(`oauth_callback_url`) doesn't change, and oauth2-proxy never has to tell the
hosts apart. The app accepts both host names: Front Door sends the request's
own host on to the edge.

For a new domain, first create only its Front Door resource, so you can publish
the ownership record before changing the app's public URL:

```sh
terraform apply -var-file=../environments/local.tfvars \
  -target=azurerm_cdn_frontdoor_custom_domain.this
terraform output -json front_door_custom_domain_validation
```

Publish the output's `name` and `value` as a TXT record with your DNS provider.
The name is fully qualified; providers that append the zone name need only the
relative part (`_dnsauth` for an apex domain, `_dnsauth.mcp` for this example).
Wait for the domain's validation state in Front Door to become **Approved**.
Then apply the full configuration, which gives the domain its route and
updates the app's settings, and deploy them (`node ../scripts/aks.mjs apply`). The GitHub OAuth App's homepage can become `public_url`; its
callback stays `oauth_callback_url`.

A deployment whose portal was on the custom domain, before it had a route of
its own, moves the portal to `portal_url`. Add `oauth_callback_url` to the
GitHub OAuth App's callbacks (it takes several) before applying, and remove
the custom domain's callback afterwards.

Once the managed certificate has deployed, point a subdomain's CNAME to
`front_door_host_name`. An apex domain such as `example.com` instead needs
your provider's ANAME/ALIAS/CNAME flattening, or an Azure DNS A alias targeting
the Front Door endpoint resource. Do not use an ordinary apex CNAME or pin
Front Door's resolved IP addresses.

If the domain already exists in this profile, import it instead of creating
another one. The configuration names the domain resource by replacing dots
in the hostname with hyphens. With matching environment settings:

```sh
terraform import -var-file=../environments/local.tfvars \
  'azurerm_cdn_frontdoor_custom_domain.this[0]' \
  '<existing Front Door custom-domain resource ID>'
terraform plan -var-file=../environments/local.tfvars
```

Review the plan before applying; importing must not replace the existing domain
or certificate. DNS remains managed by your provider, outside this configuration.
For apex domains, monitor **Pending revalidation**: managed certificate renewal
can require a fresh TXT token, not just the original record.
[Azure's custom-domain and apex guidance](https://learn.microsoft.com/azure/frontdoor/apex-domain-onboard)
describes validation, DNS and certificate rotation.

## Production mode in use

Ask a **pghacker Crawler** session in the portal, for example:
- "Index pgsql-hackers since 2025-01 and follow it."
- "Is the code current?"

It confirms the scope of large requests, since a whole archive is decades of mail. It reports what `add_source` queued, and follows retrieval and extraction with `ingest_status`. For the code, it reports the commit history's head and last sync from `list_sources.history`: there is no version to add.

The crawl scheduler's progress is in the worker's log, with `[crawl]` lines. To stop extraction, set `app_env = { PGHACKER_CRAWL = "off" }`, apply, and deploy (`node ../scripts/aks.mjs apply`): the worker restarts without the scheduler. The other knobs are `crawl_start_sessions`, `crawl_max_sessions` and the Foundry capacities in `foundry_deployments`.

Extraction leases new mail as it arrives, for up to half of the threads, and fills in the rest of the archive from the newest thread back ([extraction](../docs/reference.md#extraction)). `ingest_status.extraction.lanes`, and the log's `[crawl] Backlog:` lines every 10 minutes, show both lanes. To tune them, set `PGHACKER_EXTRACTION_NEW_MAIL_SHARE` or `PGHACKER_EXTRACTION_NEW_MAIL_DAYS` in `app_env`. To lease the largest threads never extracted first instead, set `app_env = { PGHACKER_EXTRACTION_ORDER = "largest" }`, apply and deploy; the indexer restarts with it.

Of `app_env`'s settings, the Rust services get only the extraction order's, `PGHACKER_CODE_HISTORY` and general ones such as `RUST_LOG`.

## Evaluating the deployment

The 30-question evaluation runs local agents against the deployment's public endpoint, and checks their citations and cited commits there; questions q21 to q30 need the commit history. From the repository root:

```sh
npm run stop
PGHACKER_AGENT_MCP_URL="$(terraform -chdir=deploy/terraform output -raw mcp_url)" npm start   # detached, in another terminal
npm run eval -- --mcp-url "$(terraform -chdir=deploy/terraform output -raw mcp_url)" --concurrency 1 --results eval/RESULTS-azure.md
```

Concurrency 1 keeps the agents within the endpoint's per-client rate limit.

The review evaluation runs the same way. Its skill is the one the deployment serves, which the script checks against the digests `skills/get` gives:

```sh
npm run eval -- --mcp-url "$(terraform -chdir=deploy/terraform output -raw mcp_url)" --concurrency 1 \
    --questions eval/reviews.json --skill skill://review-as-author/SKILL.md --results eval/RESULTS-reviews.md
```

Without `--skill`, the agents, which don't declare the skills extension, must find `load_skill_review_as_author` on their own; write those results to `eval/RESULTS-reviews-tool.md`.

## Operations

`kubectl` commands run in the app's namespace: `kubectl --namespace pghacker ...`, after `node ../scripts/aks.mjs credentials`.

- **Logs.** `kubectl logs deployment/<name>` for the last ones; Log Analytics (`terraform output -raw log_analytics_workspace_id`) keeps them, in Container Insights' `ContainerLogV2` table, by `PodName` and `ContainerName`. The worker's lines are prefixed `[swarm]` and `[crawl]`.
- **Egress.** The firewall's verdicts are in `AZFWApplicationRule` and `AZFWNetworkRule`. The `<name_prefix>-swarm-denied-egress` alert fires when it denies anything from the PilotSwarm pool's subnet, and emails `alert_emails`: find what in those tables, by `SourceIp`. Run the `egress-check` job again after any change to the network.
- **Usage telemetry.**
  - Events that couldn't be sent are logged as `usage events weren't sent`, under `[mcp]` or `[home]`, at most once a minute.
  - Past `usage_daily_cap_gb` (5 GB) a day, Application Insights drops the rest of the day's events.
- **Restart.** `kubectl rollout restart deployment/<name>`. Restarting the worker drops live sessions' turns; extraction leases lapse after 20 minutes and their threads are extracted again.
- **Graph sync.** The indexer applies the changes that triggers capture in short batches ([the graph](../docs/reference.md#the-graph)).
  - Each batch that changes the graph logs `synced the graph` with its `mode`, `changes` and `ms`. `ingest_status` shows `pending_changes`, `oldest_pending_at` and the last full sync.
  - A full sync runs once after the upgrade that installs the change capture, after an upgrade that changes what the graph projects, after a restore or a commit-history install, weekly as a reconcile, and when asked for: `pghacker-indexer graph-reconcile`, or `SELECT pghacker.graph_request_full('why')` as `pgh_indexer`. A reconcile that changes the graph logs a warning: the change capture missed something, or changes were committed while it ran.
- **Secrets.**
  - The CSI driver syncs a new version of a vault secret into its Kubernetes Secret within 2 minutes, but pods read their environment when they start. Rotating the GitHub OAuth App's secret therefore means a new `github-client-secret` version, then `kubectl rollout restart deployment/edge`.
  - Replacing a role's `random_password` (`terraform apply -replace=...`) changes the vault secret. `node ../scripts/aks.mjs apply` then runs the bootstrap, which sets it on the role; restart the role's Deployments once the Secret has synced.
- **Limits.**
  - The worker has 2 vCPU and 6 GiB, and up to 8 GiB (`deploy/k8s/worker.yaml`), on a `swarm_pool` node of its own; each session runs a Copilot CLI process, and the scheduler holds back above 85% of the worker's memory. For more sessions, give `swarm_pool` a larger `vm_size` and raise the worker's limit.
  - HorizonDB has `horizondb_vcores` vCores per replica (4 by default).
  - The agents share pghacker-mcp's internal listener's rate limit, `mcp_internal_rate_per_minute` and `mcp_internal_rate_burst` (300 a minute, in bursts of 60): they all connect from the worker's pod. A call over it gets 429 with Retry-After, and usage telemetry records `McpRateLimited` on the `internal` endpoint, at most once a minute.
  - Extraction's pace is bounded by `gpt-6-luna`'s capacity in `foundry_deployments`, within the subscription's quota for the region.
  - The indexer's ephemeral disk, on its node, holds the commit history's clone (about 850 MB, once a history is installed) and no mbox files.
- **Cost.** Beyond the nodes, Azure Firewall Standard and Front Door Premium each have a fixed hourly charge, and the cluster's Standard tier (`aks_sku_tier`) one for its uptime SLA.
