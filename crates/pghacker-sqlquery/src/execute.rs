// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The read-only query path: one statement in a read-only transaction that is
//! always rolled back, read in the text wire format and rendered as CSV.
//!
//! - It takes a `tokio_postgres::Client`, and returns [`QueryResponse`], an
//!   enum in which a timeout is an outcome of its own.
//! - Fields are quoted per RFC 4180: a field that holds a comma, a double
//!   quote, CR or LF, or starts or ends with a space or tab is quoted, with
//!   its double quotes doubled. NULL reads `None`, and an empty string and the
//!   text `None` are quoted (`""`, `"None"`), so NULL reads differently from
//!   every string. Column names are quoted the same way.
//! - Rows are kept by position rather than in maps keyed by column name, so
//!   columns that share a name, such as two tables' `id`, keep their own
//!   values.
//! - An empty result keeps its header row.
//! - Rows are rendered as they stream in, and the CSV stops before
//!   [`MAX_CSV_BYTES`], with a banner like the row cap's, since `max_rows`
//!   bounds rows rather than bytes.
//! - PostgreSQL's errors read as psql prints them, `ERROR:  message` with
//!   LINE, DETAIL, HINT and CONTEXT. The LINE pointer counts characters, and
//!   shows psql's 60-character window around the position rather than the
//!   whole line.

use std::pin::pin;

use futures_util::TryStreamExt;
use tokio_postgres::error::{DbError, ErrorPosition, SqlState};
use tokio_postgres::types::{FromSql, ToSql, Type};
use tokio_postgres::{Client, Column, Row, Transaction};

/// The largest CSV that query returns, in bytes: rows past it are cut, with
/// a banner.
pub const MAX_CSV_BYTES: usize = 1024 * 1024;

/// How a NULL reads in the CSV.
pub const NULL_TEXT: &str = "None";

/// What a statement came to.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryResponse {
    /// The CSV: a banner if rows were cut, the header row, and the rows. It's
    /// empty if the statement returns no columns.
    Rows {
        csv: String,
        /// The rows in the CSV.
        row_count: usize,
        /// Whether rows were cut, at `max_result_rows` or [`MAX_CSV_BYTES`].
        truncated: bool,
    },
    /// The statement ran past its timeout and was cancelled.
    TimedOut,
    /// PostgreSQL's error, as psql prints it, or why the statement didn't run.
    Failed(String),
}

/// Compose the truncation banner shown above a CSV when the row cap is hit.
pub fn truncation_banner(max_result_rows: usize) -> String {
    format!(
        "Result has more than {max_result_rows} rows, and is too large to \
         return. Only returning the first {max_result_rows} results.\n\n"
    )
}

/// The banner shown above a CSV cut at [`MAX_CSV_BYTES`], after `row_count`
/// rows.
pub fn size_banner(row_count: usize) -> String {
    format!(
        "Result is larger than {MAX_CSV_BYTES} bytes, and is too large to return. \
         Only returning the first {row_count} rows.\n\n"
    )
}

/// Appends `value` to `out` as one CSV field: NULL as [`NULL_TEXT`], and text
/// as it is unless it needs quotes ([`needs_quotes`]).
pub fn push_field(out: &mut String, value: Option<&str>) {
    let Some(text) = value else {
        out.push_str(NULL_TEXT);
        return;
    };
    if !needs_quotes(text) {
        out.push_str(text);
        return;
    }
    out.push('"');
    for part in text.split_inclusive('"') {
        out.push_str(part);
        if part.ends_with('"') {
            out.push('"');
        }
    }
    out.push('"');
}

/// Whether a text field is quoted: RFC 4180's commas, double quotes and line
/// breaks, leading or trailing blanks, which readers may trim, and the texts
/// that would otherwise read like NULL or nothing.
fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text == NULL_TEXT
        || text.contains([',', '"', '\r', '\n'])
        || text.starts_with([' ', '\t'])
        || text.ends_with([' ', '\t'])
}

/// A CSV under construction: the header row, then a line per row, kept under
/// [`MAX_CSV_BYTES`].
struct Csv {
    text: String,
    columns: usize,
    rows: usize,
}

