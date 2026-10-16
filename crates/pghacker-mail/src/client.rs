// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The public archive at www.postgresql.org: monthly mbox archives, flat
//! thread pages and raw messages, behind a rate limit shared by all clones of
//! a client, retries with backoff, and a disk cache.
//!
//! The cache keeps raw messages and flat pages forever. A month's mbox is kept
//! forever once it was retrieved after the month ended (plus a grace period
//! for late deliveries); until then, `refresh` retrieves it again, with a
//! conditional request when the archive sent validators. Without
//! `keep_mbox`, only the validators are kept: a month ingested in full is
//! retrieved again only when it may have changed, and only if it did.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, Utc};
use pghacker_core::config::Env;
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::flat;

/// The User-Agent sent unless `PGHACKER_HTTP_USER_AGENT` overrides it.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "pghacker-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (pgsql-hackers archive indexer; cached, rate-limited)"
);

/// How long after a month ends its mbox may still change: messages held for
/// moderation arrive late.
const MONTH_GRACE: TimeDelta = TimeDelta::days(2);
const MAX_ATTEMPTS: u32 = 6;
const MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Where the archive is, how to sign in, how fast to go, and where to cache.
#[derive(Clone)]
pub struct ArchiveConfig {
    /// Such as `https://www.postgresql.org`, without a trailing slash.
    pub base_url: String,
    /// The archive's published anti-scraper credentials.
    pub user: String,
    pub password: String,
    /// The limit for all requests of a client and its clones.
    pub requests_per_second: f64,
    pub cache_dir: PathBuf,
    pub user_agent: String,
    /// Whether months' mbox files stay in the cache once ingested.
    pub keep_mbox: bool,
}

impl fmt::Debug for ArchiveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveConfig")
            .field("base_url", &self.base_url)
            .field("user", &self.user)
            .field("password", &"***")
            .field("requests_per_second", &self.requests_per_second)
            .field("cache_dir", &self.cache_dir)
            .field("user_agent", &self.user_agent)
            .field("keep_mbox", &self.keep_mbox)
            .finish()
    }
}

impl ArchiveConfig {
    /// `PGHACKER_ARCHIVE_BASE_URL` (default `https://www.postgresql.org`),
    /// `PGHACKER_ARCHIVE_USER` (`archives`), `PGHACKER_ARCHIVE_PASSWORD`
    /// (`antispam`), `PGHACKER_HTTP_RATE` (2 requests per second),
    /// `PGHACKER_HTTP_USER_AGENT` and `PGHACKER_KEEP_DOWNLOADS` (true).
    pub fn from_env(env: &Env, cache_dir: PathBuf) -> Result<Self> {
        let config = Self {
            base_url: env
                .get_or("PGHACKER_ARCHIVE_BASE_URL", "https://www.postgresql.org")
                .trim()
                .trim_end_matches('/')
                .to_owned(),
            user: env.get_or("PGHACKER_ARCHIVE_USER", "archives").to_owned(),
            password: env
                .get_or("PGHACKER_ARCHIVE_PASSWORD", "antispam")
                .to_owned(),
            requests_per_second: env.parse_or("PGHACKER_HTTP_RATE", 2.0)?,
            cache_dir,
            user_agent: env
                .get_or("PGHACKER_HTTP_USER_AGENT", DEFAULT_USER_AGENT)
                .to_owned(),
            keep_mbox: env.parse_or("PGHACKER_KEEP_DOWNLOADS", true)?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.requests_per_second.is_finite() && self.requests_per_second > 0.0,
            "PGHACKER_HTTP_RATE must be a positive number of requests per second"
        );
        let url = reqwest::Url::parse(&self.base_url)
            .with_context(|| format!("the archive URL {} is invalid", self.base_url))?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "the archive URL must be http or https"
        );
        Ok(())
    }
}

/// The archive returned 404: no such message, thread or month.
#[derive(Debug)]
pub struct NotFound {
    pub url: String,
}

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the archive has no {}", self.url)
    }
}

impl std::error::Error for NotFound {}

/// A client for the archive. Cheap to clone; clones share the rate limit.
#[derive(Clone)]
pub struct ArchiveClient {
    inner: Arc<Inner>,
}

struct Inner {
    config: ArchiveConfig,
    http: reqwest::Client,
    limiter: RateLimiter,
}

impl fmt::Debug for ArchiveClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveClient")
            .field("config", &self.inner.config)
            .finish()
    }
}

