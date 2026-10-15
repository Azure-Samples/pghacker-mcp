# Deploying pghacker-mcp to Azure

This deploys pghacker-mcp in production mode. Users index whatever they ask for, and the deployment starts with the local corpus. [TERRAFORM.md](TERRAFORM.md) documents the Terraform itself: variables, outputs, secrets and state.

```mermaid
flowchart LR
  U[MCP clients and browsers] --> AFD[Front Door Standard]
  AFD -->|Front Door's IPs only| CA
  subgraph VNet
    subgraph CA[Container App: one replica]
      P[oauth2-proxy :8080] -->|/mcp, anonymous| M[pghacker-mcp]
      P -->|/, anonymous| H[home page]
      P -->|/portal after GitHub sign-in| S[PilotSwarm portal and worker]
      S --> I[pghacker-indexer]
      S -->|agents, no rate limit| M2[pghacker-mcp internal]
      C[crawl scheduler] --> S
    end
    DB[(HorizonDB PG17: private endpoint)]
    F[(Foundry: private endpoint)]
  end
  CA --> DB
  CA --> F
```

- **`https://<front door>/mcp`** is the public MCP endpoint. It is anonymous and read-only, rate-limited per client (`X-Azure-ClientIP`, IPv6 by /64), and serves only requests that came through this Front Door profile (`X-Azure-FDID`).
- **`https://<front door>/`** is the public home page (`home/`): what pghacker-mcp is, the MCP URL (its own origin's `/mcp`) and how to add it to an agent, and example prompts with their answers. Its files are public; any other path needs sign-in. oauth2-proxy answers `/robots.txt` itself (`Disallow: /`).
- **`https://<front door>/portal/`** is the PilotSwarm portal. Users sign in with GitHub, and only members of your configured `github_org`/`github_team` (or `github_users`) get in. Members are PilotSwarm `user`s (`portal_role`).
  - Only Front Door's own host name serves it. A [custom domain](#custom-domains) gets the home page and `/mcp`, and Front Door answers 404 for its other paths. Sign-in therefore happens on one host, at one callback.
  - PilotSwarm 0.8.2 has no base path: its page loads at `/portal/`, but it asks for its files at `/assets/` and its API and WebSockets at `/api/`, so oauth2-proxy sends those to it too, behind sign-in. Its share links (`/?session=...`, `/?canvasShare=...`) reach the home page, which sends them on to `/portal/` on Front Door's host name (`portal_url`), whichever host they name. `scripts/lib/routes.mjs` holds the routes; [microsoft/PilotSwarm#179](https://github.com/microsoft/PilotSwarm/issues/179) asks for a base path, which would put all of the portal under `/portal/`.
  - oauth2-proxy tells the portal who signed in through headers, which PilotSwarm's `proxy` auth provider reads. PilotSwarm 0.8.2's browser app doesn't support that provider, so the image patches support in (`scripts/lib/portal-spa.mjs`).
  - The patch is pinned to the release's bundle and fails the build on any other: re-check it when upgrading PilotSwarm, and drop it once [microsoft/PilotSwarm#154](https://github.com/microsoft/PilotSwarm/issues/154) is fixed.
  - oauth2-proxy trusts no `X-Forwarded-*` header (`--reverse-proxy=false`). Front Door and the ingress pass a client's on, and with them, oauth2-proxy would match its public routes against a forged `X-Forwarded-Uri` and let anyone past sign-in ([oauth2-proxy/oauth2-proxy#3506](https://github.com/oauth2-proxy/oauth2-proxy/issues/3506)). Its callback is a fixed URL, so it needs none.
- **Models** come from Azure AI Foundry: `gpt-6.1-sol` is the portal's default, `gpt-6-luna` runs extraction and deduplication, and `text-embedding-3-small` does embeddings. The subscription's policy refuses keys, so the app calls them with its managed identity: a proxy in the supervisor adds an Entra token to each request (`scripts/lib/foundry.mjs`), and nothing in the container holds a key.
- **Production mode**:
  - `add_source` takes any list of the postgresql.org archive (chosen months, a range, or the whole archive, plus seed threads) and any branch, tag or commit of PostgreSQL.
    - A list added without months, `follow` included, queues the whole archive.
    - A version needs no loading: the code tools read any branch, tag or commit of the [commit history](#commit-history), and `add_source` for one says which commit it names, or starts the history's sync when it has none.
  - `follow` retrieves a list's current month again every hour.
  - The indexer's job workers have lanes: worker 0 runs the recurring jobs, and the others run anything.
  - The crawl scheduler keeps extraction sessions busy as long as threads wait, as fast as Foundry allows: it starts 8, adds one a minute while saturated, halves on a 429, and stops at `crawl_max_sessions`. It runs deduplication as extracts arrive.
  - There are no caps on scope or spend. Every thread of whatever users add gets extracted by `gpt-6-luna`.

## The container

`scripts/container.mjs` runs as root under tini. It starts the Foundry proxy on loopback and bootstraps the database (creating it, the roles, extensions, schema and graph, then migrating). It then runs each service as its own user, with an environment that lists only what it needs (`scripts/lib/azure.mjs`):

| Process | User | Gets |
| --- | --- | --- |
| oauth2-proxy | `pgh-proxy` | The GitHub OAuth App's ID and secret, and the cookie secret |
| The home page's server (`scripts/home.mjs`) | `pgh-home` | The built page, its port and the portal's origin (`PGHACKER_PORTAL_URL`), for share links; no secret |
| pghacker-indexer, pghacker-mcp | `pgh-svc` | Their database role (`pgh_indexer`, `pgh_reader`); embeddings through the Foundry proxy |
| PilotSwarm worker and portal, crawl scheduler | `pgh-agent` | `pgh_swarm`, which can create its own schemas but can't read the corpus; models through the Foundry proxy |

Only the bootstrap, which runs once before the services start, gets the administrator's password. No process gets the managed identity's endpoint: the proxy, in the supervisor, uses it, and forwards only to the Foundry account. Agents have PilotSwarm's built-in shell and read mail that anyone can post, so this separation is the containment: an agent's shell can't read another user's environment or files. The database and Foundry accept connections only from the VNet. The health server on 8081 serves the probes:
- `/healthz` fails after 15 minutes of failed bootstraps, or when a service keeps exiting.
- `/readyz` passes when bootstrap is done and oauth2-proxy, pghacker-mcp and the portal answer. It reports the home page too, but doesn't wait for it, so that `/mcp` stays up if the page fails.

## First deployment

You need the Azure CLI, signed in (`az login`) with Owner on the subscription, Terraform 1.9+, and Node 24 with the local app set up (for the corpus). From the repository root, copy the synthetic example:

```sh
cp deploy/environments/example.tfvars deploy/environments/local.tfvars
```

Replace its placeholder subscription ID, names, region, model capacities, and GitHub access settings with your own. Choose globally unique registry and vault names, and leave `create_app = false` until step 8. Real `.tfvars`, `.tfvars.json`, and `.backend.hcl` files are ignored; do not force-add them or commit secrets, state, or plans.

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

4. **Everything but the app.** This step creates:
   - the VNet, HorizonDB with its parameter group and private endpoint;
   - Foundry with its three deployments and private endpoint;
   - Key Vault and its secrets, the identity, the Container Apps environment, the probe job, and Front Door.

   ```sh
   terraform apply -var-file=../environments/local.tfvars
   ```

5. **The probe.** It bootstraps the database from inside the VNet. It then checks:
   - the extensions, and that BM25, vector, trigram and Cypher queries run as `pgh_reader`;
   - the managed identity's token, and each Foundry deployment through the same proxy as the app's;
   - the archive, git.postgresql.org and GitHub.

   ```sh
   ../scripts/run-job.sh probe | jq .
   ```

   Fix what `failures` lists, then run the probe again until `ok` is true.

6. **The corpus.** Dump the local corpus (with `npm start` running locally), build its seed image from the app image, and restore it before the app exists:

   ```sh
   npm run corpus:dump                             # from the repository root: .runtime/corpus/<id>/
   acr=$(terraform -chdir=deploy/terraform output -raw acr_name)
   cd .runtime/corpus/<id>
   az acr build --registry "$acr" --platform linux/amd64 --image pghacker-seed:<id> \
     --build-arg "APP_IMAGE=$acr.azurecr.io/pghacker-mcp@sha256:<digest>" .
   ```

   Set `seed_image = "<your-registry>.azurecr.io/pghacker-seed:<id>"`, apply (this creates the restore job), and run it:

   ```sh
   terraform apply -var-file=../environments/local.tfvars
   ../scripts/run-job.sh restore | jq '{ok, corpus, rows}'
   ```

   The restore checks that the dump's checksum, migrations and embedding model match. It refuses a database that already has a corpus unless run with `--replace`, and refuses while the indexer is connected. It loads schema `pghacker` and checks every table's row count. The app rebuilds the graph from the tables when it starts.

7. **The GitHub OAuth App.**
   - Register it under your GitHub organization's **Settings > Developer settings > OAuth Apps** (or under your account), with:
     - homepage `terraform output -raw public_url`
     - callback `terraform output -raw oauth_callback_url`: oauth2-proxy's `/oauth2/callback` on Front Door's own host name, which a custom domain doesn't change. Turn off the callback's wildcard matching, which GitHub turns on for an app with a single callback: it would also accept the callback's subdomains and sub-paths, and oauth2-proxy sends exactly this URL.
   - Some organizations restrict OAuth Apps. Until an org owner approves this one, GitHub hides org and team membership from it, and every sign-in fails with a 500 (the logs say `user is missing required organization`).
     - To ask for approval, open `https://github.com/settings/connections/applications/<client ID>` and click **Request** next to your organization.
     - Until then, `github_users` (a list of logins) lets those users in. It doesn't follow the team: once the app is approved, keep only the logins that aren't on the team.
   - Store its credentials (TERRAFORM.md, "Key Vault secrets"):

   ```sh
   vault=$(terraform output -raw key_vault_name)
   az keyvault secret set --vault-name "$vault" --name github-client-id --value "<client ID>" --output none
   read -rsp 'Client secret: ' secret && echo
   az keyvault secret set --vault-name "$vault" --name github-client-secret --value "$secret" --output none && unset secret
   ```

8. **The app.** Set `create_app = true` and apply. From then on, never set it back to `false`, which destroys the app.

9. **Check it.** Front Door can take a few minutes to route a new origin.

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

   Then open `public_url`, the home page, without signing in, and `portal_url`: GitHub asks you to sign in, and a team member reaches the portal; anyone else gets 403. In a pghacker Crawler session, `ingest_status` shows the restored corpus and the graph's vertices and edges.

10. **The code.** Unless the corpus brought a commit history, build one ([Commit history](#commit-history)): the code tools have no other source.

## Updates

Build and push a new image (step 3), set its digest as `image`, and apply. Container Apps starts the new revision before it stops the old. The two overlap briefly: the new indexer serves its tools at once, but runs jobs only once the old one has stopped.

### Schema upgrades

Startup applies the migrations a database lacks, and refuses one that changed after it was applied.

| Migration | Adds |
| --- | --- |
| `0013_duplicate_candidates` | `find_duplicates`' pairs, found as extracts arrive |
| `0014_commit_graph` | The commit history in the graph |
| `0015_restore_addresses` | `restore_addresses()` ([Email addresses](#email-addresses)) |
| `0016_retire_snapshots` | Drops the snapshots ([Commit history](#commit-history)) |
| `0017_extraction_lease_versions` | The thread versions each extraction lease read |
| `0018_feature_release_at` | When a feature's release was stated |

Production applied `0016` before `0013`–`0015`. No image that reads the snapshots runs after `0016`; the previous production image, `33616a4`, reads the history only and runs on the schema through `0018` ([Rolling back to 33616a4](#rolling-back-to-33616a4)).

### Email addresses

An image with migration `0015_restore_addresses` stores mail's email addresses as written: subjects, bodies, patches and senders' names. Mail stored before it has `[email]` in their place, and senders' names without them, until restored. After deploying it, as `pgh_indexer` from the operator environment (Graph index preparation):

```sql
SELECT * FROM pghacker.restore_addresses();
```

- It queues a `restore_addresses` job per stored month, which downloads the month again, and one per 200 messages from thread pages, which are fetched one by one at the archive's rate (2 requests a second). `ingest_status` shows the jobs.
- They change only text stored exactly as ingest once made it of the archive's, and count the rest as `differs`. Changed chunks are embedded again through Foundry, changed patches are indexed again by the `patch_lines` job, and the graph follows.
- Running it again restores whatever is left; a job that's queued or running is left as it is.

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

The code tools read PostgreSQL's whole commit history ([reference](../docs/reference.md#commit-history), [the search design](../docs/search-design.md)), their only source: a deployment has no code to read until one is installed (`list_sources.history` shows it). Installing it, or rebuilding it from scratch, is a one-off execution of the probe job, given 4 vCPU and 8 GiB, that runs `scripts/history-build.mjs`:

```sh
../scripts/run-job.sh history | jq '{ok, error, build, verify: (.verify // {} | {ms, commits, files, mismatch_count, mismatches})}'
```

It fetches every branch and tag of `https://git.postgresql.org/git/postgresql.git` into a bare clone on the job's disk, builds the history into schema `pghacker_history`, and swaps it into `pghacker`. It then rebuilds every file of the branch heads and of 40 random commits and compares them with git, for the rest of the job's hour. `ok` is true when nothing differed.
- The code tools read it at once: `search`, `grep`, `read_file`, `list_files` and `blame` read any commit, and paths below `/branch/<name>/`, `/tag/<name>/` and `/commit/<rev>/`; `get_commit` and `search_commits` answer as `git show` and `git log`.
- The indexer's hourly `code_history_sync` job fetches new commits into its own clone and applies them. The clone is about 850 MB in `PGHACKER_GIT_DIR`, fetched again after a restart.
- The next `resolve_anchors` run resolves mail's file anchors to the history's paths.
- `PGHACKER_CODE_HISTORY` in `app_env` names another schema for the code tools to read; unset, `pghacker`.

Check `tools/list` (First deployment, step 9) and run the evaluation.

**Retired snapshots.** Migration `0016_retire_snapshots` drops the snapshots' tables (`code_snapshots`, `code_files`, `code_lines`) and their change capture, and cancels unfinished `code_snapshot` jobs. It records the graph keys of their files and of the history's, so that the incremental graph sync moves File and Subsystem vertices and `PART_OF` edges onto the history without a full sync: a File is `loaded` when the history has its path.

**The graph.** An image with migration `0014_commit_graph` projects the installed history's commits into the graph ([the graph](../docs/reference.md#the-graph)): a `Commit` vertex for each of its commits, and their `MODIFIES`, `DISCUSSED_IN`, `BACKPATCH_OF` and `COMMITTED` edges. With production's 105,545 commits, that's about 730,000 elements, 18% more graph.
- The upgrade asks for one full graph sync, which also re-keys mail's commit citations by their full SHA. So does each install.
- Full syncs, including the weekly reconcile, take longer accordingly: locally, the history's part took 20 s.
- The history's writers record their changes for the incremental sync: an hourly sync's few commits cost a batch of milliseconds.

### Rolling back to 33616a4

`33616a4`, the previous production image, reads the history only and
predates `0014`. After rolling back to it, as `pgh_indexer`, cancel the jobs
of kinds it doesn't know, which would fail and retry, drop the graph changes
of labels it doesn't know, and ask for a full sync:

```sql
UPDATE pghacker.jobs SET state = 'cancelled'
WHERE kind IN ('duplicates', 'restore_addresses') AND state IN ('queued', 'running');
DELETE FROM pghacker.graph_changes WHERE label IN ('BACKPATCH_OF', 'COMMITTED');
SELECT pghacker.graph_request_full('rolled back past 0014');
```

Its full sync drops those labels' counts and bookkeeping but leaves their
edges in AGE, until the next roll-forward's full sync projects them again.
It ignores the other added columns and tables: leave them, and the
migration records, in place.

While rolled back, the image stores new mail with `[email]` in place of
addresses, and sets features' releases in extraction order without dating
them (`release_at`), so keep extraction off (`PGHACKER_CRAWL=off`). After
rolling forward, run `SELECT * FROM pghacker.restore_addresses();`. If it
extracted, check the releases it set or changed against their threads
before extraction resumes, and date those without `release_at` as `0018`
did: the latest `last_at` of the feature's threads.

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
hosts apart. The app accepts both hostnames and the origin hostname; the
origin host header and certificate checks do not change.

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
updates the app. The GitHub OAuth App's homepage can become `public_url`; its
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
- "Add the PG18 source."

It confirms the scope of large requests, since a whole archive is decades of mail. It reports what `add_source` queued, and follows retrieval and extraction with `ingest_status`.

The crawl scheduler's progress is in the container's log, with `[crawl]` lines. To stop extraction, set `app_env = { PGHACKER_CRAWL = "off" }` and apply. The other knobs are `crawl_start_sessions`, `crawl_max_sessions` and the Foundry capacities in `foundry_deployments`.

Extraction leases new mail as it arrives, for up to half of the threads, and fills in the rest of the archive from the newest thread back ([extraction](../docs/reference.md#extraction)). `ingest_status.extraction.lanes`, and the log's `[crawl] Backlog:` lines every 10 minutes, show both lanes. To tune them, set `PGHACKER_EXTRACTION_NEW_MAIL_SHARE` or `PGHACKER_EXTRACTION_NEW_MAIL_DAYS` in `app_env`. To lease the largest threads never extracted first instead, set `app_env = { PGHACKER_EXTRACTION_ORDER = "largest" }` and apply; the app restarts with it.

Of `app_env`'s settings, the Rust services get only the extraction order's, `PGHACKER_CODE_HISTORY` and general ones such as `RUST_LOG`.

## Evaluating the deployment

The 30-question evaluation runs local agents against the deployment's public endpoint, and checks their citations and cited commits there; questions q21 to q30 need the commit history. From the repository root:

```sh
npm run stop
PGHACKER_AGENT_MCP_URL="$(terraform -chdir=deploy/terraform output -raw mcp_url)" npm start   # detached, in another terminal
npm run eval -- --mcp-url "$(terraform -chdir=deploy/terraform output -raw mcp_url)" --concurrency 1 --results eval/RESULTS-azure.md
```

Concurrency 1 keeps the agents within the endpoint's per-client rate limit.

## Operations

- **Logs.** Log Analytics (`terraform output -raw log_analytics_workspace_id`), table `ContainerAppConsoleLogs_CL`. Each line is prefixed with its process: `[supervisor]`, `[indexer]`, `[mcp]`, `[swarm]`, `[crawl]`, `[home]`, `[oauth2-proxy]`.
- **Restart.** `az containerapp revision restart` on the active revision. A restart drops live portal sessions' turns; extraction leases lapse after 20 minutes and their threads are extracted again.
- **Graph sync.** The indexer applies the changes that triggers capture in short batches ([the graph](../docs/reference.md#the-graph)).
  - Each batch that changes the graph logs `synced the graph` with its `mode`, `changes` and `ms`. `ingest_status` shows `pending_changes`, `oldest_pending_at` and the last full sync.
  - A full sync runs once after the upgrade that installs the change capture, after an upgrade that changes what the graph projects, after a restore or a commit-history install, weekly as a reconcile, and when asked for: `pghacker-indexer graph-reconcile`, or `SELECT pghacker.graph_request_full('why')` as `pgh_indexer`. A reconcile that changes the graph logs a warning: the change capture missed something, or changes were committed while it ran.
- **Secrets.**
  - Rotating the GitHub OAuth App's secret means a new `github-client-secret` version and then a restart.
  - Replacing a role's `random_password` (`terraform apply -replace=...`) changes the vault secret. The next bootstrap sets it on the role.
- **Front Door's ranges** change now and then. Every apply reads the `AzureFrontDoor.Backend` tag again.
- **Limits.**
  - The app has 4 vCPU and 8 GiB of memory; each extraction session runs a Copilot CLI process, and the scheduler holds back above 85% memory use.
  - HorizonDB has 32 vCores (`horizondb_vcores`).
  - `gpt-6-luna` has the subscription's whole 1M tokens per minute in westus3.
  - The ephemeral disk holds the commit history's clone (about 850 MB, once a history is installed) and no mbox files.
