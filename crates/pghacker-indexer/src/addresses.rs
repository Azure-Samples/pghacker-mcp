//! Giving mail stored before ingest kept email addresses its addresses back.
//! Ingest once stored text with each address replaced by `[email]`, and
//! senders' names without addresses; it keeps no raw messages, so the mail
//! is retrieved again. [`queue_restore`] queues `restore_addresses` jobs, as
//! migration 0015's `restore_addresses()` does: one per stored month of each
//! list, which reads the month's mbox again, and one per batch of the
//! messages that came from thread pages, which retrieves them raw. Each
//! restores only text stored exactly as ingest once made it of the archive's
//! (`pghacker_mail::store::restore_addresses`), so running it again changes
//! nothing more.

use anyhow::Result;
use serde::Serialize;
use tokio_postgres::GenericClient;

/// What [`queue_restore`] queued.
#[derive(Debug, Default, Serialize)]
pub struct Queued {
    /// Months, each read again.
    pub months: u64,
    /// Batches of messages from thread pages, and the messages.
    pub batches: u64,
    pub messages: u64,
}

/// Queues the jobs that restore the stored mail's email addresses. A job
/// already queued or running is left as it is; a finished one is queued
/// again.
pub async fn queue_restore(client: &impl GenericClient) -> Result<Queued> {
    let row = client
        .query_one(
            "SELECT months, batches, messages FROM restore_addresses()",
            &[],
        )
        .await?;
    Ok(Queued {
        months: row.get::<_, i64>(0) as u64,
        batches: row.get::<_, i64>(1) as u64,
        messages: row.get::<_, i64>(2) as u64,
    })
}
