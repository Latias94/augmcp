use crate::{server::AugServer, service, tasks::TaskManager};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct AppState {
    pub server: AugServer,
    pub tasks: TaskManager,
}

pub fn build_router(app_state: AppState) -> Router {
    // MCP service under /mcp
    let srv_factory = app_state.server.clone();
    let service = StreamableHttpService::new(
        move || Ok(srv_factory.clone()),
        LocalSessionManager::default().into(),
        Default::default(),
    );
    let server_state = app_state.clone();

    #[derive(Serialize)]
    struct HealthResp {
        status: &'static str,
        version: &'static str,
    }

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

    #[derive(Deserialize)]
    struct IndexReq {
        project_root_path: Option<String>,
        alias: Option<String>,
        force_full: Option<bool>,
        #[serde(rename = "async")]
        r#async: Option<bool>,
    }
    #[derive(Serialize)]
    struct IndexResp {
        status: String,
        result: String,
    }

    #[derive(Deserialize)]
    struct StopReq {
        project_root_path: Option<String>,
        alias: Option<String>,
    }
    #[derive(Serialize)]
    struct StopResp {
        status: String,
        result: String,
    }

    Router::new()
        .nest_service("/mcp", service)
        .route(
            "/healthz",
            get(|| async {
                Json(HealthResp {
                    status: "ok",
                    version: env!("CARGO_PKG_VERSION"),
                })
            }),
        )
        .route(
            "/api/search",
            post(
                |State(app): State<AppState>, Json(req): Json<SearchReq>| async move {
                    let cfg = app.server.get_cfg();
                    let (project_key, path) = match service::resolve_target(
                        &cfg,
                        req.alias.clone(),
                        req.project_root_path.clone(),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            return Json(SearchResp {
                                status: "error".into(),
                                result: e.to_string(),
                            });
                        }
                    };
                    if app.tasks.is_running(&project_key) {
                        return Json(SearchResp {
                            status: "accepted".into(),
                            result: "indexing in progress; please retry later".into(),
                        });
                    }
                    let skip = req.skip_index_if_indexed.unwrap_or(true);
                    let result = match service::ensure_index_then_retrieve(
                        &cfg,
                        &project_key,
                        &path,
                        &req.query,
                        skip,
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) => format!("Error: {}", e),
                    };
                    Json(SearchResp {
                        status: "success".into(),
                        result,
                    })
                },
            ),
        )
        .route(
            "/api/index",
            post(
                |State(app): State<AppState>, Json(req): Json<serde_json::Value>| async move {
                    let req: IndexReq = match serde_json::from_value(req) {
                        Ok(v) => v,
                        Err(e) => {
                            return Json(IndexResp {
                                status: "error".into(),
                                result: e.to_string(),
                            });
                        }
                    };
                    let cfg = app.server.get_cfg();
                    use crate::indexer::Aliases;
                    let mut aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                    let path = match (req.alias.clone(), req.project_root_path.clone()) {
                        (Some(a), Some(p)) => {
                            let norm = match crate::config::normalize_path(&p) {
                                Ok(s) => s,
                                Err(e) => {
                                    return Json(IndexResp {
                                        status: "error".into(),
                                        result: e.to_string(),
                                    });
                                }
                            };
                            aliases.set(a, norm);
                            let _ = aliases.save(&cfg.aliases_file());
                            p
                        }
                        (Some(a), None) => match aliases.resolve(&a) {
                            Some(p) => p.clone(),
                            None => {
                                return Json(IndexResp {
                                    status: "error".into(),
                                    result: "alias not found and no path provided".into(),
                                });
                            }
                        },
                        (None, Some(p)) => p,
                        (None, None) => {
                            return Json(IndexResp {
                                status: "error".into(),
                                result: "provide project_root_path or alias".into(),
                            });
                        }
                    };
                    let project_key = match crate::config::normalize_path(&path) {
                        Ok(x) => x,
                        Err(e) => {
                            return Json(IndexResp {
                                status: "error".into(),
                                result: e.to_string(),
                            });
                        }
                    };

                    let run_async = req.r#async.unwrap_or(false);
                    if run_async {
                        if app.tasks.is_running(&project_key) {
                            return Json(IndexResp {
                                status: "accepted".into(),
                                result: format!("indexing already in progress for {}", &path),
                            });
                        }
                        if !app.tasks.begin(&project_key) {
                            return Json(IndexResp {
                                status: "accepted".into(),
                                result: format!("indexing already in progress for {}", &path),
                            });
                        }
                        let cfg_bg = cfg.clone();
                        let path_bg = path.clone();
                        let key_bg = project_key.clone();
                        let tasks_bg = app.tasks.clone();
                        let force_full = req.force_full.unwrap_or(false);
                        let handle = tokio::spawn(async move {
                            tasks_bg.set_phase(&key_bg, "collecting");
                            let mut totals_set = false;
                            match service::index_and_persist_with_progress(
                                &cfg_bg,
                                &key_bg,
                                &path_bg,
                                force_full,
                                |p| {
                                    if !totals_set {
                                        tasks_bg.set_upload_totals(
                                            &key_bg,
                                            p.total_items,
                                            p.chunks_total,
                                            p.total_items,
                                        );
                                        totals_set = true;
                                    }
                                    tasks_bg.on_chunk(
                                        &key_bg,
                                        p.uploaded_items,
                                        p.chunk_index,
                                        p.chunk_bytes,
                                    );
                                },
                            )
                            .await
                            {
                                Ok((_total, _newn, _existing, _all)) => {
                                    tasks_bg.finish(&key_bg);
                                }
                                Err(e) => {
                                    tasks_bg.fail(&key_bg, e.to_string());
                                }
                            }
                        });
                        app.tasks.set_handle(&project_key, handle);
                        return Json(IndexResp {
                            status: "accepted".into(),
                            result: format!("async indexing started for {}", &path),
                        });
                    }

                    match service::index_and_persist(
                        &cfg,
                        &project_key,
                        &path,
                        req.force_full.unwrap_or(false),
                    )
                    .await
                    {
                        Ok((total, newn, existing, _)) => {
                            let msg = format!(
                                "Index complete: total_blobs={}, new_blobs={}, existing_blobs={}",
                                total, newn, existing
                            );
                            Json(IndexResp {
                                status: "success".into(),
                                result: msg,
                            })
                        }
                        Err(e) => Json(IndexResp {
                            status: "error".into(),
                            result: e.to_string(),
                        }),
                    }
                },
            ),
        )
        .route(
            "/api/tasks",
            get(
                |State(app): State<AppState>,
                 axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    #[derive(Serialize)]
                    struct TaskResp {
                        status: String,
                        running: bool,
                        progress: Option<crate::tasks::TaskProgress>,
                        eta_secs: Option<u64>,
                    }
                    let cfg = app.server.get_cfg();
                    let alias = params.get("alias").cloned();
                    let path = params.get("project_root_path").cloned();
                    let (key, _p) = match service::resolve_target(&cfg, alias, path) {
                        Ok(v) => v,
                        Err(_) => {
                            return axum::Json(TaskResp {
                                status: "error".into(),
                                running: false,
                                progress: None,
                                eta_secs: None,
                            });
                        }
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
                    axum::Json(TaskResp {
                        status: "success".into(),
                        running,
                        progress,
                        eta_secs: eta,
                    })
                },
            ),
        )
        .route(
            "/api/index/stop",
            post(
                |State(app): State<AppState>, Json(req): Json<serde_json::Value>| async move {
                    let req: StopReq = match serde_json::from_value(req) {
                        Ok(v) => v,
                        Err(e) => {
                            return Json(StopResp {
                                status: "error".into(),
                                result: e.to_string(),
                            });
                        }
                    };
                    let cfg = app.server.get_cfg();
                    use crate::indexer::Aliases;
                    let aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
                    let path = match (req.alias.clone(), req.project_root_path.clone()) {
                        (Some(_), Some(p)) => p,
                        (Some(a), None) => match aliases.resolve(&a) {
                            Some(p) => p.clone(),
                            None => {
                                return Json(StopResp {
                                    status: "error".into(),
                                    result: "alias not found and no path provided".into(),
                                });
                            }
                        },
                        (None, Some(p)) => p,
                        (None, None) => {
                            return Json(StopResp {
                                status: "error".into(),
                                result: "provide project_root_path or alias".into(),
                            });
                        }
                    };
                    let project_key = match crate::config::normalize_path(&path) {
                        Ok(x) => x,
                        Err(e) => {
                            return Json(StopResp {
                                status: "error".into(),
                                result: e.to_string(),
                            });
                        }
                    };
                    if app.tasks.abort(&project_key) {
                        return Json(StopResp {
                            status: "success".into(),
                            result: "aborted".into(),
                        });
                    }
                    Json(StopResp {
                        status: "error".into(),
                        result: "no running task".into(),
                    })
                },
            ),
        )
        .with_state(server_state)
}
