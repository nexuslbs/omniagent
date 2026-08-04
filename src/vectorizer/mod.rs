//! Vectorization module for OmniAgent.
//!
//! Provides background workers that generate embeddings for database messages
//! and wiki content without involving the LLM agent. Supports a lightweight
//! local hash-based vectorizer (character trigram feature hashing, 1536
//! dimensions) and an external API-based vectorizer.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use parking_lot::RwLock;
use std::time::{Duration, SystemTime};

use crate::err_msg;
use crate::err_str;
use crate::error::{AppResult, ErrorContext};

// ---------------------------------------------------------------------------
// EmbeddingProtocol
// ---------------------------------------------------------------------------

/// External embedding API protocol.
/// Identifies the API format to use. Known protocols (openai, gemini, cohere, jina)
/// have specific request/response handling. Unknown protocols default to
/// OpenAI-compatible format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingProtocol(pub String);

impl FromStr for EmbeddingProtocol {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_lowercase()))
    }
}

impl EmbeddingProtocol {
    /// Build the HTTP request components for a single text embedding.
    /// Returns (url, headers, body_json).
    pub fn build_request(
        &self,
        text: &str,
        api_url: &str,
        api_key: &Option<String>,
        model: &str,
    ) -> (String, Vec<(String, String)>, serde_json::Value) {
        match self.0.as_str() {
            "gemini" => {
                let url = format!("{}:embedContent", api_url.trim_end_matches('/'));
                let mut headers = Vec::new();
                if let Some(key) = api_key {
                    headers.push(("x-goog-api-key".to_string(), key.clone()));
                }
                let body = serde_json::json!({
                    "model": model,
                    "content": {
                        "parts": [{"text": text}]
                    }
                });
                (url, headers, body)
            }
            "cohere" => {
                let url = format!("{}/embed", api_url.trim_end_matches('/'));
                let mut headers = Vec::new();
                if let Some(key) = api_key {
                    headers.push(("Authorization".to_string(), format!("Bearer {key}")));
                }
                let body = serde_json::json!({
                    "texts": [text],
                    "model": model,
                    "input_type": "search_document",
                });
                (url, headers, body)
            }
            "jina" => {
                let url = format!("{}/embeddings", api_url.trim_end_matches('/'));
                let mut headers = Vec::new();
                if let Some(key) = api_key {
                    headers.push(("Authorization".to_string(), format!("Bearer {key}")));
                }
                let body = serde_json::json!({
                    "input": [text],
                    "model": model,
                });
                (url, headers, body)
            }
            // Default (openai and unknown): OpenAI-compatible /embeddings endpoint
            _ => {
                let url = format!("{}/embeddings", api_url.trim_end_matches('/'));
                let mut headers = Vec::new();
                if let Some(key) = api_key {
                    headers.push(("Authorization".to_string(), format!("Bearer {key}")));
                }
                let body = serde_json::json!({
                    "input": text,
                    "model": model,
                });
                (url, headers, body)
            }
        }
    }

