use augmcp::config::Config;
use serial_test::serial;
use std::{env, fs};

struct EnvGuard(Vec<(String, Option<String>)>);
impl EnvGuard {
    fn set(k: &str, v: &str) -> Self {
        let prev = env::var(k).ok();
        unsafe {
            env::set_var(k, v);
        }
        EnvGuard(vec![(k.to_string(), prev)])
    }
    fn set_many(kvs: &[(&str, &str)]) -> Self {
        let mut saved = vec![];
        for (k, v) in kvs {
            let prev = env::var(k).ok();
            unsafe {
                env::set_var(k, v);
            }
            saved.push(((*k).to_string(), prev));
        }
        EnvGuard(saved)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            match v {
                Some(val) => unsafe { env::set_var(k, val) },
                None => unsafe { env::remove_var(k) },
            }
        }
    }
}

fn set_home(dir: &str) -> EnvGuard {
    // Try to work across platforms
    EnvGuard::set_many(&[("HOME", dir), ("USERPROFILE", dir)])
}

#[test]
#[serial]
fn env_overrides_apply() {
    let td = tempfile::tempdir().unwrap();
    let _home = set_home(td.path().to_str().unwrap());

    // Ensure clean state
    let cfg_dir = td.path().join(".augmcp");
    if cfg_dir.exists() {
        fs::remove_dir_all(&cfg_dir).unwrap();
    }

    let _env = EnvGuard::set_many(&[
        ("AUGMCP_BASE_URL", "http://local"),
        ("AUGMCP_TOKEN", "ENV_TOKEN"),
        ("AUGMCP_BATCH_SIZE", "77"),
        ("AUGMCP_MAX_LINES_PER_BLOB", "1234"),
        ("AUGMCP_TEXT_EXTENSIONS", ".md,.rs"),
        ("AUGMCP_EXCLUDE_PATTERNS", "node_modules,dist"),
        ("AUGMCP_MAX_OUTPUT_LENGTH", "2048"),
        ("AUGMCP_DISABLE_CODEBASE_RETRIEVAL", "true"),
        ("AUGMCP_ENABLE_COMMIT_RETRIEVAL", "true"),
    ]);

    let cfg = Config::load_with_overrides(None, None).unwrap();
    assert_eq!(cfg.settings.base_url, "http://local");
    assert_eq!(cfg.settings.token, "ENV_TOKEN");
    assert_eq!(cfg.settings.batch_size, 77);
    assert_eq!(cfg.settings.max_lines_per_blob, 1234);
    assert_eq!(cfg.settings.text_extensions, vec![".md", ".rs"]);
    assert_eq!(cfg.settings.exclude_patterns, vec!["node_modules", "dist"]);
    assert_eq!(cfg.settings.max_output_length, 2048);
    assert!(cfg.settings.disable_codebase_retrieval);
    assert!(cfg.settings.enable_commit_retrieval);
}

#[test]
#[serial]
fn cli_overrides_take_priority() {
    let td = tempfile::tempdir().unwrap();
    let _home = set_home(td.path().to_str().unwrap());

    let _env = EnvGuard::set_many(&[("AUGMCP_BASE_URL", "http://env"), ("AUGMCP_TOKEN", "ENV")]);
    let cfg = Config::load_with_overrides(Some("http://cli".into()), Some("CLI".into())).unwrap();
    assert_eq!(cfg.settings.base_url, "http://cli");
    assert_eq!(cfg.settings.token, "CLI");
}
