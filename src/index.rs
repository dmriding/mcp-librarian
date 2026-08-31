use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::config::Paths;
use crate::discovery::{ServerConfig, Transport};

/// Backstop ceiling on the serialized `index.json`. `index.json` is read and
/// re-parsed on essentially every tool call, so unbounded growth is a
/// persistent cost amplifier. Per-seed caps (in `server.rs`) bound each entry;
/// this is a defense-in-depth ceiling on the aggregate. 64 MiB is orders of
/// magnitude above any realistic index (real ones are KB to low MB) — hitting
/// it means something is wrong, so failing the write loudly is correct.
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;

/// The cached, probed view of every known MCP server.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Index {
    pub servers: BTreeMap<String, ServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    pub name: String,
    pub transport_descriptor: String,
    pub probeable: bool,
    pub probe_status: ProbeStatus,
    pub indexed_at: DateTime<Utc>,
    #[serde(default)]
    pub tools: Vec<IndexedTool>,
    /// One-line summary suitable for `librarian_list`. May come from the
    /// manifest, from the seed payload, or auto-derived from tools.
    #[serde(default)]
    pub summary: Option<String>,
    /// Category bucket for grouping in list output. Manifest-driven.
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Ok,
    Timeout,
    Failed(String),
    /// Server is remote/cloud — we never tried to probe it.
    NotProbeable,
    /// Indexed from a `librarian_seed_playbook` call rather than a real probe.
    Seeded,
    /// A manifest exists on disk but the server is not currently installed or in the index.
    /// Lets users author bootstrap playbooks before installing a server.
    ManifestOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedTool {
    pub name: String,
    pub description: String,
    /// Compact representation of the JSON schema — properties + required.
    pub arg_summary: Option<ArgSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgSummary {
    pub required: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

impl Index {
    /// Load the index from disk. Readers do not take the write lock — they rely
    /// on the all-or-nothing semantics of `std::fs::read` + `Self::save`'s
    /// atomic-rename pattern:
    /// - `save` writes the new content to a `.tmp` sibling, then renames over
    ///   the target. On every supported OS the rename is a single atomic
    ///   directory entry swap.
    /// - `load` calls `std::fs::read`, which is a single syscall returning the
    ///   entire file's bytes from one inode. It cannot observe the in-progress
    ///   state of a concurrent rename: either the syscall sees the old inode
    ///   (fully-old content) or the new inode (fully-new content), never a mix.
    ///
    /// Keep this function a single `fs::read` — switching to a streamed read
    /// would void the invariant.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let idx: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(idx)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() > MAX_INDEX_BYTES {
            anyhow::bail!(
                "refusing to write index.json: serialized size {} bytes exceeds the {} byte ceiling. \
                 This usually means an oversized or runaway seed; trim entries or remove a server.",
                bytes.len(),
                MAX_INDEX_BYTES,
            );
        }
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming temp index into {}", path.display()))?;
        Ok(())
    }

    /// Compare two arg summaries; returns true if shapes differ enough to flag drift.
    pub fn arg_shape_drifted(old: &Option<ArgSummary>, new: &Option<ArgSummary>) -> bool {
        match (old, new) {
            (None, None) => false,
            (Some(a), Some(b)) => {
                a.required != b.required
                    || a.properties.keys().collect::<Vec<_>>()
                        != b.properties.keys().collect::<Vec<_>>()
            }
            _ => true,
        }
    }
}

/// A synthetic entry for a server whose manifest exists but isn't installed.
/// Surfaced by `librarian_list` and `librarian_help` so manifests authored in
/// advance of installation aren't invisible.
pub fn entry_manifest_only(server: &str) -> ServerEntry {
    ServerEntry {
        name: server.to_string(),
        transport_descriptor: "manifest only (not installed)".to_string(),
        probeable: false,
        probe_status: ProbeStatus::ManifestOnly,
        indexed_at: Utc::now(),
        tools: Vec::new(),
        summary: None,
        category: None,
    }
}

pub fn entry_from_unprobed(cfg: &ServerConfig) -> ServerEntry {
    let descriptor = match &cfg.transport {
        Transport::Stdio { command, args, .. } => {
            format!("stdio: {} {}", command, args.join(" "))
        }
        Transport::Remote { descriptor } => format!("remote: {descriptor}"),
    };
    let probe_status = if cfg.probeable {
        ProbeStatus::Failed("not yet probed".into())
    } else {
        ProbeStatus::NotProbeable
    };
    ServerEntry {
        name: cfg.name.clone(),
        transport_descriptor: descriptor,
        probeable: cfg.probeable,
        probe_status,
        indexed_at: Utc::now(),
        tools: Vec::new(),
        summary: None,
        category: None,
    }
}

// =================== Learned notes ===================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Workflow,
    ArgShape,
    Behavior,
    ErrorPattern,
    Tip,
    Example,
}

// Docs stay on the type, not the variants: schemars renders a doc-commented
// variant as a `oneOf` of `const` strings, which strict validators reject.
/// Evidence strength. `observed` = witnessed it, ran the call and got this back
/// (strong). `inferred` = believed but not verified (weaker; rendered in its own
/// section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoteBasis {
    Observed,
    Inferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub server: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    pub kind: NoteKind,
    pub basis: NoteBasis,
    pub claim: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub possibly_stale: bool,
}

pub fn append_note(paths: &Paths, note: &Note) -> Result<()> {
    std::fs::create_dir_all(&paths.learned_dir)?;
    let path = paths.learned_path(&note.server);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let line = serde_json::to_string(note)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn read_notes(paths: &Paths, server: &str) -> Result<Vec<Note>> {
    let path = paths.learned_path(server);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {} line {}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Note>(&line) {
            Ok(note) => out.push(note),
            Err(err) => tracing::warn!(
                "skipping malformed note in {} line {}: {}",
                path.display(),
                idx + 1,
                err
            ),
        }
    }
    Ok(out)
}

/// Rewrite the notes file. Used for setting `possibly_stale` after a probe.
pub fn write_notes(paths: &Paths, server: &str, notes: &[Note]) -> Result<()> {
    std::fs::create_dir_all(&paths.learned_dir)?;
    let path = paths.learned_path(server);
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    for note in notes {
        let line = serde_json::to_string(note)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    drop(file);
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming temp notes into {}", path.display()))?;
    Ok(())
}
