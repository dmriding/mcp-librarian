use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use crate::config::Paths;
use crate::index::{IndexedTool, Note, NoteBasis, NoteKind, ProbeStatus, ServerEntry};

// =================== Manifest ===================

#[derive(Debug, Default, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Manifest {
    #[serde(default)]
    pub meta: ManifestMeta,
    #[serde(default)]
    pub topics: Vec<ManifestTopic>,
    #[serde(default)]
    pub workflows: Vec<ManifestWorkflow>,
    #[serde(default)]
    pub tool_categories: Vec<ManifestCategory>,
    #[serde(default)]
    pub gotchas: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ManifestMeta {
    /// Bucket the server falls into in `librarian_list` (e.g. "comms", "knowledge", "browser").
    #[serde(default)]
    pub category: Option<String>,
    /// One-sentence description, used as the server's blurb in `librarian_list`.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional path/name of a paired CLI binary. Reserved for future CLI playbook generation.
    #[serde(default)]
    pub paired_cli: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ManifestTopic {
    /// Short slug used as the `topic` argument to `librarian_help`.
    pub name: String,
    /// Human-readable title for the topic page.
    pub title: String,
    /// Markdown body of the topic.
    pub body: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ManifestWorkflow {
    /// Title shown in the workflows section of the overview.
    pub title: String,
    /// Markdown body — typically a numbered call sequence.
    pub body: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ManifestCategory {
    /// Category name shown in the overview's tool list.
    pub name: String,
    /// Tool names that belong to this category.
    pub tools: Vec<String>,
}

/// Check whether a manifest has any meaningful content. Used as a safety
/// net before writing — refuses to clobber a curated manifest with an empty one.
pub fn manifest_is_empty(m: &Manifest) -> bool {
    m.meta.category.is_none()
        && m.meta.summary.is_none()
        && m.meta.paired_cli.is_none()
        && m.topics.is_empty()
        && m.workflows.is_empty()
        && m.tool_categories.is_empty()
        && m.gotchas.is_empty()
}

pub fn load_manifest(paths: &Paths, server: &str) -> Result<Option<Manifest>> {
    let path = paths.manifest_path(server);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = toml::from_str(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(manifest))
}

/// List server names that have manifests on disk (`<name>.toml`, excluding `.bak`).
/// Used by `librarian_list` and `librarian_help` to surface manifests authored before
/// the corresponding server is installed.
pub fn list_manifest_servers(paths: &Paths) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let dir = &paths.manifest_dir;
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("iterating {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Strict ".toml" suffix, NOT ".toml.bak" or anything else.
        if let Some(stem) = name.strip_suffix(".toml")
            && !stem.is_empty()
        {
            out.push(stem.to_string());
        }
    }
    Ok(out)
}

/// Default manifest for the librarian itself — injected when no user-authored
/// `librarian.toml` exists. Keeps the librarian from showing as orphaned in its
/// own list output. A user-written manifest still wins (regular load path).
pub fn synthetic_librarian_manifest() -> Manifest {
    Manifest {
        meta: ManifestMeta {
            category: Some("meta".into()),
            summary: Some(
                "Indexes your other MCP servers and emits playbooks on demand. \
                 Call `librarian_help(\"librarian\")` for the full playbook."
                    .into(),
            ),
            paired_cli: None,
        },
        ..Default::default()
    }
}

#[allow(dead_code)]
pub fn manifest_path_for(paths: &Paths, server: &str) -> std::path::PathBuf {
    paths.manifest_path(server)
}

// =================== Rendering: librarian_list ===================

pub fn render_list(entries: &[(ServerEntry, Option<Manifest>)], category_filter: Option<&str>) -> String {
    let mut buckets: BTreeMap<String, Vec<&(ServerEntry, Option<Manifest>)>> = BTreeMap::new();
    for pair in entries {
        // Precedence: manifest meta wins (authored canon), then the entry's
        // own category (set by seed_playbook for hosted servers without a
        // manifest yet), then "uncategorized" as a last resort.
        let cat = pair
            .1
            .as_ref()
            .and_then(|m| m.meta.category.clone())
            .or_else(|| pair.0.category.clone())
            .unwrap_or_else(|| "uncategorized".to_string());
        if let Some(filter) = category_filter
            && !cat.eq_ignore_ascii_case(filter)
        {
            continue;
        }
        buckets.entry(cat).or_default().push(pair);
    }

    let mut out = String::new();
    out.push_str("# MCP Server Index\n\n");

    if buckets.is_empty() {
        out.push_str("*(no servers found)*\n");
    } else {
        for (cat, mut items) in buckets {
            items.sort_by(|a, b| a.0.name.cmp(&b.0.name));
            let _ = writeln!(out, "## {cat}\n");
            for (entry, manifest) in items {
                let summary = manifest
                    .as_ref()
                    .and_then(|m| m.meta.summary.clone())
                    .or_else(|| entry.summary.clone())
                    .or_else(|| auto_summary(entry))
                    .unwrap_or_else(|| "*(no summary)*".to_string());
                // Marker quality: a bare `(seeded)` told us nothing about whether
                // the entry was a thin auth-handshake stub or a rich 18-tool seed.
                // Include the tool count so a glance at `librarian_list` tells you
                // which seeds are worth drilling into and which are placeholders.
                let probe_marker: String = match &entry.probe_status {
                    ProbeStatus::Ok => String::new(),
                    ProbeStatus::Seeded => {
                        let n = entry.tools.len();
                        match n {
                            0 => " (seeded — no tools)".to_string(),
                            1 => " (seeded — 1 tool)".to_string(),
                            _ => format!(" (seeded — {n} tools)"),
                        }
                    }
                    ProbeStatus::NotProbeable => " (remote)".to_string(),
                    ProbeStatus::Timeout => " (probe timed out)".to_string(),
                    ProbeStatus::Failed(_) => " (probe failed)".to_string(),
                    ProbeStatus::ManifestOnly => " (manifest only — not installed)".to_string(),
                };
                let _ = writeln!(out, "- **{}**{} — {summary}", entry.name, probe_marker);
            }
            out.push('\n');
        }
    }

    out.push_str("---\n");
    out.push_str("*Call `librarian_help(server)` for details. ");
    out.push_str("`librarian_help(server, topic)` for a deep dive. ");
    out.push_str("`librarian_search(query)` if unsure which server has the tool you need.*\n");
    out
}

fn auto_summary(entry: &ServerEntry) -> Option<String> {
    if entry.tools.is_empty() {
        return None;
    }
    let n = entry.tools.len();
    let first: Vec<&str> = entry.tools.iter().take(3).map(|t| t.name.as_str()).collect();
    Some(format!(
        "{n} tools (e.g. {})",
        first.join(", ")
    ))
}

// =================== Rendering: librarian_help ===================

pub fn render_help(
    entry: &ServerEntry,
    manifest: Option<&Manifest>,
    notes: &[Note],
    topic: Option<&str>,
) -> String {
    match topic {
        None => render_overview(entry, manifest, notes),
        Some(t) => render_topic(entry, manifest, notes, t),
    }
}

fn render_overview(entry: &ServerEntry, manifest: Option<&Manifest>, notes: &[Note]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} Playbook", entry.name);
    out.push('\n');

    // Summary
    if let Some(summary) = manifest.and_then(|m| m.meta.summary.as_deref()) {
        let _ = writeln!(out, "{summary}\n");
    }

    // Probe-status banner if not OK
    match &entry.probe_status {
        ProbeStatus::Ok | ProbeStatus::Seeded => {}
        ProbeStatus::NotProbeable => out.push_str(
            "> *Remote/cloud server — schema was not probed. Coverage comes from manifest and learned notes.*\n\n",
        ),
        ProbeStatus::Timeout => out.push_str(
            "> *Probe timed out. Schema list may be stale; try `librarian_refresh`.*\n\n",
        ),
        ProbeStatus::Failed(msg) => {
            let _ = writeln!(out, "> *Probe failed: {msg}. Using manifest + learned notes only.*\n");
        }
        ProbeStatus::ManifestOnly => out.push_str(
            "> *Manifest only — this server is NOT currently installed on your system. \
             The playbook below comes from the manifest you authored in advance. \
             Install the server (add it to `~/.claude.json`) and run `librarian_refresh` to probe its real tool list.*\n\n",
        ),
    }

    // Workflows: manifest first, then workflow-kind observed notes
    let workflow_notes: Vec<&Note> = notes
        .iter()
        .filter(|n| n.kind == NoteKind::Workflow && n.basis == NoteBasis::Observed)
        .collect();
    let has_workflows = manifest.map(|m| !m.workflows.is_empty()).unwrap_or(false)
        || !workflow_notes.is_empty();
    if has_workflows {
        out.push_str("## Key Workflows\n\n");
        if let Some(m) = manifest {
            for wf in &m.workflows {
                let _ = writeln!(out, "### {}\n\n{}\n", wf.title, wf.body.trim_end());
            }
        }
        for note in &workflow_notes {
            let stale = if note.possibly_stale { " ⚠possibly stale" } else { "" };
            let _ = writeln!(
                out,
                "- *(learned {}{})* {}",
                note.timestamp.format("%Y-%m-%d"),
                stale,
                note.claim
            );
        }
        out.push('\n');
    }

    // Tool categories: prefer probed tools if we have them; otherwise fall
    // back to manifest-declared categories so manifest-only servers
    // (authored before install) still show their structure.
    let has_manifest_categories = manifest.map(|m| !m.tool_categories.is_empty()).unwrap_or(false);
    if !entry.tools.is_empty() {
        out.push_str("## Tool Categories\n\n");
        let groups = group_tools(entry, manifest);
        for (cat, tools) in &groups {
            let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
            let _ = writeln!(out, "- **{cat}**: {}", names.join(", "));
        }
        out.push('\n');
    } else if has_manifest_categories
        && let Some(m) = manifest
    {
        out.push_str("## Tool Categories\n\n");
        out.push_str("*(from manifest — not yet probed against an installed server)*\n\n");
        for cat in &m.tool_categories {
            let _ = writeln!(out, "- **{}**: {}", cat.name, cat.tools.join(", "));
        }
        out.push('\n');
    }

    // Gotchas: manifest + behavior/error notes
    let gotcha_notes: Vec<&Note> = notes
        .iter()
        .filter(|n| {
            matches!(n.kind, NoteKind::Behavior | NoteKind::ErrorPattern | NoteKind::Tip)
                && n.basis == NoteBasis::Observed
        })
        .collect();
    let has_gotchas =
        manifest.map(|m| !m.gotchas.is_empty()).unwrap_or(false) || !gotcha_notes.is_empty();
    if has_gotchas {
        out.push_str("## Important Gotchas\n\n");
        if let Some(m) = manifest {
            for g in &m.gotchas {
                let _ = writeln!(out, "- {g}");
            }
        }
        for n in &gotcha_notes {
            let stale = if n.possibly_stale { " ⚠" } else { "" };
            let _ = writeln!(
                out,
                "- *(learned {}{})* {}",
                n.timestamp.format("%Y-%m-%d"),
                stale,
                n.claim
            );
        }
        out.push('\n');
    }

    // Inferred (speculative) notes — separate, weaker section
    let inferred: Vec<&Note> = notes.iter().filter(|n| n.basis == NoteBasis::Inferred).collect();
    if !inferred.is_empty() {
        out.push_str("## Inferred (unverified)\n\n");
        for n in &inferred {
            let stale = if n.possibly_stale { " ⚠" } else { "" };
            let _ = writeln!(
                out,
                "- *(inferred {}{})* {}",
                n.timestamp.format("%Y-%m-%d"),
                stale,
                n.claim
            );
        }
        out.push('\n');
    }

    // Topics footer
    if let Some(m) = manifest
        && !m.topics.is_empty()
    {
        out.push_str("## Available Topics\n\n");
        for t in &m.topics {
            let _ = writeln!(out, "- **{}** — {}", t.name, t.title);
        }
        let _ = writeln!(
            out,
            "\nCall `librarian_help(\"{}\", topic)` for detailed guidance on any topic.",
            entry.name
        );
    }

    // Seed-only nudge: if this entry was seeded (no manifest authored) and has
    // some tools, point the agent at the manifest-authoring flow. Closes the
    // discovery loop between "the server exists in the index" and "the server
    // has a real curated playbook".
    if manifest.is_none()
        && matches!(entry.probe_status, ProbeStatus::Seeded)
        && !entry.tools.is_empty()
    {
        out.push_str("\n---\n");
        let _ = writeln!(
            out,
            "*This is a **seeded entry** — names and a one-line summary, no curated workflows \
             or gotchas. For a richer playbook, fetch the vendor's MCP docs via \
             `librarian_fetch_docs(url=...)` and author a manifest. \
             Schema reference: `librarian_help(\"librarian\", \"manifest_schema\")`. \
             Commit via `librarian_manifest_write(server=\"{}\", manifest_toml=\"...\")` — \
             propose first (you'll get a preview + token), the user approves, then commit.*",
            entry.name
        );
    }

    out
}

fn render_topic(
    entry: &ServerEntry,
    manifest: Option<&Manifest>,
    notes: &[Note],
    topic: &str,
) -> String {
    let mut out = String::new();
    let manifest_topic = manifest
        .and_then(|m| m.topics.iter().find(|t| t.name.eq_ignore_ascii_case(topic)));

    // Virtual topic names map to the manifest's structural sections.
    // User-defined manifest topics with the same name always win.
    let virtual_kind: Option<VirtualTopic> = if manifest_topic.is_none() {
        match topic.to_lowercase().as_str() {
            "gotchas" => Some(VirtualTopic::Gotchas),
            "workflows" => Some(VirtualTopic::Workflows),
            "categories" | "tool_categories" => Some(VirtualTopic::Categories),
            _ => None,
        }
    } else {
        None
    };

    let title = if let Some(t) = manifest_topic {
        t.title.clone()
    } else {
        match virtual_kind {
            Some(VirtualTopic::Gotchas) => "Gotchas".to_string(),
            Some(VirtualTopic::Workflows) => "Workflows".to_string(),
            Some(VirtualTopic::Categories) => "Tool categories".to_string(),
            None => topic.to_string(),
        }
    };
    let _ = writeln!(out, "# {} — {}", entry.name, title);
    out.push('\n');

    if let Some(t) = manifest_topic {
        let _ = writeln!(out, "{}\n", t.body.trim_end());
    } else if let Some(v) = virtual_kind {
        render_virtual_topic(&mut out, entry, manifest, notes, v);
    } else {
        out.push_str(
            "> *No manifest topic by that name. Showing related learned notes only.*\n\n",
        );
    }

    // Related notes for this topic. Filtering is always computed (we use
    // `related.is_empty()` further down as a fallback condition), but we
    // skip the section render when a virtual topic already enumerated the
    // relevant notes — otherwise the same note appears in two sections.
    let related: Vec<&Note> = notes
        .iter()
        .filter(|n| n.topic.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(topic)))
        .collect();
    if !related.is_empty() && virtual_kind.is_none() {
        out.push_str("## Related Notes\n\n");
        let (observed, inferred): (Vec<&Note>, Vec<&Note>) =
            related.iter().copied().partition(|n| n.basis == NoteBasis::Observed);
        for n in observed {
            let stale = if n.possibly_stale { " ⚠" } else { "" };
            let _ = writeln!(
                out,
                "- *(observed {}{})* {}",
                n.timestamp.format("%Y-%m-%d"),
                stale,
                n.claim
            );
        }
        if !inferred.is_empty() {
            out.push_str("\n*Inferred:*\n");
            for n in inferred {
                let stale = if n.possibly_stale { " ⚠" } else { "" };
                let _ = writeln!(
                    out,
                    "- *(inferred {}{})* {}",
                    n.timestamp.format("%Y-%m-%d"),
                    stale,
                    n.claim
                );
            }
        }
    }

    // If the topic name matches a tool category, render the tools
    let category = manifest
        .and_then(|m| m.tool_categories.iter().find(|c| c.name.eq_ignore_ascii_case(topic)));
    if let Some(cat) = category {
        out.push_str("\n## Tools in this category\n\n");
        for tool_name in &cat.tools {
            if let Some(tool) = entry.tools.iter().find(|t| &t.name == tool_name) {
                render_tool_detail(&mut out, tool);
            }
        }
    } else if manifest_topic.is_none() && related.is_empty() {
        // Last-resort: maybe the topic *is* a tool name. Render its detail.
        if let Some(tool) = entry.tools.iter().find(|t| t.name.eq_ignore_ascii_case(topic)) {
            out.push_str("\n## Tool detail\n\n");
            render_tool_detail(&mut out, tool);
        }
    }

    out
}

