//! Indexer: collect files, honor .gitignore, split large files, hash path+content.

use anyhow::{anyhow, Context, Result};
use encoding_rs::Encoding;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::{HashMap, HashSet}, fs, path::{Path, PathBuf}};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectsIndex(pub HashMap<String, Vec<String>>);

impl ProjectsIndex {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() { return Ok(Self::default()); }
        let text = fs::read_to_string(path)?;
        let v = serde_json::from_str::<HashMap<String, Vec<String>>>(&text)
            .unwrap_or_default();
        Ok(Self(v))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        let text = serde_json::to_string_pretty(&self.0)?;
        fs::write(path, text)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Aliases(pub HashMap<String, String>); // alias -> normalized_path

impl Aliases {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() { return Ok(Self::default()); }
        let text = fs::read_to_string(path)?;
        let v = serde_json::from_str::<HashMap<String, String>>(&text).unwrap_or_default();
        Ok(Self(v))
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        let text = serde_json::to_string_pretty(&self.0)?;
        fs::write(path, text)?;
        Ok(())
    }
    pub fn resolve<'a>(&'a self, alias: &str) -> Option<&'a String> { self.0.get(alias) }
    pub fn set(&mut self, alias: String, normalized_path: String) { self.0.insert(alias, normalized_path); }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobUpload {
    pub path: String,
    pub content: String,
}

/// Read file bytes and decode using multiple encodings (UTF-8 -> GBK -> GB2312 -> ISO-8859-1),
/// fall back to UTF-8 lossy if all failed.
fn read_text_with_encodings(p: &Path) -> Result<String> {
    let bytes = fs::read(p).with_context(|| format!("read file bytes: {}", p.display()))?;
    // try UTF-8
    if let Ok(s) = std::str::from_utf8(&bytes) { return Ok(s.to_string()); }
    // try fallback encodings
    for label in ["gbk", "gb2312", "iso-8859-1"] {
        if let Some(enc) = Encoding::for_label(label.as_bytes()) {
            let (cow, _, _) = enc.decode(&bytes);
            return Ok(cow.into_owned());
        }
    }
    // last resort
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn build_exclude_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for pat in patterns {
        // Support plain names like "node_modules" and wildcard patterns.
        let g = Glob::new(pat).with_context(|| format!("invalid glob pattern: {pat}"))?;
        b.add(g);
    }
    Ok(b.build()?)
}

fn is_text_ext(path: &Path, text_exts: &HashSet<String>) -> bool {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let dot = format!(".{}", ext.to_lowercase());
        return text_exts.contains(&dot);
    }
    false
}

fn should_exclude(rel: &str, globset: &GlobSet) -> bool { globset.is_match(rel) }

fn hash_blob_name(path: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Collect blobs from a project directory with .gitignore and exclude patterns.
pub fn collect_blobs(
    project_root: &Path,
    text_exts: &HashSet<String>,
    max_lines: usize,
    exclude_patterns: &[String],
) -> Result<Vec<BlobUpload>> {
    if !project_root.exists() { return Err(anyhow!("project root not found: {}", project_root.display())); }
    let globset = build_exclude_globset(exclude_patterns)?;
    let mut blobs = Vec::new();

    let mut walk = WalkBuilder::new(project_root);
    walk.git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(false);

    for dent in walk.build() {
        let dent = match dent { Ok(d) => d, Err(_) => continue };
        let p = dent.path();
        if p.is_dir() { continue; }
        // relative string with forward slashes
        let rel = pathdiff::diff_paths(p, project_root)
            .unwrap_or_else(|| PathBuf::from(""));
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() { continue; }

        if should_exclude(&rel_str, &globset) { continue; }
        if !is_text_ext(p, text_exts) { continue; }

        let content = match read_text_with_encodings(p) { Ok(s) => s, Err(_) => continue };
        // split by max_lines
        let lines: Vec<&str> = content.split_inclusive(['\n', '\r']).collect();
        if lines.len() <= max_lines {
            blobs.push(BlobUpload { path: rel_str, content });
        } else {
            let total = (lines.len() + max_lines - 1) / max_lines;
            for (i, chunk) in lines.chunks(max_lines).enumerate() {
                let chunk_content = chunk.concat();
                let chunk_path = format!("{}#chunk{}of{}", rel_str, i + 1, total);
                blobs.push(BlobUpload { path: chunk_path, content: chunk_content });
            }
        }
    }
    Ok(blobs)
}

/// Compute incremental upload set: returns (new_blobs, all_blob_names).
pub fn incremental_plan(
    project_key: &str,
    blobs: &[BlobUpload],
    projects: &ProjectsIndex,
) -> (Vec<BlobUpload>, Vec<String>) {
    let existing: HashSet<String> = projects.0.get(project_key)
        .cloned().unwrap_or_default().into_iter().collect();
    let mut all_blob_names = Vec::with_capacity(blobs.len());
    let mut new_blobs = Vec::new();
    for b in blobs {
        let h = hash_blob_name(&b.path, &b.content);
        if !existing.contains(&h) { new_blobs.push(BlobUpload { path: b.path.clone(), content: b.content.clone() }); }
        all_blob_names.push(h);
    }
    (new_blobs, all_blob_names)
}
