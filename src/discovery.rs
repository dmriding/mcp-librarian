use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config;

/// A server we've discovered in someone's config. May or may not be probeable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub source: ConfigSource,
    pub transport: Transport,
    /// True if we can spawn this and talk MCP over its stdio.
    /// False for remote/cloud/HTTP servers we can't reach with TokioChildProcess.
    pub probeable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigSource {
    ClaudeDesktop(PathBuf),
    ClaudeCode(PathBuf),
    Override(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Remote / cloud server we can't probe by spawning. Still listed in
    /// `librarian_list`; content comes from manifests and learned notes.
    Remote {
        /// Free-form descriptor (URL, "claude.ai-mediated", etc.). We don't
        /// fetch this — it's just so the agent knows what kind of thing it is.
        descriptor: String,
    },
}

/// Raw shape of `claude_desktop_config.json` / `~/.claude.json` `mcpServers` block.
#[derive(Debug, Deserialize)]
struct RawClaudeConfig {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, RawServerEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawServerEntry {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Remote {
        url: String,
        #[serde(default, rename = "type")]
        ty: Option<String>,
    },
    /// Permissive catch-all so unknown shapes don't fail the whole load.
    Unknown(serde_json::Value),
}

pub fn discover() -> Result<Vec<ServerConfig>> {
    let mut servers = Vec::new();

    if let Some(path) = config::override_config_path()
        && path.exists()
    {
        servers.extend(load_from(&path, |p| {
            ConfigSource::Override(p.to_path_buf())
        })?);
    }

    for path in claude_desktop_candidates() {
        if path.exists() {
            servers.extend(load_from(&path, |p| {
                ConfigSource::ClaudeDesktop(p.to_path_buf())
            })?);
        }
    }

    for path in claude_code_candidates() {
        if path.exists() {
            servers.extend(load_from(&path, |p| {
                ConfigSource::ClaudeCode(p.to_path_buf())
            })?);
        }
    }

    // De-duplicate by name. Override wins, then Claude Code, then Claude Desktop.
    let mut out: BTreeMap<String, ServerConfig> = BTreeMap::new();
    let priority = |s: &ConfigSource| match s {
        ConfigSource::Override(_) => 3,
        ConfigSource::ClaudeCode(_) => 2,
        ConfigSource::ClaudeDesktop(_) => 1,
    };
    for srv in servers {
        match out.get(&srv.name) {
            Some(existing) if priority(&existing.source) >= priority(&srv.source) => {}
            _ => {
                out.insert(srv.name.clone(), srv);
            }
        }
    }
    Ok(out.into_values().collect())
}

fn load_from(path: &Path, tag: impl Fn(&Path) -> ConfigSource) -> Result<Vec<ServerConfig>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let raw: RawClaudeConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;

    let mut out = Vec::new();
    for (name, entry) in raw.mcp_servers {
        let server = match entry {
            RawServerEntry::Stdio { command, args, env } => ServerConfig {
                name,
                source: tag(path),
                transport: Transport::Stdio { command, args, env },
                probeable: true,
            },
            RawServerEntry::Remote { url, ty } => ServerConfig {
                name,
                source: tag(path),
                transport: Transport::Remote {
                    descriptor: format!("{}: {}", ty.unwrap_or_else(|| "remote".into()), url),
                },
                probeable: false,
            },
            RawServerEntry::Unknown(value) => ServerConfig {
                name,
                source: tag(path),
                transport: Transport::Remote {
                    descriptor: format!("unrecognized entry: {value}"),
                },
                probeable: false,
            },
        };
        out.push(server);
    }
    Ok(out)
}

#[cfg(windows)]
fn claude_desktop_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(
            PathBuf::from(appdata)
                .join("Claude")
                .join("claude_desktop_config.json"),
        );
    }
    out
}

#[cfg(not(windows))]
fn claude_desktop_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = config::home_dir() {
        // macOS
        out.push(home.join("Library/Application Support/Claude/claude_desktop_config.json"));
        // Linux (best-effort)
        out.push(home.join(".config/Claude/claude_desktop_config.json"));
    }
    out
}

fn claude_code_candidates() -> Vec<PathBuf> {
    // ~/.claude.json on every platform (Claude Code).
    let mut out = Vec::new();
    if let Some(home) = config::home_dir() {
        out.push(home.join(".claude.json"));
        // Per-project mcp configs live next to the project; we don't crawl
        // every project on disk. Users can point MCP_LIBRARIAN_CONFIG at one
        // if they need it.
    }
    out
}
