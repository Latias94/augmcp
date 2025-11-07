use augmcp::backend;
// use augmcp::indexer::{ProjectsIndex, collect_blobs, incremental_plan};
use augmcp::service;
use augmcp::{AugServer, config::Config};
use axum::Json;
use axum::extract::State;
use clap::{Parser, ValueEnum};
use rmcp::serve_server;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use serde::{Deserialize, Serialize};
use tracing_appender::rolling;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
// (unused prev imports removed)

#[derive(Debug, Clone, ValueEnum)]
enum TransportKind {
    Stdio,
    Http,
}

#[derive(Parser, Debug)]
#[command(
    name = "augmcp",
    version,
    about = "MCP server for code indexing + retrieval"
)]
struct Cli {
    /// Transport: stdio or http
    #[arg(long, value_enum, default_value = "http")]
    transport: TransportKind,
    /// HTTP bind address when transport=http
    #[arg(long, default_value = "127.0.0.1:8888")]
    bind: String,
    /// Override BASE_URL
    #[arg(long)]
    base_url: Option<String>,
    /// Override TOKEN
    #[arg(long)]
    token: Option<String>,
    /// Persist overrides to settings file
    #[arg(long, default_value_t = false)]
    persist_config: bool,
    /// One-shot run without MCP: project path
    #[arg(long)]
    oneshot_path: Option<String>,
    /// One-shot run without MCP: query
    #[arg(long)]
    oneshot_query: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load_with_overrides(cli.base_url, cli.token)?;

