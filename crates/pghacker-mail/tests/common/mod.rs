//! What the tests share: mail stored as ingest once stored it, and a
//! message's stored text.

use pghacker_core::text::redact_emails;
use pghacker_mail::{chunk, parse};
use tokio_postgres::Client;

/// Rewrites a stored message as ingest once stored it, before it kept email
/// addresses: each replaced by `[email]`, the sender's name without them,
/// with its chunks made of that text, its patches indexed and its author's
/// name counted that way.
pub async fn store_as_once(client: &Client, message_id: &str, email: Option<&str>) {
    let row = client
        .query_one(
            "SELECT id, author_id, subject, from_name, body, body_clean FROM messages WHERE message_id = $1",
            &[&message_id],
        )
        .await
        .unwrap();
    let (id, author, name): (i64, i64, String) = (row.get(0), row.get(1), row.get(3));
    let redact = |text: &str| redact_emails(text).into_owned();
    let once_name = redact(&parse::display_name_without_addresses(&name, email));
    let subject = redact(row.get(2));
    let body = redact(row.get(4));
    let clean = redact(row.get(5));
    client
        .execute(
            "UPDATE messages SET subject = $2, from_name = $3, body = $4, body_clean = $5 WHERE id = $1",
            &[&id, &subject, &once_name, &body, &clean],
        )
        .await
        .unwrap();
    for row in client
        .query(
            "SELECT id, filename, content FROM patches WHERE message_id = $1",
            &[&id],
        )
        .await
        .unwrap()
    {
        let (patch, filename, content): (i64, String, String) =
            (row.get(0), row.get(1), row.get(2));
        client
            .execute(
                "UPDATE patches SET filename = $2, content = $3, text_ids = '{1}' WHERE id = $1",
                &[&patch, &redact(&filename), &redact(&content)],
            )
            .await
            .unwrap();
    }
    client
        .execute("DELETE FROM message_chunks WHERE message_id = $1", &[&id])
        .await
        .unwrap();
    for (ord, content) in chunk::chunk(&subject, &clean).iter().enumerate() {
        client
            .execute(
                "INSERT INTO message_chunks (message_id, thread_id, ord, content)
                 SELECT id, thread_id, $2, $3 FROM messages WHERE id = $1",
                &[&id, &(ord as i32), content],
            )
            .await
            .unwrap();
    }
    client
        .execute(
            "UPDATE author_names SET name = $3 WHERE author_id = $1 AND name = $2",
            &[&author, &name, &once_name],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE authors SET display_name = $2 WHERE id = $1",
            &[&author, &once_name],
        )
        .await
        .unwrap();
}

/// A message's stored text: its own, its patches', its chunks', its thread's
/// subject and its author's names.
pub async fn stored_text(client: &Client, message_id: &str) -> Vec<String> {
    let mut text = Vec::new();
    for sql in [
        "SELECT unnest(ARRAY[m.subject, m.from_name, m.body, m.body_clean, t.subject, a.display_name])
         FROM messages m JOIN threads t ON t.id = m.thread_id JOIN authors a ON a.id = m.author_id
         WHERE m.message_id = $1",
        "SELECT p.filename || ': ' || p.content FROM patches p JOIN messages m ON m.id = p.message_id
         WHERE m.message_id = $1 ORDER BY p.filename",
        "SELECT c.ord || ': ' || c.content FROM message_chunks c JOIN messages m ON m.id = c.message_id
         WHERE m.message_id = $1 ORDER BY c.ord",
        "SELECT n.name || ': ' || n.count FROM author_names n JOIN messages m ON m.author_id = n.author_id
         WHERE m.message_id = $1 ORDER BY n.name",
    ] {
        for row in client.query(sql, &[&message_id]).await.unwrap() {
            text.push(row.get(0));
        }
    }
    text
}
