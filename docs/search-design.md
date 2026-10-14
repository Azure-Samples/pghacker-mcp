# pghacker-mcp search design

*October, 2026 | Maxim Lukiyanov | Azure PostgreSQL*

## Why the commit history

Agents ask about code at any point in time: how a function looked in 9.6, who wrote a line and where it was argued, whether a fix reached the back branches. Code search originally read 15 snapshots, the heads of master and 14 release branches loaded in production (48M lines, 5.9 GB with indexes), and answered only there. The commit history stores every line of every commit of every branch once, in less space (5.7 GB with indexes, 105,442 commits), and serves the code tools at any commit, plus blame, log and show. Patch search covers the other half of the record: the patches posted to pgsql-hackers.

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

Text that isn't UTF-8 stays binary to the tools, as in the snapshots: search and grep skip it, and blame refuses such a version. Its lines are kept all the same (read as Latin-1, re-encoded exactly), so blaming a later UTF-8 version, and diffs, follow them back as git does; lines whose bytes changed are new instances.

## Indexes and queries

A trigram GIN index on `line` finds candidates; a btree on (`lineage`, `hi`, `lo`) scopes them to files. `alive` is only ever a filter: GiST scans of multiranges can miss rows on PostgreSQL 14 and later (reported on pgsql-hackers in August 2026, not yet fixed). The engine chooses among three plans:

- **One commit:** the trigram scan and `alive_at(commit)` together; rows map to versions through a cached per-commit map.
- **Scope first:** read the scoped versions' lines, then apply the text filters, when the files searched hold 200,000 lines or fewer or the pattern has no trigrams.
- **Mounted trees:** each line checked at its own version's birth, for paths under the virtual folders below.

`OFFSET 0` fences keep the planner from repeating the trigram scan per version. Trigram scans take the scope's lineages as a bound array, which PostgreSQL hashes and can AND with the btree; an array built by a subquery is compared member by member on every row rechecked, which made whole-tree searches for common terms take over 10 seconds. Past 200,000 candidate lines (a separate cap), a search answers from the first files in tree order and says its totals are lower bounds, as the snapshots do; the history re-reads files in that order to match them.

## Revisions and virtual folders

`version` takes a major (19) or release line (9.6), meaning its branch's head; a release (18.3), meaning its tag; a branch or tag; a SHA prefix (7+ hex digits); `<ref>@<date>`, its branch's last commit by that day's end; and `~N`. The default is the newest release branch's head. Any path can name another commit's tree: `/commit/<rev>/`, `/branch/<name>/` or `/tag/<name>/`. Ranges `A..B` mean what they mean to git log.

## Tools

| Tool | Like | What it adds |
| --- | --- | --- |
| `search`, `grep`, `read_file`, `list_files` | ripgrep, grep, cat, ls | With the history: any commit and virtual folders; search's `history` argument finds every line a range of commits had, with the commits that added and removed it |
| `blame` | `git blame` | Passes through `.git-blame-ignore-revs` (pgindent) by one-for-one links; `reverse` names the commit that removed each line; `proposals` names the earliest posted patch that added each line |
| `get_commit` | `git show` | Trailers, linked threads and the posted patch the commit matches best, back-patch siblings, first releases, diffstat, diff rendered from the weave |
| `search_commits` | `git log` | Range, message, author, paths, dates; `-G` and `-S` read from the weave |
| `search_patches` | grep over the archive's patches | Lines by thread, first and latest version, the commit that landed them |

`blame`, `get_commit` and `search_commits` are listed only once `PGHACKER_CODE_HISTORY` points the code tools at the history (until then the code tools read the snapshots as before); `search_patches` always is, and names landing commits once that flag is set.

**Commit metadata.** Trailers (`Discussion`, `Author`, `Reviewed-by`, `Backpatch-through`, ...) are parsed, keeping names without email addresses. Commits on different branches form a back-patch group, one per branch, when they share a whitespace-insensitive patch id, or a subject and author within a week and a changed file. Commits link to threads through `Discussion` trailers and messages citing their SHA.

**Patches.** `patch_texts` holds each distinct line of every mail patch once, by kind (added, removed, context, other) without its prefix; `patches.text_ids` lists a patch's lines in order, so a line is found in every version that has it, at its place. One parser reads unified and context diffs.

## Building and syncing

A bulk build takes 6 minutes on 12 cores (2 walking the commits) and 1.6 GB of memory; it writes a schema of its own and swaps it into `pghacker` in one transaction. An hourly job applies new commits through the same walker, then refreshes back-patch groups and thread links; a build to N and syncs to M equal one build to M, down to weave order. The `patch_lines` job indexes patches 200 at a time until none waits; meanwhile `search_patches` says how many are left.

## Validation

- **Exactness:** every regular file of 79 commits (39 branch heads, 40 at random; 345,081 files, 203M lines, text read as Latin-1 included) rebuilds to its git blob byte for byte; index and sequential scans agree.
- **Engine parity:** 168,255 search and grep calls (replayed agent sessions and generated PG19 workloads) at the 15 production heads gave no wrong answer. In the latest run, 48 calls differed, each where an engine gave no full answer: 29 were incomplete in both (time budget, candidate cap), 17 only on the snapshots, and 2 were snapshot errors; none was incomplete on the history alone. Latency was 1.17× the snapshots' at the median and 1.10× at the 95th percentile; replayed agent greps ran 0.84×. Calls that read every line of large parts of the tree add the most time, taking 1.4–1.6× as long: the history reads about eight times as many pages for the same lines. Earlier runs found three history bugs, since fixed: a mis-bound parameter, capped results in the wrong order, and spelling suggestions scanning all history. Replayed on production's data (60,105 calls), the benchmark gave no wrong answer, but 21 calls were incomplete on the history alone: whole-tree searches for "format", slowed by the subquery array above. With the array bound, the same workload replayed locally had none, and those calls take 0.2–0.4 s, against 0.7–0.9 s on the snapshots (up to 2.4 s before the line keys are cached).
- **git parity**, on random commits and files of PostgreSQL's history: `blame` agrees with `git blame` on 99.05% and 99.84% of lines in two samples (96–97% with ignore-revs, where git also matches lines fuzzily). `get_commit` equals `git show` on 98.3–98.7% of files, numstat on 99.3–99.5%. `search_commits` equals `git log` for every filter in tests.

**Known differences.** Diffs come from imara-diff's Myers with git's indent heuristic, not git's xdiff: where a change aligns several ways, as in files of repeated lines, they can differ, and blame, `get_commit` and `-G`/`-S` follow the choice (the tests use histories without such ties). `--ignore-revs-file` also guesses through hunks that change the line count; the history passes only one-for-one replacements. File modes, symlinks and submodules aren't recorded.

## Rollout

1. Build the history in production with a one-off job, start the hourly sync and `patch_lines` backfill, and replay the benchmark on production's data.
2. If it gives no wrong answers and comparable latency, set `PGHACKER_CODE_HISTORY`; check the 20 test questions. Unsetting it returns the tools to the snapshots.
3. After a period of use without regressions, retire the snapshot tables and loader.
