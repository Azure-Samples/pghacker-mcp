# pghacker-mcp design

*October, 2026 | Maxim Lukiyanov | Azure PostgreSQL*

The waves and test subsets below are the original plan; the [README](../README.md),
[reference](reference.md) and [search design](search-design.md) describe the system as built.

## Why pghacker-mcp

PostgreSQL's design history lives in about 600K pgsql-hackers messages and the source tree. Asked why a feature works as it does, what happened to SQL/PGQ, or how a committer would review a patch, agents search it blindly, taking minutes and missing evidence. pghacker-mcp indexes both once and answers in seconds, with citations, for PostgreSQL developers and builders.

## Scope

| Wave | Scope |
| --- | --- |
| **1** | Local PilotSwarm: pgsql-hackers slices become threads, authors, features, decisions, positions, anchors and an AGE graph, with full-text, semantic and hybrid search, `author_perspective` and code search over a PG19 subset. Done when code search matches ripgrep on replayed agent searches and 20 test questions get correct, cited answers. |
| **2** | Commits and diffs for the PG19 subset: history tools, diff search, `Discussion:` links to threads. |
| **3** | Mort: crawler sessions, both containers, public endpoint, full PG19 and archive, hourly refresh. |
| **4** | Rest of the Loop: patches, commit and PR graph data, feature trends, semantic code search, welcome page, telemetry. Out of scope: CommitFest site, docs, other lists, MCP App UI. |

## Architecture

![pghacker-mcp architecture](assets/pghacker-architecture.svg)

- **pghacker-mcp:** async Rust (tokio, axum, `rmcp`), stateless, public without sign-in, rate-limited per IP. No LLM calls beyond query embeddings; read-only role with a 10-second timeout, which code searches replace with their own budgets.
- **pghacker-indexer:** async Rust (tokio, `tokio-postgres`, `mail-parser`, `gix`), internal. Async workers fetch, parse and embed sources and build the graph; MCP tools serve crawler sessions.
- **Crawler sessions:** a `pghacker-crawler` package on PilotSwarm's generic crawler. A supervisor picks sources; a model (`gpt-6-luna`) extracts and deduplicates.

Both services use HorizonDB directly: PilotSwarm facts and graph API can't run pg\_trgm, BM25 or Cypher. Waves 1–2 run on the local starter plus AGE, pgvector, pg\_textsearch.

## Data model

Tables in schema `pghacker` are authoritative. Messages and extracts support three search modes: `fulltext` ranks by BM25 (pg_textsearch), `semantic` by pgvector similarity, and `hybrid` fuses both rankings. pg_trgm indexes names, paths, code and diffs.

