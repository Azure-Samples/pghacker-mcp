//! Catalog readiness for concrete AGE and mapping-table access paths.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio_postgres::{GenericClient, Transaction};

use crate::migrate::quote_ident;
use crate::{EDGE_LABELS, VERTEX_LABELS};

const CATALOG: &str = include_str!("../../../config/graph-indexes.sql");

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    EdgeId,
    EdgeEndpoint,
    VertexId,
    MappingEndpoint,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    Missing,
    Invalid,
    NameConflict,
    MissingRelation,
    Unsupported,
}

#[derive(Debug, Deserialize)]
pub struct Requirement {
    pub kind: Kind,
    pub label: Option<String>,
    pub schema: String,
    pub table: String,
    pub table_oid: Option<u32>,
    pub owner: Option<String>,
    pub columns: Vec<String>,
    pub index_name: String,
    pub state: State,
    pub usable_indexes: Vec<String>,
}

pub async fn inspect(client: &(impl GenericClient + Sync)) -> Result<Vec<Requirement>> {
    let value: serde_json::Value = client
        .query_one(CATALOG, &[])
        .await
        .context("cannot inspect graph index catalogs")?
        .get(0);
    serde_json::from_value(value).context("invalid graph index catalog response")
}

fn require_ready(requirement: &Requirement) -> Result<()> {
    if requirement.state != State::Ready {
        bail!(
            "graph index prerequisite {:?}: {}.{} ({}) as owner {}; \
             run scripts/graph-indexes.mjs --plan and obtain preparation approval before upgrade",
            requirement.state,
            quote_ident(&requirement.schema),
            quote_ident(&requirement.table),
            requirement.columns.join(", "),
            requirement.owner.as_deref().unwrap_or("<missing relation>")
        );
    }
    Ok(())
}

fn check_labels(requirements: &[Requirement], vertices: &[&str], edges: &[&str]) -> Result<()> {
    for (labels, kind) in [(vertices, Kind::VertexId), (edges, Kind::EdgeId)] {
        for label in labels {
            let requirement = requirements
                .iter()
                .find(|r| r.kind == kind && r.label.as_deref() == Some(label))
                .with_context(|| {
                    format!("the graph has no registered label {label}; run bootstrap")
                })?;
            require_ready(requirement)?;
        }
    }
    for requirement in requirements.iter().filter(|r| {
        r.kind == Kind::EdgeEndpoint && edges.contains(&r.label.as_deref().unwrap_or(""))
    }) {
        require_ready(requirement)?;
    }
    Ok(())
}

pub(crate) async fn check_age(
    client: &(impl GenericClient + Sync),
    vertices: &[&str],
    edges: &[&str],
) -> Result<()> {
    check_labels(&inspect(client).await?, vertices, edges)
}

/// Re-checks catalogs even when the index migration is already recorded.
pub async fn check(client: &(impl GenericClient + Sync)) -> Result<()> {
    let requirements = inspect(client).await?;
    check_labels(&requirements, VERTEX_LABELS, EDGE_LABELS)?;
    let mappings: Vec<_> = requirements
        .iter()
        .filter(|r| r.kind == Kind::MappingEndpoint)
        .collect();
    if mappings.len() != 2 {
        bail!("the catalog has no complete mapping endpoint index contract");
    }
    for requirement in mappings {
        require_ready(requirement)?;
    }
    Ok(())
}

pub(crate) async fn index_new_labels(tx: &Transaction<'_>, labels: &[String]) -> Result<()> {
    let requirements = inspect(tx).await?;
    for label in labels {
        let requirement = requirements
            .iter()
            .find(|r| r.kind == Kind::EdgeId && r.label.as_ref() == Some(label))
            .with_context(|| format!("the new graph label {label} is missing"))?;
        match requirement.state {
            State::Ready => {}
            State::Missing => {
                tx.batch_execute(&format!(
                    "CREATE INDEX {} ON ONLY {}.{} USING btree (\"id\")",
                    quote_ident(&requirement.index_name),
                    quote_ident(&requirement.schema),
                    quote_ident(&requirement.table),
                ))
                .await
                .with_context(|| format!("cannot index the newly created graph label {label}"))?;
            }
            _ => require_ready(requirement)?,
        }
    }
    Ok(())
}