    // Setup logging: console (info) + rolling file (debug)
    let log_dir = cfg.log_dir();
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = rolling::daily(&log_dir, "augmcp.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_ansi(true))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();

    if cli.persist_config {
        cfg.save()?;
    }
    tracing::info!(config_file = %cfg.settings_path.display(), data_dir = %cfg.data_dir.display(), log_file = %log_dir.join("augmcp.log").display(), "paths initialized");

    // One-shot direct execution (no MCP) for quick testing
    if let (Some(path), Some(query)) = (cli.oneshot_path.clone(), cli.oneshot_query.clone()) {
        let project_key = augmcp::config::normalize_path(&path)?;
        let (_total, _newn, _existing, all_blob_names) = service::index_and_persist(&cfg, &project_key, &path, false).await?;
        let result = backend::retrieve_formatted(&cfg, &all_blob_names, &query).await?;
        println!("{}", result);
        return Ok(());
    }
    let server = AugServer::new(cfg);

    match cli.transport {
        TransportKind::Stdio => {
            println!("augmcp stdio server started");
            let io = (tokio::io::stdin(), tokio::io::stdout());
            serve_server(server, io).await?;
        }
        TransportKind::Http => {
            #[derive(Debug, Deserialize)]
            struct SearchReq {
                project_root_path: Option<String>,
                alias: Option<String>,
                query: String,
                skip_index_if_indexed: Option<bool>,
            }
            #[derive(Debug, Serialize)]
            struct SearchResp {
                status: String,
                result: String,
            }
            #[derive(Clone)]
            struct AppState {
                server: AugServer,
                tasks: augmcp::tasks::TaskManager,
            }

            let app_state = AppState {
                server: server.clone(),
                tasks: augmcp::tasks::TaskManager::new(),
            };
            let srv_factory = app_state.server.clone();
            let service = StreamableHttpService::new(
                move || Ok(srv_factory.clone()),
                LocalSessionManager::default().into(),
                Default::default(),
            );
            let server_state = app_state.clone();
            let router = axum::Router::new()
                .nest_service("/mcp", service)
                .route("/healthz", axum::routing::get(|| async {
                    #[derive(serde::Serialize)]
                    struct HealthResp { status: &'static str, version: &'static str }
                    axum::Json(HealthResp{ status: "ok", version: env!("CARGO_PKG_VERSION") })
                }))
                .route("/api/search", axum::routing::post(
                    |State(app): State<AppState>, Json(req): Json<SearchReq>| async move {
                        let cfg = app.server.get_cfg();
                        let (project_key, path) = match service::resolve_target(&cfg, req.alias.clone(), req.project_root_path.clone()) {
                            Ok(v) => v,
                            Err(e) => return Json(SearchResp{ status: "error".into(), result: e.to_string() })
                        };
                        tracing::info!(path = %path, "/api/search invoked");
                        if app.tasks.is_running(&project_key) {
                            return Json(SearchResp{ status: "accepted".into(), result: "indexing in progress; please retry later".into() });
                        }
                        let skip = req.skip_index_if_indexed.unwrap_or(true);
                        let result = match service::ensure_index_then_retrieve(&cfg, &project_key, &path, &req.query, skip).await {
                            Ok(s) => s,
                            Err(e) => format!("Error: {}", e),
                        };
                        Json(SearchResp{ status: "success".into(), result })
                    }
                ))
                .route("/api/index", axum::routing::post(
                    |State(app): State<AppState>, Json(req): Json<serde_json::Value>| async move {
                        #[derive(Deserialize)]
                        struct IndexReq {
                            project_root_path: Option<String>,
                            alias: Option<String>,
                            force_full: Option<bool>,
                            #[serde(rename = "async")] r#async: Option<bool>,
                        }
                        #[derive(Serialize)]
                        struct IndexResp { status: String, result: String }
                        let req: IndexReq = match serde_json::from_value(req) { Ok(v) => v, Err(e) => return Json(IndexResp{ status: "error".into(), result: e.to_string() }) };
                        let cfg = app.server.get_cfg();
                        use augmcp::indexer::Aliases;
                        let mut aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                        let path = match (req.alias.clone(), req.project_root_path.clone()) {
                            (Some(a), Some(p)) => { let norm = match augmcp::config::normalize_path(&p) { Ok(s)=>s, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()})}; aliases.set(a, norm); let _ = aliases.save(&cfg.aliases_file()); p },
                            (Some(a), None) => match aliases.resolve(&a) { Some(p) => p.clone(), None => return Json(IndexResp{ status:"error".into(), result: "alias not found and no path provided".into()}) },
                            (None, Some(p)) => p,
                            (None, None) => return Json(IndexResp{ status:"error".into(), result: "provide project_root_path or alias".into()}),
                        };
                        let project_key = match augmcp::config::normalize_path(&path) { Ok(x)=>x, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()}) };
                        let run_async = req.r#async.unwrap_or(false);
                        // 去重：异步索引若已存在任务则直接返回
                        if run_async {
                            if app.tasks.is_running(&project_key) {
                                return Json(IndexResp{ status: "accepted".into(), result: format!("indexing already in progress for {}", &path) });
                            }
                        }
                        if run_async {
                            let cfg_bg = cfg.clone();
                            let path_bg = path.clone();
                            let key_bg = project_key.clone();
                            // 标记任务开始，便于 /api/tasks 立即可见
                            let _ = app.tasks.begin(&project_key);
                            let _tasks_map = app.tasks.clone();
                            let force_full = req.force_full.unwrap_or(false);
                            let handle = tokio::spawn(async move {
                                tracing::info!(path = %path_bg, force_full = force_full, "HTTP /api/index async start");
                                let tasks_bg = _tasks_map.clone();
                                tasks_bg.set_phase(&key_bg, "collecting");
                                let mut totals_set = false;
                                match service::index_and_persist_with_progress(&cfg_bg, &key_bg, &path_bg, force_full, |p| {
                                    if !totals_set {
                                        tasks_bg.set_upload_totals(&key_bg, p.total_items, p.chunks_total, p.total_items);
                                        totals_set = true;
                                    }
                                    tasks_bg.on_chunk(&key_bg, p.uploaded_items, p.chunk_index, p.chunk_bytes);
                                }).await {
                                    Ok((_total, _newn, _existing, all)) => {
                                        tracing::info!(blobs = all.len(), "HTTP /api/index async done");
                                        tasks_bg.finish(&key_bg);
                                    }
                                    Err(e) => {
                                        tracing::error!(error=%e.to_string(), "async index failed");
                                        tasks_bg.fail(&key_bg, e.to_string());
                                    }
                                }
                                return;
                                
                                
                                
                                
                                
                                // 完成后移除任务标记 tasks.finish(&key_bg);
                            });
                            app.tasks.set_handle(&project_key, handle);
                            return Json(IndexResp{ status: "accepted".into(), result: format!("async indexing started for {}", &path) });
                        }

                        tracing::info!(path = %path, force_full = req.force_full.unwrap_or(false), "HTTP /api/index start");
                        match service::index_and_persist(&cfg, &project_key, &path, req.force_full.unwrap_or(false)).await {
                            Ok((total, newn, existing, _)) => {
                                let msg = format!("Index complete: total_blobs={}, new_blobs={}, existing_blobs={}", total, newn, existing);
                                tracing::info!("HTTP /api/index done: {}", msg);
                                Json(IndexResp{ status:"success".into(), result: msg })
                            }
                            Err(e) => Json(IndexResp{ status:"error".into(), result: e.to_string() })
                        }
                    }
                ))
                .route("/api/tasks", axum::routing::get(
                    |State(app): State<AppState>, axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                        #[derive(Serialize)]
                        struct TaskResp { status: String, running: bool, progress: Option<augmcp::tasks::TaskProgress>, eta_secs: Option<u64> }
                        let cfg = app.server.get_cfg();
                        let alias = params.get("alias").cloned();
                        let path = params.get("project_root_path").cloned();
                        let (key, _p) = match service::resolve_target(&cfg, alias, path) {
                            Ok(v) => v,
                            Err(_) => return axum::Json(TaskResp{ status: "error".into(), running: false, progress: None, eta_secs: None }),
                        };
                        let running = app.tasks.is_running(&key);
                        let progress = app.tasks.get(&key);
                        let mut eta = None;
                        if let Some(p) = &progress {
                            if p.chunk_index > 0 && p.chunks_total > 0 && p.updated_at >= p.started_at {
                                let elapsed = p.updated_at.saturating_sub(p.started_at);
                                let remaining_chunks = p.chunks_total.saturating_sub(p.chunk_index);
                                if elapsed > 0 && remaining_chunks > 0 {
                                    let avg = elapsed / (p.chunk_index as u64).max(1);
                                    eta = Some(avg.saturating_mul(remaining_chunks as u64));
                                }
                            }
                        }
                        axum::Json(TaskResp{ status: "success".into(), running, progress, eta_secs: eta })
                    }
                ))
                .route("/api/index/stop", axum::routing::post(
                    |State(app): State<AppState>, Json(req): Json<serde_json::Value>| async move {
                        #[derive(Deserialize)]
                        struct StopReq { project_root_path: Option<String>, alias: Option<String> }
                        #[derive(Serialize)]
                        struct StopResp { status: String, result: String }
                        let req: StopReq = match serde_json::from_value(req) { Ok(v) => v, Err(e) => return Json(StopResp{ status: "error".into(), result: e.to_string() }) };
                        let cfg = app.server.get_cfg();
                        use augmcp::indexer::Aliases;
                        let aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                        let path = match (req.alias.clone(), req.project_root_path.clone()) {
                            (Some(_), Some(p)) => p,
                            (Some(a), None) => match aliases.resolve(&a) { Some(p) => p.clone(), None => return Json(StopResp{ status:"error".into(), result: "alias not found and no path provided".into()}) },
                            (None, Some(p)) => p,
                            (None, None) => return Json(StopResp{ status:"error".into(), result: "provide project_root_path or alias".into()}),
                        };
                        let project_key = match augmcp::config::normalize_path(&path) { Ok(x)=>x, Err(e)=> return Json(StopResp{ status:"error".into(), result: e.to_string()}) };
                        if app.tasks.abort(&project_key) { tracing::info!(path = %path, "HTTP /api/index/stop: aborted running task"); return Json(StopResp{ status: "success".into(), result: "aborted".into() }); } Json(StopResp{ status: "error".into(), result: "no running task".into() })
                    }
                ))
                .with_state(server_state);
            let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
            tracing::info!("augmcp http server listening on {}", &cli.bind);
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }

    Ok(())
}
