//! Configuration management for augmcp.
//!
//! Reads `~/.augmcp/settings.toml`, creates with defaults on first run.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use toml;

const ROOT_DIR_NAME: &str = ".augmcp";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(alias = "BATCH_SIZE")]
    pub batch_size: usize,
    #[serde(alias = "MAX_LINES_PER_BLOB")]
    pub max_lines_per_blob: usize,
    #[serde(alias = "BASE_URL")]
    pub base_url: String,
    #[serde(alias = "TOKEN")]
    pub token: String,
    #[serde(alias = "TEXT_EXTENSIONS")]
    pub text_extensions: Vec<String>,
    #[serde(alias = "EXCLUDE_PATTERNS")]
    pub exclude_patterns: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            batch_size: 10,
            max_lines_per_blob: 800,
            base_url: "https://api.example.com".to_string(),
            token: "your-token-here".to_string(),
            text_extensions: vec![
                ".py", ".js", ".ts", ".jsx", ".tsx", ".java", ".go", ".rs", ".cpp", ".c", ".h",
                ".hpp", ".cs", ".rb", ".php", ".md", ".txt", ".json", ".yaml", ".yml", ".toml",
                ".xml", ".html", ".css", ".scss", ".sql", ".sh", ".bash",
            ]
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
            exclude_patterns: vec![
                ".venv",
                "venv",
                ".env",
                "env",
                "node_modules",
                ".git",
                ".svn",
                ".hg",
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".tox",
                ".eggs",
                "*.egg-info",
                "dist",
                "build",
                ".idea",
                ".vscode",
                ".DS_Store",
                "*.pyc",
                "*.pyo",
                "*.pyd",
                ".Python",
                "pip-log.txt",
                "pip-delete-this-directory.txt",
                ".coverage",
                "htmlcov",
                ".gradle",
                "target",
                "bin",
                "obj",
            ]
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub settings: Settings,
    pub root_dir: PathBuf,
    pub data_dir: PathBuf,
    pub settings_path: PathBuf,
}

impl Config {
    pub fn load_with_overrides(base_url: Option<String>, token: Option<String>) -> Result<Self> {
        let root_dir = home::home_dir()
            .ok_or_else(|| anyhow!("failed to resolve home dir"))?
            .join(ROOT_DIR_NAME);
        let cfg_dir = root_dir.clone();
        let data_dir = root_dir.join("data");
        fs::create_dir_all(&cfg_dir)?;
        fs::create_dir_all(&data_dir)?;
        let settings_path = cfg_dir.join("settings.toml");

        let mut settings = if settings_path.exists() {
            let text = fs::read_to_string(&settings_path)?;
            toml::from_str::<Settings>(&text).unwrap_or_default()
        } else {
            let s = Settings::default();
            let text = toml::to_string_pretty(&s)?;
            fs::write(&settings_path, text)?;
            s
        };

        if let Some(u) = base_url {
            settings.base_url = u;
        }
        if let Some(t) = token {
            settings.token = t;
        }

        Ok(Self {
            settings,
            root_dir,
            data_dir,
            settings_path,
        })
    }

    pub fn text_extensions_set(&self) -> HashSet<String> {
        self.settings.text_extensions.iter().cloned().collect()
    }

    pub fn projects_file(&self) -> PathBuf {
        self.data_dir.join("projects.json")
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&self.settings)?;
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.settings_path, text)?;
        Ok(())
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root_dir.join("log")
    }

    pub fn aliases_file(&self) -> PathBuf {
        self.root_dir.join("aliases.json")
    }
}

/// Normalize a path to an absolute forward-slash representation.
pub fn normalize_path<P: AsRef<Path>>(p: P) -> Result<String> {
    let abs = dunce::canonicalize(p)?;
    let s = abs.to_string_lossy().replace('\\', "/");
    Ok(s)
}
