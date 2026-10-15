//! Code search over pg_trgm: the search operation, grep command lines,
//! read_file and list_files, over the commit history that pghacker-code
//! builds: every commit's files and lines, stored once as file versions and
//! line instances.
//!
//! PostgreSQL narrows the lines with pg_trgm for selective patterns, or
//! scans the selected files first for small scopes and short patterns. A
//! Rust matcher, ripgrep's engine, decides exactly which lines match. The
//! `grep` tool reads GNU grep command lines and prints what GNU grep 3.11
//! prints, refusing incomplete searches rather than counting partial output.

pub mod blame;
pub mod commit;
pub mod diff;
pub mod engine;
pub mod files;
pub mod glob;
pub mod grep;
pub mod history;
pub mod history_search;
pub mod log;
pub mod patches;
pub mod pattern;
pub mod range;
pub mod rev;
pub mod search;
pub mod snapshot;
mod sqlutil;

pub use blame::{BlameArgs, BlameResult, blame};
pub use commit::{CommitResult, GetCommitArgs, get_commit};
pub use files::{
    ListFilesArgs, ListFilesResult, ReadFileArgs, ReadFileResult, list_files, read_file,
};
pub use grep::{GrepArgs, grep};
pub use log::{SearchCommitsArgs, SearchCommitsResult, search_commits};
pub use patches::{SearchPatchesArgs, SearchPatchesResult, search_patches};
pub use search::{SearchArgs, SearchResult, search};
pub use snapshot::{Snapshot, resolve_snapshot};

/// The search tool's description, for agents.
pub const SEARCH_DESCRIPTION: &str = "\
Search the PostgreSQL source at one commit, or with history over a range of commits. `match`: \
text (default), regex (ripgrep syntax) or identifier (a name in any spelling: \"parse node\" \
finds parseNode, parse_node, PARSE_NODE). A list of queries finds lines with any of them. Paths \
are relative to the source root, such as src/backend/parser/gram.y; list_files shows the \
folders indexed. Narrow with paths, include/exclude globs and also/not patterns (| grep, \
grep -v); choose output lines (with context), files or counts (group_by match: counts per \
distinct match); page with limit and offset; order coverage puts files with the most terms \
first. Results give totals, `cut` when more remain, `complete` false when totals are only lower \
bounds (past the time budget or 200000 candidate lines), `budget_exceeded` when the time ran \
out, and similar names when nothing matched. The page limit does not limit the work needed to \
compute totals. With history (all, a revision and its ancestors, or A..B), results are lines, \
each once per path and text, with the commit that first added it and those that removed it. \
budget_ms defaults to 10000, at most 15000.";

/// The grep tool's description, for agents.
pub const GREP_DESCRIPTION: &str = "\
Run a grep command line over the PostgreSQL source as GNU grep would at the source root, \
such as: grep -rn \"makeNode(\" src/backend --include=*.c | head -20. -E, -F and -P set the \
regex dialect (none: basic regex); -i -w -x -v -c -l -L -o -n -h -H -m -A -B -C, \
--include, --exclude, --exclude-dir and wildcards work as in grep; without a file or \
folder it searches the whole tree; a leading cd DIR && sets the folder. Trailing head, \
tail, wc, sort, uniq, cut and grep stages run on the output. Paths are relative to the \
source root (absolute paths into a PostgreSQL checkout work too). /history/<path> searches \
every commit, and /history/<range>/<path> a revision and its ancestors or A..B, printing each \
line at /commit/<sha>/<path>; not with other files, -v, -L or context. Incomplete searches \
fail instead of returning partial pipeline counts; output is cut at 1 MiB. budget_ms defaults \
to 10000, at most 15000. Not egrep, fgrep, git grep or rg: use search.";

/// The read_file tool's description, for agents.
pub const READ_FILE_DESCRIPTION: &str = "\
Read a PostgreSQL source file with line numbers: path relative to the source root (such as \
src/backend/parser/gram.y), optional start_line and end_line. Returns at most 2000 lines and \
200 KB, the first line whole however long; `cut` says more remain, and total_lines gives the \
file's length.";

/// The blame tool's description, for agents.
pub const BLAME_DESCRIPTION: &str = "\
Show which commit added each line of a PostgreSQL source file, as git blame does: path (relative \
to the source root, or below /branch/<name>/, /tag/<name>/ or /commit/<rev>/), optional start_line \
and end_line (at most 2000 lines), and version. It follows renames and, by default, passes \
through the reformatting commits (pgindent runs) in .git-blame-ignore-revs to the commit that \
wrote the line. reverse gives the commit that removed each line after this version, on its \
branch. Commits come once each, with date, author, branch and subject. With proposals, each \
line also names the earliest patch posted to the lists that added it to this file.";

/// The get_commit tool's description, for agents.
pub const GET_COMMIT_DESCRIPTION: &str = "\
Show a PostgreSQL commit as git show does: commit (a SHA or prefix, a branch or tag, or a revision \
such as REL_17_STABLE~2), with its message and parsed trailers (Author, Reviewed-by, \
Discussion, Backpatch-through...), the pgsql-hackers threads its Discussion trailers name, the \
same change back-patched to other branches, the first release on each branch with the change, \
the files changed with line counts, and the unified diff, up to 3000 lines (paths limits it to \
files or folders; context sets the lines around changes; diff: false leaves it out); `cut` says \
more remain.";

/// The search_commits tool's description, for agents.
pub const SEARCH_COMMITS_DESCRIPTION: &str = "\
Find PostgreSQL commits as git log does, newest first: range (a branch, tag or SHA and its \
ancestors, or A..B; default every branch), message (a regular expression), author (also \
matching Author and Co-authored-by trailers), paths (files, folders or globs), since and until \
(dates). changed_lines finds commits that added or removed a line matching a regular \
expression (git log -G); occurrences finds commits that changed how many times a text occurs \
in a file (git log -S), showing the lines. Each commit comes with its branch, line counts, \
Discussion links and the other branches that got the same change; get_commit shows one whole. \
`incomplete` says commits may be missing (past the budget, or past 300000 lines matching \
changed_lines or occurrences). budget_ms defaults to 10000, at most 15000.";

/// The search_patches tool's description, for agents.
pub const SEARCH_PATCHES_DESCRIPTION: &str = "\
Find lines in the patches posted to pgsql-hackers, by thread: query (text, or with match regex \
a regular expression, or identifier a whole word), lines (changed: added or removed, the \
default; added; removed; context; all), and paths, author, since, until and thread_id filters. \
Each thread comes with the first and latest patch version that has the lines, the lines with \
their files and line numbers, and, with the commit history, the linked commits that added them. \
`incomplete` says matches may be missing (past the budget, 50000 matching lines or 20000 \
patches). Patches not yet indexed aren't searched; a search that finds nothing counts them. \
budget_ms defaults to 10000, at most 15000.";

/// The list_files tool's description, for agents.
pub const LIST_FILES_DESCRIPTION: &str = "\
List folders and files at a PostgreSQL commit: path (default the root), a glob such as *.c or \
nodes/*.h, and depth (default 1, or every level with a glob). The result names the version and \
commit and the folders indexed (loaded_paths); paths are relative to the source root.";

/// What search, grep, read_file and list_files add to their descriptions:
/// the commit history makes every commit readable.
pub const HISTORY_NOTE: &str = "\
Every commit of the commit history is readable (get_ontology shows what is installed): version \
also takes a branch or tag as of a date (REL_17_STABLE@2024-06-01) or commits back (master~3), \
and paths below /branch/<name>/, /tag/<name>/ or /commit/<rev>/ read those trees in the same \
call.";