**Extracts** cite Message-IDs and verbatim quotes: **features** (proposal to commit, revert or rejection; release), **decisions** (outcome, alternatives, rationale) and **positions** (an author's stance in a thread).

**Graph.** The AGE graph `pghacker_graph` adapts the `postgres-hackers/v2` node types:

| Source | Edges |
| --- | --- |
| **Mail** | `AUTHORED` Author → Message, `REPLIES_TO` Message → Message, `IN_THREAD` Message → Thread |
| **Anchors** | `MENTIONS` Message → File, Commit, CommitFestEntry or Release; `PART_OF` File → Subsystem |
| **Extracts** | `DISCUSSES` Thread → Feature, `DECIDES` Decision → Feature, `TARGETS` Feature → Release, `IMPLEMENTS` File → Feature, `HOLDS` Author → Position, `ABOUT` Position → Feature or Decision, `EVIDENCED_BY` extract → Message |
| **git history** | `MODIFIES` Commit → File, `DISCUSSED_IN` Commit → Thread, `BACKPATCH_OF` other group member → first Commit, `COMMITTED` Author → Commit (the one matching the commit's author name) |

**Anchors.** PostgreSQL has no pull requests: a PR maps to a CommitFest entry or patch series, and a comment to a Message-ID and quote. A file anchor is a path from the source root, which resolves to `/branch/<name>/<path>` or `/commit/<sha>/<path>` in the commit history; an abbreviated commit SHA resolves to the full one.

## Ingestion

Deterministic code handles verbatim facts and models only judgments; the agent-built v2 graph needed 127 recovery batches for two months of mail. Crawler sessions use these indexer tools:

- **Sources:** `add_source`, `remove_source` and `list_sources` pick mailing lists: the test slices locally, any list, months and threads in Azure. Async workers retrieve them (monthly mbox, flat thread pages), thread mail, match authors by email, record patches, chunk and embed; a followed list's current month is retrieved again hourly. PostgreSQL's code isn't a source: an operator builds the commit history, an hourly job syncs it, and `list_sources` and `ingest_status` report it.
- **Email addresses:** subjects, bodies, patches and senders' names keep their addresses, which the tools and the graph show; `author_emails`, which matches senders to authors, isn't readable by pghacker-mcp or the graph.
- **Progress:** `ingest_status` reports what each source has retrieved, parsed and extracted.
- **Extraction:** `next_extraction_batch` leases threads never extracted, or changed since, with messages, authors and anchors, by default from two lanes: new mail (the last 7 days, oldest first, up to half the leases while both lanes have threads) and the backfill (newest first). Mail that arrives during a lease leaves its thread to extract again. Before submitting, the model deduplicates: `match_extracts` finds existing features and decisions like each draft, and the model reuses one or declares a new one. `submit_extracts` rejects unreviewed near-duplicates, citations outside the thread and inexact quotes. A feature's release is the one major version it shipped in, or targets until it's committed, never a branch it was back-ported to. Each extraction that gives one replaces it, and the indexer saves only one major version.
- **Repair:** `find_duplicates` lists the likely duplicates the indexer finds as extracts arrive, and `mark_distinct` records pairs that differ; `merge_authors` and `merge_extracts` make logged merges that `undo_merge` reverses.

## MCP tools

**Mail and graph:** `get_thread`, `search_authors`, `get_author`, `get_ontology`, and three tools that take `mode` (`fulltext`, `semantic` or `hybrid`, the default): `search_threads`, `search_extracts` and `author_perspective` (cited positions on a topic).

**SQL:** `query`: after a `sqlparser` check (one statement of at most 64 KiB, no transaction control), it runs the SQL, AGE `cypher()` included, in a rolled-back read-only transaction with a 10-second timeout, and returns up to `max_rows` rows (100 by default, at most 1,000) and 1 MiB as CSV. Getting a connection takes at most 5 seconds, and the whole call 20. It reads results in PostgreSQL's text format, through a vendored `tokio-postgres` fork (`bind_text`).

**Code search** has a first-class `search` plus a `grep` tool.

| `search` parameters | Values (grep equivalent) |
| --- | --- |
| **query, match** | Text, regex or name, or a list meaning any; `match` sets which (`-e`, `-E`) |
| **version, history** | A commit (default: the newest release branch's head), or with `history` a range of commits: `all`, a revision and its ancestors, or `A..B` |
| **case_sensitive, whole_word** | Case-sensitive by default; identifiers ignore case (`-i`, `-w`) |
| **paths, include, exclude, also, not** | Files, folders and globs (with `history`, globs only in include and exclude); patterns a line must or must not also match (`\| grep`, `-v`) |
| **output, group_by, context** | Lines, files or counts, per file or match, with context (`-l`, `-c`, `-C`); `history` gives lines only |
| **limit, offset, order, budget_ms** | A page with totals, path or coverage order, and a time budget (`head`, `-m`) |

`search`, `grep`, `search_patches` and `search_commits` take `budget_ms`: 10
seconds by default, at most 15. `search` and `grep` read at most 200,000
candidate lines; past the budget or the cap, `search` totals are lower bounds,
and `grep` fails rather than print partial counts. The
[reference](reference.md#limits) lists each tool's limits.

Results add totals, cut and budget flags, and similar names when nothing matches at a commit. `grep` takes grep command lines with their folder: flags such as `-E`, `-F` and `-P` set the dialect, and trailing `head`, `tail`, `wc -l`, `sort | uniq -c`, `cut -c` and `grep` stages run on its output. `read_file` and `list_files` read files; `blame`, `get_commit` and `search_commits` answer as git blame, show and log.

## Test data

| Wave | Subset and purpose |
| --- | --- |
| **1** | June–July 2026 mbox (≈5K messages, 190 MB), to compare with the v2 graph |
| **1** | SQL/PGQ and SQL/JSON threads from archive search: reverted and uncommitted work |
| **1** | `src/backend/{parser,utils/adt}`, `src/include/nodes` and `contrib/pg_trgm` at a pinned `REL_19_STABLE` SHA: 206 files with the SQL/JSON code |
| **2** | The 540 of 3,662 PG19 commits since 2025-07-01 that touch that subset |
| **3** | The full tree (7,652 files) and archive (≈600K messages) |

## Risks

- **Extensions:** HorizonDB must host AGE, pgvector, pg_trgm and pg_textsearch 1.5+.
- **Public SQL:** if grants prove weak, use a separate database or allow only Cypher templates.
- **Quality and scale:** tune dedup, threading and identity early; 1–2M chunk vectors need 6–12 GB.

### References

- [PilotSwarm](https://github.com/microsoft/PilotSwarm) and its [pgsql-hackers crawler spec](https://github.com/microsoft/PilotSwarm/blob/main/docs/proposals/enhancedfactstore/11-pgsql-hackers-sample-crawler.md).