    /// Extract a single embedding vector from a protocol response.
    pub fn extract_embedding(&self, response: &serde_json::Value) -> AppResult<Vec<f32>> {
        match self.0.as_str() {
            "gemini" => {
                let embedding = response
                    .get("embedding")
                    .and_then(|e| e.get("values"))
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| err_str!("missing embedding.values in response"))?;
                embedding
                    .iter()
                    .map(|v| {
                        v.as_f64()
                            .map(|f| f as f32)
                            .ok_or_else(|| err_str!("non-numeric value in embedding"))
                    })
                    .collect()
            }
            "cohere" => {
                let embedding = response
                    .get("embeddings")
                    .and_then(|e| e.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|first| first.as_array())
                    .ok_or_else(|| err_str!("missing embeddings[0] in response"))?;
                embedding
                    .iter()
                    .map(|v| {
                        v.as_f64()
                            .map(|f| f as f32)
                            .ok_or_else(|| err_str!("non-numeric value in embedding"))
                    })
                    .collect()
            }
            // Default (openai, jina, and unknown): OpenAI-compatible data[0].embedding
            _ => {
                let embedding = response
                    .get("data")
                    .and_then(|d| d.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|first| first.get("embedding"))
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| err_str!("missing data[0].embedding in response"))?;
                embedding
                    .iter()
                    .map(|v| {
                        v.as_f64()
                            .map(|f| f as f32)
                            .ok_or_else(|| err_str!("non-numeric value in embedding"))
                    })
                    .collect()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vectorizer trait and implementations
// ---------------------------------------------------------------------------

/// Trait for generating text embeddings asynchronously.
#[async_trait]
pub trait Vectorizer: Send + Sync {
    async fn generate_embedding(&self, text: &str) -> Vec<f32>;
}

/// Lightweight local vectorizer using character trigram feature hashing.
///
/// Algorithm: split text into overlapping 3-character windows, hash each
/// trigram to a bucket (0..1535) using `DefaultHasher`, increment the bucket
/// value, then normalize to unit length. Deterministic, zero dependencies.
pub struct HashVectorizer;

#[async_trait]
impl Vectorizer for HashVectorizer {
    async fn generate_embedding(&self, text: &str) -> Vec<f32> {
        let dim = 1536;
        let mut vec = vec![0.0f32; dim];

        // Collect all character trigrams (overlapping windows of 3 chars)
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 3 {
            // Too short to form a trigram; return a zero vector.
            return vec;
        }

        for window in chars.windows(3) {
            let trigram: String = window.iter().collect();
            let mut hasher = DefaultHasher::new();
            trigram.hash(&mut hasher);
            let hash = hasher.finish();
            let bucket = (hash as usize) % dim;
            vec[bucket] += 1.0;
        }

        // Normalize to unit length
        let magnitude: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for val in vec.iter_mut() {
                *val /= magnitude;
            }
        }

        vec
    }
}

/// Alternative vectorizer that calls an external HTTP API for embeddings.
pub struct ApiVectorizer {
    protocol: EmbeddingProtocol,
    api_url: String,
    api_key: Option<String>,
    model: String,
    client: reqwest::Client,
}

impl ApiVectorizer {
    pub fn new(
        protocol: EmbeddingProtocol,
        api_url: String,
        api_key: Option<String>,
        model: String,
    ) -> Self {
        Self {
            protocol,
            api_url,
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Vectorizer for ApiVectorizer {
    async fn generate_embedding(&self, text: &str) -> Vec<f32> {
        let (url, headers, body) =
            self.protocol
                .build_request(text, &self.api_url, &self.api_key, &self.model);

        let mut req = self.client.post(&url);
        for (key, value) in &headers {
            req = req.header(key.as_str(), value.as_str());
        }
        req = req.json(&body);

        let response = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!("ApiVectorizer: request failed: {:?}", e);
                return vec![0.0f32; 1536];
            }
        };

        let resp: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("ApiVectorizer: failed to decode response: {:?}", e);
                return vec![0.0f32; 1536];
            }
        };

