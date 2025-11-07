use augmcp::{
    AppState, AugServer,
    config::{Config, Settings},
};
use axum::http::{Request, StatusCode};
use axum::{Json, routing::post};
use axum::{
    Router,
    body::{self, Body},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tower::util::ServiceExt;

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

async fn start_slow_stub() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/batch-upload",
            post(|Json(p): Json<UploadPayload>| async move {
                // 模拟慢速上传
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
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
            post(|Json(p): Json<RetrievalPayload>| async move {
                Json(RetrievalResp {
                    formatted_retrieval: format!("OK: {}", p.information_request),
                })
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{}:{}", addr.ip(), addr.port());
    let h = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, h)
}

fn test_cfg(base_url: String, root: &std::path::Path) -> Config {
    let root_dir = root.join("cfg");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    Config {
        settings: Settings {
            batch_size: 1,
            max_lines_per_blob: 100,
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
async fn http_index_async_cancel_and_search() {
    let (base_url, _h) = start_slow_stub().await;
    let td = tempfile::tempdir().unwrap();
    let cfg = test_cfg(base_url, td.path());

    // 准备项目目录（5个小文件，batch_size=1 -> 多个chunk）
    let proj = td.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    for i in 0..5 {
        std::fs::write(proj.join(format!("f{i}.txt")), format!("c{i}\n")).unwrap();
    }
    let path_str = proj.to_string_lossy().to_string();

    let server = AugServer::new(cfg.clone());
    let app_state = AppState {
        server,
        tasks: augmcp::tasks::TaskManager::new(),
    };
    let router = augmcp::http_router::build_router(app_state);

    // 启动异步索引
    let body = json!({"project_root_path": path_str, "async": true});
    let req = Request::post("/api/index")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 查询任务，应该在运行
    let q = format!("/api/tasks?project_root_path={}", proj.to_string_lossy());
    let req = Request::get(&q).body(Body::empty()).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 立刻停止任务
    let stop = json!({"project_root_path": proj.to_string_lossy()});
    let req = Request::post("/api/index/stop")
        .header("content-type", "application/json")
        .body(Body::from(stop.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 轮询任务直到不在运行
    for _ in 0..20 {
        let req = Request::get(&q).body(Body::empty()).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if v["running"].as_bool() == Some(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // 同步索引一次（确保有缓存）
    let body = json!({"project_root_path": proj.to_string_lossy()});
    let req = Request::post("/api/index")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 搜索（命中缓存 -> 直接检索）
    let body = json!({"project_root_path": proj.to_string_lossy(), "query": "hello"});
    let req = Request::post("/api/search")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
