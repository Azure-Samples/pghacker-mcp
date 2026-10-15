//! Against the real archive: run with `cargo test -p pghacker-mail --test
//! live -- --ignored`. Two requests, then the parsers must agree with what
//! the archive serves.

use pghacker_core::config::Env;
use pghacker_mail::{ArchiveClient, ArchiveConfig, parse};
use tempfile::TempDir;

/// The SQL/PGQ thread, one of the test slices' seeds.
const SEED: &str = "CAExHW5suNPNFcphpabGmJnmF6EmQ2=oJ+KLWAE+o6WWScaSuDQ@mail.gmail.com";

#[tokio::test]
#[ignore = "retrieves from www.postgresql.org"]
async fn the_parsers_read_what_the_archive_serves() {
    let cache = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let config = ArchiveConfig::from_env(&Env::process(), cache.path().to_path_buf()).unwrap();
    let client = ArchiveClient::new(config).unwrap();

    let ids = client.thread_message_ids(SEED).await.unwrap();
    assert!(ids.len() > 100, "{} messages", ids.len());
    assert!(ids.iter().any(|id| id == SEED));
    assert!(ids.iter().all(|id| !id.contains(['<', '>', ' ', '&'])));

    let raw = client.raw_message(SEED).await.unwrap();
    assert!(raw.starts_with(b"From "));
    let msg = parse::parse(&raw).unwrap();
    assert_eq!(msg.message_id, SEED);
    assert!(msg.sent_at.is_some() && msg.envelope_date.is_some());
    assert!(!msg.from_name.is_empty() && !msg.from_name.contains('@'));
    assert!(msg.from_email.is_some());
    assert!(msg.subject.contains("SQL/PGQ"), "{}", msg.subject);
    // The thread's root, first on the page, is among its ancestors.
    assert!(
        msg.references.contains(&ids[0]) || msg.in_reply_to.as_ref() == Some(&ids[0]),
        "{:?} {:?}",
        msg.in_reply_to,
        msg.references
    );
    assert!(!msg.body.is_empty());

    // Both are cached now.
    assert!(client.is_raw_message_cached(SEED).await);
    assert!(client.flat_page_path(SEED).is_file());
}
