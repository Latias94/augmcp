//! REST backend client for uploading blobs and performing retrieval.

use crate::{config::Config, indexer::{BlobUpload, ProjectsIndex}};
use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct BatchUploadPayload<'a> { blobs: &'a [BlobUpload] }

#[derive(Debug, Deserialize)]
struct BatchUploadResp { #[serde(default)] blob_names: Vec<String> }

#[derive(Debug, Serialize)]
struct RetrievalBlobs<'a> {
    checkpoint_id: Option<String>,
    added_blobs: &'a [String],
    deleted_blobs: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RetrievalPayload<'a> {
    information_request: &'a str,
    blobs: RetrievalBlobs<'a>,
    dialog: Vec<serde_json::Value>,
    max_output_length: u32,
    disable_codebase_retrieval: bool,
    enable_commit_retrieval: bool,
}

#[derive(Debug, Deserialize)]
struct RetrievalResp { #[serde(default)] formatted_retrieval: String }

fn auth_client(token: &str, timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("augmcp/0.1")
        .build()
        .expect("reqwest client")
}

async fn retry<F, Fut, T>(mut f: F, retries: usize, base_delay_ms: u64) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..retries {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < retries {
                    let delay = base_delay_ms * (1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("retry failed")))
}

pub async fn upload_new_blobs(cfg: &Config, new_blobs: &[BlobUpload]) -> Result<Vec<String>> {
    if new_blobs.is_empty() { return Ok(Vec::new()); }
    let url = format!("{}/batch-upload", cfg.settings.base_url.trim_end_matches('/'));
    let client = auth_client(&cfg.settings.token, 30);
    let payload = BatchUploadPayload { blobs: new_blobs };

    let resp: BatchUploadResp = retry(
        || async {
            let r = client.post(&url)
                .bearer_auth(&cfg.settings.token)
                .json(&payload)
                .send().await?;
            if !r.status().is_success() {
                let sc = r.status();
                let t = r.text().await.unwrap_or_default();
                return Err(anyhow!("upload failed: {} {}", sc, t));
            }
            Ok(r.json::<BatchUploadResp>().await?)
        },
        3,
        1000,
    ).await?;
    Ok(resp.blob_names)
}

pub async fn retrieve_formatted(
    cfg: &Config,
    all_blob_names: &[String],
    query: &str,
) -> Result<String> {
    let url = format!("{}/agents/codebase-retrieval", cfg.settings.base_url.trim_end_matches('/'));
    let client = auth_client(&cfg.settings.token, 60);
    let payload = RetrievalPayload {
        information_request: query,
        blobs: RetrievalBlobs { checkpoint_id: None, added_blobs: all_blob_names, deleted_blobs: vec![] },
        dialog: vec![],
        max_output_length: 0,
        disable_codebase_retrieval: false,
        enable_commit_retrieval: false,
    };

    let resp: RetrievalResp = retry(
        || async {
            let r = client.post(&url)
                .bearer_auth(&cfg.settings.token)
                .json(&payload)
                .send().await?;
            if !r.status().is_success() {
                let sc = r.status();
                let t = r.text().await.unwrap_or_default();
                return Err(anyhow!("retrieve failed: {} {}", sc, t));
            }
            Ok(r.json::<RetrievalResp>().await?)
        },
        3,
        2000,
    ).await?;

    if resp.formatted_retrieval.trim().is_empty() {
        Ok("No relevant code context found for your query.".to_string())
    } else {
        Ok(resp.formatted_retrieval)
    }
}