fn render_tool_detail(out: &mut String, tool: &IndexedTool) {
    let _ = writeln!(out, "### `{}`", tool.name);
    if !tool.description.is_empty() {
        let _ = writeln!(out, "{}\n", tool.description.trim());
    }
    if let Some(args) = &tool.arg_summary {
        if !args.required.is_empty() {
            let _ = writeln!(out, "**Required:** {}", args.required.join(", "));
        }
        let optional: Vec<&String> = args
            .properties
            .keys()
            .filter(|k| !args.required.contains(*k))
            .collect();
        if !optional.is_empty() {
            let optional_names: Vec<String> = optional.iter().map(|s| (*s).clone()).collect();
            let _ = writeln!(out, "**Optional:** {}", optional_names.join(", "));
        }
        out.push_str("\n```\n");
        for (k, v) in &args.properties {
            let _ = writeln!(out, "{k}: {v}");
        }
        out.push_str("```\n\n");
    }
}

// =================== Virtual topics ===================

#[derive(Debug, Clone, Copy)]
enum VirtualTopic {
    Gotchas,
    Workflows,
    Categories,
}

fn render_virtual_topic(
    out: &mut String,
    entry: &ServerEntry,
    manifest: Option<&Manifest>,
    notes: &[Note],
    kind: VirtualTopic,
) {
    match kind {
        VirtualTopic::Gotchas => {
            // Manifest gotchas
            if let Some(m) = manifest
                && !m.gotchas.is_empty()
            {
                for g in &m.gotchas {
                    let _ = writeln!(out, "- {g}");
                }
                out.push('\n');
            }
            // Plus observed behavior/error_pattern/tip notes
            let relevant: Vec<&Note> = notes
                .iter()
                .filter(|n| {
                    matches!(
                        n.kind,
                        NoteKind::Behavior | NoteKind::ErrorPattern | NoteKind::Tip
                    ) && n.basis == NoteBasis::Observed
                })
                .collect();
            if !relevant.is_empty() {
                out.push_str("### Learned gotchas (observed)\n\n");
                for n in &relevant {
                    let stale = if n.possibly_stale { " ⚠" } else { "" };
                    let _ = writeln!(
                        out,
                        "- *(learned {}{})* {}",
                        n.timestamp.format("%Y-%m-%d"),
                        stale,
                        n.claim
                    );
                }
                out.push('\n');
            }
            if manifest.map(|m| m.gotchas.is_empty()).unwrap_or(true) && relevant.is_empty() {
                out.push_str("*(no gotchas filed yet)*\n");
            }
        }
        VirtualTopic::Workflows => {
            if let Some(m) = manifest
                && !m.workflows.is_empty()
            {
                for wf in &m.workflows {
                    let _ = writeln!(out, "### {}\n\n{}\n", wf.title, wf.body.trim_end());
                }
            }
            // Plus observed workflow-kind notes
            let workflow_notes: Vec<&Note> = notes
                .iter()
                .filter(|n| n.kind == NoteKind::Workflow && n.basis == NoteBasis::Observed)
                .collect();
            if !workflow_notes.is_empty() {
                out.push_str("### Learned workflows (observed)\n\n");
                for n in &workflow_notes {
                    let stale = if n.possibly_stale { " ⚠" } else { "" };
                    let _ = writeln!(
                        out,
                        "- *(learned {}{})* {}",
                        n.timestamp.format("%Y-%m-%d"),
                        stale,
                        n.claim
                    );
                }
                out.push('\n');
            }
            if manifest.map(|m| m.workflows.is_empty()).unwrap_or(true)
                && workflow_notes.is_empty()
            {
                out.push_str("*(no workflows filed yet)*\n");
            }
        }
        VirtualTopic::Categories => {
            if entry.tools.is_empty() {
                out.push_str(
                    "*(no tools indexed for this server — run `librarian_refresh` if installed, \
                     or rely on the manifest's `tool_categories` definitions)*\n",
                );
            } else {
                let groups = group_tools(entry, manifest);
                for (cat, tools) in &groups {
                    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
                    let _ = writeln!(out, "- **{cat}**: {}", names.join(", "));
                }
                out.push('\n');
            }
        }
    }
}

