//! rmcp server exposing `search_context` tool.

use crate::{config::{self, Config}, indexer::{collect_blobs, incremental_plan, ProjectsIndex, Aliases}, backend};
use anyhow::Result;
use parking_lot::Mutex;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    service::RequestContext,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Absolute path to the project root (use forward slashes on Windows). Optional when alias is provided
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root_path: Option<String>,
    /// Optional project alias registered previously
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// When true (default), skip indexing if project already has cached blobs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_index_if_indexed: Option<bool>,
    /// Natural language query
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexArgs {
    /// Absolute path to the project root (use forward slashes on Windows). Optional if alias resolves
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root_path: Option<String>,
    /// Optional alias to bind to the path (on first index) or to resolve to an existing path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Force full re-index (ignore cache)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_full: Option<bool>,
}

#[derive(Clone)]
pub struct AugServer {
    cfg: Arc<Mutex<Config>>, // runtime overrides supported
    tool_router: ToolRouter<AugServer>,
}

impl AugServer {
    pub fn new(cfg: Config) -> Self {
        Self { cfg: Arc::new(Mutex::new(cfg)), tool_router: Self::tool_router() }
    }

    pub fn get_cfg(&self) -> Config { self.cfg.lock().clone() }
}

#[tool_router]
impl AugServer {
    /// Search for relevant code context. If project has cache and skip_index_if_indexed=true (default),
    /// it queries directly; otherwise it performs incremental indexing first.
    #[tool(description = "Search relevant code context. Auto-index when not indexed; otherwise query directly (configurable).")]
    pub async fn search_context(&self, Parameters(args): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let cfg = self.get_cfg();
        let aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
        let path_opt = match (&args.alias, &args.project_root_path) {
            (Some(a), _) => aliases.resolve(a).cloned(),
            (None, Some(p)) => Some(p.clone()),
            _ => None,
        };
        let path = match path_opt {
            Some(p) => p,
            None => return Ok(CallToolResult::success(vec![Content::text("Error: provide project_root_path or alias".to_string())])),
        };
        tracing::info!(path = %path, "search_context invoked");
        let project_key = match config::normalize_path(&path) { Ok(s) => s, Err(e) => return Ok(CallToolResult::success(vec![Content::text(format!("Error: {}", e))])) };

        let skip_if_indexed = args.skip_index_if_indexed.unwrap_or(true);

        // Step 1: load projects.json and decide whether to (re)index
        let mut projects = match ProjectsIndex::load(&cfg.projects_file()) { Ok(p) => p, Err(_) => ProjectsIndex::default() };
        let mut all_blob_names: Vec<String> = Vec::new();
        let mut need_index = true;
        if skip_if_indexed {
            if let Some(existing) = projects.0.get(&project_key) {
                if !existing.is_empty() {
                    all_blob_names = existing.clone();
                    need_index = false;
                    tracing::info!(blobs = all_blob_names.len(), "using existing index (skip_index_if_indexed=true)");
                }
            }
        }

        // Step 2: if need_index, collect and upload incrementally
        if need_index {
            tracing::info!("collecting files and splitting blobs");
            let blobs = match collect_blobs(
                std::path::Path::new(&path),
                &cfg.text_extensions_set(),
                cfg.settings.max_lines_per_blob,
                &cfg.settings.exclude_patterns,
            ) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::success(vec![Content::text(format!("Error: {}", e))])),
            };
            if blobs.is_empty() {
                return Ok(CallToolResult::success(vec![Content::text("Error: No text files found in project".to_string())]));
            }

            let (new_blobs, all_names) = incremental_plan(&project_key, &blobs, &projects);
            tracing::info!(total = blobs.len(), new = new_blobs.len(), "incremental indexing computed");

