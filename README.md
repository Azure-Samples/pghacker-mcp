# pghacker-mcp

pghacker-mcp answers questions about PostgreSQL's design history and source code, with citations. Ask why a feature works as it does, or what happened to SQL/PGQ, and an agent finds the pgsql-hackers threads, commits and code that answer it, and quotes them.

The project is powered by PostgreSQL itself. The mail, the graph and PostgreSQL's full commit history, live in [Azure HorizonDB](https://azure.microsoft.com/products/horizondb) (PostgreSQL when run locally), and PostgreSQL indexes do the searching: trigram for code and text, vector for semantic search, BM25 for full-text search, and B-trees over the history's commit ranges, with Apache AGE for the graph. Typical searches across the whole commit history return in milliseconds.

It is a [PilotSwarm](https://github.com/microsoft/PilotSwarm) 0.8.2 app in three parts:

- **pghacker-mcp**: a public, read-only [MCP](https://modelcontextprotocol.io) (Model Context Protocol) server, in Rust, that serves the mail and the code to agents.
- **pghacker-indexer**: an internal Rust service that retrieves the mail and PostgreSQL's commit history, indexes them and projects them into a graph. Its MCP tools serve the crawler agents.
- **Crawler agents**: PilotSwarm sessions that read the threads and record the features, decisions and authors' positions in them, with verbatim quotes.

## Quick start

You need Node.js 24 or later, Rust (the toolchain in `rust-toolchain.toml`), and Docker with Docker Compose, on macOS or Linux.

```bash
npm run setup   # download PilotSwarm, install dependencies, build, and create .env
npm start       # start PostgreSQL, both services, PilotSwarm's worker and portal, and the home page
```

| What | Address |
| --- | --- |
| The home page: the MCP URL, how to add it to an agent, and examples | http://127.0.0.1:3001 |
| The portal, to chat with the agents | http://127.0.0.1:3001/portal/ |
| pghacker-mcp, for your MCP client | http://127.0.0.1:3102/mcp, or http://127.0.0.1:3001/mcp |
| pghacker-indexer, for the crawler agents | http://127.0.0.1:3101/mcp |
| PostgreSQL 17 | 127.0.0.1:55433 |

Everything listens on 127.0.0.1 only, and the portal has no sign-in, so **don't expose it**. `npm run stop` stops it all and keeps the data.

The agents need a language model: set `GITHUB_TOKEN` in your shell for GitHub Copilot's models, or add another provider in the portal's Admin Console. For semantic search, set the embedding settings, `HORIZON_EMBED_*`, in `.env` (`.env.example` shows them); without them, the mail searches use full text only. The [reference](docs/reference.md#configuration) lists every setting.

## Load the data

Locally, pghacker-mcp works on a slice of the data: two months of pgsql-hackers (June–July 2026), whole threads on SQL/PGQ and SQL/JSON, and PostgreSQL's commit history.

1. **Mail.** In the portal, ask a **pghacker Crawler** session to add pgsql-hackers (locally, the slice above), or call the indexer's `add_source` with `{kind: mailing_list, name: pgsql-hackers}`. The indexer downloads the mail, embeds it and adds it to the graph; `ingest_status` shows its progress.
2. **Code.** Build the commit history once, from a clone. It takes about 6 minutes; add `--path src/backend/parser` to keep only that folder.

   ```bash
   git clone --mirror https://git.postgresql.org/git/postgresql.git .runtime/postgres.git
   export $(grep ^DATABASE_URL= .env)   # the build runs as the database's administrator
   cargo run --release -p pghacker-indexer -- history build --git-dir .runtime/postgres.git \
       --url https://git.postgresql.org/git/postgresql.git --install
   ```

   The running services read it at once, without a restart, and the indexer applies new commits every hour.
3. **Extraction.** Crawler agents read the threads:

   ```bash
   npm run crawl -- --pilot              # 10 threads in one session, to review first
   npm run crawl -- --concurrency 8      # then the rest, 8 sessions at a time
   npm run crawl -- --dedup              # then one session that merges duplicates
   ```

## Use it

Connect any MCP client to `http://127.0.0.1:3102/mcp`, or ask a **pghacker Evaluator** session in the portal. Answers cite Message-IDs, verbatim quotes, commit SHAs and file paths.

| Area | pghacker-mcp's tools |
| --- | --- |
| Mail | `search_threads`, `get_thread`, `search_extracts`, `author_perspective`, `search_authors`, `get_author` |
| Code | `search`, `grep`, `read_file`, `list_files` and `blame`, at any commit; `get_commit` and `search_commits`, like `git show` and `git log`; `search_patches`, in the patches posted to the list |
| SQL and graph | `get_ontology` describes the data; `query` runs one read-only SQL statement, Cypher included |

It also serves a skill, [`review-as-author`](skills/review-as-author/SKILL.md): a review of a patch, commit or design as a given pgsql-hackers author would give it (Tom Lane, say), each point quoting that author's own messages. Clients that support MCP's skills extension find it at `skill://review-as-author/SKILL.md`. Elsewhere, attach that resource, or install the folder as a skill in your agent; the [reference](docs/reference.md#skills) explains.

## Check it

```bash
npm test             # the launcher, configuration, readiness and evaluation helpers
npm run rust:check   # rustfmt, clippy and the Rust tests
npm run smoke        # with the app running: both servers and their skills, the portal, the worker and the agents
npm run eval         # with the data loaded: 30 questions (eval/questions.json), into eval/RESULTS.md
npm run eval -- --questions eval/reviews.json --skill skills/review-as-author/SKILL.md --results eval/RESULTS-reviews.md
```

The last reviews 10 patches as their actual reviewers would have, with the `review-as-author` skill; without `--skill`, it gives the baseline.

## Deploy to Azure

[deploy/README.md](deploy/README.md) deploys the app to Azure Container Apps with Terraform, starting from the local corpus. There, the home page and `/mcp` are public, the portal, at `/portal/` on Front Door's own host name, requires GitHub sign-in, and the indexer takes whatever sources its users ask for.

## Privacy and telemetry

A deployment made with this repository's Terraform records usage telemetry in its own Application Insights: for each request to `/mcp`, the tool, its arguments' names and the shape of its answer (or the skill file read), the client, and an approximate location; and each view of the home page. What you type into tools, such as SQL and search terms, is recorded only if the deployment sets `usage_details_enabled = true`. Now and then, as you start a new question in an interactive client such as GitHub Copilot, it also asks through the client's own form whether the last answer helped, and records your choice: the one thing pghacker-mcp writes, and only to telemetry, never to the data it serves. Your request runs whatever you choose. Add `?feedback=off` to the MCP URL never to be asked. To turn feedback off, set `feedback_rate_per_hour = 0`, or to turn off all telemetry, `usage_enabled = false`, and run `terraform apply`. [PRIVACY.md](PRIVACY.md) has the notice.

## Learn more

- [Reference](docs/reference.md): settings, tools and their limits, the commit history, the graph, extraction, tests and the repository's layout.
- [Design](docs/design.md): why pghacker-mcp exists, and its plan.
- [Search design](docs/search-design.md): how the code tools read every commit.
- [Deployment](deploy/README.md): Azure, upgrades and operations.

## Contributing

This project welcomes contributions and suggestions. Most contributions require you to agree to a
Contributor License Agreement (CLA) declaring that you have the right to, and actually do, grant us
the rights to use your contribution. For details, visit [Contributor License Agreements](https://cla.opensource.microsoft.com).

When you submit a pull request, a CLA bot will automatically determine whether you need to provide
a CLA and decorate the PR appropriately (e.g., status check, comment). Simply follow the instructions
provided by the bot. You will only need to do this once across all repos using our CLA.

[CONTRIBUTING.md](CONTRIBUTING.md) explains how to build, check and propose a change, and [SUPPORT.md](SUPPORT.md) how to get help.

## Code of conduct

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or
contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## Security

**Please do not report security vulnerabilities through public GitHub issues.** To report one, follow [SECURITY.md](SECURITY.md), which points to Microsoft's reporting guidance for its repositories at [https://aka.ms/SECURITY.md](https://aka.ms/SECURITY.md).

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft
trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship.
Any use of third-party trademarks or logos are subject to those third-party's policies.

## License

pghacker-mcp is licensed under the [MIT License](LICENSE). [NOTICE](NOTICE) lists the third-party code in this repository and its license.
