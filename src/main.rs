use augmcp::backend;
use augmcp::service;
use augmcp::{AppState, AugServer, config::Config};
use clap::{Parser, ValueEnum};
use rmcp::serve_server;
//
use tracing_appender::rolling;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
        let (_total, _newn, _existing, all_blob_names) =
            service::index_and_persist(&cfg, &project_key, &path, false).await?;
        let result = backend::retrieve_formatted(&cfg, &all_blob_names, &query).await?;
        println!("{}", result);
        return Ok(());
    }

    let server = AugServer::new(cfg.clone());

    match cli.transport {
        TransportKind::Stdio => {
            println!("augmcp stdio server started");
            let io = (tokio::io::stdin(), tokio::io::stdout());
            serve_server(server, io).await?;
        }
        TransportKind::Http => {
            let app_state = AppState {
                server: server.clone(),
                tasks: augmcp::tasks::TaskManager::new(),
            };
            let router = augmcp::http_router::build_router(app_state);
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
