// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The PostgreSQL side of a search: candidate lines through pg_trgm, the time
//! budget, and the lines around matches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{TryStreamExt, pin_mut};
use tokio_postgres::GenericClient;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;

use crate::pattern::Filter;

/// The default and largest time budgets, in milliseconds.
pub const DEFAULT_BUDGET_MS: u32 = 10_000;
pub const MAX_BUDGET_MS: u32 = 15_000;

/// The most candidate lines one search reads; more makes its totals lower
/// bounds.
pub const MAX_CANDIDATES: usize = 200_000;

/// A search's time limit.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    start: Instant,
    limit: Duration,
}

impl Budget {
    pub fn new(ms: Option<u32>) -> Self {
        let ms = ms.unwrap_or(DEFAULT_BUDGET_MS).clamp(1, MAX_BUDGET_MS);
        Self {
            start: Instant::now(),
            limit: Duration::from_millis(u64::from(ms)),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Milliseconds left, at least 1.
    pub fn remaining_ms(&self) -> u64 {
        self.limit
            .saturating_sub(self.start.elapsed())
            .as_millis()
            .max(1) as u64
    }

    pub fn exceeded(&self) -> bool {
        self.start.elapsed() >= self.limit
    }

    /// Sets the statement timeout to the time left, for the call's
    /// transaction; false, setting nothing, once the budget is spent.
    pub async fn apply(&self, client: &impl GenericClient) -> Result<bool> {
        if self.exceeded() {
            return Ok(false);
        }
        client
            .execute(
                "SELECT set_config('statement_timeout', $1, true)",
                &[&format!("{}ms", self.remaining_ms())],
            )
            .await
            .context("cannot set statement_timeout")?;
        Ok(true)
    }
}

/// The savepoint a call goes back to after a statement timeout.
const SAVEPOINT: &str = "pghacker_budget";

/// A call's transaction, which holds its budget as a transaction-local
/// statement_timeout: the setting ends with the transaction, so it can't
/// outlive the call and reach the connection's next user. A cancelled call
/// drops its scope, which queues a ROLLBACK on the connection, as a
/// dropped `tokio_postgres::Transaction` does. Pass a client in autocommit
/// mode.
pub struct TimeoutScope<'a> {
    client: &'a tokio_postgres::Client,
    open: bool,
}

impl<'a> TimeoutScope<'a> {
    /// Begins the call's transaction, with the whole budget as its
    /// statement timeout.
    pub async fn begin(client: &'a impl GenericClient, budget: &Budget) -> Result<Self> {
        let scope = Self {
            client: client.client(),
            open: true,
        };
        client
            .batch_execute(&format!(
                "BEGIN; SET LOCAL statement_timeout = {}; SAVEPOINT {SAVEPOINT}",
                budget.remaining_ms()
            ))
            .await
            .context("cannot set statement_timeout")?;
        Ok(scope)
    }

    /// Ends the call's transaction. It only read, so it is rolled back.
    pub async fn end(mut self) {
        if let Err(error) = self.client.batch_execute("ROLLBACK").await {
            tracing::warn!(%error, "cannot end code search's transaction");
        }
        self.open = false;
    }
}

impl Drop for TimeoutScope<'_> {
    fn drop(&mut self) {
        if self.open {
            // Sent without waiting, ahead of anything the next user sends.
            self.client.__private_api_rollback(None);
        }
    }
}

/// After a statement timeout aborted the call's transaction, goes back to
/// its start (and its first statement timeout) so later statements can run.
pub async fn recover(client: &impl GenericClient) {
    if let Err(error) = client
        .batch_execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
        .await
    {
        tracing::debug!(%error, "cannot recover from a statement timeout");
    }
}

/// Runs CPU-bound work on the fetched rows, such as exact pattern matching,
/// on Tokio's blocking threads, so it can't stall the async workers.
pub async fn off_runtime<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Ok(value),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(anyhow::anyhow!("code search work was cancelled: {error}")),
    }
}

/// Whether `error` is a cancellation by statement_timeout.
pub fn is_timeout(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::QUERY_CANCELED)
}

