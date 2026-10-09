//! The archive client against a local stand-in for the archive: retries,
//! authentication, encoding, caching and month refreshes.

use std::sync::{Arc, Mutex};

use chrono::{Datelike, Utc};
use pghacker_mail::{ArchiveClient, ArchiveConfig};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
struct Request {
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

fn response(status: u16, body: &[u8]) -> Response {
    Response {
        status,
        headers: Vec::new(),
        body: body.to_vec(),
    }
}

type Handler = Arc<dyn Fn(&Request, usize) -> Response + Send + Sync>;

/// An HTTP/1.1 server on a free port that answers with `handler`, given the
/// request and how many requests came before it; and the requests it saw.
async fn serve(handler: Handler) -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                loop {
                    let head_end = loop {
                        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                        let mut chunk = [0u8; 4096];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                    buffer.drain(..head_end + 4);
                    let mut lines = head.split("\r\n");
                    let path = lines
                        .next()
                        .and_then(|l| l.split(' ').nth(1))
                        .unwrap_or("")
                        .to_owned();
                    let headers = lines
                        .filter_map(|l| l.split_once(": "))
                        .map(|(n, v)| (n.to_owned(), v.to_owned()))
                        .collect();
                    let request = Request { path, headers };
                    let index = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(request.clone());
                        seen.len() - 1
                    };
                    let reply = handler(&request, index);
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\ncontent-length: {}\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        out.push_str(&format!("{name}: {value}\r\n"));
                    }
                    out.push_str("\r\n");
                    let mut bytes = out.into_bytes();
                    bytes.extend_from_slice(&reply.body);
                    if socket.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (format!("http://{address}"), log)
}

fn client(base_url: &str, cache: &TempDir) -> ArchiveClient {
    ArchiveClient::new(ArchiveConfig {
        base_url: base_url.to_owned(),
        user: "archives".into(),
        password: "antispam".into(),
        requests_per_second: 50.0,
        cache_dir: cache.path().to_path_buf(),
        user_agent: "pghacker-mail tests".into(),
    })
    .unwrap()
}

fn scratch() -> TempDir {
    TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

const RAW: &[u8] =
    b"From list-owner@lists.example.org Mon Jun 01 00:00:00 2026\nMessage-ID: <a+b=c/d@example.org>\nSubject: x\n\nbody\n";

#[tokio::test]
async fn retries_refusals_authenticates_and_caches_raw_messages() {
    let (url, log) = serve(Arc::new(|_: &Request, n: usize| match n {
        0 => Response {
            status: 503,
            headers: vec![("retry-after", "0".into())],
            body: Vec::new(),
        },
        1 => Response {
            status: 429,
            headers: vec![("retry-after", "0".into())],
            body: Vec::new(),
        },
        _ => response(200, RAW),
    }))
    .await;
    let cache = scratch();
    let client = client(&url, &cache);
    assert_eq!(
        client.raw_message("<a+b=c/d@example.org>").await.unwrap(),
        RAW
    );
    assert_eq!(
        client.raw_message("a+b=c/d@example.org").await.unwrap(),
        RAW
    );
    let requests = log.lock().unwrap().clone();
    assert_eq!(requests.len(), 3, "the second read comes from the cache");
    let request = &requests[2];
    assert_eq!(request.path, "/message-id/raw/a%2Bb%3Dc%2Fd%40example.org");
    // archives:antispam
    assert_eq!(
        request.header("authorization"),
        Some("Basic YXJjaGl2ZXM6YW50aXNwYW0=")
    );
    assert_eq!(request.header("user-agent"), Some("pghacker-mail tests"));
    assert!(client.is_raw_message_cached("a+b=c/d@example.org").await);
}

#[tokio::test]
async fn missing_and_hidden_messages_are_errors_and_not_cached() {
    let (url, log) = serve(Arc::new(|request: &Request, _| {
        if request.path.contains("missing") {
            response(404, b"<html>Not found</html>")
        } else {
            response(200, b"This message has been hidden.")
        }
    }))
    .await;
    let cache = scratch();
    let client = client(&url, &cache);
    let error = client.raw_message("missing@example.org").await.unwrap_err();
    assert!(
        error.downcast_ref::<pghacker_mail::NotFound>().is_some(),
        "{error:#}"
    );
    assert!(client.raw_message("hidden@example.org").await.is_err());
    assert!(client.raw_message("hidden@example.org").await.is_err());
    assert!(!client.is_raw_message_cached("hidden@example.org").await);
    assert_eq!(log.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn lists_thread_messages_from_the_flat_page() {
    let page = "<table><tr><th scope=\"row\">Message-ID:</th>\n<td><a href=\"/message-id/r%40example.org\">r@example.org</a></td></tr>\
                <tr><th scope=\"row\">Message-ID:</th>\n<td><a href=\"/message-id/s%40example.org\">s@example.org</a></td></tr></table>";
    let (url, log) = serve(Arc::new(move |_: &Request, _| {
        response(200, page.as_bytes())
    }))
    .await;
    let cache = scratch();
    let client = client(&url, &cache);
    let ids = client.thread_message_ids("s@example.org").await.unwrap();
    assert_eq!(ids, ["r@example.org", "s@example.org"]);
    assert_eq!(
        client.thread_message_ids("s@example.org").await.unwrap(),
        ids
    );
    assert_eq!(
        log.lock().unwrap()[0].path,
        "/message-id/flat/s%40example.org"
    );
    assert_eq!(log.lock().unwrap().len(), 1);
    client
        .thread_message_ids_with("s@example.org", true)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
}

const MBOX_V1: &[u8] =
    b"From a@example.org Mon Jun 01 00:00:00 2026\nMessage-ID: <1@example.org>\n\none\n";
const MBOX_V2: &[u8] = b"From a@example.org Mon Jun 01 00:00:00 2026\nMessage-ID: <1@example.org>\n\none\n\nFrom a@example.org Mon Jun 01 00:00:01 2026\nMessage-ID: <2@example.org>\n\ntwo\n";

#[tokio::test]
async fn complete_months_come_from_the_cache() {
    let (url, log) = serve(Arc::new(|_: &Request, _| response(200, MBOX_V1))).await;
    let cache = scratch();
    let client = client(&url, &cache);
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", "2026-05", false)
            .await
            .unwrap(),
        MBOX_V1
    );
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", "2026-05", true)
            .await
            .unwrap(),
        MBOX_V1
    );
    let requests = log.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].path,
        "/list/pgsql-hackers/mbox/pgsql-hackers.202605"
    );
    assert!(
        client
            .mbox_month("pgsql-hackers", "2026-5", false)
            .await
            .is_err()
    );
    assert!(client.mbox_month("../etc", "2026-05", false).await.is_err());
}