// =================== Tool grouping ===================

fn group_tools<'a>(
    entry: &'a ServerEntry,
    manifest: Option<&'a Manifest>,
) -> BTreeMap<String, Vec<&'a IndexedTool>> {
    let mut groups: BTreeMap<String, Vec<&IndexedTool>> = BTreeMap::new();

    // If manifest defines categories, honor them.
    if let Some(m) = manifest
        && !m.tool_categories.is_empty()
    {
        let mut placed = std::collections::HashSet::new();
        for cat in &m.tool_categories {
            let mut bucket = Vec::new();
            for name in &cat.tools {
                if let Some(t) = entry.tools.iter().find(|t| &t.name == name) {
                    bucket.push(t);
                    placed.insert(t.name.clone());
                }
            }
            if !bucket.is_empty() {
                groups.insert(cat.name.clone(), bucket);
            }
        }
        // Anything not placed goes to "other"
        let other: Vec<&IndexedTool> = entry
            .tools
            .iter()
            .filter(|t| !placed.contains(&t.name))
            .collect();
        if !other.is_empty() {
            groups.insert("other".to_string(), other);
        }
        return groups;
    }

    // Otherwise: auto-group by prefix (first underscore- or hyphen-separated token).
    for tool in &entry.tools {
        let prefix = prefix_of(&tool.name);
        groups.entry(prefix).or_default().push(tool);
    }

    // If every tool collapsed into ONE bucket (every tool shares the same first
    // token — e.g. `notion-*`), recursing one level down often turns that flat
    // 14-tool list into useful sub-groups (`create`, `update`, `get`, ...).
    // Only recurse when there's enough payoff: at least 4 tools, the recursion
    // yields 2+ buckets, and at least one bucket has 2+ tools (otherwise we've
    // just produced a verbose flat list with weird headers).
    if groups.len() == 1 && entry.tools.len() >= 4 {
        let only_prefix = groups.keys().next().cloned().unwrap_or_default();
        let recursed = recurse_prefix_grouping(&only_prefix, &entry.tools);
        let useful = recursed.len() >= 2 && recursed.values().any(|v| v.len() >= 2);
        if useful {
            return recursed;
        }
    }

    groups
}

