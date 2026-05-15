use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use std::path::PathBuf;
use std::time::Duration;

#[allow(dead_code)]
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROBE_CONCURRENCY: usize = 6;
pub const DEFAULT_BRIEF_TOKEN_BUDGET: usize = 1500;

#[derive(Debug, Clone)]
pub struct Paths {
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
    pub manifest_dir: PathBuf,
    pub learned_dir: PathBuf,
    pub cache_file: PathBuf,
    /// Cached fetched documentation pages, keyed by URL hash.
    pub docs_cache_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "netviper", "mcp-librarian")
            .context("could not resolve platform directories")?;

        let cache_dir = dirs.cache_dir().to_path_buf();
        let config_dir = dirs.config_dir().to_path_buf();
        let manifest_dir = config_dir.join("manifests");
        let learned_dir = dirs.data_dir().join("learned");
        let cache_file = cache_dir.join("index.json");
        let docs_cache_dir = cache_dir.join("docs");

        Ok(Self {
            cache_dir,
            config_dir,
            manifest_dir,
            learned_dir,
            cache_file,
            docs_cache_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.cache_dir,
            &self.config_dir,
            &self.manifest_dir,
            &self.learned_dir,
            &self.docs_cache_dir,
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn manifest_path(&self, server: &str) -> PathBuf {
        self.manifest_dir.join(format!("{server}.toml"))
    }

    /// Backup file alongside `<server>.toml`. One step of history — auto-written
    /// before every manifest commit so `librarian_manifest_restore` can swap back.
    pub fn manifest_backup_path(&self, server: &str) -> PathBuf {
        self.manifest_dir.join(format!("{server}.toml.bak"))
    }

    pub fn learned_path(&self, server: &str) -> PathBuf {
        self.learned_dir.join(format!("{server}.jsonl"))
    }
}

/// MCP_LIBRARIAN_CONFIG can point at a JSON file enumerating servers,
/// useful for testing and for non-standard setups.
pub fn override_config_path() -> Option<PathBuf> {
    std::env::var_os("MCP_LIBRARIAN_CONFIG").map(PathBuf::from)
}

#[allow(dead_code)]
pub fn home_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}
