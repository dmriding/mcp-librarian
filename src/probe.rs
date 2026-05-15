use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::config::{PROBE_CONCURRENCY, PROBE_TIMEOUT};
use crate::discovery::{ServerConfig, Transport};
use crate::index::{ArgSummary, IndexedTool, ProbeStatus, ServerEntry};

pub async fn probe_all(configs: &[ServerConfig]) -> Vec<ServerEntry> {
    let sem = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
    let mut handles = Vec::new();

    for cfg in configs.iter().cloned() {
        let sem = sem.clone();
        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            probe_one(cfg).await
        });
        handles.push(handle);
    }

    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(entry) => out.push(entry),
            Err(err) => tracing::error!("probe task panicked: {err}"),
        }
    }
    out
}

pub async fn probe_one(cfg: ServerConfig) -> ServerEntry {
    let mut entry = crate::index::entry_from_unprobed(&cfg);
    entry.indexed_at = Utc::now();

    let (command, args, env) = match &cfg.transport {
        Transport::Stdio { command, args, env } => (command.clone(), args.clone(), env.clone()),
        Transport::Remote { .. } => {
            entry.probe_status = ProbeStatus::NotProbeable;
            return entry;
        }
    };

    match try_probe(&command, &args, &env).await {
        Ok(tools) => {
            entry.tools = tools;
            entry.probe_status = ProbeStatus::Ok;
        }
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("timeout") || msg.contains("deadline") {
                entry.probe_status = ProbeStatus::Timeout;
            } else {
                entry.probe_status = ProbeStatus::Failed(msg);
            }
        }
    }
    entry
}

async fn try_probe(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<Vec<IndexedTool>> {
    let resolved = resolve_command(command)
        .with_context(|| format!("resolving command '{command}'"))?;

    let mut cmd = Command::new(&resolved);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stderr(Stdio::null());

    let (proc, _stderr) = TokioChildProcess::builder(cmd)
        .spawn()
        .with_context(|| format!("spawning {}", resolved.display()))?;

    let result = tokio::time::timeout(PROBE_TIMEOUT, async {
        let client = ().serve(proc).await.context("serving rmcp client")?;
        let tools = client
            .peer()
            .list_all_tools()
            .await
            .context("list_all_tools")?;
        // Best-effort shutdown; ignore errors.
        let _ = client.cancel().await;
        anyhow::Ok(tools)
    })
    .await
    .map_err(|_| anyhow!("probe timeout after {:?}", PROBE_TIMEOUT))??;

    Ok(result.into_iter().map(convert_tool).collect())
}

fn convert_tool(t: rmcp::model::Tool) -> IndexedTool {
    let description = t
        .description
        .map(|c| c.to_string())
        .unwrap_or_default();
    let arg_summary = summarize_schema(&t.input_schema);
    IndexedTool {
        name: t.name.to_string(),
        description,
        arg_summary,
    }
}

fn summarize_schema(schema: &serde_json::Map<String, serde_json::Value>) -> Option<ArgSummary> {
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let properties: BTreeMap<String, String> = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), describe_property(v)))
                .collect()
        })
        .unwrap_or_default();

    if required.is_empty() && properties.is_empty() {
        None
    } else {
        Some(ArgSummary {
            required,
            properties,
        })
    }
}

fn describe_property(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return "any".to_string(),
    };
    let ty = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("any");
    let desc = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if desc.is_empty() {
        ty.to_string()
    } else {
        // Truncate long descriptions — we want one-liners.
        let short: String = desc.chars().take(120).collect();
        if desc.len() > short.len() {
            format!("{ty} — {short}…")
        } else {
            format!("{ty} — {short}")
        }
    }
}

#[cfg(windows)]
fn resolve_command(cmd: &str) -> Result<PathBuf> {
    let p = PathBuf::from(cmd);
    if p.is_absolute() {
        return Ok(p);
    }
    let pathext =
        std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let exts: Vec<String> = pathext
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let path_var = std::env::var_os("PATH").unwrap_or_default();

    // If the command already carries an extension, just verify it on PATH.
    let has_ext = p.extension().is_some();

    for dir in std::env::split_paths(&path_var) {
        if has_ext {
            let candidate = dir.join(cmd);
            if candidate.is_file() {
                return Ok(candidate);
            }
            continue;
        }
        // No extension — try each PATHEXT in turn.
        for ext in &exts {
            let candidate = dir.join(format!("{cmd}{ext}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        // Also try the bare name (Unix-style scripts on Windows do exist).
        let bare = dir.join(cmd);
        if bare.is_file() {
            return Ok(bare);
        }
    }
    Err(anyhow!(
        "command '{cmd}' not found on PATH (tried PATHEXT: {pathext})"
    ))
}

#[cfg(not(windows))]
fn resolve_command(cmd: &str) -> Result<PathBuf> {
    // tokio::process::Command does the right thing on Unix.
    Ok(PathBuf::from(cmd))
}
