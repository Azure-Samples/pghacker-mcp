# pghacker-mcp search design

*October, 2026 | Maxim Lukiyanov | Azure PostgreSQL*

## Why the commit history

Agents ask about code at any point in time: how a function looked in 9.6, who wrote a line and where it was argued, whether a fix reached the back branches. Code search originally read 15 snapshots, the heads of master and 14 release branches loaded in production (48M lines, 5.9 GB with indexes), and answered only there. The commit history stores every line of every commit of every branch once, in less space (5.7 GB with indexes, 105,442 commits), and serves the code tools at any commit, plus blame, log and show. It replaced the snapshots once it matched them ([Validation](#validation)); migration `0016_retire_snapshots` dropped them. Patch search covers the other half of the record: the patches posted to pgsql-hackers.

## Data model

**Addresses.** Each commit gets an int8 address. Master's commits, counted from 0, are *k*·2³². The *n*-th branch (from 0) forked at master commit *f* gets its own block of 2²⁴ addresses from *f*·2³² + *n*·2²⁴ + 1 and numbers its commits consecutively in it: up to 255 branches per fork point and 16M commits per branch. So the descendants of a commit at address *a* are one interval: [*a*, ∞) on master, *a* to its block's end on a branch. Addresses never change, as history only appends. PostgreSQL's history is a tree (no merges; branches fork from master); the builder refuses anything else.

**Line instances (the weave).** One row per line instance; a changed line is a new instance, as in `git blame`. An instance is alive at the commits whose addresses lie in [`lo`, `hi`), except where `alive` has holes. That multirange is set only for the 17% of instances that one branch deleted while others kept them:

```sql
lo <= X AND X < hi AND (alive IS NULL OR alive @> X)
```

Within a file's lineage (its identity across renames at git's 50% similarity; not copies), the `weave` key orders every line it ever had consistently with every version. A file version's lines are the lineage's instances alive at its birth (the commit that made it), in weave order; line numbers come from cached weave keys. When a hunk adds as many lines as it removes, each new instance records the one it replaced ("one-for-one").

| Table | Rows | Holds |
| --- | --- | --- |
| `code_line_instances` | 24.1M | Text, lineage, weave key, alive range, line replaced one-for-one |
| `code_file_versions` | 436K | Path, blob, alive range, flags; one per content change or rename |
| `code_commits` | 105K | Metadata, message, parsed trailers, patch id, back-patch group |
| `code_commit_files` | 445K | Each changed file as `git show --stat` counts it |

Text that isn't UTF-8 is binary to the tools: searches at a commit skip it, and `read_file` and `blame` refuse it. Without a NUL byte, its lines are kept all the same (read as Latin-1, re-encoded exactly), so history searches, blame of a later UTF-8 version and diffs follow them back; lines whose bytes changed are new instances.

## Indexes and queries

`search`, `grep`, `search_patches` and `search_commits` take `budget_ms`: 10
seconds by default, at most 15. Each sets the statement timeout for its
queries and restores the previous setting before returning, errors included.
The budget is checked between steps, so the step in progress can run past
it. `query` keeps the reader's 10-second timeout, with 5 seconds to get a
connection and 20 for the whole call. The README lists the [other
limits](../README.md#code-search-budgets-and-large-archives).

A trigram GIN index on `line` finds candidates; a btree on (`lineage`, `hi`, `lo`) scopes them to files. `alive` is only ever a filter: GiST scans of multiranges can miss rows on PostgreSQL 14 and later (reported on pgsql-hackers in August 2026, not yet fixed). The engine chooses among three plans:

- **One commit:** the trigram scan and `alive_at(commit)` together; rows map to versions through a cached per-commit map.
- **Scope first:** read the scoped versions' lines, then apply the text filters, when the files searched hold 200,000 lines or fewer or the pattern has no trigrams.
- **Mounted trees:** each line checked at its own version's birth, for paths under the virtual folders below.

`OFFSET 0` fences keep the planner from repeating the trigram scan per version. Trigram scans take the scope's lineages as a bound array, which PostgreSQL hashes and can AND with the btree; an array built by a subquery is compared member by member on every row rechecked, which made whole-tree searches for common terms take over 10 seconds. Past 200,000 candidate lines (a separate cap), a search at one commit answers from the first files in tree order, and a history search from the candidates it read, sorted by path; either says its totals are lower bounds. An incomplete `grep` fails rather than print partial counts.

## Revisions and virtual folders

`version` takes a major (19) or release line (9.6), meaning its branch's head; a release (18.3), meaning its tag; a branch or tag; a SHA prefix (7+ hex digits); `<ref>@<date>`, its branch's last commit by that day's end; and `~N`. The default is the newest release branch's head. Any path can name another commit's tree: `/commit/<rev>/`, `/branch/<name>/` or `/tag/<name>/`. Ranges `A..B` mean what they mean to git log.

## Tools

| Tool | Like | What it adds |
| --- | --- | --- |
| `search`, `grep`, `read_file`, `list_files` | ripgrep, grep, cat, ls | Any commit and virtual folders; search's `history` argument finds every line a range of commits had, with the commit that first added it and those that removed it |
| `blame` | `git blame` | Passes through `.git-blame-ignore-revs` (pgindent) by one-for-one links; `reverse` names the commit that removed each line; `proposals` names the earliest posted patch that added each line |
| `get_commit` | `git show` | Trailers, linked threads and the posted patch the commit matches best, back-patch siblings, first releases, diffstat, diff rendered from the weave |
| `search_commits` | `git log` | Range, message, author, paths, dates; `-G` and `-S` read from the weave |
| `search_patches` | grep over the archive's patches | Lines by thread, first and latest version, the commit that landed them |

The history is the code tools' only source; `PGHACKER_CODE_HISTORY` can name another schema than `pghacker`, such as a build's before it is installed. Until one is built, the code tools have no code to read (`get_ontology` shows what is installed); `search_patches` still searches the mail's patches, without the commits that landed them.

With `history`, `search` gives lines only, each once per path and text, for paths from the root (globs go in `include` and `exclude`). `grep` reads history below `/history/` or `/history/<range>/`, printing each line at `/commit/<sha>/<path>`; it can't mix those with other files, `-v`, `-L` or context.

**Commit metadata.** Trailers (`Discussion`, `Author`, `Reviewed-by`, `Backpatch-through`, ...) are parsed, keeping names without email addresses. Commits on different branches form a back-patch group, one per branch, when they share a whitespace-insensitive patch id, or a subject and author within a week and a changed file. Commits link to threads through `Discussion` trailers and messages citing their SHA.

**Patches.** `patch_texts` holds each distinct line of every mail patch once, by kind (added, removed, context, other) without its prefix; `patches.text_ids` lists a patch's lines in order, so a line is found in every version that has it, at its place. One parser reads unified and context diffs.

## Building and syncing

A bulk build takes 6 minutes on 12 cores (2 walking the commits) and 1.6 GB of memory; it writes a schema of its own and swaps it into `pghacker` in one transaction. An hourly job applies new commits through the same walker, then refreshes back-patch groups and thread links; a build to N and syncs to M equal one build to M, down to weave order. The `patch_lines` job indexes patches 200 at a time until none waits; meanwhile, a `search_patches` that finds nothing says how many are left.

The AGE graph projects the installed history: a `Commit` vertex per commit, `MODIFIES` its files, `DISCUSSED_IN` its threads, `BACKPATCH_OF` from each other group member to its back-patch group's first commit, and `COMMITTED` from the one mail author with its author name. The history's tables have no triggers, since an install swaps them in whole: the sync, the links' refresh and the regroup record what they change for the graph's incremental sync, and an install asks for a full one.

## Validation

- **Exactness:** every regular file of 79 commits (39 branch heads, 40 at random; 345,081 files, 203M lines, text read as Latin-1 included) rebuilds to its git blob byte for byte; index and sequential scans agree.
- **Parity with the snapshots,** before their retirement: replays of 168,255 search and grep calls (agent sessions and generated PG19 workloads, at the 15 production heads) and of 60,105 calls on production's data gave no wrong answer, and no call was incomplete on the history alone. Latency was 1.09× the snapshots' at the median and 1.08× at the 95th percentile on production (1.17× and 1.10× locally). Calls that read every line of large parts of the tree took 1.4–1.6× as long: the history reads about eight times as many pages for the same lines.
- **git parity**, on random commits and files of PostgreSQL's history: `blame` agrees with `git blame` on 99.05% and 99.84% of lines in two samples (96–97% with ignore-revs, where git also matches lines fuzzily). `get_commit` equals `git show` on 98.3–98.7% of files, numstat on 99.3–99.5%. `search_commits` equals `git log` for every filter in tests.

**Known differences.** Diffs come from imara-diff's Myers with git's indent heuristic, not git's xdiff: where a change aligns several ways, as in files of repeated lines, they can differ, and blame, `get_commit` and `-G`/`-S` follow the choice (the tests use histories without such ties). `--ignore-revs-file` also guesses through hunks that change the line count; the history passes only one-for-one replacements. File modes, symlinks and submodules aren't recorded.
