//! Code search over pg_trgm: the search operation, grep command lines,
//! read_file and list_files, over source snapshots that pghacker-code loads
//! into `code_files` and `code_lines`.
//!
//! PostgreSQL narrows the lines with pg_trgm for selective patterns, or
//! scans the selected files first for small scopes and short patterns. A
//! Rust matcher, ripgrep's engine, decides exactly which lines match. The
//! `grep` tool reads GNU grep command lines and prints what GNU grep 3.11
//! prints, refusing incomplete searches rather than counting partial output.

pub mod engine;
pub mod files;
pub mod glob;
pub mod grep;
pub mod history;
pub mod pattern;
pub mod rev;
pub mod search;
pub mod snapshot;

pub use files::{
    ListFilesArgs, ListFilesResult, ReadFileArgs, ReadFileResult, list_files, read_file,
};
pub use grep::{GrepArgs, grep};
pub use search::{SearchArgs, SearchResult, search};
pub use snapshot::{Snapshot, resolve_snapshot};

/// The search tool's description, for agents.
pub const SEARCH_DESCRIPTION: &str = "\
Search the indexed PostgreSQL source, using scoped scans for short patterns. `match`: text (default), regex \
(ripgrep syntax) or identifier (a name in any spelling: \"parse node\" finds parseNode, \
parse_node, PARSE_NODE). A list of queries finds lines with any of them. Paths are relative \
to the source root, such as src/backend/parser/gram.y; list_files shows which folders are \
loaded. Narrow with paths, include/exclude globs and also/not patterns (| grep, grep -v); \
choose output lines (with context), files or counts (group_by match: counts per distinct \
match); page with limit and offset; order coverage puts files with the most terms first. \
Results give totals, `cut` when more remain, `complete` false when totals are only lower \
bounds, `budget_exceeded` when the time ran out, and similar names when nothing matched. \
The page limit does not limit the work needed to compute totals.";

/// The grep tool's description, for agents.
pub const GREP_DESCRIPTION: &str = "\
Run a grep command line over the PostgreSQL source as GNU grep would at the source root, \
such as: grep -rn \"makeNode(\" src/backend --include=*.c | head -20. -E, -F and -P set the \
regex dialect (none: basic regex); -i -w -x -v -c -l -L -o -n -h -H -m -A -B -C, \
--include, --exclude, --exclude-dir and wildcards work as in grep; without a file or \
folder it searches the whole tree; a leading cd DIR && sets the folder. Trailing head, \
tail, wc, sort, uniq, cut and grep stages run on the output. Paths are relative to the \
source root (absolute paths into a PostgreSQL checkout work too). Incomplete searches \
fail instead of returning partial pipeline counts. Not egrep, fgrep, git grep or rg: use search.";

/// The read_file tool's description, for agents.
pub const READ_FILE_DESCRIPTION: &str = "\
Read a PostgreSQL source file with line numbers: path relative to the source root (such as \
src/backend/parser/gram.y), optional start_line and end_line. Returns at most 2000 lines or \
200 KB; `cut` says more remain, and total_lines gives the file's length.";

/// The list_files tool's description, for agents.
pub const LIST_FILES_DESCRIPTION: &str = "\
List folders and files of the loaded PostgreSQL source: path (default the root), a glob such \
as *.c or nodes/*.h, and depth (default 1). The result names the version and commit and the \
folders loaded (loaded_paths); paths are relative to the source root.";
