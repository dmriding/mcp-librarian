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

/// Maximum length for a server name. 64 chars is generous for real names
/// (the longest server I've seen in the wild is ~25 chars) and short enough
/// to prevent abuse via overlong filenames.
pub const MAX_SERVER_NAME_LEN: usize = 64;

/// Validate that a `server` argument is safe to splice into a filesystem
/// path. The librarian maps `server` → `<manifest_dir>/<server>.toml` and
/// `<learned_dir>/<server>.jsonl`. Without validation, an agent-supplied
/// name like `"../../etc/passwd"` would let a write tool escape its data
/// directory.
///
/// Allowed characters: `[A-Za-z0-9_.-]`. Length 1..=64. Leading `.` is
/// forbidden (rules out `.`, `..`, hidden files). No path separators,
/// no control bytes, no NUL.
pub fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!(
            "Error: `server` is empty. Action: pass the name as it appears in your MCP config \
             (e.g. \"slack\", \"forge\")."
        );
    }
    if name.len() > MAX_SERVER_NAME_LEN {
        anyhow::bail!(
            "Error: `server` is too long ({} > {MAX_SERVER_NAME_LEN}). \
             Action: trim it. Real server names are typically <30 chars.",
            name.len(),
        );
    }
    if name.starts_with('.') {
        anyhow::bail!(
            "Error: `server` starts with `.` (`{name}`) which is reserved. \
             Action: pick a name that doesn't begin with a dot. `.` and `..` \
             are filesystem path components, not server names."
        );
    }
    for (i, c) in name.char_indices() {
        if !is_allowed_server_char(c) {
            anyhow::bail!(
                "Error: `server` contains disallowed character `{c}` at position {i} (`{name}`). \
                 Action: server names must match `[A-Za-z0-9_.-]+`. No spaces, slashes, or other \
                 punctuation — this name is used in filesystem paths.",
            );
        }
    }
    Ok(())
}

fn is_allowed_server_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

#[allow(dead_code)]
pub fn home_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}
