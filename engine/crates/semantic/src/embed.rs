//! Where embeddings come from — the customer's endpoint, or nowhere.
//!
//! Drums never possesses model credentials of its own. An embedder exists
//! only when the customer points one at us by environment: an
//! OpenAI-compatible `/v1/embeddings` endpoint (OpenAI itself, an inference
//! gateway, an on-prem server — the shape is a de-facto standard), named by
//! `DRUMS_EMBED_URL` and `DRUMS_EMBED_MODEL`, authorized by
//! `DRUMS_EMBED_API_KEY`. The key lives in the environment only — the same
//! discipline as `DRUMS_POSTHOG_API_KEY`, for the same reason: a key in a
//! config file is a key in a backup, a dotfiles repo, and a support
//! screenshot.

use serde::Deserialize;

pub const ENV_URL: &str = "DRUMS_EMBED_URL";
pub const ENV_MODEL: &str = "DRUMS_EMBED_MODEL";
pub const ENV_KEY: &str = "DRUMS_EMBED_API_KEY";

/// The sentence `doctor` and `drums ask` print when no embedder is
/// configured. One string, owned here, so the two surfaces cannot drift.
pub const MISSING_EMBED_ENV: &str = "semantic search runs lexical-only — for vector search set DRUMS_EMBED_URL, DRUMS_EMBED_MODEL, and DRUMS_EMBED_API_KEY (env only, never the config file) to any OpenAI-compatible embeddings endpoint";

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("could not reach the embedding endpoint: {0}")]
    Unreachable(String),
    #[error("the embedding endpoint rejected our credentials")]
    Unauthorized,
    #[error("unexpected response shape from the embedding endpoint: {0}")]
    Shape(String),
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// An OpenAI-compatible `/v1/embeddings` client.
pub struct OpenAiCompatible {
    url: String,
    model: String,
    key: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never appears in Debug output, logs, or panics.
        f.debug_struct("OpenAiCompatible")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("key", &"[redacted]")
            .finish()
    }
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingRow>,
}

#[derive(Deserialize)]
struct EmbeddingRow {
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl Embedder for OpenAiCompatible {
    fn model(&self) -> &str {
        &self.model
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/v1/embeddings", self.url.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.key)
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .map_err(|e| EmbedError::Unreachable(e.to_string()))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(EmbedError::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(EmbedError::Shape(format!("status {}", response.status())));
        }
        let body: EmbeddingsResponse =
            response.json().await.map_err(|e| EmbedError::Shape(e.to_string()))?;
        if body.data.len() != texts.len() {
            return Err(EmbedError::Shape(format!(
                "{} inputs, {} embeddings back",
                texts.len(),
                body.data.len()
            )));
        }
        // Order by the response's own index field: the spec allows data out
        // of order, and pairing embeddings to the wrong texts would poison
        // the cache invisibly — every search subtly wrong, nothing failing.
        let mut rows = body.data;
        rows.sort_by_key(|r| r.index);
        Ok(rows.into_iter().map(|r| r.embedding).collect())
    }
}

/// The embedder the environment describes, or `None` with no fallback
/// invented. All three variables or nothing — a partial configuration is a
/// mistake worth surfacing, so it is also `None` (and `doctor` names which
/// pieces are missing).
pub fn embedder_from_env() -> Option<OpenAiCompatible> {
    let url = std::env::var(ENV_URL).ok()?;
    let model = std::env::var(ENV_MODEL).ok()?;
    let key = std::env::var(ENV_KEY).ok()?;
    if url.trim().is_empty() || model.trim().is_empty() || key.trim().is_empty() {
        return None;
    }
    Some(OpenAiCompatible { url, model, key, client: reqwest::Client::new() })
}

/// Which of the three variables are unset — for `doctor`, which names fixes.
pub fn missing_env() -> Vec<&'static str> {
    [ENV_URL, ENV_MODEL, ENV_KEY]
        .into_iter()
        .filter(|name| std::env::var(name).map(|v| v.trim().is_empty()).unwrap_or(true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_key() {
        let e = OpenAiCompatible {
            url: "https://example.test".into(),
            model: "m".into(),
            key: "sk-secret-value".into(),
            client: reqwest::Client::new(),
        };
        let dbg = format!("{e:?}");
        assert!(!dbg.contains("sk-secret-value"), "{dbg}");
        assert!(dbg.contains("[redacted]"));
    }

    #[test]
    fn the_missing_sentence_names_all_three_variables() {
        for name in [ENV_URL, ENV_MODEL, ENV_KEY] {
            assert!(MISSING_EMBED_ENV.contains(name), "{name} absent from the fix sentence");
        }
        assert!(MISSING_EMBED_ENV.contains("never the config file"));
    }
}