fn prefix_of(name: &str) -> String {
    let token = name
        .split(['_', '-'])
        .next()
        .unwrap_or(name);
    if token.is_empty() || token == name {
        "misc".to_string()
    } else {
        token.to_string()
    }
}

/// Group tools by their SECOND prefix token after a known common first prefix.
/// `claude.ai_Notion` tools all start with `notion-`; recursing yields
/// `create` (4 tools), `update` (3), `get` (3), `fetch` (1), `search` (1), etc.
fn recurse_prefix_grouping<'a>(
    common_prefix: &str,
    tools: &'a [IndexedTool],
) -> BTreeMap<String, Vec<&'a IndexedTool>> {
    let mut out: BTreeMap<String, Vec<&IndexedTool>> = BTreeMap::new();
    for tool in tools {
        // Strip the common prefix and any separator after it, then take the
        // first token of what remains. Fall back to "misc" if nothing's left.
        let after = tool
            .name
            .strip_prefix(common_prefix)
            .map(|s| s.trim_start_matches(['_', '-']))
            .unwrap_or(tool.name.as_str());
        let second = after
            .split(['_', '-'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("misc")
            .to_string();
        out.entry(second).or_default().push(tool);
    }
    out
}

// =================== Self help ===================

pub fn render_self() -> String {
    let mut s = String::new();
    s.push_str("# librarian — Playbook\n\n");
    s.push_str(
        "Index of your other MCP servers. One tool call returns a directory; \
         drill-down playbooks are paid for only when you need them. Designed to \
         get smarter as you use it.\n\n",
    );
    s.push_str("## Tool Categories\n\n");
    s.push_str("- **Read**: librarian_list, librarian_help, librarian_search, librarian_manifest_diff\n");
    s.push_str("- **Write**: librarian_note, librarian_seed_playbook, librarian_seed_batch, librarian_manifest_write, librarian_manifest_restore\n");
    s.push_str("- **Fetch**: librarian_fetch_docs (read public vendor docs to bootstrap hosted-server playbooks)\n");
    s.push_str("- **Maintenance**: librarian_refresh\n");
    s.push_str("- **Bootstrap**: librarian_onboarding (one-shot prompt for first-install setup)\n\n");
    s.push_str("## Key Workflows\n\n");
    s.push_str("### First-call orientation\n");
    s.push_str("1. `librarian_list()` — landscape of all servers, grouped by category\n");
    s.push_str("2. `librarian_help(server)` — overview for one server (workflows + categories + gotchas)\n");
    s.push_str("3. `librarian_help(server, topic)` — drill into one workflow\n\n");
    s.push_str("### Bootstrapping a cloud server\n");
    s.push_str("1. You see `claude.ai_Foo` in your deferred tool list\n");
    s.push_str("2. `librarian_seed_playbook(server=\"claude.ai_Foo\", tools_dump=...)` with the tool list you can see\n");
    s.push_str("3. Future sessions see it via `librarian_list` and can drill into it\n\n");
    s.push_str("### Filing what you learn\n");
    s.push_str("1. You just used a tool and discovered a non-obvious behavior\n");
    s.push_str("2. `librarian_note(server, kind=\"workflow\"|\"behavior\"|..., basis=\"observed\", claim=\"...\")`\n");
    s.push_str("3. Next session sees the note attached to the right help page\n\n");
    s.push_str("### Authoring a manifest (the curated playbook for a server)\n");
    s.push_str("1. Construct the `Manifest` (meta, tool_categories, workflows, topics, gotchas)\n");
    s.push_str("2. `librarian_manifest_write(server, manifest)` — WITHOUT confirm_token, returns a preview + token\n");
    s.push_str("3. Show the preview to the user verbatim. Wait for them to type \"I agree\" or \"yes\".\n");
    s.push_str("4. Re-call with the same manifest plus `confirm_token=...` to commit\n");
    s.push_str("5. `librarian_manifest_diff(server)` to inspect what changed; `librarian_manifest_restore(server)` to undo\n\n");
    s.push_str("### Bootstrapping a hosted MCP server from vendor docs\n");
    s.push_str("For cloud/claude.ai-mediated servers the librarian can't probe directly, vendor public docs are higher-signal than the schema dump:\n");
    s.push_str("1. `librarian_fetch_docs(url=\"https://docs.vendor.com/mcp\")` — returns cleaned doc content\n");
    s.push_str("2. Optionally pass `extra_urls=[...]` to grab additional pages in one rate-limited call\n");
    s.push_str("3. Read the content, synthesize a `Manifest` (meta, categories, workflows, gotchas)\n");
    s.push_str("4. Run the `librarian_manifest_write` propose → user-approve → commit dance with that manifest\n");
    s.push_str("5. Future sessions see the manifest via `librarian_list` and drill in via `librarian_help(server)`\n\n");
    s.push_str("## Important Gotchas\n\n");
    s.push_str(
        "- **`basis: observed` vs `inferred`**: only file `observed` for things you just witnessed. \
         `inferred` is rendered in a weaker section so future agents know to trust it less.\n",
    );
    s.push_str(
        "- **`kind: workflow`** is the highest-value kind — multi-step sequences save more context \
         than schema dumps. Prefer workflows over arg_shape notes when you can.\n",
    );
    s.push_str(
        "- **Refresh is explicit**: `librarian_list` never reprobes. Call `librarian_refresh` after \
         installing a new MCP server.\n",
    );
    s.push_str(
        "- **Drift flag**: after a refresh, notes whose underlying tool schema changed get \
         `⚠possibly stale` markers. They're not deleted — read them and decide.\n",
    );
    s.push_str("\n## Available Topics\n\n");
    s.push_str(
        "- **manifest_schema** — full TOML schema reference for `librarian_manifest_write` \
         (field names, nesting, the root-keys-before-sections trap, a minimal working example)\n",
    );
    s.push_str("\nCall `librarian_help(\"librarian\", topic)` to drill in.\n");
    s
}

/// Render the onboarding prompt returned by `librarian_onboarding`. The output
/// is itself a prompt: the agent reads it and performs the actions described
/// (refresh, then enumerate hosted servers from its deferred-tools reminder
/// and call seed_playbook for each). Static content — no per-call state.
pub fn render_onboarding() -> String {
    let mut s = String::new();
    s.push_str("# librarian onboarding — index every MCP server you can see\n\n");
    s.push_str(
        "You're a fresh agent. This is a one-time bootstrap that populates the librarian \
         index with every MCP server connected to this client. After this, future sessions \
         can hit `librarian_list()` once and see the whole landscape.\n\n",
    );

    s.push_str("## Step 1 — auto-discover local stdio servers\n\n");
    s.push_str(
        "Local stdio servers (the ones in `~/.claude.json` or `claude_desktop_config.json`) \
         can be spawned and probed. One call indexes all of them:\n\n",
    );
    s.push_str("```\nlibrarian_refresh()\n```\n\n");
    s.push_str(
        "This reads your MCP client config, spawns each stdio server briefly, asks for its \
         tool list, and persists schemas to the index.\n\n",
    );

    s.push_str("## Step 2 — see what's still missing\n\n");
    s.push_str(
        "```\nlibrarian_list()\n```\n\n\
         Compare the result against the MCP servers you can see in your own context (look at \
         the deferred-tools reminder — tools like `mcp__claude_ai_*`, `mcp__SomeServer__*`, etc.). \
         Anything visible to you but NOT in the list is a hosted/cloud server the librarian can't \
         probe by spawning — those need manual seeding.\n\n",
    );

    s.push_str("## Step 3 — seed every missing hosted server in ONE approved batch\n\n");
    s.push_str(
        "Don't make N separate seed calls. Build a single batch payload with every hosted/cloud \
         MCP server visible in your deferred-tools reminder that didn't show up in \
         `librarian_list`, then submit it for one user approval:\n\n",
    );
    s.push_str("```\nlibrarian_seed_batch(\n");
    s.push_str("    servers=[\n");
    s.push_str("        {\n");
    s.push_str("            \"server\": \"<exact name as in tool prefixes, e.g. claude.ai_Slack>\",\n");
    s.push_str("            \"summary\": \"<one-sentence description>\",\n");
    s.push_str("            \"category\": \"<bucket — see below>\",\n");
    s.push_str("            \"tools\": [\n");
    s.push_str("                {\"name\": \"tool_name_1\", \"description\": \"one-line summary\"},\n");
    s.push_str("                {\"name\": \"tool_name_2\", \"description\": \"...\"}\n");
    s.push_str("            ]\n");
    s.push_str("        },\n");
    s.push_str("        { ... next server ... },\n");
    s.push_str("        ...\n");
    s.push_str("    ]\n");
    s.push_str(")\n```\n\n");
    s.push_str(
        "The first call returns a structured preview (every server, category, tool count, plus a \
         warning about any collisions with existing entries) and a `confirm_token`. \
         **Show the preview to the user verbatim** and ask them to type \"I agree\" or \"yes\". \
         Once they approve, re-call with the SAME `servers` list plus the `confirm_token` to commit.\n\n",
    );
    s.push_str(
        "Category buckets: `comms` | `design` | `productivity` | `knowledge` | `crm` | \
         `prospecting` | `storage` | `developer-tools` | `meta` | `data` | `utility`.\n\n",
    );
    s.push_str(
        "For just one or two servers you discover later, use `librarian_seed_playbook` instead — \
         single server, no batch overhead, no approval gate.\n\n",
    );

    s.push_str("### Conventions\n\n");
    s.push_str(
        "- **Server name**: use the name AS IT APPEARS in your tool prefixes. \
         `mcp__claude_ai_Slack__slack_send_message` → server is `claude.ai_Slack`. \
         If the prefix is something like `mcp__Foo__bar`, the server is `Foo`.\n",
    );
    s.push_str(
        "- **Skip duplicates**: if a server appears both as stdio (already in `librarian_list`) \
         AND as a hosted variant, seed only the hosted one IF its tools genuinely differ. \
         Otherwise skip — the stdio entry already has real probed schemas.\n",
    );
    s.push_str(
        "- **Tool descriptions** are optional but improve `librarian_search` quality. One sentence \
         each, copied or paraphrased from the tool's own description.\n",
    );
    s.push_str(
        "- **`required` and `properties`** on each tool are optional. Names alone are enough \
         to make the server appear in `librarian_help` and `librarian_search`.\n\n",
    );

    s.push_str("## Step 4 — verify\n\n");
    s.push_str(
        "```\nlibrarian_list()\n```\n\n\
         Every connected MCP should now appear under the right category. Show the result to the \
         user as confirmation.\n\n",
    );

    s.push_str("## Optional — author full manifests for high-value servers\n\n");
    s.push_str(
        "Seeded entries have names, summaries, and tool lists, but no workflows, gotchas, or \
         topic drill-downs. For the 2–3 servers you'll use most, follow up with:\n\n",
    );
    s.push_str(
        "1. `librarian_fetch_docs(url=\"<vendor's MCP/API docs URL>\")` to pull cleaned vendor docs\n\
         2. Synthesize a manifest (see `librarian_help(\"librarian\", \"manifest_schema\")` for the TOML reference)\n\
         3. `librarian_manifest_write(server, manifest_toml=\"...\")` — propose (user reviews) → commit\n\n\
         A curated manifest beats a seeded entry: it surfaces workflows like \"resolve channel ID before posting\" \
         and gotchas like \"bot must be invited to the channel first\" that turn into instant context for every future session.\n",
    );

    s
}

/// Render a topic page for the librarian itself. Currently the only topic is
/// `manifest_schema` — the answer to "how do I structure the manifest_toml
/// argument?" without trial-and-error.
pub fn render_librarian_topic(topic: &str) -> String {
    let normalized = topic.to_lowercase().replace('-', "_");
    match normalized.as_str() {
        "manifest_schema" | "manifest" | "schema" => render_manifest_schema_topic(),
        _ => format!(
            "# librarian — `{topic}`\n\n\
             *(no topic by that name. Known topics: `manifest_schema`.)*\n"
        ),
    }
}

fn render_manifest_schema_topic() -> String {
    // Hand-written TOML reference. Kept tight so it fits in agent context
    // without truncation. The example at the bottom is intentionally minimal
    // but complete — copy-pasting it produces a valid manifest.
    let mut s = String::new();
    s.push_str("# librarian — manifest schema\n\n");
    s.push_str(
        "Reference for the `manifest_toml` argument to `librarian_manifest_write`. \
         Pass a single TOML string. Triple-quoted blocks (`\"\"\"...\"\"\"`) handle \
         multi-line bodies without escape mania.\n\n",
    );

    s.push_str("## TOML grammar trap (read this FIRST — load-bearing)\n\n");
    s.push_str(
        "**The single most common way to silently corrupt a manifest:** writing \
         `gotchas = [...]` at the end of the file, after `[meta]` / `[[topics]]` / etc. \
         Don't do it.\n\n",
    );
    s.push_str(
        "In TOML, root-level keys MUST appear BEFORE any `[section]` or `[[section]]` header. \
         Once a section header opens, every subsequent key belongs to that section until another \
         header arrives. There is NO way to 'close' a section and return to root. So `gotchas` \
         written below a section gets scoped to that section, our parser silently drops it (the \
         Manifest struct expects gotchas at root), and you commit a manifest with zero gotchas \
         even though you wrote eight.\n\n",
    );
    s.push_str(
        "**The librarian detects this and rejects loudly at propose time** (see the validation \
         section below). But the cheapest fix is to put `gotchas` at the top of your file.\n\n",
    );
    s.push_str("**Correct ordering** (root-level keys first, then sections):\n```toml\n");
    s.push_str("gotchas = [\"item 1\", \"item 2\"]    # root-level, MUST come first\n\n");
    s.push_str("[meta]\ncategory = \"comms\"\nsummary  = \"...\"\n\n");
    s.push_str("[[tool_categories]]\nname  = \"Read\"\ntools = [\"foo_get\"]\n");
    s.push_str("```\n\n");
    s.push_str("**Wrong** (gotchas silently scoped into `[meta]`, dropped):\n```toml\n");
    s.push_str("[meta]\ncategory = \"comms\"\n\n");
    s.push_str("gotchas = [\"item 1\"]    # WRONG — this becomes meta.gotchas, lost\n");
    s.push_str("```\n\n");
    s.push_str("**Also wrong** (gotchas scoped into the LAST `[[topics]]` table):\n```toml\n");
    s.push_str("[[topics]]\nname = \"auth\"\ntitle = \"...\"\nbody = \"...\"\n\n");
    s.push_str("gotchas = [\"item 1\"]    # WRONG — this becomes topics[N].gotchas, lost\n");
    s.push_str("```\n\n");
    s.push_str(
        "**Rule of thumb:** the document is shaped like an upside-down funnel. \
         Loose stuff at the top, structured tables below. Never the other way around.\n\n",
    );

    s.push_str("## Fields\n\n");
    s.push_str("### Root\n");
    s.push_str(
        "- `gotchas` — array of strings. Single-line caveats and footguns. \
         Surfaced under `## Important Gotchas` in `librarian_help(server)`.\n\n",
    );

    s.push_str("### `[meta]` (table)\n");
    s.push_str("- `category` — string. Bucket name for `librarian_list` grouping (e.g. `\"comms\"`, `\"browser\"`).\n");
    s.push_str("- `summary` — string. One-sentence description shown in `librarian_list`.\n");
    s.push_str("- `paired_cli` — string, optional. Reserved for future CLI playbook generation.\n\n");

    s.push_str("### `[[tool_categories]]` (array of tables — note DOUBLE brackets)\n");
    s.push_str("- `name` — string. Category label shown in the overview.\n");
    s.push_str("- `tools` — array of strings. Tool names that belong in this category.\n\n");

    s.push_str("### `[[workflows]]` (array of tables)\n");
    s.push_str("- `title` — string. Workflow name shown in the overview.\n");
    s.push_str("- `body` — string (multi-line OK). Typically a numbered call sequence.\n\n");

    s.push_str("### `[[topics]]` (array of tables)\n");
    s.push_str("- `name` — string. Short slug used as the `topic` argument to `librarian_help(server, topic)`.\n");
    s.push_str("- `title` — string. Human-readable title for the topic page.\n");
    s.push_str("- `body` — string (multi-line OK). Markdown body.\n\n");

    s.push_str("## Minimal working example\n\n");
    s.push_str("```toml\n");
    s.push_str("gotchas = [\n");
    s.push_str("    \"Authentication requires an API key in the environment.\",\n");
    s.push_str("    \"Rate limit is 60 requests/minute per key.\",\n");
    s.push_str("]\n\n");
    s.push_str("[meta]\n");
    s.push_str("category = \"data\"\n");
    s.push_str("summary  = \"One-line description of what the server does.\"\n\n");
    s.push_str("[[tool_categories]]\n");
    s.push_str("name  = \"Read\"\n");
    s.push_str("tools = [\"foo_get\", \"foo_list\"]\n\n");
    s.push_str("[[tool_categories]]\n");
    s.push_str("name  = \"Write\"\n");
    s.push_str("tools = [\"foo_create\", \"foo_update\"]\n\n");
    s.push_str("[[workflows]]\n");
    s.push_str("title = \"Read a record by id\"\n");
    s.push_str("body  = \"\"\"\n");
    s.push_str("1. `foo_list()` to see what's available\n");
    s.push_str("2. `foo_get(id=...)` to load the full record\n");
    s.push_str("\"\"\"\n\n");
    s.push_str("[[topics]]\n");
    s.push_str("name  = \"auth\"\n");
    s.push_str("title = \"Authentication setup\"\n");
    s.push_str("body  = \"\"\"\n");
    s.push_str("Set `FOO_API_KEY` in the environment. The key needs `read:foo` scope.\n");
    s.push_str("\"\"\"\n");
    s.push_str("```\n\n");

    s.push_str("## Validation\n\n");
    s.push_str(
        "`librarian_manifest_write` always runs in propose mode first (no `confirm_token`): \
         that call validates the TOML and returns a structured preview. Use it as a \
         dry-run — preview a candidate manifest, abandon the token, iterate. Tokens \
         expire after 5 minutes; no commit happens without one.\n\n",
    );
    s.push_str("**What the propose call catches before you commit:**\n\n");
    s.push_str(
        "- TOML syntax errors (with line/col).\n\
         - **Misplaced `gotchas` key**: if your TOML has a `gotchas = [...]` array that \
           landed inside a sub-table because of ordering (see the grammar trap above), the \
           propose call rejects loudly with the offending parent named (e.g. \"found inside \
           `[meta]`\"). No silent data loss.\n\
         - Empty manifests (no meta, no categories, no workflows, no topics, no gotchas) \
           are refused — empty content shouldn't clobber a curated file.\n\n",
    );
    s.push_str(
        "**Read the preview carefully.** The preview shows a COUNT for every section, \
         including zero. If you submitted 8 gotchas and the preview shows `Gotchas: 0 entries`, \
         that's a smoking gun — the array got lost. Reject the proposal, fix the TOML, retry.\n",
    );
    s
}

// =================== Search ===================

pub fn render_search(query: &str, hits: &[(String, String, String, i64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Search: \"{query}\"\n");
    if hits.is_empty() {
        out.push_str("*(no matches — try a broader query, or `librarian_list()` to browse)*\n");
        return out;
    }
    for (server, tool, desc, _score) in hits {
        let short = desc.chars().take(140).collect::<String>();
        let _ = writeln!(out, "- **{server} / {tool}** — {short}");
    }
    let _ = writeln!(
        out,
        "\n*Call `librarian_help(server, topic)` to drill in (topic can be a tool name, workflow title, or category), or load the tool's schema directly via your client.*"
    );
    out
}

// =================== Manifest write preview ===================

/// Render a structured preview of what `librarian_manifest_write` would write.
/// Enumerates every concrete addition so the human reviewer has something specific
/// to scan — confirmation theater (vague summaries) is worse than no confirmation.
pub fn render_manifest_preview(
    server: &str,
    manifest: &Manifest,
    target_path: &Path,
    existing: Option<&Manifest>,
) -> String {
    // Deliberately tight. The agent already has the proposed manifest content
    // in its own context; this preview is for STRUCTURAL confirmation, not
    // verbatim re-rendering. Keeping it short also dodges client-side hangs
    // observed in some MCP hosts when responses contain dense markdown with
    // backticks/em-dashes (a Slack-sized full-content preview hung Claude
    // Desktop on the post-call render).
    let mut out = String::new();
    out.push_str("## MANIFEST WRITE — PREVIEW (NOT YET COMMITTED)\n\n");
    let _ = writeln!(out, "Server: {server}");
    let _ = writeln!(out, "Target: {}", target_path.display());
    if existing.is_some() {
        out.push_str("Mode:   OVERWRITE (an existing manifest will be backed up to .toml.bak)\n\n");
    } else {
        out.push_str("Mode:   CREATE\n\n");
    }

    out.push_str("Meta\n");
    let _ = writeln!(
        out,
        "  category: {}",
        manifest.meta.category.as_deref().unwrap_or("(none)")
    );
    let _ = writeln!(
        out,
        "  summary:  {}",
        manifest.meta.summary.as_deref().unwrap_or("(none)")
    );
    if let Some(cli) = &manifest.meta.paired_cli {
        let _ = writeln!(out, "  paired_cli: {cli}");
    }
    out.push('\n');

    // Always render every section's count — even zero. Silent absence is a
    // real failure mode: TOML scoping rules can silently scope a root-level
    // `gotchas = [...]` to a previous `[section]` table, producing a parsed
    // Manifest with empty gotchas. The agent (and user) can only catch that
    // by scanning the preview for counts. An "always show" line means an
    // unexpected zero is visible at a glance.
    let _ = writeln!(out, "Tool categories ({}):", manifest.tool_categories.len());
    if manifest.tool_categories.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for c in &manifest.tool_categories {
            let _ = writeln!(out, "  - {} ({} tools)", c.name, c.tools.len());
        }
    }
    out.push('\n');

    let _ = writeln!(out, "Workflows ({}):", manifest.workflows.len());
    if manifest.workflows.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for w in &manifest.workflows {
            let _ = writeln!(out, "  - {}", w.title);
        }
    }
    out.push('\n');

    let _ = writeln!(out, "Topics ({}):", manifest.topics.len());
    if manifest.topics.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for t in &manifest.topics {
            let _ = writeln!(out, "  - {} ({})", t.name, t.title);
        }
    }
    out.push('\n');

    let _ = writeln!(out, "Gotchas: {} entries", manifest.gotchas.len());
    // Soft nudge when zero — gotchas are typically the highest-signal section
    // of a manifest (real-world usage patterns, footguns, env-var requirements).
    // Make the absence visible, not just the count. Not a hard reject — some
    // servers legitimately have nothing to flag. ASCII-only to keep this clear
    // of Claude Desktop's preview-renderer hang triggers.
    if manifest.gotchas.is_empty() {
        out.push_str(
            "*Note: no gotchas listed. Real-world usage patterns and footguns are \
             typically the highest-signal part of a playbook. Consider adding 2-3 \
             before committing.*\n",
        );
    }
    out.push('\n');

    out
}