impl Csv {
    fn new(columns: &[Column]) -> Self {
        let mut text = String::new();
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            push_field(&mut text, Some(column.name()));
        }
        Self {
            text,
            columns: columns.len(),
            rows: 0,
        }
    }

    /// Adds `line`, unless it would take the CSV past [`MAX_CSV_BYTES`].
    /// Without columns, rows have no text and only count.
    fn push(&mut self, line: &str) -> bool {
        if self.columns == 0 {
            self.rows += 1;
            return true;
        }
        if self.text.len() + 1 + line.len() > MAX_CSV_BYTES {
            return false;
        }
        self.text.push('\n');
        self.text.push_str(line);
        self.rows += 1;
        true
    }
}

/// Run a read-only query inside `BEGIN TRANSACTION READ ONLY; … ROLLBACK;`.
/// Returns the CSV of up to `max_result_rows` rows, with a truncation banner
/// when there were more, or why it failed.
pub async fn run_readonly_query(
    client: &mut Client,
    sql: &str,
    max_result_rows: usize,
    statement_timeout_ms: Option<i64>,
) -> QueryResponse {
    let txn = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return QueryResponse::Failed(describe_error(&e, sql)),
    };

    // `client.transaction()` issues `BEGIN`; `SET TRANSACTION READ ONLY` runs
    // before any statement, so this is equivalent to `BEGIN TRANSACTION READ
    // ONLY`. A positive per-query budget sets `statement_timeout` for the txn.
    //
    // Result columns are fetched in the text wire format (see
    // `run_query_capture`), which means PostgreSQL renders every value with its
    // type's output function. Those output functions consult session GUCs, so
    // pin the ones that affect rendering to keep responses deterministic across
    // servers whose defaults differ. The rule is: pin output-only GUCs, and
    // never pin one that also steers parsing. Determinism is not worth
    // reinterpreting the caller's input — a rendering difference is visible in
    // the response, whereas a parsing difference silently changes what the
    // query asked for.
    //
    // `DateStyle` is two settings in one — an output format and an input
    // date-order — so only its output half is pinned. Writing the order too
    // (`'ISO, MDY'`) would reinterpret the caller's date literals: on a `DMY`
    // server `'01/02/2024'::date` would silently move from 1 Feb to 2 Jan, and
    // `'13/01/2024'::date` would start erroring. Bare `'ISO'` sets only the
    // output field, leaving the order at the server's value, so `SHOW
    // DateStyle` still reports e.g. `ISO, DMY`.
    //
    // `bytea_output` and `extra_float_digits` have no input side at all —
    // `bytea` accepts both the hex and escape forms whatever the setting says,
    // and `extra_float_digits` only widens the digits printed for a float.
    //
    // `IntervalStyle` is deliberately *not* pinned. It has the same dual nature
    // as `DateStyle` but no output-only half to fall back on, so there is no
    // safe value: on a server set to `sql_standard`, `interval '-1 2:03:04'`
    // means -1 day -02:03:04, and pinning `'postgres'` would silently reread it
    // as -1 day +02:03:04. The accepted consequence is that interval rendering
    // follows the server's `IntervalStyle`, so one interval may come back as
    // `1 day 02:03:04` (`postgres`), `@ 1 day 2 hours 3 mins 4 secs`
    // (`postgres_verbose`), `1 2:03:04` (`sql_standard`) or `P1DT2H3M4S`
    // (`iso_8601`). Non-deterministic text is the lesser cost.
    let mut setup = String::from(
        "SET TRANSACTION READ ONLY; \
         SET LOCAL DateStyle = 'ISO'; \
         SET LOCAL extra_float_digits = 1; \
         SET LOCAL bytea_output = 'hex';",
    );
    if let Some(ms) = statement_timeout_ms.filter(|ms| *ms > 0) {
        setup.push_str(&format!(" SET LOCAL statement_timeout = {ms};"));
    }
    if let Err(e) = txn.batch_execute(&setup).await {
        return QueryResponse::Failed(describe_error(&e, sql));
    }

    let inner = run_query_capture(&txn, sql, max_result_rows, statement_timeout_ms).await;

    // Read-only path: always discard the transaction (dropping would also roll
    // back, but an explicit rollback keeps the connection state obvious).
    let _ = txn.rollback().await;
    inner
}

