//! rmcp server exposing `search_context` tool.

use crate::config::Config;
use anyhow::Result;
use parking_lot::Mutex;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
        Self {
            cfg: Arc::new(Mutex::new(cfg)),
            tool_router: Self::tool_router(),
        }
    }

    pub fn get_cfg(&self) -> Config {
        self.cfg.lock().clone()
    }
}

#[tool_router]
impl AugServer {
    /// Search for relevant code context. If project has cache and skip_index_if_indexed=true (default),
    /// it queries directly; otherwise it performs incremental indexing first.
    #[tool(
        description = "Search relevant code context. Auto-index when not indexed; otherwise query directly (configurable)."
    )]
    pub async fn search_context(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.get_cfg();
        let (project_key, path) = match crate::service::resolve_target(
            &cfg,
            args.alias.clone(),
            args.project_root_path.clone(),
        ) {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]));
            }
        };
        tracing::info!(path = %path, "search_context invoked");
        let skip = args.skip_index_if_indexed.unwrap_or(true);
        let formatted = match crate::service::ensure_index_then_retrieve(
            &cfg,
            &project_key,
            &path,
            &args.query,
            skip,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => format!("Error: {}", e),
        };
        Ok(CallToolResult::success(vec![Content::text(formatted)]))
    }
    #[tool(
        description = "Index a project and persist cache. Optionally bind an alias or force full re-index."
    )]
    pub async fn index_project(
        &self,
        Parameters(args): Parameters<IndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.get_cfg();
        let (project_key, path) = match crate::service::resolve_target(
            &cfg,
            args.alias.clone(),
            args.project_root_path.clone(),
        ) {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]));
            }
        };
        let force_full = args.force_full.unwrap_or(false);
        tracing::info!(path = %path, force_full, "index_project invoked");
        match crate::service::index_and_persist(&cfg, &project_key, &path, force_full).await {
            Ok((total, newn, existing, _)) => {
                let stats = format!(
                    "Index complete: total_blobs={}, new_blobs={}, existing_blobs={}",
                    total, newn, existing
                );
                Ok(CallToolResult::success(vec![Content::text(stats)]))
            }
            Err(e) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Error: {}",
                e
            ))])),
        }
    }
}

#[tool_handler]
impl ServerHandler for AugServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "augmcp tools: search_context(project_root_path?|alias?, query, skip_index_if_indexed?=true); index_project(project_root_path?|alias?, force_full?=false). Use forward slashes on Windows."
                    .to_string(),
            ),
        }
    }
}
