use augmcp::{
    backend,
    config::{Config, Settings},
    service,
};
use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use std::{fs, net::SocketAddr, path::Path, sync::Arc};
use tokio::net::TcpListener;

#[derive(Deserialize)]
struct UploadPayload {
    blobs: Vec<augmcp::indexer::BlobUpload>,
}
#[derive(Serialize)]
struct UploadResp {
    blob_names: Vec<String>,
}
#[derive(Deserialize)]
struct RetrievalPayload {
    information_request: String,
}
#[derive(Serialize)]
struct RetrievalResp {
    formatted_retrieval: String,
}

async fn start_stub_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/batch-upload",
            post(|Json(p): Json<UploadPayload>| async move {
                let names = p
                    .blobs
                    .into_iter()
                    .map(|b| format!("n:{}", b.path))
                    .collect();
                Json(UploadResp { blob_names: names })
            }),
        )
        .route(
            "/agents/codebase-retrieval",
            post(|Json(_p): Json<RetrievalPayload>| async move {
                Json(RetrievalResp {
                    formatted_retrieval: "OK".to_string(),
                })
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}

fn cfg_with_base(base_url: String, data_root: &Path) -> Config {
    let root_dir = data_root.join("cfg");
    let data_dir = data_root.join("data");
    fs::create_dir_all(&root_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    Config {
        settings: Settings {
            batch_size: 10,
            max_lines_per_blob: 1000,
            base_url,
            token: "T".into(),
            text_extensions: vec![".txt".into()],
            exclude_patterns: vec![],
            max_output_length: 0,
            disable_codebase_retrieval: false,
            enable_commit_retrieval: false,
        },
        root_dir: root_dir.clone(),
        data_dir: data_dir.clone(),
        settings_path: root_dir.join("settings.toml"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn persist_and_incremental_and_concurrent() {
    let (addr, _h) = start_stub_server().await;
    let base_url = format!("http://{}:{}", addr.ip(), addr.port());
    let td = tempfile::tempdir().unwrap();
    let cfg = cfg_with_base(base_url, td.path());

    // Prepare two projects
    let proj_a = td.path().join("projA");
    let proj_b = td.path().join("projB");
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    fs::write(proj_a.join("a.txt"), "A\n").unwrap();
    fs::write(proj_b.join("b.txt"), "B\n").unwrap();

    let key_a = augmcp::config::normalize_path(&proj_a).unwrap();
    let key_b = augmcp::config::normalize_path(&proj_b).unwrap();

    // First index both concurrently -> should both persist
    let cfg_a = cfg.clone();
    let cfg_b = cfg.clone();
    let pa = proj_a.to_string_lossy().to_string();
    let pb = proj_b.to_string_lossy().to_string();
    let (ra, rb) = tokio::join!(
        service::index_and_persist(&cfg_a, &key_a, &pa, false),
        service::index_and_persist(&cfg_b, &key_b, &pb, false)
    );
    let ra = ra.unwrap();
    let rb = rb.unwrap();
    assert!(ra.0 >= 1 && ra.1 >= 1);
    assert!(rb.0 >= 1 && rb.1 >= 1);

    // Second index on A with no changes -> new=0
    let (t, newn, existing, _all) = service::index_and_persist(&cfg, &key_a, &pa, false)
        .await
        .unwrap();
    assert!(t >= 1);
    assert_eq!(newn, 0, "No changes should yield 0 new blobs");
    assert!(existing >= 1);
}