#[tokio::test]
async fn the_current_month_refreshes_conditionally() {
    let (url, log) = serve(Arc::new(|request: &Request, n: usize| {
        if request.header("if-none-match") == Some("\"v1\"") && n < 2 {
            return response(304, b"");
        }
        let (etag, body) = if n == 0 {
            ("\"v1\"", MBOX_V1)
        } else {
            ("\"v2\"", MBOX_V2)
        };
        Response {
            status: 200,
            headers: vec![("etag", etag.into())],
            body: body.to_vec(),
        }
    }))
    .await;
    let cache = scratch();
    let client = client(&url, &cache);
    let now = Utc::now();
    let month = format!("{:04}-{:02}", now.year(), now.month());
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", &month, false)
            .await
            .unwrap(),
        MBOX_V1
    );
    // Without refresh, the cached copy.
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", &month, false)
            .await
            .unwrap(),
        MBOX_V1
    );
    assert_eq!(log.lock().unwrap().len(), 1);
    // Refreshing asks whether it changed: not yet, then yes.
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", &month, true)
            .await
            .unwrap(),
        MBOX_V1
    );
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", &month, true)
            .await
            .unwrap(),
        MBOX_V2
    );
    let requests = log.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].header("if-none-match"), Some("\"v1\""));
    assert_eq!(
        client
            .mbox_month("pgsql-hackers", &month, false)
            .await
            .unwrap(),
        MBOX_V2
    );
}

#[tokio::test]
async fn the_rate_limit_spaces_requests() {
    let (url, _) = serve(Arc::new(|_: &Request, _| response(200, RAW))).await;
    let cache = scratch();
    let client = ArchiveClient::new(ArchiveConfig {
        requests_per_second: 10.0,
        ..client(&url, &cache).config().clone()
    })
    .unwrap();
    let start = std::time::Instant::now();
    let clones: Vec<_> = (0..4)
        .map(|i| {
            let client = client.clone();
            tokio::spawn(async move { client.raw_message(&format!("{i}@example.org")).await })
        })
        .collect();
    for task in clones {
        task.await.unwrap().unwrap();
    }
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(300),
        "{:?}",
        start.elapsed()
    );
}