/// Canonical serialization for comparing two manifests by value. Used to verify
/// that a commit-mode call carries the same content the user approved.
pub fn manifest_fingerprint(m: &Manifest) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

// =================== Cache wrappers (convenience for server.rs) ===================

#[allow(dead_code)]
pub fn manifest_or_none(paths: &Paths, server: &str) -> Option<Manifest> {
    load_manifest(paths, server).ok().flatten()
}

pub fn write_manifest(paths: &Paths, server: &str, manifest: &Manifest) -> Result<()> {
    std::fs::create_dir_all(&paths.manifest_dir)?;
    let target = paths.manifest_path(server);
    let backup = paths.manifest_backup_path(server);
    let tmp = paths.manifest_dir.join(format!("{server}.toml.write-tmp"));

    // Serialize first — a serialization error must not leave a half-written
    // target on disk.
    let s = toml::to_string_pretty(manifest)?;

    // Always back up the existing manifest first — one step of history is
    // the safety net for `librarian_manifest_restore`. Callers must hold the
    // librarian write lock during this whole sequence (see `lockfile`), so
    // a concurrent writer can't slip a different "current" between the copy
    // and the rename below.
    if target.exists() {
        std::fs::copy(&target, &backup)
            .with_context(|| format!("backing up {} to {}", target.display(), backup.display()))?;
    }

    // Atomic publish: write the new content to a temp file, then rename onto
    // the target. `rename` is atomic on every supported OS when source and
    // destination are on the same filesystem (always true here — same dir),
    // so readers see either the fully-old content or the fully-new content,
    // never a partial write.
    std::fs::write(&tmp, s)
        .with_context(|| format!("writing temp {}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("renaming {} into {}", tmp.display(), target.display()))?;
    Ok(())
}

/// Lookup mtimes of the current manifest and its backup. Either may be `None`
/// if the corresponding file doesn't exist. Used by callers that render a
/// diff or restore preview, so the user can see which side is the newer file
/// on disk (and thus which direction a restore will move).
pub fn manifest_mtimes(
    paths: &Paths,
    server: &str,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    (
        mtime_of(&paths.manifest_path(server)),
        mtime_of(&paths.manifest_backup_path(server)),
    )
}

fn mtime_of(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let st = meta.modified().ok()?;
    Some(DateTime::<Utc>::from(st))
}

/// Format a pair of (current, backup) mtimes as labeled strings — appending
/// `(older)` / `(newer)` so a reader can tell which file on disk is the more
/// recent one without doing date math in their head. This is the load-bearing
/// disambiguation for `librarian_manifest_diff` after a restore (where the
/// backup is now the newer file and the diff direction inverts).
pub fn format_mtime_pair(
    current: Option<DateTime<Utc>>,
    backup: Option<DateTime<Utc>>,
) -> (String, String) {
    let fmt = |t: Option<DateTime<Utc>>| {
        t.map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "(unknown)".to_string())
    };
    let curr_s = fmt(current);
    let bak_s = fmt(backup);
    match (current, backup) {
        (Some(c), Some(b)) if c > b => (format!("{curr_s} (newer)"), format!("{bak_s} (older)")),
        (Some(c), Some(b)) if c < b => (format!("{curr_s} (older)"), format!("{bak_s} (newer)")),
        _ => (curr_s, bak_s),
    }
}

/// Read the backup manifest, if any. Returns `Ok(None)` if no `.bak` file exists.
pub fn load_manifest_backup(paths: &Paths, server: &str) -> Result<Option<Manifest>> {
    let path = paths.manifest_backup_path(server);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let m: Manifest = toml::from_str(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(m))
}

/// Swap the current manifest with its backup. Reversible — calling restore twice
/// in a row leaves you where you started.
pub fn restore_manifest(paths: &Paths, server: &str) -> Result<()> {
    let target = paths.manifest_path(server);
    let backup = paths.manifest_backup_path(server);
    if !backup.exists() {
        anyhow::bail!(
            "Error: no backup exists for `{server}`. Action: there's nothing to restore — \
             backups are only created when a manifest is written via `librarian_manifest_write`."
        );
    }
    let backup_content = std::fs::read_to_string(&backup)
        .with_context(|| format!("reading {}", backup.display()))?;
    let current_content = if target.exists() {
        Some(
            std::fs::read_to_string(&target)
                .with_context(|| format!("reading {}", target.display()))?,
        )
    } else {
        None
    };

    // Atomic swap. Both writes go through a temp + rename so a reader between
    // them sees either the pre-restore state on both sides or the post-restore
    // state on both sides, never a half-applied swap. Caller is expected to
    // hold the librarian write lock so two concurrent restores can't trample
    // each other's backup either.
    let target_tmp = paths.manifest_dir.join(format!("{server}.toml.write-tmp"));
    let backup_tmp = paths.manifest_dir.join(format!("{server}.toml.bak.write-tmp"));

    std::fs::write(&target_tmp, &backup_content)
        .with_context(|| format!("writing temp {}", target_tmp.display()))?;
    std::fs::rename(&target_tmp, &target)
        .with_context(|| format!("renaming {} into {}", target_tmp.display(), target.display()))?;

    if let Some(c) = current_content {
        std::fs::write(&backup_tmp, c)
            .with_context(|| format!("writing temp {}", backup_tmp.display()))?;
        std::fs::rename(&backup_tmp, &backup)
            .with_context(|| format!("renaming {} into {}", backup_tmp.display(), backup.display()))?;
    }
    Ok(())
}

/// Render a structured diff between two manifests. The output enumerates added,
/// removed, and changed items by section so it's easy to scan.
pub fn diff_manifests(prev: &Manifest, curr: &Manifest) -> String {
    let mut out = String::new();
    let mut any_changes = false;

    // ----- meta -----
    let meta_changes = diff_meta(&prev.meta, &curr.meta);
    if !meta_changes.is_empty() {
        any_changes = true;
        out.push_str("## meta\n\n");
        out.push_str(&meta_changes);
        out.push('\n');
    }

    // ----- tool_categories (compare by name) -----
    let cat_changes = diff_by_key(
        &prev.tool_categories,
        &curr.tool_categories,
        |c| c.name.clone(),
        |old, new| {
            if old.tools != new.tools {
                Some(format!(
                    "  - was: {}\n  - now: {}",
                    if old.tools.is_empty() { "*(empty)*".to_string() } else { old.tools.join(", ") },
                    if new.tools.is_empty() { "*(empty)*".to_string() } else { new.tools.join(", ") },
                ))
            } else {
                None
            }
        },
    );
    if !cat_changes.is_empty() {
        any_changes = true;
        out.push_str("## tool_categories\n\n");
        out.push_str(&cat_changes);
        out.push('\n');
    }

    // ----- workflows (compare by title) -----
    let wf_changes = diff_by_key(
        &prev.workflows,
        &curr.workflows,
        |w| w.title.clone(),
        |old, new| {
            if old.body.trim() != new.body.trim() {
                Some("  - body differs".to_string())
            } else {
                None
            }
        },
    );
    if !wf_changes.is_empty() {
        any_changes = true;
        out.push_str("## workflows\n\n");
        out.push_str(&wf_changes);
        out.push('\n');
    }

    // ----- topics (compare by name) -----
    let topic_changes = diff_by_key(
        &prev.topics,
        &curr.topics,
        |t| t.name.clone(),
        |old, new| {
            if old.title != new.title || old.body.trim() != new.body.trim() {
                Some("  - title or body differs".to_string())
            } else {
                None
            }
        },
    );
    if !topic_changes.is_empty() {
        any_changes = true;
        out.push_str("## topics\n\n");
        out.push_str(&topic_changes);
        out.push('\n');
    }

    // ----- gotchas (set diff) -----
    let prev_gotchas: std::collections::BTreeSet<&str> =
        prev.gotchas.iter().map(String::as_str).collect();
    let curr_gotchas: std::collections::BTreeSet<&str> =
        curr.gotchas.iter().map(String::as_str).collect();
    let added: Vec<&&str> = curr_gotchas.difference(&prev_gotchas).collect();
    let removed: Vec<&&str> = prev_gotchas.difference(&curr_gotchas).collect();
    if !added.is_empty() || !removed.is_empty() {
        any_changes = true;
        out.push_str("## gotchas\n\n");
        for a in &added {
            let _ = writeln!(out, "- ADDED: {a}");
        }
        for r in &removed {
            let _ = writeln!(out, "- REMOVED: {r}");
        }
        out.push('\n');
    }

    if !any_changes {
        out.push_str("*(no differences — backup and current are identical)*\n");
    }
    out
}

fn diff_meta(prev: &ManifestMeta, curr: &ManifestMeta) -> String {
    let mut out = String::new();
    for (field, p, c) in [
        ("category", &prev.category, &curr.category),
        ("summary", &prev.summary, &curr.summary),
        ("paired_cli", &prev.paired_cli, &curr.paired_cli),
    ] {
        if p != c {
            let _ = writeln!(
                out,
                "- {field}: was `{}`, now `{}`",
                p.as_deref().unwrap_or("(none)"),
                c.as_deref().unwrap_or("(none)")
            );
        }
    }
    out
}

fn diff_by_key<T, F, G>(
    prev: &[T],
    curr: &[T],
    key: F,
    changed: G,
) -> String
where
    F: Fn(&T) -> String,
    G: Fn(&T, &T) -> Option<String>,
{
    let mut out = String::new();
    let prev_map: BTreeMap<String, &T> = prev.iter().map(|t| (key(t), t)).collect();
    let curr_map: BTreeMap<String, &T> = curr.iter().map(|t| (key(t), t)).collect();
    for (k, c) in &curr_map {
        match prev_map.get(k) {
            None => {
                let _ = writeln!(out, "- ADDED: `{k}`");
            }
            Some(p) => {
                if let Some(detail) = changed(p, c) {
                    let _ = writeln!(out, "- CHANGED: `{k}`\n{detail}");
                }
            }
        }
    }
    for k in prev_map.keys() {
        if !curr_map.contains_key(k) {
            let _ = writeln!(out, "- REMOVED: `{k}`");
        }
    }
    out
}