/// What the cache records about a month's mbox.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MboxMeta {
    url: String,
    fetched_at: Option<DateTime<Utc>>,
    etag: Option<String>,
    last_modified: Option<String>,
    bytes: u64,
    /// Every message of the retrieved mbox is stored.
    #[serde(default)]
    ingested: bool,
}

/// A month's mbox, or the news that it hasn't changed since it was ingested
/// in full (when the cache doesn't keep mbox files).
#[derive(Debug, PartialEq, Eq)]
pub enum Mbox {
    Data(Vec<u8>),
    Unchanged,
}

enum Fetched {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

enum Attempt {
    Done(Fetched),
    Status(StatusCode, Option<Duration>),
}

impl ArchiveClient {
    pub fn new(config: ArchiveConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("cannot build the HTTP client")?;
        let interval = Duration::from_secs_f64(1.0 / config.requests_per_second);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                limiter: RateLimiter::new(interval),
            }),
        })
    }

    pub fn config(&self) -> &ArchiveConfig {
        &self.inner.config
    }

    /// The mbox of `list` for `month` (YYYY-MM). A complete month comes from
    /// the cache; the current month is retrieved again when `refresh` is set.
    /// Without `keep_mbox`, a month ingested in full is [`Mbox::Unchanged`]
    /// unless a refresh finds that it changed.
    pub async fn mbox_month(&self, list: &str, month: &str, refresh: bool) -> Result<Mbox> {
        validate_list(list)?;
        let (year, month_no) = parse_month(month)?;
        let url = format!(
            "{}/list/{list}/mbox/{list}.{year:04}{month_no:02}",
            self.inner.config.base_url
        );
        let path = self.mbox_path(list, month);
        let meta_path = path.with_extension("mbox.json");
        let complete_at = month_end(year, month_no)? + MONTH_GRACE;
        let now = Utc::now();

        let cached = tokio::fs::metadata(&path).await.is_ok();
        let mut meta = read_meta(&meta_path).await;
        let ingested = !cached && meta.as_ref().is_some_and(|m| m.ingested);
        if cached {
            let fetched_at = match meta.as_ref().and_then(|m| m.fetched_at) {
                Some(at) => Some(at),
                None => file_mtime(&path).await,
            };
            let complete = fetched_at.is_some_and(|at| at >= complete_at);
            if complete || (!refresh && now < complete_at) {
                return read_file(&path).await.map(Mbox::Data);
            }
        } else if ingested {
            let fetched_at = meta.as_ref().and_then(|m| m.fetched_at);
            let complete = fetched_at.is_some_and(|at| at >= complete_at);
            if complete || !refresh {
                return Ok(Mbox::Unchanged);
            }
        } else {
            meta = None;
        }

        let validators = meta.as_ref().filter(|_| cached || ingested);
        match self.fetch(&url, validators).await? {
            Fetched::NotModified => {
                tracing::debug!(%url, "the mbox has not changed");
                let mut meta = meta.unwrap_or_default();
                meta.fetched_at = Some(now);
                write_meta(&meta_path, &meta).await?;
                if cached {
                    read_file(&path).await.map(Mbox::Data)
                } else {
                    Ok(Mbox::Unchanged)
                }
            }
            Fetched::Body {
                bytes,
                etag,
                last_modified,
            } => {
                ensure!(
                    bytes.is_empty() || bytes.starts_with(b"From "),
                    "{url} did not return an mbox"
                );
                if self.inner.config.keep_mbox {
                    write_atomic(&path, &bytes).await?;
                } else if cached {
                    remove_file(&path).await?;
                }
                let meta = MboxMeta {
                    url,
                    fetched_at: Some(now),
                    etag,
                    last_modified,
                    bytes: bytes.len() as u64,
                    ingested: false,
                };
                write_meta(&meta_path, &meta).await?;
                Ok(Mbox::Data(bytes))
            }
        }
    }

    /// The mbox of `list` for `month` whether or not it changed since it was
    /// ingested: the cached file, which holds every message ingested from the
    /// month, else retrieved again, leaving the cache as it is.
    pub async fn mbox_month_again(&self, list: &str, month: &str) -> Result<Vec<u8>> {
        validate_list(list)?;
        let (year, month_no) = parse_month(month)?;
        let path = self.mbox_path(list, month);
        if tokio::fs::metadata(&path).await.is_ok() {
            return read_file(&path).await;
        }
        let url = format!(
            "{}/list/{list}/mbox/{list}.{year:04}{month_no:02}",
            self.inner.config.base_url
        );
        let bytes = self.fetch_body(&url).await?;
        ensure!(
            bytes.is_empty() || bytes.starts_with(b"From "),
            "{url} did not return an mbox"
        );
        Ok(bytes)
    }

    /// Records that every message of `list`'s mbox for `month`, as last
    /// retrieved, is stored, so that a month that doesn't change isn't
    /// retrieved again when the cache doesn't keep mbox files.
    pub async fn mark_mbox_ingested(&self, list: &str, month: &str) -> Result<()> {
        let meta_path = self.mbox_path(list, month).with_extension("mbox.json");
        if let Some(mut meta) = read_meta(&meta_path).await
            && !meta.ingested
        {
            meta.ingested = true;
            write_meta(&meta_path, &meta).await?;
        }
        Ok(())
    }

    /// The months the archive has for `list`, oldest first, from the list's
    /// page; [`NotFound`] when the archive has no such list.
    pub async fn list_months(&self, list: &str) -> Result<Vec<String>> {
        validate_list(list)?;
        let url = format!("{}/list/{list}/", self.inner.config.base_url);
        let bytes = self.fetch_body(&url).await?;
        let months = parse_list_months(&String::from_utf8_lossy(&bytes), list);
        ensure!(!months.is_empty(), "{url} lists no months");
        Ok(months)
    }

    /// The Message-IDs of the thread that contains `message_id`, oldest
    /// first, from the archive's flat thread page (cached forever).
    pub async fn thread_message_ids(&self, message_id: &str) -> Result<Vec<String>> {
        self.thread_message_ids_with(message_id, false).await
    }

    /// Like [`Self::thread_message_ids`]; `refresh` retrieves the page again,
    /// for threads that may have grown since it was cached.
    pub async fn thread_message_ids_with(
        &self,
        message_id: &str,
        refresh: bool,
    ) -> Result<Vec<String>> {
        let html = self.flat_page(message_id, refresh).await?;
        let ids = flat::message_ids(&html);
        ensure!(
            !ids.is_empty(),
            "the flat page of {message_id} lists no messages"
        );
        Ok(ids)
    }

    /// The archive's flat page of the thread that contains `message_id`.
    pub async fn flat_page(&self, message_id: &str, refresh: bool) -> Result<String> {
        let id = crate::parse::normalize_message_id(message_id);
        ensure!(!id.is_empty(), "the Message-ID is empty");
        let path = self.flat_page_path(&id);
        if !refresh && let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        let url = format!(
            "{}/message-id/flat/{}",
            self.inner.config.base_url,
            encode_path_segment(&id)
        );
        let bytes = self.fetch_body(&url).await?;
        let html = String::from_utf8_lossy(&bytes).into_owned();
        ensure!(
            flat::looks_like_flat_page(&html),
            "{url} did not return a thread page"
        );
        write_atomic(&path, &bytes).await?;
        Ok(html)
    }

    /// The raw RFC 822 message (first line an mbox `From ` line), cached
    /// forever.
    pub async fn raw_message(&self, message_id: &str) -> Result<Vec<u8>> {
        let id = crate::parse::normalize_message_id(message_id);
        ensure!(!id.is_empty(), "the Message-ID is empty");
        let path = self.raw_message_path(&id);
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(bytes);
        }
        let url = format!(
            "{}/message-id/raw/{}",
            self.inner.config.base_url,
            encode_path_segment(&id)
        );
        let bytes = self.fetch_body(&url).await?;
        ensure!(
            looks_like_message(&bytes),
            "{url} did not return a message (hidden or removed?)"
        );
        write_atomic(&path, &bytes).await?;
        Ok(bytes)
    }

    /// Whether the raw message is in the cache, so that reading it costs no
    /// request.
    pub async fn is_raw_message_cached(&self, message_id: &str) -> bool {
        tokio::fs::metadata(self.raw_message_path(message_id))
            .await
            .is_ok()
    }

    /// Where the cache keeps `list`'s mbox for `month`.
    pub fn mbox_path(&self, list: &str, month: &str) -> PathBuf {
        self.inner
            .config
            .cache_dir
            .join("mbox")
            .join(list)
            .join(format!("{month}.mbox"))
    }

    /// Where the cache keeps the flat page of `message_id`'s thread.
    pub fn flat_page_path(&self, message_id: &str) -> PathBuf {
        self.id_path(
            "flat",
            &crate::parse::normalize_message_id(message_id),
            "html",
        )
    }

    /// Where the cache keeps the raw message `message_id`.
    pub fn raw_message_path(&self, message_id: &str) -> PathBuf {
        self.id_path(
            "raw",
            &crate::parse::normalize_message_id(message_id),
            "eml",
        )
    }

    fn id_path(&self, kind: &str, id: &str, extension: &str) -> PathBuf {
        let digest = hex::encode(Sha256::digest(id.as_bytes()));
        self.inner
            .config
            .cache_dir
            .join(kind)
            .join(&digest[..2])
            .join(format!("{digest}.{extension}"))
    }

    async fn fetch_body(&self, url: &str) -> Result<Vec<u8>> {
        match self.fetch(url, None).await? {
            Fetched::Body { bytes, .. } => Ok(bytes),
            Fetched::NotModified => bail!("{url} returned 304 to an unconditional request"),
        }
    }

    /// GET with the rate limit, basic auth and retries on 429, 5xx and
    /// transport errors.
    async fn fetch(&self, url: &str, validators: Option<&MboxMeta>) -> Result<Fetched> {
        let config = &self.inner.config;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.inner.limiter.acquire().await;
            let mut request = self
                .inner
                .http
                .get(url)
                .basic_auth(&config.user, Some(&config.password));
            if let Some(meta) = validators {
                if let Some(etag) = &meta.etag {
                    request = request.header(IF_NONE_MATCH, etag);
                }
                if let Some(modified) = &meta.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, modified);
                }
            }
            let started = Instant::now();
            let result: Result<Attempt, reqwest::Error> = async {
                let response = request.send().await?;
                let status = response.status();
                if status == StatusCode::NOT_MODIFIED {
                    return Ok(Attempt::Done(Fetched::NotModified));
                }
                if !status.is_success() {
                    let retry_after = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    return Ok(Attempt::Status(status, retry_after));
                }
                let header = |name| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
                        .map(str::to_owned)
                };
                let etag = header(ETAG);
                let last_modified = header(LAST_MODIFIED);
                let bytes = response.bytes().await?.to_vec();
                Ok(Attempt::Done(Fetched::Body {
                    bytes,
                    etag,
                    last_modified,
                }))
            }
            .await;
            let delay = match result {
                Ok(Attempt::Done(fetched)) => {
                    tracing::debug!(
                        %url,
                        ms = started.elapsed().as_millis() as u64,
                        "retrieved from the archive"
                    );
                    return Ok(fetched);
                }
                Ok(Attempt::Status(StatusCode::NOT_FOUND, _)) => {
                    return Err(anyhow!(NotFound {
                        url: url.to_owned()
                    }));
                }
                Ok(Attempt::Status(status, retry_after))
                    if attempt < MAX_ATTEMPTS
                        && (status == StatusCode::TOO_MANY_REQUESTS
                            || status.is_server_error()) =>
                {
                    tracing::warn!(%url, %status, attempt, "the archive refused; retrying");
                    retry_after
                        .map(|d| d.min(Duration::from_secs(600)))
                        .unwrap_or_else(|| backoff(attempt))
                }
                Ok(Attempt::Status(status, _)) => bail!("{url} returned {status}"),
                Err(error) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(%url, %error, attempt, "the archive request failed; retrying");
                    backoff(attempt)
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot retrieve {url}"));
                }
            };
            tokio::time::sleep(delay).await;
        }
    }
}

