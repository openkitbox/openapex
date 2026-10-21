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
    pub fn from_config(config: &EmbeddingConfig) -> Option<Arc<Self>> {
        if !config.is_enabled() {
            return None;
        }
        let client = crate::http_client()
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
    /// Batched; the returned vectors are in input order.
    pub async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH_SIZE) {
            let prepared: Vec<String> = batch
                .iter()
                .map(|text| match &self.document_prefix {
                    Some(prefix) => format!("{prefix}{text}"),
                    None => text.clone(),
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

#[cfg(test)]
pub(crate) mod stub {
    //! A tiny OpenAI-shaped `/embeddings` server for tests: it answers every
    //! connection with one JSON body, and derives each vector from the text it
    //! received so a test can control ranking.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Start a stub on an ephemeral port. `vector_for` maps one input text to
    /// its vector. Returns the base URL and a shutdown handle.
    pub(crate) async fn spawn<F>(vector_for: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str) -> Vec<f32> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        let vector_for = std::sync::Arc::new(vector_for);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let vector_for = std::sync::Arc::clone(&vector_for);
                tokio::spawn(async move {
                    let Some(body) = read_body(&mut socket).await else {
                        return;
                    };
                    let request: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let inputs: Vec<String> = request
                        .get("input")
                        .and_then(|value| value.as_array())
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(|value| value.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let data: Vec<serde_json::Value> = inputs
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            serde_json::json!({
                                "index": index,
                                "embedding": vector_for(text),
                            })
                        })
                        .collect();
                    let payload = serde_json::json!({
                        "model": "stub",
                        "data": data,
                        "usage": { "prompt_tokens": 1, "total_tokens": 1 },
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// Read one HTTP request body (Content-Length framed).
    async fn read_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(split) = find_headers_end(&buffer) else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let body_start = split + 4;
            if buffer.len() >= body_start + length {
                return String::from_utf8(buffer[body_start..body_start + length].to_vec()).ok();
            }
        }
    }

    fn find_headers_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    /// A config pointing at a stub.
    pub(crate) fn config(endpoint: &str) -> super::super::EmbeddingConfig {
        super::super::EmbeddingConfig {
            provider: super::super::EmbeddingProvider::OpenAiCompatible,
            endpoint: Some(endpoint.to_owned()),
            api_key: Some("test-key".to_owned()),
            model: Some("stub".to_owned()),
            query_instruction: None,
            document_prefix: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_round_trip_through_the_blob_encoding() {
        let vector = vec![0.5_f32, -1.25, 3.75, 0.0];
        assert_eq!(from_blob(&to_blob(&vector)), vector);
        // A truncated blob decodes to the whole components it has.
        assert_eq!(from_blob(&[0, 0, 128, 63, 9]), vec![1.0]);
    }

    #[test]
    fn cosine_ranks_and_guards_against_mismatched_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &c).abs() < 1e-6);
        // Different widths (a vector from another model) never match.
        assert_eq!(cosine(&a, &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    /// The client against a real HTTP stub: batch order preserved, prefixes
    /// applied on the right side, and the bearer token sent.
    #[tokio::test]
    async fn the_client_batches_orders_and_prefixes() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured = std::sync::Arc::clone(&seen);
        let (endpoint, _handle) = stub::spawn(move |text: &str| {
            captured.lock().expect("seen lock").push(text.to_owned());
            vec![text.len() as f32, 1.0]
        })
        .await;

        let config = EmbeddingConfig {
            query_instruction: Some("query: ".to_owned()),
            document_prefix: Some("passage: ".to_owned()),
            ..stub::config(&endpoint)
        };
        let embedder = Embedder::from_config(&config).expect("an embedder");

        let documents = embedder
            .embed_documents(&["alpha".to_owned(), "beta".to_owned()])
            .await
            .expect("documents");
        assert_eq!(documents.len(), 2);
        // Vectors come back in input order even though the response order is
        // the stub's.
        assert_eq!(documents[0][0], "passage: alpha".len() as f32);
        assert_eq!(documents[1][0], "passage: beta".len() as f32);

        let query = embedder.embed_query("alpha").await.expect("query");
        assert_eq!(query[0], "query: alpha".len() as f32);

        let texts = seen.lock().expect("seen lock").clone();
        assert_eq!(
            texts,
            vec!["passage: alpha", "passage: beta", "query: alpha"],
            "prefixes go to the right side of the call"
        );
    }

    /// A non-2xx answer surfaces the body, so a misconfigured key or model is
    /// diagnosable from the tool's warning.
    #[tokio::test]
    async fn a_rejected_call_reports_the_status_and_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let body = "{\"error\":\"invalid api key\"}";
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let embedder =
            Embedder::from_config(&stub::config(&format!("http://{addr}"))).expect("an embedder");
        let error = embedder.embed_query("alpha").await.expect_err("a 401");
        let message = format!("{error:#}");
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("invalid api key"), "{message}");
    }

    #[test]
    fn identity_folds_in_the_document_prefix() {
        let base = EmbeddingConfig {
            provider: super::super::EmbeddingProvider::OpenAiCompatible,
            endpoint: Some("http://x".to_owned()),
            model: Some("m".to_owned()),
            ..EmbeddingConfig::default()
        };
        assert_eq!(base.identity(), "m");
        assert!(base.is_enabled());
        // The same model under a prefix is a different vector space.
        let prefixed = EmbeddingConfig {
            document_prefix: Some("search_document: ".to_owned()),
            ..base.clone()
        };
        assert_eq!(prefixed.identity(), "m|search_document: ");
        // Incomplete config never enables.
        assert!(
            !EmbeddingConfig {
                model: None,
                ..base
            }
            .is_enabled()
        );
    }
}
