// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Embeddings through an OpenAI-compatible endpoint, configured as PilotSwarm
//! configures its embedder: `HORIZON_EMBED_URL`, `_MODEL`, `_DIM`,
//! `_API_KEY`, `_API_KEY_HEADER` (default `api-key`, Azure's) and `_BEARER`
//! (inferred for an `Authorization` header).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pgvector::HalfVector;
use serde::Deserialize;
use serde_json::json;

use crate::config::Env;

/// The dimensions of the schema's `halfvec` columns.
pub const DIMENSIONS: usize = 1536;

/// Inputs per request.
const BATCH: usize = 64;

/// The longest input sent, in characters: well under the models' token limit.
const MAX_INPUT_CHARS: usize = 24_000;

const TRUE_VALUES: [&str; 4] = ["1", "true", "yes", "on"];

#[derive(Clone)]
pub struct EmbedConfig {
    pub url: String,
    pub model: String,
    pub dim: usize,
    pub api_key: Option<String>,
    pub api_key_header: String,
    pub bearer: bool,
}

impl std::fmt::Debug for EmbedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedConfig")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("dim", &self.dim)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("api_key_header", &self.api_key_header)
            .field("bearer", &self.bearer)
            .finish()
    }
}

impl EmbedConfig {
    /// The embedder from `HORIZON_EMBED_*`, or `None` when URL, model or
    /// dimensions are missing, as PilotSwarm treats it: search degrades to
    /// full text.
    pub fn from_env(env: &Env) -> Result<Option<Self>> {
        let (Some(url), Some(model), Some(dim)) = (
            env.get("HORIZON_EMBED_URL"),
            env.get("HORIZON_EMBED_MODEL"),
            env.get("HORIZON_EMBED_DIM"),
        ) else {
            return Ok(None);
        };
        let dim: usize = dim
            .trim()
            .parse()
            .map_err(|_| anyhow!("HORIZON_EMBED_DIM must be a positive integer"))?;
        if dim != DIMENSIONS {
            bail!("HORIZON_EMBED_DIM must be {DIMENSIONS}, the schema's vector dimensions");
        }
        let api_key_header = env.get("HORIZON_EMBED_API_KEY_HEADER").map(str::to_owned);
        let bearer = match env.get("HORIZON_EMBED_BEARER") {
            Some(value) => TRUE_VALUES.contains(&value.trim().to_ascii_lowercase().as_str()),
            None => api_key_header
                .as_deref()
                .is_some_and(|header| header.eq_ignore_ascii_case("authorization")),
        };
        Ok(Some(Self {
            url: url.trim().to_owned(),
            model: model.trim().to_owned(),
            dim,
            api_key: env
                .get("HORIZON_EMBED_API_KEY")
                .map(|key| key.trim().to_owned()),
            api_key_header: api_key_header.unwrap_or_else(|| "api-key".to_owned()),
            bearer,
        }))
    }
}

/// The embeddings client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Embedder {
    config: EmbedConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    index: usize,
    embedding: Vec<f32>,
}

impl Embedder {
    pub fn new(config: EmbedConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("pghacker/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("cannot build the embeddings client")?;
        Ok(Self { config, http })
    }

    /// The embedder from the environment, if it's configured.
    pub fn from_env(env: &Env) -> Result<Option<Self>> {
        EmbedConfig::from_env(env)?.map(Self::new).transpose()
    }

    pub fn config(&self) -> &EmbedConfig {
        &self.config
    }

    /// One input's embedding.
    pub async fn embed_one(&self, input: &str) -> Result<HalfVector> {
        let mut vectors = self.embed(&[input.to_owned()]).await?;
        vectors
            .pop()
            .ok_or_else(|| anyhow!("the embeddings endpoint returned nothing"))
    }

    /// The inputs' embeddings, in order, in batches.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<HalfVector>> {
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(BATCH) {
            let vectors = self.request(batch).await?;
            out.extend(vectors.into_iter().map(|v| HalfVector::from_f32_slice(&v)));
        }
        Ok(out)
    }

    async fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<&str> = inputs
            .iter()
            .map(|input| {
                let input = crate::text::truncate_chars(input, MAX_INPUT_CHARS);
                if input.trim().is_empty() { " " } else { input }
            })
            .collect();
        let mut body = json!({ "input": inputs, "model": self.config.model });
        if self.config.model.starts_with("text-embedding-3") {
            body["dimensions"] = json!(self.config.dim);
        }
        let mut delay = Duration::from_millis(500);
        for attempt in 1..=6 {
            let mut request = self.http.post(&self.config.url).json(&body);
            if let Some(key) = &self.config.api_key {
                let value = if self.config.bearer {
                    format!("Bearer {key}")
                } else {
                    key.clone()
                };
                request = request.header(self.config.api_key_header.as_str(), value);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if attempt < 6 => {
                    tracing::warn!(%error, attempt, "the embeddings request failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                    continue;
                }
                Err(error) => return Err(error).context("the embeddings request failed"),
            };
            let status = response.status();
            if status.is_success() {
                let mut parsed: Response = response
                    .json()
                    .await
                    .context("the embeddings response is invalid")?;
                parsed.data.sort_by_key(|datum| datum.index);
                if parsed.data.len() != inputs.len() {
                    bail!(
                        "the embeddings endpoint returned {} vectors for {} inputs",
                        parsed.data.len(),
                        inputs.len()
                    );
                }
                for datum in &parsed.data {
                    if datum.embedding.len() != self.config.dim {
                        bail!(
                            "the embeddings endpoint returned {} dimensions, not {}",
                            datum.embedding.len(),
                            self.config.dim
                        );
                    }
                }
                return Ok(parsed
                    .data
                    .into_iter()
                    .map(|datum| datum.embedding)
                    .collect());
            }
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if retryable && attempt < 6 {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(delay)
                    .min(Duration::from_secs(60));
                tracing::warn!(%status, attempt, wait_ms = wait.as_millis() as u64, "embeddings throttled or failed; retrying");
                tokio::time::sleep(wait).await;
                delay = (delay * 2).min(Duration::from_secs(30));
                continue;
            }
            let text = response.text().await.unwrap_or_default();
            bail!(
                "the embeddings endpoint answered {status}: {}",
                crate::text::truncate_chars(&text, 500)
            );
        }
        unreachable!("the loop returns on its last attempt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_environment_like_pilotswarm() {
        assert!(EmbedConfig::from_env(&Env::default()).unwrap().is_none());
        let env = Env::from_pairs([
            ("HORIZON_EMBED_URL", "https://x/embeddings"),
            ("HORIZON_EMBED_MODEL", "text-embedding-3-small"),
            ("HORIZON_EMBED_DIM", "1536"),
            ("HORIZON_EMBED_API_KEY", "k"),
        ]);
        let config = EmbedConfig::from_env(&env).unwrap().unwrap();
        assert_eq!(config.api_key_header, "api-key");
        assert!(!config.bearer);
        let env = Env::from_pairs([
            ("HORIZON_EMBED_URL", "https://x/embeddings"),
            ("HORIZON_EMBED_MODEL", "m"),
            ("HORIZON_EMBED_DIM", "1536"),
            ("HORIZON_EMBED_API_KEY_HEADER", "Authorization"),
        ]);
        assert!(EmbedConfig::from_env(&env).unwrap().unwrap().bearer);
        let env = Env::from_pairs([
            ("HORIZON_EMBED_URL", "u"),
            ("HORIZON_EMBED_MODEL", "m"),
            ("HORIZON_EMBED_DIM", "768"),
        ]);
        assert!(EmbedConfig::from_env(&env).is_err());
    }
}
