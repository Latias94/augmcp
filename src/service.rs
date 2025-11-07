use crate::{
    backend::{self, UploadProgress},
    config::{self, Config},
    indexer::{Aliases, ProjectsIndex, collect_blobs, incremental_plan},
};
use anyhow::{Result, anyhow};
use std::path::Path;

/// 解析 alias 与路径，返回 (normalized_project_key, path_string)。
/// 若同时提供 alias 和 path，则绑定 alias -> normalized_path 并持久化。
pub fn resolve_target(
    cfg: &Config,
    alias: Option<String>,
    path: Option<String>,
) -> Result<(String, String)> {
    let mut aliases = Aliases::load(&cfg.aliases_file()).unwrap_or_default();
    let path = match (alias.clone(), path.clone()) {
        (Some(a), Some(p)) => {
            let norm = config::normalize_path(&p)?;
            aliases.set(a, norm);
            let _ = aliases.save(&cfg.aliases_file());
            p
        }
        (Some(a), None) => aliases
            .resolve(&a)
            .cloned()
            .ok_or_else(|| anyhow!("alias not found and no path provided"))?,
        (None, Some(p)) => p,
        (None, None) => return Err(anyhow!("provide project_root_path or alias")),
    };
    let project_key = config::normalize_path(&path)?;
    Ok((project_key, path))
}

/// 收集 -> 增量计划 -> 可选上传 -> 持久化项目索引，返回 (total_blobs, new_blobs, existing_blobs, all_blob_names)
pub async fn index_and_persist(
    cfg: &Config,
    project_key: &str,
    path: &str,
    force_full: bool,
) -> Result<(usize, usize, usize, Vec<String>)> {
    let p = Path::new(path);
    let blobs = collect_blobs(
        p,
        &cfg.text_extensions_set(),
        cfg.settings.max_lines_per_blob,
        &cfg.settings.exclude_patterns,
    )?;
    if blobs.is_empty() {
        return Err(anyhow!("No text files found in project"));
    }
    let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
    if force_full {
        projects.0.remove(project_key);
    }
    let (new_blobs, all_names) = incremental_plan(project_key, &blobs, &projects);
    let total = all_names.len();
    let newn = new_blobs.len();
    let existing = total.saturating_sub(newn);
    if !new_blobs.is_empty() {
        tracing::info!(uploading = new_blobs.len(), "uploading new blobs (service)");
        let _ = backend::upload_new_blobs(cfg, &new_blobs).await?;
    }
    projects
        .0
        .insert(project_key.to_string(), all_names.clone());
    let _ = projects.save(&cfg.projects_file());
    Ok((total, newn, existing, all_names))
}

/// 与 index_and_persist 类似，但允许传入上传进度回调。
pub async fn index_and_persist_with_progress<F>(
    cfg: &Config,
    project_key: &str,
    path: &str,
    force_full: bool,
    mut on_progress: F,
) -> Result<(usize, usize, usize, Vec<String>)>
where
    F: FnMut(UploadProgress),
{
    let p = Path::new(path);
    let blobs = collect_blobs(
        p,
        &cfg.text_extensions_set(),
        cfg.settings.max_lines_per_blob,
        &cfg.settings.exclude_patterns,
    )?;
    if blobs.is_empty() {
        return Err(anyhow!("No text files found in project"));
    }
    let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
    if force_full {
        projects.0.remove(project_key);
    }
    let (new_blobs, all_names) = incremental_plan(project_key, &blobs, &projects);
    let total = all_names.len();
    let newn = new_blobs.len();
    let existing = total.saturating_sub(newn);
    if !new_blobs.is_empty() {
        tracing::info!(
            uploading = new_blobs.len(),
            "uploading new blobs (service+progress)"
        );
        let _ =
            backend::upload_new_blobs_with_progress(cfg, &new_blobs, |p| on_progress(p)).await?;
    }
    projects
        .0
        .insert(project_key.to_string(), all_names.clone());
    let _ = projects.save(&cfg.projects_file());
    Ok((total, newn, existing, all_names))
}

/// 若需要索引则先索引（可跳过已有缓存），随后检索并返回格式化文本。
pub async fn ensure_index_then_retrieve(
    cfg: &Config,
    project_key: &str,
    path: &str,
    query: &str,
    skip_index_if_indexed: bool,
) -> Result<String> {
    let mut projects = ProjectsIndex::load(&cfg.projects_file()).unwrap_or_default();
    let mut all_blob_names: Vec<String> = Vec::new();
    let mut need_index = true;
    if skip_index_if_indexed {
        if let Some(existing) = projects.0.get(project_key) {
            if !existing.is_empty() {
                all_blob_names = existing.clone();
                need_index = false;
                tracing::info!(
                    blobs = all_blob_names.len(),
                    "using existing index (skip_index_if_indexed=true)"
                );
            }
        }
    }
    if need_index {
        let (_t, _n, _e, all) = index_and_persist(cfg, project_key, path, false).await?;
        all_blob_names = all;
    }
    let formatted = backend::retrieve_formatted(cfg, &all_blob_names, query).await?;
    Ok(formatted)
}
