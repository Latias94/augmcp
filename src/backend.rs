//! REST backend client for uploading blobs and performing retrieval.

use crate::{config::Config, indexer::BlobUpload};
use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct BatchUploadPayload<'a> {
    blobs: &'a [BlobUpload],
}

#[derive(Debug, Deserialize)]
struct BatchUploadResp {
    #[serde(default)]
    blob_names: Vec<String>,
}

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
struct RetrievalResp {
    #[serde(default)]
    formatted_retrieval: String,
}

fn auth_client(timeout_secs: u64) -> Client {
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct UploadProgress {
    pub chunk_index: usize,
    pub chunks_total: usize,
    pub uploaded_items: usize,
    pub total_items: usize,
    pub chunk_items: usize,
    pub chunk_bytes: usize,
}

pub async fn upload_new_blobs_with_progress<F>(
    cfg: &Config,
    new_blobs: &[BlobUpload],
    mut on_progress: F,
) -> Result<Vec<String>>
where
    F: FnMut(UploadProgress),
{
    if new_blobs.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "{}/batch-upload",
        cfg.settings.base_url.trim_end_matches('/')
    );
    let client = auth_client(30);

    let batch_size = cfg.settings.batch_size.max(1);
    let mut all_blob_names: Vec<String> = Vec::new();
    let total = new_blobs.len();
    let total_chunks = (total + batch_size - 1) / batch_size;

    for (idx, chunk) in new_blobs.chunks(batch_size).enumerate() {
        let payload = BatchUploadPayload { blobs: chunk };
        let resp: BatchUploadResp = retry(
            || async {
                let r = client
                    .post(&url)
                    .bearer_auth(&cfg.settings.token)
                    .json(&payload)
                    .send()
                    .await?;
                if !r.status().is_success() {
                    let sc = r.status();
                    let t = r.text().await.unwrap_or_default();
                    return Err(anyhow!("upload failed: {} {}", sc, t));
                }
                Ok(r.json::<BatchUploadResp>().await?)
            },
            3,
            1000,
        )
        .await?;
        all_blob_names.extend(resp.blob_names);
        let uploaded_cnt = ((idx + 1) * batch_size).min(total);
        let chunk_bytes: usize = chunk.iter().map(|b| b.content.len()).sum();
        on_progress(UploadProgress {
            chunk_index: idx + 1,
            chunks_total: total_chunks,
            uploaded_items: uploaded_cnt,
            total_items: total,
            chunk_items: chunk.len(),
            chunk_bytes,
        });
        // 让出调度，便于任务被及时取消（/api/index/stop）
        tokio::task::yield_now().await;
    }
    Ok(all_blob_names)
}

pub async fn upload_new_blobs(cfg: &Config, new_blobs: &[BlobUpload]) -> Result<Vec<String>> {
    if new_blobs.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "{}/batch-upload",
        cfg.settings.base_url.trim_end_matches('/')
    );
    let client = auth_client(30);

    // 分批上传，避免一次性 payload 过大导致 413（Payload Too Large）
    let batch_size = cfg.settings.batch_size.max(1);
    let mut all_blob_names: Vec<String> = Vec::new();
    let total = new_blobs.len();
    let total_chunks = (total + batch_size - 1) / batch_size;
    tracing::info!(
        total_new = total,
        batch_size,
        chunks = total_chunks,
        "upload start"
    );
    for (idx, chunk) in new_blobs.chunks(batch_size).enumerate() {
        let payload = BatchUploadPayload { blobs: chunk };
        let resp: BatchUploadResp = retry(
            || async {
                let r = client
                    .post(&url)
                    .bearer_auth(&cfg.settings.token)
                    .json(&payload)
                    .send()
                    .await?;
                if !r.status().is_success() {
                    let sc = r.status();
                    let t = r.text().await.unwrap_or_default();
                    return Err(anyhow!("upload failed: {} {}", sc, t));
                }
                Ok(r.json::<BatchUploadResp>().await?)
            },
            3,
            1000,
        )
        .await?;
        all_blob_names.extend(resp.blob_names);
        let uploaded_cnt = ((idx + 1) * batch_size).min(total);
        let percent = uploaded_cnt as f64 * 100.0 / total as f64;
        // 估算字节数（可选）
        let chunk_bytes: usize = chunk.iter().map(|b| b.content.len()).sum();
        tracing::info!(
            chunk = idx + 1,
            chunks = total_chunks,
            uploaded = uploaded_cnt,
            total,
            percent = format!("{percent:.1}%"),
            chunk_items = chunk.len(),
            chunk_bytes,
            "upload progress"
        );
        // 让出调度，便于任务被及时取消（/api/index/stop）
        tokio::task::yield_now().await;
    }

    Ok(all_blob_names)
}

pub async fn retrieve_formatted(
    cfg: &Config,
    all_blob_names: &[String],
    query: &str,
) -> Result<String> {
    let url = format!(
        "{}/agents/codebase-retrieval",
        cfg.settings.base_url.trim_end_matches('/')
    );
    let client = auth_client(60);
    let payload = RetrievalPayload {
        information_request: query,
        blobs: RetrievalBlobs {
            checkpoint_id: None,
            added_blobs: all_blob_names,
            deleted_blobs: vec![],
        },
        dialog: vec![],
        max_output_length: cfg.settings.max_output_length,
        disable_codebase_retrieval: cfg.settings.disable_codebase_retrieval,
        enable_commit_retrieval: cfg.settings.enable_commit_retrieval,
    };

    let resp: RetrievalResp = retry(
        || async {
            let r = client
                .post(&url)
                .bearer_auth(&cfg.settings.token)
                .json(&payload)
                .send()
                .await?;
            if !r.status().is_success() {
                let sc = r.status();
                let t = r.text().await.unwrap_or_default();
                return Err(anyhow!("retrieve failed: {} {}", sc, t));
            }
            Ok(r.json::<RetrievalResp>().await?)
        },
        3,
        2000,
    )
    .await?;

    if resp.formatted_retrieval.trim().is_empty() {
        Ok("No relevant code context found for your query.".to_string())
    } else {
        Ok(resp.formatted_retrieval)
    }
}