/// Lines to read at the history's commit: in these files (or all of the
/// commit's), matching every `require` filter and none of the exact
/// `reject` filters.
pub struct LineQuery<'a> {
    /// Where the commit is in the history.
    pub history: &'a crate::history::History,
    /// Some files are mounted from other commits than the history's.
    pub mounted: bool,
    pub file_ids: Option<&'a [i32]>,
    pub require: Vec<&'a Filter>,
    pub reject: Vec<&'a Filter>,
    pub cap: usize,
    /// Scan the selected files before applying text predicates, rather than
    /// reading common postings from the global trigram index.
    pub scope_first: bool,
}

pub fn scope_first(lines: u64, require: &[&Filter]) -> bool {
    lines <= MAX_CANDIDATES as u64 || !require.iter().any(|filter| filter.has_index_literal())
}

/// A candidate line.
#[derive(Clone, Debug)]
pub struct Row {
    pub file_id: i32,
    pub line_no: i32,
    pub line: String,
}

/// What the query returned.
#[derive(Debug, Default)]
pub struct Candidates {
    pub rows: Vec<Row>,
    /// More candidates than the cap.
    pub capped: bool,
    pub timed_out: bool,
}

type Param = Box<dyn ToSql + Sync + Send>;

/// Appends the query's text filters on `l.line`.
pub(crate) fn push_filters(sql: &mut String, params: &mut Vec<Param>, q: &LineQuery<'_>) {
    let ors = |conds: &[crate::pattern::Cond], params: &mut Vec<Param>| -> String {
        let parts: Vec<String> = conds
            .iter()
            .map(|c| {
                params.push(Box::new(c.pattern.clone()));
                let cond = format!("l.line {} ${}::text", c.op.sql(), params.len());
                match &c.literal {
                    Some(like) => {
                        params.push(Box::new(like.clone()));
                        let op = if c.op == crate::pattern::Op::ReI {
                            "ILIKE"
                        } else {
                            "LIKE"
                        };
                        format!("({cond} AND l.line {op} ${}::text)", params.len())
                    }
                    None => cond,
                }
            })
            .collect();
        parts.join(" OR ")
    };
    for filter in &q.require {
        if let Filter::Any { conds, .. } = filter {
            let or = ors(conds, params);
            sql.push_str(&format!(" AND ({or})"));
        }
    }
    for filter in &q.reject {
        match filter {
            Filter::Any { conds, exact: true } => {
                let or = ors(conds, params);
                sql.push_str(&format!(" AND NOT ({or})"));
            }
            Filter::All { exact: true } => sql.push_str(" AND false"),
            _ => {}
        }
    }
}

/// Streams the query's rows through `map` until the cap or the statement
/// timeout, noting either in `out`.
pub(crate) async fn stream_rows<T>(
    client: &impl GenericClient,
    sql: &str,
    params: &[Param],
    cap: usize,
    budget: &Budget,
    out: &mut Candidates,
    map: impl Fn(&tokio_postgres::Row) -> T,
) -> Result<Vec<T>> {
    let mut rows = Vec::new();
    if !budget.apply(client).await? {
        out.timed_out = true;
        return Ok(rows);
    }
    let stream = match client
        .query_raw(
            sql,
            params.iter().map(|p| p.as_ref() as &(dyn ToSql + Sync)),
        )
        .await
    {
        Ok(stream) => stream,
        Err(error) if is_timeout(&error) => {
            out.timed_out = true;
            recover(client).await;
            return Ok(rows);
        }
        Err(error) => return Err(error).context("the line query failed"),
    };
    pin_mut!(stream);
    loop {
        match stream.try_next().await {
            Ok(Some(row)) => {
                if rows.len() == cap {
                    out.capped = true;
                    break;
                }
                if budget.exceeded() {
                    out.timed_out = true;
                    break;
                }
                rows.push(map(&row));
            }
            Ok(None) => break,
            Err(error) if is_timeout(&error) => {
                out.timed_out = true;
                recover(client).await;
                break;
            }
            Err(error) => return Err(error).context("the line query failed"),
        }
    }
    Ok(rows)
}

