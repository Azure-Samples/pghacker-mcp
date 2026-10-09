//! The PostgreSQL side of a search: candidate lines through pg_trgm, the time
//! budget, and the lines around matches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{TryStreamExt, pin_mut};
use tokio_postgres::GenericClient;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;

use crate::pattern::Filter;

/// The default and largest time budgets, in milliseconds.
pub const DEFAULT_BUDGET_MS: u32 = 4_000;
pub const MAX_BUDGET_MS: u32 = 10_000;

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
}

/// Sets the session's statement_timeout, returning the previous value.
pub async fn set_timeout(client: &impl GenericClient, ms: u64) -> Result<String> {
    let row = client
        .query_one(
            "SELECT current_setting('statement_timeout'), \
                    set_config('statement_timeout', $1, false)",
            &[&format!("{ms}ms")],
        )
        .await
        .context("cannot set statement_timeout")?;
    Ok(row.get(0))
}

/// Restores statement_timeout; a failure (in an aborted transaction) is
/// left for the caller's rollback to undo.
pub async fn restore_timeout(client: &impl GenericClient, old: &str) {
    let _ = client
        .execute("SELECT set_config('statement_timeout', $1, false)", &[&old])
        .await;
}

/// Whether `error` is a cancellation by statement_timeout.
pub fn is_timeout(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::QUERY_CANCELED)
}

/// Lines to read: in these files (or the whole snapshot), matching every
/// `require` filter and none of the exact `reject` filters.
pub struct LineQuery<'a> {
    pub snapshot_id: i32,
    pub file_ids: Option<&'a [i32]>,
    pub require: Vec<&'a Filter>,
    pub reject: Vec<&'a Filter>,
    pub cap: usize,
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

/// The query's SQL and parameters.
pub fn line_sql(q: &LineQuery<'_>) -> (String, Vec<Param>) {
    let mut params: Vec<Param> = Vec::new();
    let mut sql = String::from("SELECT l.file_id, l.line_no, l.line FROM code_lines l WHERE ");
    match q.file_ids {
        Some(ids) => {
            params.push(Box::new(ids.to_vec()));
            sql.push_str("l.file_id = ANY($1::int4[])");
        }
        None => {
            params.push(Box::new(q.snapshot_id));
            sql.push_str(
                "l.file_id = ANY(ARRAY(SELECT id FROM code_files WHERE snapshot_id = $1))",
            );
        }
    }
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
            let or = ors(conds, &mut params);
            sql.push_str(&format!(" AND ({or})"));
        }
    }
    for filter in &q.reject {
        match filter {
            Filter::Any { conds, exact: true } => {
                let or = ors(conds, &mut params);
                sql.push_str(&format!(" AND NOT ({or})"));
            }
            Filter::All { exact: true } => sql.push_str(" AND false"),
            _ => {}
        }
    }
    sql.push_str(&format!(" LIMIT {}", q.cap + 1));
    (sql, params)
}

/// Reads candidate lines, streaming, until the cap or the statement timeout.
pub async fn candidates(client: &impl GenericClient, q: &LineQuery<'_>) -> Result<Candidates> {
    let (sql, params) = line_sql(q);
    let mut out = Candidates::default();
    let stream = match client
        .query_raw(
            sql.as_str(),
            params.iter().map(|p| p.as_ref() as &(dyn ToSql + Sync)),
        )
        .await
    {
        Ok(stream) => stream,
        Err(error) if is_timeout(&error) => {
            out.timed_out = true;
            return Ok(out);
        }
        Err(error) => return Err(error).context("the line query failed"),
    };
    pin_mut!(stream);
    loop {
        match stream.try_next().await {
            Ok(Some(row)) => {
                if out.rows.len() == q.cap {
                    out.capped = true;
                    break;
                }
                out.rows.push(Row {
                    file_id: row.get(0),
                    line_no: row.get(1),
                    line: row.get(2),
                });
            }
            Ok(None) => break,
            Err(error) if is_timeout(&error) => {
                out.timed_out = true;
                break;
            }
            Err(error) => return Err(error).context("the line query failed"),
        }
    }
    Ok(out)
}

/// Lines `lo..=hi` of files, as (file id, line number) → text.
pub async fn line_ranges(
    client: &impl GenericClient,
    ranges: &[(i32, i32, i32)],
) -> Result<HashMap<(i32, i32), String>> {
    let mut out = HashMap::new();
    if ranges.is_empty() {
        return Ok(out);
    }
    let ids: Vec<i32> = ranges.iter().map(|r| r.0).collect();
    let los: Vec<i32> = ranges.iter().map(|r| r.1).collect();
    let his: Vec<i32> = ranges.iter().map(|r| r.2).collect();
    let rows = client
        .query(
            "SELECT l.file_id, l.line_no, l.line \
             FROM unnest($1::int4[], $2::int4[], $3::int4[]) AS r(fid, lo, hi) \
             JOIN code_lines l ON l.file_id = r.fid AND l.line_no BETWEEN r.lo AND r.hi",
            &[&ids, &los, &his],
        )
        .await
        .context("cannot read context lines")?;
    for row in rows {
        out.insert((row.get(0), row.get(1)), row.get(2));
    }
    Ok(out)
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
    fn builds_the_line_query() {
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
        let q = LineQuery {
            snapshot_id: 7,
            file_ids: Some(&ids),
            require: vec![&positive],
            reject: vec![&negative, &inexact],
            cap: 10,
        };
        let (sql, params) = line_sql(&q);
        assert_eq!(
            sql,
            "SELECT l.file_id, l.line_no, l.line FROM code_lines l WHERE \
             l.file_id = ANY($1::int4[]) AND (l.line LIKE $2::text OR l.line ~* $3::text) \
             AND NOT (l.line LIKE $4::text) LIMIT 11"
        );
        assert_eq!(params.len(), 4);
    }

    #[test]
    fn merges_ranges() {
        assert_eq!(
            merge_ranges(vec![(1, 5, 7), (1, 1, 3), (1, 4, 4), (2, 1, 2), (1, 9, 9)]),
            vec![(1, 1, 7), (1, 9, 9), (2, 1, 2)]
        );
    }
}
