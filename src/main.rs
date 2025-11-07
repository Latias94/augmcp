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

            let srv_factory = server.clone();
            let service = StreamableHttpService::new(
                move || Ok(srv_factory.clone()),
                LocalSessionManager::default().into(),
                Default::default(),
            );
            let server_state = server.clone();
            let router = axum::Router::new()
                .nest_service("/mcp", service)
                .route("/api/search", axum::routing::post(
                    |State(srv): State<AugServer>, Json(req): Json<SearchReq>| async move {
                        use augmcp::indexer::Aliases;
                        let cfg = srv.get_cfg();
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
                    |State(srv): State<AugServer>, Json(req): Json<serde_json::Value>| async move {
                        #[derive(Deserialize)]
                        struct IndexReq { project_root_path: Option<String>, alias: Option<String>, force_full: Option<bool> }
                        #[derive(Serialize)]
                        struct IndexResp { status: String, result: String }
                        let req: IndexReq = match serde_json::from_value(req) { Ok(v) => v, Err(e) => return Json(IndexResp{ status: "error".into(), result: e.to_string() }) };
                        let cfg = srv.get_cfg();
                        use augmcp::indexer::Aliases;
                        let mut aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                        let path = match (req.alias.clone(), req.project_root_path.clone()) {
                            (Some(a), Some(p)) => { let norm = match augmcp::config::normalize_path(&p) { Ok(s)=>s, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()})}; aliases.set(a, norm); let _ = aliases.save(&cfg.aliases_file()); p },
                            (Some(a), None) => match aliases.resolve(&a) { Some(p) => p.clone(), None => return Json(IndexResp{ status:"error".into(), result: "alias not found and no path provided".into()}) },
                            (None, Some(p)) => p,
                            (None, None) => return Json(IndexResp{ status:"error".into(), result: "provide project_root_path or alias".into()}),
                        };
                        let project_key = match augmcp::config::normalize_path(&path) { Ok(x)=>x, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()}) };
                        let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
                        if req.force_full.unwrap_or(false) { projects.0.remove(&project_key); }
                        let blobs = match collect_blobs(
                            std::path::Path::new(&path),
                            &cfg.text_extensions_set(),
                            cfg.settings.max_lines_per_blob,
                            &cfg.settings.exclude_patterns,
                        ) { Ok(v)=>v, Err(e)=> return Json(IndexResp{ status:"error".into(), result: e.to_string()}) };
                        if blobs.is_empty() { return Json(IndexResp{ status:"error".into(), result: "No text files found in project".into()}); }
                        let (new_blobs, all_names) = incremental_plan(&project_key, &blobs, &projects);
                        if !new_blobs.is_empty() {
                            if let Err(e) = backend::upload_new_blobs(&cfg, &new_blobs).await {
                                return Json(IndexResp{ status:"error".into(), result: format!("upload failed: {}", e) });
                            }
                        }
                        projects.0.insert(project_key, all_names.clone());
                        let _ = projects.save(&cfg.projects_file());
                        let msg = format!("Index complete: total_blobs={}, new_blobs={}, existing_blobs={}", all_names.len(), new_blobs.len(), all_names.len().saturating_sub(new_blobs.len()));
                        Json(IndexResp{ status:"success".into(), result: msg })
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