/// Reads candidate lines, streaming, until the cap or the statement timeout.
pub async fn candidates(
    client: &impl GenericClient,
    q: &LineQuery<'_>,
    budget: &Budget,
) -> Result<Candidates> {
    crate::history::candidates(client, q, q.history, budget).await
}

/// Lines `lo..=hi` of files, as (file id, line number) → text.
pub async fn line_ranges(
    client: &impl GenericClient,
    history: &crate::history::History,
    ranges: &[(i32, i32, i32)],
    budget: &Budget,
) -> Result<HashMap<(i32, i32), String>> {
    if ranges.is_empty() {
        return Ok(HashMap::new());
    }
    if !budget.apply(client).await? {
        bail!("the code-search time budget ran out before reading context; narrow the search");
    }
    crate::history::line_ranges(client, history, ranges)
        .await
        .context("cannot read context lines")
}

/// Merges per-line ranges into disjoint ranges per file.
pub fn merge_ranges(mut ranges: Vec<(i32, i32, i32)>) -> Vec<(i32, i32, i32)> {
    ranges.sort_unstable();
    let mut out: Vec<(i32, i32, i32)> = Vec::with_capacity(ranges.len());
    for (file, lo, hi) in ranges {
        match out.last_mut() {
            Some(last) if last.0 == file && lo <= last.2 + 1 => last.2 = last.2.max(hi),
            _ => out.push((file, lo, hi)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{Cond, Op};

    #[test]
    fn budgets_default_to_ten_seconds_and_clamp_to_fifteen() {
        for (requested, expected) in [
            (None, 10_000),
            (Some(0), 1),
            (Some(8_000), 8_000),
            (Some(12_000), 12_000),
            (Some(15_000), 15_000),
            (Some(u32::MAX), 15_000),
        ] {
            assert_eq!(Budget::new(requested).limit.as_millis(), expected);
        }
    }

    #[test]
    fn builds_the_line_filters() {
        let positive = Filter::Any {
            conds: vec![
                Cond {
                    op: Op::Like,
                    pattern: "%a%".into(),
                    literal: None,
                },
                Cond {
                    op: Op::ReI,
                    pattern: "b+".into(),
                    literal: None,
                },
            ],
            exact: true,
        };
        let negative = Filter::Any {
            conds: vec![Cond {
                op: Op::Like,
                pattern: "%c%".into(),
                literal: None,
            }],
            exact: true,
        };
        let inexact = Filter::Any {
            conds: vec![Cond {
                op: Op::Re,
                pattern: ".".into(),
                literal: None,
            }],
            exact: false,
        };
        let ids = [1, 2];
        let h = crate::history::History {
            schema: "pghacker".into(),
            addr: 7,
            build: "build".into(),
        };
        let q = LineQuery {
            history: &h,
            mounted: false,
            file_ids: Some(&ids),
            require: vec![&positive],
            reject: vec![&negative, &inexact],
            cap: 10,
            scope_first: false,
        };
        let (mut sql, mut params) = (String::new(), Vec::new());
        push_filters(&mut sql, &mut params, &q);
        assert_eq!(
            sql,
            " AND (l.line LIKE $1::text OR l.line ~* $2::text) AND NOT (l.line LIKE $3::text)"
        );
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn scopes_short_patterns_before_filtering() {
        let weak = Filter::Any {
            conds: vec![Cond {
                op: Op::Like,
                pattern: "%for (%".into(),
                literal: None,
            }],
            exact: true,
        };
        let strong = Filter::Any {
            conds: vec![Cond {
                op: Op::Like,
                pattern: "%makeNode%".into(),
                literal: None,
            }],
            exact: true,
        };
        assert!(scope_first(10, &[&strong]));
        assert!(scope_first(1_000_000, &[&weak]));
        assert!(scope_first(1_000_000, &[&Filter::All { exact: false }]));
        assert!(!scope_first(1_000_000, &[&weak, &strong]));
    }

    #[test]
    fn merges_ranges() {
        assert_eq!(
            merge_ranges(vec![(1, 5, 7), (1, 1, 3), (1, 4, 4), (2, 1, 2), (1, 9, 9)]),
            vec![(1, 1, 7), (1, 9, 9), (2, 1, 2)]
        );
    }
}