async fn run_query_capture(
    txn: &Transaction<'_>,
    sql: &str,
    max_result_rows: usize,
    statement_timeout_ms: Option<i64>,
) -> QueryResponse {
    let stmt = match txn.prepare(sql).await {
        Ok(s) => s,
        Err(e) => return map_query_error(e, sql, statement_timeout_ms),
    };

    // Bound the fetch at the protocol level: bind a portal and execute it with a
    // row limit of `max_result_rows + 1`. PostgreSQL suspends the portal after
    // that many rows, so the cap bounds server work and network transfer, not
    // just client memory and response rendering. The one extra row is the
    // truncation probe.
    let probe_cap = max_result_rows.saturating_add(1);
    let fetch = i32::try_from(probe_cap).unwrap_or(i32::MAX);
    // Bind the portal so PostgreSQL returns every column in the *text* wire
    // format (see `vendor/tokio-postgres/FORK-README.md`). The server then
    // renders each value with its type's output function, exactly as `psql`
    // receives it, which is the only way a generic client can display types it
    // has no decoder for — network types, `bytea`, `interval`, ranges, enums,
    // composites and every extension type such as pgvector's `vector`.
    //
    // The result format lives in the `Bind` message while the row limit lives
    // in `Execute`, so the server-side fetch cap below is unaffected.
    let params: &[&(dyn ToSql + Sync)] = &[];
    let portal = match txn.bind_text(&stmt, params).await {
        Ok(p) => p,
        Err(e) => return map_query_error(e, sql, statement_timeout_ms),
    };
    // Rows stream in, so a CSV cut at MAX_CSV_BYTES stops holding them: the
    // driver drops the rest of the portal's rows as they arrive.
    let rows = match txn.query_portal_raw(&portal, fetch).await {
        Ok(r) => r,
        Err(e) => return map_query_error(e, sql, statement_timeout_ms),
    };
    let mut rows = pin!(rows);

    let columns = stmt.columns();
    let mut csv = Csv::new(columns);
    let mut banner = None;
    loop {
        let row = match rows.try_next().await {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(e) => return map_query_error(e, sql, statement_timeout_ms),
        };
        if csv.rows == max_result_rows {
            banner = Some(truncation_banner(max_result_rows));
            break;
        }
        let line = match render_row(&row, columns) {
            Ok(line) => line,
            Err(response) => return response,
        };
        if !csv.push(&line) {
            banner = Some(size_banner(csv.rows));
            break;
        }
    }

    let truncated = banner.is_some();
    let text = if csv.columns == 0 {
        String::new()
    } else {
        match banner {
            Some(banner) => banner + &csv.text,
            None => csv.text,
        }
    };
    QueryResponse::Rows {
        csv: text,
        row_count: csv.rows,
        truncated,
    }
}

/// One row's CSV line.
fn render_row(row: &Row, columns: &[Column]) -> Result<String, QueryResponse> {
    let mut line = String::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        match value_at(row, index, column.type_()) {
            Ok(value) => push_field(&mut line, value.as_deref()),
            Err(e) => {
                return Err(QueryResponse::Failed(format_executing_error(&format!(
                    "could not deserialize column \"{}\" (type {}): {}",
                    column.name(),
                    column.type_(),
                    e
                ))));
            }
        }
    }
    Ok(line)
}

fn map_query_error(
    e: tokio_postgres::Error,
    sql: &str,
    statement_timeout_ms: Option<i64>,
) -> QueryResponse {
    if e.code() == Some(&SqlState::QUERY_CANCELED) && statement_timeout_ms.is_some_and(|ms| ms > 0)
    {
        return QueryResponse::TimedOut;
    }
    QueryResponse::Failed(describe_error(&e, sql))
}

/// PostgreSQL's error as psql prints it, or the driver's.
fn describe_error(e: &tokio_postgres::Error, sql: &str) -> String {
    match e.as_db_error() {
        Some(db) => format_db_error_psql(db, sql),
        None => format_executing_error(&e.to_string()),
    }
}