/// `segment` percent-encoded for a URL path: everything but RFC 3986's
/// unreserved characters, as the archive's own links encode Message-IDs.
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len() * 3 / 2);
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The months (YYYY-MM) that a list's archive page links to, oldest first:
/// links such as `/list/pgsql-hackers/2026-07/`.
pub fn parse_list_months(html: &str, list: &str) -> Vec<String> {
    let needle = format!("/list/{list}/");
    let mut months = std::collections::BTreeSet::new();
    let mut rest = html;
    while let Some(at) = rest.find(&needle) {
        rest = &rest[at + needle.len()..];
        if let Some(month) = rest.get(..8)
            && month.ends_with('/')
            && parse_month(&month[..7]).is_ok()
        {
            months.insert(month[..7].to_owned());
        }
    }
    months.into_iter().collect()
}

/// One request per interval, in arrival order, across all clones.
struct RateLimiter {
    interval: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(Instant::now()),
        }
    }

    async fn acquire(&self) {
        let at = {
            let mut next = self.next.lock().await;
            let at = (*next).max(Instant::now());
            *next = at + self.interval;
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(7)).min(MAX_BACKOFF)
}

fn validate_list(list: &str) -> Result<()> {
    ensure!(
        !list.is_empty()
            && list
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'),
        "{list:?} is not a mailing list name"
    );
    Ok(())
}