            if !new_blobs.is_empty() {
                tracing::info!(uploading = new_blobs.len(), "uploading new blobs");
                match backend::upload_new_blobs(&cfg, &new_blobs).await {
                    Ok(_) => {}
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error: Upload failed after retries. {}",
                            e
                        ))]));
                    }
                }
            }
            projects.0.insert(project_key.clone(), all_names.clone());
            let _ = projects.save(&cfg.projects_file());
            tracing::info!(blobs = all_names.len(), "index updated and saved");
            all_blob_names = all_names;
        }

        // Step 4: persist merged blob names for project
        projects.0.insert(project_key.clone(), all_blob_names.clone());
        let _ = projects.save(&cfg.projects_file());
        tracing::info!(blobs = all_blob_names.len(), "index updated and saved");

        // Step 5: retrieve formatted result
        tracing::info!("calling backend retrieval");
        let formatted = match backend::retrieve_formatted(&cfg, &all_blob_names, &args.query).await {
            Ok(s) => s,
            Err(e) => format!("Error: {}", e),
        };
        tracing::info!("retrieval finished");

        Ok(CallToolResult::success(vec![Content::text(formatted)]))
    }

    /// Explicitly index a project (incremental by default). You can optionally bind or use an alias.
    #[tool(description = "Index a project (incremental). Optionally bind/use an alias; support force_full.")]
    pub async fn index_project(&self, Parameters(args): Parameters<IndexArgs>) -> Result<CallToolResult, McpError> {
        let cfg = self.get_cfg();
        let mut aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
        // Resolve path
        let path = match (args.alias.clone(), args.project_root_path.clone()) {
            (Some(a), Some(p)) => { // bind alias to path
                let norm = match config::normalize_path(&p) { Ok(s) => s, Err(e) => return Ok(CallToolResult::success(vec![Content::text(format!("Error: {}", e))])) };
                aliases.set(a, norm.clone());
                let _ = aliases.save(&cfg.aliases_file());
                p
            }
            (Some(a), None) => match aliases.resolve(&a) { Some(p) => p.clone(), None => return Ok(CallToolResult::success(vec![Content::text("Error: alias not found and no path provided".to_string())])) },
            (None, Some(p)) => p,
            (None, None) => return Ok(CallToolResult::success(vec![Content::text("Error: provide project_root_path or alias".to_string())])),
        };
        let project_key = match config::normalize_path(&path) { Ok(s) => s, Err(e) => return Ok(CallToolResult::success(vec![Content::text(format!("Error: {}", e))])) };
        let force_full = args.force_full.unwrap_or(false);

        tracing::info!(path = %path, force_full, "index_project invoked");

        // Collect
        let blobs = match collect_blobs(
            std::path::Path::new(&path),
            &cfg.text_extensions_set(),
            cfg.settings.max_lines_per_blob,
            &cfg.settings.exclude_patterns,
        ) {
            Ok(v) => v,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(format!("Error: {}", e))])),
        };
        if blobs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text("Error: No text files found in project".to_string())]));
        }

        let mut projects = match ProjectsIndex::load(&cfg.projects_file()) { Ok(p) => p, Err(_) => ProjectsIndex::default() };
        if force_full { projects.0.remove(&project_key); }

        let (new_blobs, all_names) = incremental_plan(&project_key, &blobs, &projects);
        tracing::info!(total = blobs.len(), new = new_blobs.len(), existing = (all_names.len().saturating_sub(new_blobs.len())), "incremental indexing computed");

        if !new_blobs.is_empty() {
            tracing::info!(uploading = new_blobs.len(), "uploading new blobs");
            if let Err(e) = backend::upload_new_blobs(&cfg, &new_blobs).await {
                return Ok(CallToolResult::success(vec![Content::text(format!("Error: Upload failed after retries. {}", e))]));
            }
        }
        projects.0.insert(project_key.clone(), all_names.clone());
        let _ = projects.save(&cfg.projects_file());
        tracing::info!(blobs = all_names.len(), "index updated and saved");

        let stats = format!(
            "Index complete: total_blobs={}, new_blobs={}, existing_blobs={}",
            all_names.len(), new_blobs.len(), all_names.len().saturating_sub(new_blobs.len())
        );
        Ok(CallToolResult::success(vec![Content::text(stats)]))
    }
}

#[tool_handler]
impl ServerHandler for AugServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some("augmcp: search_context(project_root_path, query)".to_string()),
        }
    }
}