/// The prefix of a failure that isn't PostgreSQL's error, such as a closed
/// connection.
fn format_executing_error(detail: &str) -> String {
    format!("Error executing statement: {detail}")
}

/// Render a tokio-postgres `DbError` as psql prints it with its default
/// verbosity:
///
/// 1. `<severity>:  <message>`
/// 2. `LINE <n>: <text>\n        ^` when a statement position is present
/// 3. `DETAIL:  <text>` when the `detail` field is set
/// 4. `HINT:  <text>` when the `hint` field is set
/// 5. `CONTEXT:  <text>` when the `where_` field is set
fn format_db_error_psql(db: &DbError, sql: &str) -> String {
    let mut out = format!("{}:  {}", db.severity(), db.message());
    if let Some(ErrorPosition::Original(position)) = db.position()
        && let Some(line_block) = format_line_pointer(sql, *position as usize)
    {
        out.push('\n');
        out.push_str(&line_block);
    }
    if let Some(detail) = db.detail() {
        out.push_str("\nDETAIL:  ");
        out.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        out.push_str("\nHINT:  ");
        out.push_str(hint);
    }
    if let Some(context) = db.where_() {
        out.push_str("\nCONTEXT:  ");
        out.push_str(context);
    }
    out
}

/// psql's pointer at `position`, PostgreSQL's 1-based character offset into
/// `query`: `LINE <n>: ` and the line, cut like psql's to 60 characters
/// around the position with `...` where cut, then a caret under the position.
fn format_line_pointer(query: &str, position: usize) -> Option<String> {
    const DISPLAY_SIZE: usize = 60;
    const MIN_RIGHT_CUT: usize = 10;
    let target = position.checked_sub(1)?;
    let mut line_no = 1usize;
    let mut line_start = 0usize;
    let mut line_start_char = 0usize;
    for (char_index, (byte_index, ch)) in query.char_indices().enumerate() {
        if char_index >= target {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = byte_index + 1;
            line_start_char = char_index + 1;
        }
    }
    let line_end = query[line_start..]
        .find('\n')
        .map_or(query.len(), |n| line_start + n);
    let line: Vec<char> = query[line_start..line_end]
        .trim_end_matches('\r')
        .chars()
        .map(|ch| if ch == '\t' { ' ' } else { ch })
        .collect();
    let column = (target - line_start_char).min(line.len());
    let (mut begin, mut end) = (0, line.len());
    if line.len() > DISPLAY_SIZE {
        if column + MIN_RIGHT_CUT < DISPLAY_SIZE {
            end = DISPLAY_SIZE;
        } else {
            end = (column + MIN_RIGHT_CUT).min(line.len());
            begin = end - DISPLAY_SIZE;
        }
    }
    let prefix = format!("LINE {line_no}: ");
    let mut shown = String::new();
    if begin > 0 {
        shown.push_str("...");
    }
    shown.extend(&line[begin..end]);
    if end < line.len() {
        shown.push_str("...");
    }
    let dots = if begin > 0 { 3 } else { 0 };
    let padding = " ".repeat(prefix.len() + dots + column - begin);
    Some(format!("{prefix}{shown}\n{padding}^"))
}

/// Decodes a column of a **text-format** portal.
///
/// `run_query_capture` binds its portal with `bind_text`, so every column buffer
/// holds the value already rendered by PostgreSQL's output function. Accepting
/// every `Type` is therefore correct here — and is *only* correct because the
/// caller controls the result format code. Using this on a binary portal would
/// misparse the data.
struct RawText(String);

impl<'a> FromSql<'a> for RawText {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawText(std::str::from_utf8(raw)?.to_owned()))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// Renders a scalar boolean's `t`/`f` as `True`/`False`.
///
/// This is the one place where the server's text isn't passed through
/// untouched. Only scalar `bool` (OID 16) is mapped: `bool[]` (OID 1000) and
/// every other type keep PostgreSQL's text, so an array reads `{t,f}`.
fn bool_text(raw: &str) -> Option<&'static str> {
    match raw {
        "t" => Some("True"),
        "f" => Some("False"),
        _ => None,
    }
}