/// `YYYY-MM` as (year, month).
pub(crate) fn parse_month(month: &str) -> Result<(i32, u32)> {
    let invalid = || anyhow!("{month:?} is not a month (YYYY-MM)");
    let (year, month_no) = month.split_once('-').ok_or_else(invalid)?;
    ensure!(
        year.len() == 4 && month_no.len() == 2,
        "{month:?} is not a month (YYYY-MM)"
    );
    let year: i32 = year.parse().map_err(|_| invalid())?;
    let month_no: u32 = month_no.parse().map_err(|_| invalid())?;
    ensure!((1..=12).contains(&month_no), invalid());
    Ok((year, month_no))
}

fn month_end(year: i32, month: u32) -> Result<DateTime<Utc>> {
    let (year, month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let start = NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .ok_or_else(|| anyhow!("invalid month"))?;
    debug_assert!(start.year() == year);
    Ok(start.and_utc())
}

fn looks_like_message(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    if head.starts_with(b"From ") {
        return true;
    }
    let text = String::from_utf8_lossy(head);
    let first = text.lines().next().unwrap_or("");
    !first.starts_with('<')
        && first
            .split_once(':')
            .is_some_and(|(name, _)| !name.is_empty() && !name.contains(' '))
}

async fn read_file(path: &Path) -> Result<Vec<u8>> {
    tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))
}

async fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let modified: SystemTime = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    Some(modified.into())
}

async fn remove_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

async fn read_meta(path: &Path) -> Option<MboxMeta> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_meta(path: &Path, meta: &MboxMeta) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(meta)?).await
}

/// Writes `bytes` to a temporary file next to `path`, then renames it, so a
/// reader never sees a partial file.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", path.display()))?;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let temporary = dir.join(format!(
        ".{}.{}.{}.partial",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("cache"),
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    tokio::fs::write(&temporary, bytes)
        .await
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_message_ids_like_the_archive() {
        assert_eq!(
            encode_path_segment("CA+x=y/z@mail.example.org"),
            "CA%2Bx%3Dy%2Fz%40mail.example.org"
        );
        assert_eq!(encode_path_segment("a b%#?é"), "a%20b%25%23%3F%C3%A9");
    }

    #[test]
    fn months_parse_and_end() {
        assert_eq!(parse_month("2026-06").unwrap(), (2026, 6));
        assert!(parse_month("2026-13").is_err());
        assert!(parse_month("202606").is_err());
        assert!(parse_month("26-06").is_err());
        assert_eq!(
            month_end(2026, 12).unwrap().to_rfc3339(),
            "2027-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn list_pages_link_their_months() {
        let html = r#"<a href="/list/pgsql-hackers/2026-07/">July</a>
            <a href="/list/pgsql-hackers/1997-06/">June</a><a href="/list/pgsql-hackers/2026-07/">again</a>
            <a href="/list/pgsql-hackers/since/202607010000/">since</a>
            <a href="/list/pgsql-general/2001-01/">another list</a><a href="/list/pgsql-hackers/2026-13/">bad</a>"#;
        assert_eq!(
            parse_list_months(html, "pgsql-hackers"),
            ["1997-06", "2026-07"]
        );
        assert!(parse_list_months("<html></html>", "pgsql-hackers").is_empty());
    }

    #[test]
    fn config_comes_from_the_environment() {
        let env = Env::from_pairs([("PGHACKER_HTTP_RATE", "0.5")]);
        let config = ArchiveConfig::from_env(&env, PathBuf::from("cache")).unwrap();
        assert_eq!(config.base_url, "https://www.postgresql.org");
        assert_eq!(config.user, "archives");
        assert_eq!(config.requests_per_second, 0.5);
        assert!(!format!("{config:?}").contains(&config.password));
        let env = Env::from_pairs([("PGHACKER_HTTP_RATE", "0")]);
        assert!(ArchiveConfig::from_env(&env, PathBuf::from("cache")).is_err());
    }

    #[test]
    fn recognizes_messages() {
        assert!(looks_like_message(
            b"From x Mon Jun 01 00:00:00 2026\nA: b\n"
        ));
        assert!(looks_like_message(b"Received: from x\n"));
        assert!(!looks_like_message(b"<!doctype html>\n<html>"));
        assert!(!looks_like_message(b"This message has been hidden."));
    }

    #[tokio::test]
    async fn the_rate_limit_spaces_requests() {
        let limiter = RateLimiter::new(Duration::from_millis(50));
        let start = Instant::now();
        for _ in 0..4 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() >= Duration::from_millis(150));
    }
}
