use augmcp::{config::Config, AugServer};
use clap::{Parser, ValueEnum};
use rmcp::serve_server;
use rmcp::transport::streamable_http_server::{StreamableHttpService, session::local::LocalSessionManager};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_appender::rolling;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use augmcp::indexer::{collect_blobs, ProjectsIndex, incremental_plan};
use augmcp::backend;
use std::{sync::Arc, collections::HashMap};
use parking_lot::Mutex;

#[derive(Debug, Clone, ValueEnum)]
enum TransportKind { Stdio, Http }

#[derive(Parser, Debug)]
#[command(name = "augmcp", version, about = "MCP server for code indexing + retrieval")] 
struct Cli {
    /// Transport: stdio or http
    #[arg(long, value_enum, default_value = "stdio")]
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
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer().with_ansi(true))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file_writer))
        .init();

    if cli.persist_config { cfg.save()?; }
    tracing::info!(config_file = %cfg.settings_path.display(), data_dir = %cfg.data_dir.display(), log_file = %log_dir.join("augmcp.log").display(), "paths initialized");
    
    // One-shot direct execution (no MCP) for quick testing
    if let (Some(path), Some(query)) = (cli.oneshot_path.clone(), cli.oneshot_query.clone()) {
        let project_key = augmcp::config::normalize_path(&path)?;
        let blobs = collect_blobs(
            std::path::Path::new(&path),
            &cfg.text_extensions_set(),
            cfg.settings.max_lines_per_blob,
            &cfg.settings.exclude_patterns,
        )?;
        if blobs.is_empty() {
            println!("Error: No text files found in project");
            return Ok(());
        }
        let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
        let (new_blobs, all_blob_names) = incremental_plan(&project_key, &blobs, &projects);
        if !new_blobs.is_empty() {
            let _ = backend::upload_new_blobs(&cfg, &new_blobs).await?;
        }
        projects.0.insert(project_key, all_blob_names.clone());
        let _ = projects.save(&cfg.projects_file());
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
            struct SearchReq { project_root_path: Option<String>, alias: Option<String>, query: String, skip_index_if_indexed: Option<bool> }
            #[derive(Debug, Serialize)]
            struct SearchResp { status: String, result: String }
            #[derive(Clone)]
            struct AppState { server: AugServer, tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> }

            let app_state = AppState { server: server.clone(), tasks: Arc::new(Mutex::new(HashMap::new())) };
            let srv_factory = app_state.server.clone();
            let service = StreamableHttpService::new(
                move || Ok(srv_factory.clone()),
                LocalSessionManager::default().into(),
                Default::default(),
            );
            let server_state = app_state.clone();
            let router = axum::Router::new()
                .nest_service("/mcp", service)
                .route("/api/search", axum::routing::post(
                    |State(app): State<AppState>, Json(req): Json<SearchReq>| async move {
                        use augmcp::indexer::Aliases;
                        let cfg = app.server.get_cfg();
                        let aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                        let path_opt = match (&req.alias, &req.project_root_path) {
                            (Some(a), _) => aliases.resolve(a).cloned(),
                            (None, Some(p)) => Some(p.clone()),
                            _ => None,
                        };
                        let path = match path_opt { Some(p) => p, None => return Json(SearchResp{ status: "error".into(), result: "provide project_root_path or alias".into() }) };
                        tracing::info!(path = %path, "/api/search invoked");
                        let project_key = match augmcp::config::normalize_path(&path) { Ok(x) => x, Err(e) => return Json(SearchResp{ status: "error".into(), result: format!("normalize error: {}", e)}) };

                        let skip_if_indexed = req.skip_index_if_indexed.unwrap_or(true);
                        let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
                        let mut all_blob_names: Vec<String> = Vec::new();
                        let mut need_index = true;
                        if skip_if_indexed {
                            if let Some(existing) = projects.0.get(&project_key) {
                                if !existing.is_empty() { all_blob_names = existing.clone(); need_index = false; }
                            }
                        }
                        if need_index {
                            // 若该路径存在异步索引任务，在进行中则告知客户端稍后重试
                            if app.tasks.lock().contains_key(&project_key) {
                                return Json(SearchResp{ status: "accepted".into(), result: "indexing in progress; please retry later".into() });
                            }
                            let blobs = match collect_blobs(
                                std::path::Path::new(&path),
                                &cfg.text_extensions_set(),
                                cfg.settings.max_lines_per_blob,
                                &cfg.settings.exclude_patterns,
                            ) { Ok(v) => v, Err(e) => return Json(SearchResp{ status: "error".into(), result: e.to_string()}) };
                            if blobs.is_empty() { return Json(SearchResp{ status: "error".into(), result: "No text files found in project".into()}); }
                            let (new_blobs, all_names) = incremental_plan(&project_key, &blobs, &projects);
                            if !new_blobs.is_empty() {
                                tracing::info!("uploading {} new blobs", new_blobs.len());
                                if let Err(e) = backend::upload_new_blobs(&cfg, &new_blobs).await {
                                    return Json(SearchResp{ status: "error".into(), result: format!("upload failed: {}", e)});
                                }
                            }
                            projects.0.insert(project_key, all_names.clone());
                            let _ = projects.save(&cfg.projects_file());
                            all_blob_names = all_names;
                        }
                        let result = match backend::retrieve_formatted(&cfg, &all_blob_names, &req.query).await { Ok(s) => s, Err(e) => format!("Error: {}", e) };
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
                            if app.tasks.lock().contains_key(&project_key) {
                                return Json(IndexResp{ status: "accepted".into(), result: format!("indexing already in progress for {}", &path) });
                            }
                        }
                        if run_async {
                            let cfg_bg = cfg.clone();
                            let path_bg = path.clone();
                            let key_bg = project_key.clone();
                            let tasks_map = app.tasks.clone();
                            let handle = tokio::spawn(async move {
                                tracing::info!(path = %path_bg, force_full = req.force_full.unwrap_or(false), "HTTP /api/index async start");
                                let mut projects = ProjectsIndex::load(&cfg_bg.projects_file()).unwrap_or_default();
                                if req.force_full.unwrap_or(false) { projects.0.remove(&key_bg); }
                                let blobs = match collect_blobs(
                                    std::path::Path::new(&path_bg),
                                    &cfg_bg.text_extensions_set(),
                                    cfg_bg.settings.max_lines_per_blob,
                                    &cfg_bg.settings.exclude_patterns,
                                ) { Ok(v)=>v, Err(e)=> { tracing::error!(error=%e.to_string(), "collect_blobs failed"); return; } };
                                tracing::info!(collected = blobs.len(), "files collected (async)");
                                if blobs.is_empty() { tracing::warn!("No text files found in project"); return; }
                                let (new_blobs, all_names) = incremental_plan(&key_bg, &blobs, &projects);
                                tracing::info!(total = blobs.len(), new = new_blobs.len(), existing = (all_names.len().saturating_sub(new_blobs.len())), "incremental computed");
                                if !new_blobs.is_empty() {
                                    tracing::info!(uploading = new_blobs.len(), "uploading new blobs (async)");
                                    if let Err(e) = backend::upload_new_blobs(&cfg_bg, &new_blobs).await {
                                        tracing::error!(error=%e.to_string(), "upload failed (async)");
                                        return;
                                    }
                                }
                                projects.0.insert(key_bg.clone(), all_names.clone());
                                let _ = projects.save(&cfg_bg.projects_file());
                                tracing::info!(blobs = all_names.len(), "HTTP /api/index async done");
                                // 完成后移除任务标记
                                let mut map = tasks_map.lock();
                                map.remove(&key_bg);
                            });
                            app.tasks.lock().insert(project_key.clone(), handle);
                            return Json(IndexResp{ status: "accepted".into(), result: format!("async indexing started for {}", &path) });
                        }

                        tracing::info!(path = %path, force_full = req.force_full.unwrap_or(false), "HTTP /api/index start");
                        let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
                        if req.force_full.unwrap_or(false) { projects.0.remove(&project_key); }
                        let blobs = match collect_blobs(
                            std::path::Path::new(&path),
                            &cfg.text_extensions_set(),
                            cfg.settings.max_lines_per_blob,
                            &cfg.settings.exclude_patterns,
                        ) { Ok(v)=>v, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()}) };
                        tracing::info!(collected = blobs.len(), "files collected");
                        if blobs.is_empty() { return Json(IndexResp{ status:"error".into(), result: "No text files found in project".into()}); }
                        let (new_blobs, all_names) = incremental_plan(&project_key, &blobs, &projects);
                        tracing::info!(total = blobs.len(), new = new_blobs.len(), existing = (all_names.len().saturating_sub(new_blobs.len())), "incremental computed");
                        if !new_blobs.is_empty() {
                            tracing::info!(uploading = new_blobs.len(), "uploading new blobs");
                            if let Err(e) = backend::upload_new_blobs(&cfg, &new_blobs).await {
                                return Json(IndexResp{ status:"error".into(), result: format!("upload failed: {}", e) });
                            }
                        }
                        projects.0.insert(project_key, all_names.clone());
                        let _ = projects.save(&cfg.projects_file());
                        let msg = format!("Index complete: total_blobs={}, new_blobs={}, existing_blobs={}", all_names.len(), new_blobs.len(), all_names.len().saturating_sub(new_blobs.len()));
                        tracing::info!("HTTP /api/index done: {}", msg);
                        Json(IndexResp{ status:"success".into(), result: msg })
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
                            (Some(a), Some(p)) => p,
                            (Some(a), None) => match aliases.resolve(&a) { Some(p) => p.clone(), None => return Json(StopResp{ status:"error".into(), result: "alias not found and no path provided".into()}) },
                            (None, Some(p)) => p,
                            (None, None) => return Json(StopResp{ status:"error".into(), result: "provide project_root_path or alias".into()}),
                        };
                        let project_key = match augmcp::config::normalize_path(&path) { Ok(x)=>x, Err(e)=> return Json(StopResp{ status:"error".into(), result: e.to_string()}) };
                        let mut map = app.tasks.lock();
                        if let Some(handle) = map.remove(&project_key) {
                            handle.abort();
                            tracing::info!(path = %path, "HTTP /api/index/stop: aborted running task");
                            return Json(StopResp{ status: "success".into(), result: "aborted".into() });
                        }
                        Json(StopResp{ status: "error".into(), result: "no running task".into() })
                    }
                ))
                .with_state(server_state);
            let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
            tracing::info!("augmcp http server listening on {}", &cli.bind);
            axum::serve(listener, router)
                .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
                .await?;
        }
    }

    Ok(())
}