/// Reads one column of a text-format row.
///
/// A SQL `NULL` arrives as a missing buffer (length `-1` on the wire) and stays
/// `None`, which [`push_field`] renders as [`NULL_TEXT`]. That keeps NULL
/// distinguishable from an empty string, which `psql` does not do.
fn value_at(
    row: &Row,
    idx: usize,
    ty: &Type,
) -> std::result::Result<Option<String>, tokio_postgres::Error> {
    let Some(raw) = row.try_get::<_, Option<RawText>>(idx)? else {
        return Ok(None);
    };
    if *ty == Type::BOOL
        && let Some(mapped) = bool_text(&raw.0)
    {
        return Ok(Some(mapped.to_string()));
    }
    Ok(Some(raw.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(value: Option<&str>) -> String {
        let mut out = String::new();
        push_field(&mut out, value);
        out
    }

    #[test]
    fn raw_text_accepts_every_type_and_decodes_utf8() {
        // The whole point of the text-format portal: a decoder that works for
        // types this crate has no binary support for, including extension
        // types it has never heard of.
        for ty in [
            Type::TEXT,
            Type::INET,
            Type::CIDR,
            Type::MACADDR,
            Type::BYTEA,
            Type::INTERVAL,
            Type::NUMERIC,
            Type::JSONB,
            Type::INT4_ARRAY,
        ] {
            assert!(RawText::accepts(&ty), "RawText must accept {ty}");
        }
        assert_eq!(
            RawText::from_sql(&Type::INET, b"192.168.1.1").unwrap().0,
            "192.168.1.1"
        );
        // Non-UTF-8 is an error rather than a silently mangled cell.
        assert!(RawText::from_sql(&Type::TEXT, &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn bool_text_maps_only_t_and_f() {
        assert_eq!(bool_text("t"), Some("True"));
        assert_eq!(bool_text("f"), Some("False"));
        // `bool[]` renders as `{t,f}` and must pass through untouched.
        assert_eq!(bool_text("{t,f}"), None);
        assert_eq!(bool_text("true"), None);
    }

    #[test]
    fn truncation_banner_names_the_row_cap() {
        let banner = truncation_banner(10_000);
        assert_eq!(
            banner,
            "Result has more than 10000 rows, and is too large to return. \
             Only returning the first 10000 results.\n\n"
        );
    }

    #[test]
    fn plain_text_is_unquoted() {
        assert_eq!(field(Some("hello")), "hello");
        assert_eq!(field(Some("a b")), "a b");
        assert_eq!(field(Some("{1;2}")), "{1;2}");
        assert_eq!(field(Some("héllo")), "héllo");
    }

    #[test]
    fn null_is_none_and_differs_from_every_string() {
        assert_eq!(field(None), "None");
        assert_eq!(field(Some("None")), "\"None\"");
        assert_eq!(field(Some("")), "\"\"");
        assert_eq!(field(Some("none")), "none");
    }

    #[test]
    fn fields_are_quoted_per_rfc_4180() {
        assert_eq!(field(Some("a,b")), "\"a,b\"");
        assert_eq!(field(Some("say \"hi\"")), "\"say \"\"hi\"\"\"");
        assert_eq!(field(Some("\"")), "\"\"\"\"");
        assert_eq!(field(Some("line1\nline2")), "\"line1\nline2\"");
        assert_eq!(field(Some("cr\r")), "\"cr\r\"");
        assert_eq!(field(Some(" pad")), "\" pad\"");
        assert_eq!(field(Some("pad ")), "\"pad \"");
        assert_eq!(field(Some("\ttab")), "\"\ttab\"");
        assert_eq!(field(Some("{1,2,3}")), "\"{1,2,3}\"");
        assert_eq!(field(Some(r#"{"a": [1, 2]}"#)), r#""{""a"": [1, 2]}""#);
    }

    #[test]
    fn quoted_fields_read_back_with_a_csv_reader() {
        let values = [
            Some("a,b"),
            Some("say \"hi\""),
            Some("x\r\ny"),
            Some(""),
            None,
            Some("None"),
            Some(" pad "),
            Some("\"\""),
        ];
        let mut line = String::new();
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                line.push(',');
            }
            push_field(&mut line, *value);
        }
        assert_eq!(read_csv_line(&line), values.map(|v| v.map(str::to_owned)));
    }

    /// A minimal RFC 4180 reader of one record, mapping an unquoted `None`
    /// to NULL.
    fn read_csv_line(line: &str) -> Vec<Option<String>> {
        let mut fields = Vec::new();
        let mut chars = line.chars().peekable();
        loop {
            if chars.peek() == Some(&'"') {
                chars.next();
                let mut text = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            text.push('"');
                        } else {
                            break;
                        }
                    } else {
                        text.push(ch);
                    }
                }
                fields.push(Some(text));
            } else {
                let mut text = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch == ',' {
                        break;
                    }
                    text.push(ch);
                    chars.next();
                }
                fields.push(if text == NULL_TEXT { None } else { Some(text) });
            }
            match chars.next() {
                Some(',') => continue,
                None => return fields,
                Some(other) => panic!("unexpected {other:?} after a field in {line:?}"),
            }
        }
    }

    #[test]
    fn csv_lines_stay_under_the_size_limit() {
        let mut csv = Csv {
            text: "a".to_owned(),
            columns: 1,
            rows: 0,
        };
        let line = "x".repeat(MAX_CSV_BYTES / 2);
        assert!(csv.push(&line));
        assert!(!csv.push(&line));
        assert_eq!(csv.rows, 1);
        assert!(csv.text.len() <= MAX_CSV_BYTES);
        let fits = "y".repeat(MAX_CSV_BYTES - csv.text.len() - 1);
        assert!(csv.push(&fits));
        assert_eq!(csv.text.len(), MAX_CSV_BYTES);
        assert!(!csv.push(""));
    }

    fn caret(columns: usize) -> String {
        format!("{}^", " ".repeat(columns))
    }

    #[test]
    fn line_pointer_marks_the_position() {
        assert_eq!(
            format_line_pointer("SELECT * FROM nope", 15).unwrap(),
            format!("LINE 1: SELECT * FROM nope\n{}", caret(8 + 14))
        );
        assert_eq!(
            format_line_pointer("SELECT 1\nFROM nope", 15).unwrap(),
            format!("LINE 2: FROM nope\n{}", caret(8 + 5))
        );
        // At the end of the input, as for "syntax error at end of input".
        assert_eq!(
            format_line_pointer("SELECT 1 +", 11).unwrap(),
            format!("LINE 1: SELECT 1 +\n{}", caret(8 + 10))
        );
        assert_eq!(format_line_pointer("SELECT 1", 0), None);
    }

    #[test]
    fn line_pointer_counts_characters_not_bytes() {
        // PostgreSQL counts characters: "é" is one, though two bytes.
        let sql = "SELECT 'é', 'ü' FROM nope";
        let position = sql.chars().position(|c| c == 'n').unwrap() + 1;
        assert_eq!(position, 22);
        assert_eq!(
            format_line_pointer(sql, position).unwrap(),
            format!("LINE 1: {sql}\n{}", caret(8 + 21))
        );
    }

    #[test]
    fn long_lines_are_cut_around_the_position() {
        let sql = format!("SELECT {} FROM nope WHERE x", "a, ".repeat(40));
        let position = sql.find("nope").unwrap() + 1;
        let pointer = format_line_pointer(&sql, position).unwrap();
        let (line, caret) = pointer.split_once('\n').unwrap();
        assert!(line.starts_with("LINE 1: ..."), "{pointer}");
        assert!(line.ends_with("..."), "{pointer}");
        assert_eq!(line.chars().count(), "LINE 1: ".len() + 3 + 60 + 3);
        let at = caret.len() - 1;
        assert_eq!(&line[at..at + 4], "nope", "{pointer}");

        let early = format!("SELECT nope, {}", "b, ".repeat(40));
        let pointer = format_line_pointer(&early, 8).unwrap();
        let (line, caret) = pointer.split_once('\n').unwrap();
        assert!(line.starts_with("LINE 1: SELECT nope"), "{pointer}");
        assert!(line.ends_with("..."), "{pointer}");
        assert_eq!(caret, format!("{}^", " ".repeat("LINE 1: SELECT ".len())));
    }
}
