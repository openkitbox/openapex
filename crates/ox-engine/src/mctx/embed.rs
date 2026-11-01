//! Semantic search support: the embeddings client, vector encoding, and the
//! cosine ranking `ctx_search`'s memory lane uses.
//!
//! One request shape only — OpenAI's `POST {endpoint}/embeddings` with
//! `{"model", "input"}` and `data[].embedding` back. Provider dialects for one
//! non-streaming call would cost more than the four config fields they replace.
//!
//! Vectors are stored as `f32` little-endian blobs beside the text that
//! produced them, along with the model identity (model id plus document
//! prefix): the same model under a different prefix is a different vector
//! space, so a row embedded under another identity is re-embedded rather than
//! compared.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use super::EmbeddingConfig;

/// Inputs per request. Providers cap batching (Zhipu allows 64); 32 keeps a
/// large backfill inside one call while staying under every cap seen.
const BATCH_SIZE: usize = 32;

/// The embeddings client for one endpoint/model.
pub struct Embedder {
    client: reqwest::Client,
    /// Base URL without the trailing `/embeddings`.
    endpoint: String,
    api_key: Option<String>,
    /// Model id, as the endpoint knows it.
    model: String,
    /// Identity written beside every stored vector (see the module docs).
    identity: String,
    query_instruction: Option<String>,
    document_prefix: Option<String>,
}

impl Embedder {
    /// Build from config, or `None` when semantic search is off or the config
    /// is incomplete.
    #[must_use]
    pub fn from_config(config: &EmbeddingConfig) -> Option<Arc<Self>> {
        if !config.is_enabled() {
            return None;
        }
        let client = ox_core::http_client()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .ok()?;
        Some(Arc::new(Self {
            client,
            endpoint: config
                .endpoint
                .as_deref()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
            api_key: config.api_key.clone().filter(|key| !key.is_empty()),
            model: config.model.clone().unwrap_or_default(),
            identity: config.identity(),
            query_instruction: config
                .query_instruction
                .clone()
                .filter(|prefix| !prefix.is_empty()),
            document_prefix: config
                .document_prefix
                .clone()
                .filter(|prefix| !prefix.is_empty()),
        }))
    }

    /// The identity stored beside this embedder's vectors.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Embed the query text (with the query instruction, when configured).
    pub async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let text = match &self.query_instruction {
            Some(instruction) => format!("{instruction}{query}"),
            None => query.to_owned(),
        };
        let mut vectors = self.embed(&[text]).await?;
        vectors
            .pop()
            .context("the embeddings endpoint returned no vector")
    }

    /// Embed stored texts (with the document prefix, when configured).
    /// Batched; the returned vectors are in input order. Borrowed slices: the
    /// caller already owns the texts, and only the endpoint request needs an
    /// owned copy.
    pub async fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH_SIZE) {
            let prepared: Vec<String> = batch
                .iter()
                .map(|text| match &self.document_prefix {
                    Some(prefix) => format!("{prefix}{text}"),
                    None => (*text).to_owned(),
                })
                .collect();
            out.extend(self.embed(&prepared).await?);
        }
        Ok(out)
    }

    /// One request; vectors are ordered by the response's `index`.
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut request = self
            .client
            .post(format!("{}/embeddings", self.endpoint))
            .json(&serde_json::json!({ "model": self.model, "input": inputs }));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .context("call the embeddings endpoint")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read the embeddings response")?;
        if !status.is_success() {
            anyhow::bail!(
                "embeddings endpoint returned HTTP {status}: {}",
                truncate(&body, 300)
            );
        }
        let parsed: EmbeddingResponse =
            serde_json::from_str(&body).context("parse the embeddings response")?;
        let mut data = parsed.data;
        data.sort_by_key(|row| row.index);
        if data.len() != inputs.len() {
            anyhow::bail!(
                "embeddings endpoint returned {} vectors for {} inputs",
                data.len(),
                inputs.len()
            );
        }
        Ok(data.into_iter().map(|row| row.embedding).collect())
    }
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    #[serde(default)]
    data: Vec<EmbeddingRow>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingRow {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Cosine similarity. `0.0` for a zero or mismatched-length pair (a partial
/// backfill can leave rows embedded under another model with another width).
pub(crate) fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut left_norm = 0.0f32;
    let mut right_norm = 0.0f32;
    for (a, b) in left.iter().zip(right) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// Encode a vector for storage: `f32` little-endian, 4 bytes per component.
pub(crate) fn to_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode a stored vector. A blob whose length is not a multiple of four (a
/// corrupt row) decodes to what it can and yields a vector that cannot match
/// anything, rather than failing the whole search.
pub(crate) fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}