        match self.protocol.extract_embedding(&resp) {
            Ok(embedding) => embedding,
            Err(e) => {
                tracing::warn!(
                    "ApiVectorizer: failed to extract embedding: {:?}, response: {}",
                    e,
                    resp
                );
                vec![0.0f32; 1536]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VectorizerConfig
// ---------------------------------------------------------------------------

/// Configuration for the vectorization subsystem.
#[derive(Debug, Clone)]
pub struct VectorizerConfig {
    /// Which vectorizer method to use: "local" or "api".
    pub method: String,
    /// API endpoint URL when method is "api".
    pub api_url: Option<String>,
    /// Embedding API protocol when method is "api".
    pub protocol: String,
    /// API key for the embedding service.
    pub api_key: Option<String>,
    /// Model name for the embedding service.
    pub api_model: Option<String>,
    /// Number of messages to process per batch.
    pub batch_size: usize,
    /// Poll interval in seconds between processing runs.
    pub poll_interval_secs: u64,
}

impl Default for VectorizerConfig {
    fn default() -> Self {
        Self {
            method: "local".to_string(),
            api_url: None,
            protocol: "openai".to_string(),
            api_key: None,
            api_model: None,
            batch_size: 50,
            poll_interval_secs: 3600,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert Vec<f32> to Postgres-compatible text representation.
// ---------------------------------------------------------------------------

/// Pub because it's used by agent process_message for semantic search queries.
pub fn vector_to_string(vec: &[f32]) -> String {
    let parts: Vec<String> = vec.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(","))
}

// ---------------------------------------------------------------------------
// State tracking for wiki worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WikiState {
    /// Map from file path to last-known modification timestamp (epoch seconds).
    files: std::collections::HashMap<String, u64>,
}

impl WikiState {
    fn load(path: &Path) -> AppResult<Self> {
        if path.exists() {
            let content =
                std::fs::read_to_string(path).ctx("Failed to read vectorizer state file")?;
            serde_json::from_str(&content).ctx("Failed to parse vectorizer state")
        } else {
            Ok(Self {
                files: std::collections::HashMap::new(),
            })
        }
    }

    fn save(&self, path: &Path) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content).ctx("Failed to write vectorizer state file")
    }
}

// ---------------------------------------------------------------------------
// MessageVectorizer worker
// ---------------------------------------------------------------------------

/// Background worker that polls for messages without embeddings in Postgres,
/// generates embeddings, and updates the `embedding` column.
pub struct MessageVectorizer {
    pool: PgPool,
    vectorizer: Box<dyn Vectorizer>,
    config: VectorizerConfig,
}

impl MessageVectorizer {
    pub fn new(pool: PgPool, vectorizer: Box<dyn Vectorizer>, config: VectorizerConfig) -> Self {
        Self {
            pool,
            vectorizer,
            config,
        }
    }

    pub async fn run(&self) {
        let interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            if let Err(e) = self.process_batch().await {
                tracing::error!("MessageVectorizer: batch processing error: {:?}", e);
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn process_batch(&self) -> AppResult<()> {
        let messages =
            super::db::types::find_messages_without_embeddings(&self.pool, self.config.batch_size as i64)
                .await?;

        if messages.is_empty() {
            return Ok(());
        }

        tracing::info!(
            "MessageVectorizer: processing {} messages without embeddings",
            messages.len()
        );

        for msg in &messages {
            let embedding = self.vectorizer.generate_embedding(&msg.content).await;
            let emb_str = vector_to_string(&embedding);
            super::db::types::update_message_embedding(&self.pool, msg.id, &emb_str).await?;
            tracing::debug!(
                "MessageVectorizer: updated embedding for message {}",
                msg.id
            );
        }

        tracing::info!(
            "MessageVectorizer: finished batch of {} messages",
            messages.len()
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WikiVectorizer worker
// ---------------------------------------------------------------------------

/// Background worker that scans wiki .md files, generates embeddings, and
/// upserts them into a Qdrant collection via REST API.
pub struct WikiVectorizer {
    wiki_dir: String,
    qdrant_url: String,
    vectorizer: Box<dyn Vectorizer>,
    config: VectorizerConfig,
    state_path: String,
    client: reqwest::Client,
}

impl WikiVectorizer {
    pub fn new(
        wiki_dir: String,
        qdrant_url: String,
        vectorizer: Box<dyn Vectorizer>,
        config: VectorizerConfig,
        data_dir: &str,
    ) -> Self {
        let state_path = format!("{}/vectorizer-state.json", data_dir);
        Self {
            wiki_dir,
            qdrant_url,
            vectorizer,
            config,
            state_path,
            client: reqwest::Client::new(),
        }
    }

    pub async fn run(&self) {
        // Ensure Qdrant wiki collection exists
        if let Err(e) = self.ensure_collection().await {
            tracing::error!(
                "WikiVectorizer: failed to ensure Qdrant collection: {:?}",
                e
            );
        }

        let interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            if let Err(e) = self.process_files().await {
                tracing::error!("WikiVectorizer: file processing error: {:?}", e);
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn ensure_collection(&self) -> AppResult<()> {
        let url = format!("{}/collections/wiki", self.qdrant_url);
        let body = serde_json::json!({
            "name": "wiki",
            "vectors": {
                "size": 1536,
                "distance": "Cosine"
            }
        });

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .ctx("Failed to create Qdrant wiki collection")?;

        if resp.status().is_success() || resp.status().as_u16() == 409 {
            // 409 = already exists, which is fine
            tracing::info!("WikiVectorizer: Qdrant wiki collection ready");
            Ok(())
        } else {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            Err(err_str!(
                "Qdrant collection creation failed ({}): {}",
                status,
                text
            ))
        }
    }

    async fn process_files(&self) -> AppResult<()> {
        let state_path = Path::new(&self.state_path);
        let mut state = WikiState::load(state_path).unwrap_or(WikiState {
            files: std::collections::HashMap::new(),
        });

        let wiki_dir = Path::new(&self.wiki_dir);
        if !wiki_dir.exists() {
            tracing::warn!(
                "WikiVectorizer: wiki directory does not exist: {}",
                self.wiki_dir
            );
            return Ok(());
        }

        // Collect .md files recursively
        let mut entries = Vec::new();
        for entry in walkdir::WalkDir::new(wiki_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
                {
                    entries.push(path.to_path_buf());
                }
            }
        }

        let mut changed_count = 0u64;
        let mut points = Vec::new();

        for path in &entries {
            let path_str = path.to_string_lossy().to_string();
            let metadata = std::fs::metadata(path)?;
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            // Check if file was modified since last scan
            let last_mtime = state.files.get(&path_str).copied().unwrap_or(0);
            if mtime <= last_mtime {
                continue; // No change
            }

            // Read file content
            let content = std::fs::read_to_string(path)
                .ctx(format!("Failed to read wiki file: {}", path_str))?;

            // Strip frontmatter (YAML/TOML between --- delimiters)
            let body = strip_frontmatter(&content);

            if body.trim().is_empty() {
                tracing::debug!("WikiVectorizer: skipping empty file: {}", path_str);
                state.files.insert(path_str.clone(), mtime);
                continue;
            }

            // Generate embedding
            let embedding = self.vectorizer.generate_embedding(body).await;

            // Derive a deterministic ID from the path (must be unsigned for Qdrant)
            let mut hasher = DefaultHasher::new();
            path_str.hash(&mut hasher);
            let point_id = hasher.finish();

            // Derive a title from the filename
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string();

            points.push(serde_json::json!({
                "id": point_id,
                "vector": embedding,
                "payload": {
                    "path": path_str,
                    "title": title,
                    "updated": mtime.to_string()
                }
            }));

            state.files.insert(path_str, mtime);
            changed_count += 1;
        }

        if points.is_empty() {
            return Ok(());
        }

        // Upsert to Qdrant
        let url = format!("{}/collections/wiki/points?wait=true", self.qdrant_url);
        let payload = serde_json::json!({ "points": points });

        // Qdrant 1.18+ uses PUT for upserting points
        let resp = self
            .client
            .put(&url)
            .json(&payload)
            .send()
            .await
            .ctx("Failed to upsert wiki points to Qdrant")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            err_msg!("Qdrant upsert failed ({}): {}", status, text);
        }

        // Save updated state
        state.save(state_path)?;

        tracing::info!(
            "WikiVectorizer: upserted {} wiki documents to Qdrant",
            changed_count
        );

        Ok(())
    }
}

/// Strip YAML/TOML frontmatter delimited by `---` lines from markdown content.
fn strip_frontmatter(content: &str) -> &str {
    let content = content.trim_start();
    if let Some(after) = content.strip_prefix("---") {
        if let Some(end) = after.find("---") {
            let after_stripped = &after[end + 3..];
            return after_stripped.trim_start();
        }
    }
    content
}

// ---------------------------------------------------------------------------
// spawn_vectorizers
// ---------------------------------------------------------------------------

/// Spawn both vectorization workers as tokio tasks if enabled in config.
///
/// This function does not return until cancellation (i.e., it loops forever
/// via `futures::future::pending()`). It is intended to be spawned as its own
/// tokio task from main.
pub async fn spawn_vectorizers(
    pool: PgPool,
    config: Arc<RwLock<crate::agent::AgentConfig>>,
    data_dir: &str,
) {
    struct MakeVectorizerConfig<'a> {
        api_url: &'a Option<String>,
        protocol: &'a str,
        api_key: &'a Option<String>,
        api_model: &'a Option<String>,
    }

    fn make_vectorizer(
        method: &str,
        target: &str,
        config: MakeVectorizerConfig<'_>,
    ) -> Box<dyn Vectorizer> {
        match method {
            "api" => {
                if let Some(ref url) = config.api_url {
                    let proto = EmbeddingProtocol::from_str(config.protocol)
                        .expect("EmbeddingProtocol::from_str is infallible");

                    let model = config
                        .api_model
                        .clone()
                        .unwrap_or_else(|| "text-embedding-ada-002".to_string());
                    tracing::info!(
                        "{}: Using ApiVectorizer with endpoint: {}, protocol: {:?}, model: {}",
                        target,
                        url,
                        proto,
                        model
                    );
                    Box::new(ApiVectorizer::new(
                        proto,
                        url.clone(),
                        config.api_key.clone(),
                        model,
                    ))
                } else {
                    tracing::warn!(
                        "{}: method=api but no api_url set; falling back to local",
                        target
                    );
                    Box::new(HashVectorizer)
                }
            }
            _ => {
                tracing::info!("{}: Using HashVectorizer (local feature hashing)", target);
                Box::new(HashVectorizer)
            }
        }
    }

    let (
        vectorize_messages,
        messages_method,
        messages_api_url,
        messages_protocol,
        messages_api_key,
        messages_api_model,
        messages_interval,
        vectorize_wiki,
        wiki_method,
        wiki_api_url,
        wiki_protocol,
        wiki_api_key,
        wiki_api_model,
        wiki_interval,
        qdrant_url,
    ) = {
        let cfg = config.read();
        (
            cfg.vectorize_messages,
            cfg.messages_vectorization_method.clone(),
            cfg.messages_vectorization_api_url.clone(),
            cfg.messages_vectorization_protocol.clone(),
            cfg.messages_vectorization_api_key.clone(),
            cfg.messages_vectorization_api_model.clone(),
            cfg.messages_vectorization_interval_secs,
            cfg.vectorize_wiki,
            cfg.wiki_vectorization_method.clone(),
            cfg.wiki_vectorization_api_url.clone(),
            cfg.wiki_vectorization_protocol.clone(),
            cfg.wiki_vectorization_api_key.clone(),
            cfg.wiki_vectorization_api_model.clone(),
            cfg.wiki_vectorization_interval_secs,
            String::new(), // Qdrant: moved to plugin concern
        )
    };

    // Spawn message vectorizer (with its own config)
    if vectorize_messages {
        let pool_clone = pool.clone();
        let messages_config = VectorizerConfig {
            method: messages_method,
            api_url: messages_api_url,
            protocol: messages_protocol,
            api_key: messages_api_key,
            api_model: messages_api_model,
            poll_interval_secs: messages_interval,
            ..Default::default()
        };
        let vec = MessageVectorizer::new(
            pool_clone,
            make_vectorizer(
                &messages_config.method,
                "messages",
                MakeVectorizerConfig {
                    api_url: &messages_config.api_url,
                    protocol: &messages_config.protocol,
                    api_key: &messages_config.api_key,
                    api_model: &messages_config.api_model,
                },
            ),
            messages_config,
        );
        tokio::spawn(async move {
            tracing::info!("MessageVectorizer worker started");
            vec.run().await;
        });
    } else {
        tracing::info!("Message vectorization disabled");
    }

    // Spawn wiki vectorizer (with its own config)
    if vectorize_wiki {
        let wiki_config = VectorizerConfig {
            method: wiki_method,
            api_url: wiki_api_url,
            protocol: wiki_protocol,
            api_key: wiki_api_key,
            api_model: wiki_api_model,
            poll_interval_secs: wiki_interval,
            ..Default::default()
        };
        let wiki_vec = WikiVectorizer::new(
            format!(
                "{}/profiles/{}/wiki",
                data_dir,
                crate::profile::default_profile_name()
            ),
            qdrant_url.clone(),
            make_vectorizer(
                &wiki_config.method,
                "wiki",
                MakeVectorizerConfig {
                    api_url: &wiki_config.api_url,
                    protocol: &wiki_config.protocol,
                    api_key: &wiki_config.api_key,
                    api_model: &wiki_config.api_model,
                },
            ),
            wiki_config,
            data_dir,
        );
        tokio::spawn(async move {
            tracing::info!("WikiVectorizer worker started");
            wiki_vec.run().await;
        });
    } else {
        tracing::info!("Wiki vectorization disabled");
    }

    // Keep running until cancelled (we never return)
    futures::future::pending::<()>().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── vector_to_string ────────────────────────────────────────────────────

    #[test]
    fn test_vector_to_string_empty() {
        let v: Vec<f32> = vec![];
        assert_eq!(vector_to_string(&v), "[]");
    }

    #[test]
    fn test_vector_to_string_single() {
        let v = vec![0.5];
        assert_eq!(vector_to_string(&v), "[0.5]");
    }

    #[test]
    fn test_vector_to_string_multiple() {
        let v = vec![1.0, 2.0, 3.0];
        let s = vector_to_string(&v);
        assert!(s.starts_with('['));
        assert!(s.ends_with(']'));
        assert!(s.contains("1"));
        assert!(s.contains("2"));
        assert!(s.contains("3"));
        // Check comma separation
        assert!(s.contains(","));
    }

    #[test]
    fn test_vector_to_string_negative() {
        let v = vec![-1.5, 0.0, 3.14];
        let s = vector_to_string(&v);
        assert!(s.contains("-1.5"));
        assert!(s.contains("0"));
        assert!(s.contains("3.14"));
    }

    #[test]
    fn test_vector_to_string_large() {
        let v: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let s = vector_to_string(&v);
        assert!(s.starts_with('['));
        assert!(s.ends_with(']'));
        // Should contain first and last values
        assert!(s.contains("0"));
        assert!(s.contains("99"));
    }

    // ── HashVectorizer ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_hash_vectorizer_deterministic() {
        let vec = HashVectorizer;
        let a = vec.generate_embedding("hello world").await;
        let b = vec.generate_embedding("hello world").await;
        assert_eq!(a, b, "HashVectorizer should be deterministic");
    }

    #[tokio::test]
    async fn test_hash_vectorizer_dimension() {
        let vec = HashVectorizer;
        let embedding = vec.generate_embedding("some input text").await;
        assert_eq!(embedding.len(), 1536);
    }

    #[tokio::test]
    async fn test_hash_vectorizer_similar_inputs_similar_embeddings() {
        let vec = HashVectorizer;
        let a = vec.generate_embedding("hello world foo bar").await;
        let b = vec.generate_embedding("hello world foo baz").await;
        // Different inputs should produce different embeddings
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_hash_vectorizer_short_input() {
        let vec = HashVectorizer;
        // Input shorter than 3 chars: should produce a zero vector
        let embedding = vec.generate_embedding("ab").await;
        assert_eq!(embedding.len(), 1536);
        assert!(embedding.iter().all(|&x| x == 0.0));
    }

    #[tokio::test]
    async fn test_hash_vectorizer_empty_input() {
        let vec = HashVectorizer;
        let embedding = vec.generate_embedding("").await;
        assert_eq!(embedding.len(), 1536);
        assert!(embedding.iter().all(|&x| x == 0.0));
    }

    // ── EmbeddingProtocol::build_request ────────────────────────────────────

    #[test]
    fn test_embedding_protocol_openai_build_request() {
        let protocol = EmbeddingProtocol("openai".to_string());
        let (url, headers, body) = protocol.build_request(
            "test text",
            "https://api.openai.com/v1",
            &None,
            "text-embedding-ada-002",
        );
        assert_eq!(url, "https://api.openai.com/v1/embeddings");
        assert!(headers.is_empty());
        assert_eq!(body["input"], "test text");
        assert_eq!(body["model"], "text-embedding-ada-002");
    }

    #[test]
    fn test_embedding_protocol_openai_with_api_key() {
        let protocol = EmbeddingProtocol("openai".to_string());
        let (_, headers, _) = protocol.build_request(
            "test",
            "https://api.openai.com/v1",
            &Some("sk-test".to_string()),
            "ada",
        );
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer sk-test");
    }

    #[test]
    fn test_embedding_protocol_gemini_build_request() {
        let protocol = EmbeddingProtocol("gemini".to_string());
        let (url, headers, body) = protocol.build_request(
            "test text",
            "https://generativelanguage.googleapis.com/v1beta/models/text-embedding-004",
            &Some("test-key".to_string()),
            "models/text-embedding-004",
        );
        assert!(url.ends_with(":embedContent"));
        assert!(!headers.is_empty());
        assert_eq!(headers[0].0, "x-goog-api-key");
        assert_eq!(headers[0].1, "test-key");
        assert_eq!(body["model"], "models/text-embedding-004");
        assert_eq!(body["content"]["parts"][0]["text"], "test text");
    }

    #[test]
    fn test_embedding_protocol_cohere_build_request() {
        let protocol = EmbeddingProtocol("cohere".to_string());
        let (url, headers, body) = protocol.build_request(
            "test text",
            "https://api.cohere.com/v1",
            &Some("co-key".to_string()),
            "embed-english-v3.0",
        );
        assert!(url.ends_with("/embed"));
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer co-key");
        assert_eq!(body["texts"][0], "test text");
        assert_eq!(body["model"], "embed-english-v3.0");
        assert_eq!(body["input_type"], "search_document");
    }

    #[test]
    fn test_embedding_protocol_jina_build_request() {
        let protocol = EmbeddingProtocol("jina".to_string());
        let (url, headers, body) = protocol.build_request(
            "test text",
            "https://api.jina.ai/v1",
            &Some("jina-key".to_string()),
            "jina-embeddings-v3",
        );
        assert!(url.ends_with("/embeddings"));
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(body["input"][0], "test text");
        assert_eq!(body["model"], "jina-embeddings-v3");
    }

    #[test]
    fn test_embedding_protocol_unknown_defaults_to_openai() {
        let protocol = EmbeddingProtocol("unknown-protocol".to_string());
        let (url, headers, body) = protocol.build_request(
            "test",
            "https://example.com",
            &None,
            "some-model",
        );
        // Unknown protocols should default to OpenAI-compatible format
        assert!(url.ends_with("/embeddings"));
        assert!(headers.is_empty());
        assert_eq!(body["input"], "test");
    }

    #[test]
    fn test_embedding_protocol_no_trailing_slash_handling() {
        let protocol = EmbeddingProtocol("openai".to_string());
        let (url, _, _) = protocol.build_request(
            "test",
            "https://api.openai.com/v1/",
            &None,
            "model",
        );
        // Should strip trailing slash before appending /embeddings
        assert_eq!(url, "https://api.openai.com/v1/embeddings");
    }

    // ── EmbeddingProtocol::extract_embedding ────────────────────────────────

    #[test]
    fn test_extract_embedding_openai_success() {
        let protocol = EmbeddingProtocol("openai".to_string());
        let response = json!({
            "data": [{"embedding": [0.1, 0.2, 0.3]}],
            "model": "text-embedding-ada-002",
        });
        let result = protocol.extract_embedding(&response).unwrap();
        assert_eq!(result, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn test_extract_embedding_openai_missing_field() {
        let protocol = EmbeddingProtocol("openai".to_string());
        let response = json!({});
        let result = protocol.extract_embedding(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_embedding_gemini_success() {
        let protocol = EmbeddingProtocol("gemini".to_string());
        let response = json!({
            "embedding": {
                "values": [0.5, 0.6, 0.7]
            }
        });
        let result = protocol.extract_embedding(&response).unwrap();
        assert_eq!(result, vec![0.5, 0.6, 0.7]);
    }

    #[test]
    fn test_extract_embedding_gemini_missing_field() {
        let protocol = EmbeddingProtocol("gemini".to_string());
        let response = json!({"embedding": {}});
        let result = protocol.extract_embedding(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_embedding_cohere_success() {
        let protocol = EmbeddingProtocol("cohere".to_string());
        let response = json!({
            "embeddings": [[0.8, 0.9]],
            "meta": {}
        });
        let result = protocol.extract_embedding(&response).unwrap();
        assert_eq!(result, vec![0.8, 0.9]);
    }

    #[test]
    fn test_extract_embedding_cohere_missing_field() {
        let protocol = EmbeddingProtocol("cohere".to_string());
        let response = json!({});
        let result = protocol.extract_embedding(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_embedding_jina_openai_compatible() {
        // Jina uses the default (OpenAI-compatible) response format
        let protocol = EmbeddingProtocol("jina".to_string());
        let response = json!({
            "data": [{"embedding": [1.0, 2.0]}]
        });
        let result = protocol.extract_embedding(&response).unwrap();
        assert_eq!(result, vec![1.0, 2.0]);
    }

    #[test]
    fn test_extract_embedding_unknown_protocol() {
        // Unknown protocols use the default OpenAI-compatible extraction
        let protocol = EmbeddingProtocol("custom".to_string());
        let response = json!({
            "data": [{"embedding": [3.0, 4.0]}]
        });
        let result = protocol.extract_embedding(&response).unwrap();
        assert_eq!(result, vec![3.0, 4.0]);
    }

    // ── EmbeddingProtocol from_str ──────────────────────────────────────────

    #[test]
    fn test_embedding_protocol_from_str() {
        let p: EmbeddingProtocol = "OPENAI".parse().unwrap();
        assert_eq!(p, EmbeddingProtocol("openai".to_string()));
    }

    #[test]
    fn test_embedding_protocol_from_str_mixed_case() {
        let p: EmbeddingProtocol = "Gemini".parse().unwrap();
        assert_eq!(p, EmbeddingProtocol("gemini".to_string()));
    }

    // ── VectorizerConfig ────────────────────────────────────────────────────

    #[test]
    fn test_vectorizer_config_defaults() {
        let cfg = VectorizerConfig::default();
        assert_eq!(cfg.method, "local");
        assert_eq!(cfg.protocol, "openai");
        assert_eq!(cfg.batch_size, 50);
        assert_eq!(cfg.poll_interval_secs, 3600);
        assert!(cfg.api_url.is_none());
        assert!(cfg.api_key.is_none());
        assert!(cfg.api_model.is_none());
    }
}
