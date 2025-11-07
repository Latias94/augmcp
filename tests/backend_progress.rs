use augmcp::{
    backend,
    config::{Config, Settings},
    indexer::BlobUpload,
};
use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

#[derive(Deserialize)]
struct UploadPayload {
    blobs: Vec<BlobUpload>,
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
                    .map(|b| {
                        // Not important to match actual hash; just return something per blob
                        format!("stub:{}:{}", b.path.len(), b.content.len())
                    })
                    .collect();
                Json(UploadResp { blob_names: names })
            }),
        )
        .route(
            "/agents/codebase-retrieval",
            post(|Json(p): Json<RetrievalPayload>| async move {
                Json(RetrievalResp {
                    formatted_retrieval: format!("OK: {}", p.information_request),
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

fn test_config(base_url: String) -> Config {
    let td = tempfile::tempdir().unwrap();
    let root_dir = td.path().join("cfg");
    let data_dir = td.path().join("data");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    Config {
        settings: Settings {
            batch_size: 2,
            max_lines_per_blob: 100,
            base_url,
            token: "TEST".into(),
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
async fn upload_progress_and_retrieval() {
    let (addr, _h) = start_stub_server().await;
    let base_url = format!("http://{}:{}", addr.ip(), addr.port());
    let cfg = test_config(base_url);

    // Prepare 5 blobs -> 3 chunks with batch_size=2
    let blobs: Vec<BlobUpload> = (0..5)
        .map(|i| BlobUpload {
            path: format!("f{i}.txt"),
            content: format!("c{i}"),
        })
        .collect();

    let events: Arc<Mutex<Vec<backend::UploadProgress>>> = Arc::new(Mutex::new(vec![]));
    let ev2 = events.clone();
    let _ = backend::upload_new_blobs_with_progress(&cfg, &blobs, move |p| {
        ev2.lock().unwrap().push(p);
    })
    .await
    .unwrap();

    let got = events.lock().unwrap().clone();
    assert_eq!(
        got.len(),
        3,
        "expected 3 chunks for 5 items with batch_size=2"
    );
    assert_eq!(got.last().unwrap().uploaded_items, 5);

    // Retrieval
    let ans = backend::retrieve_formatted(&cfg, &[], "hello")
        .await
        .unwrap();
    assert!(ans.starts_with("OK: hello"));
}
