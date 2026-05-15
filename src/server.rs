use anyhow::Result;
use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::config::{DEFAULT_BRIEF_TOKEN_BUDGET, Paths};
use crate::discovery;
use crate::fetch::{self, FetchState};
use crate::index::{
    self, ArgSummary, Index, IndexedTool, Note, NoteBasis, NoteKind, ProbeStatus, ServerEntry,
};
use crate::playbook::{self, Manifest};
use crate::probe;

/// Time-to-live for pending manifest-write tokens. The agent must commit within
/// this window after the user approves; otherwise re-propose.
const PENDING_WRITE_TTL_SECS: i64 = 5 * 60;

/// A pending mutation awaiting commit. The action variant pins what the user
/// approved — committing a different shape of action rejects.
#[derive(Clone)]
struct PendingWrite {
    server: String,
    action: PendingAction,
    expires_at: DateTime<Utc>,
}

#[derive(Clone)]
enum PendingAction {
    /// The fingerprint pins the exact manifest content the user approved.
    Write { fingerprint: String, overwrite: bool },
    /// Swap current ↔ backup for the named server.
    Restore,
}

#[derive(Clone)]
pub struct LibrarianServer {
    paths: Arc<Paths>,
    pending_writes: Arc<Mutex<HashMap<String, PendingWrite>>>,
    fetch_state: FetchState,
    tool_router: ToolRouter<Self>,
}

impl LibrarianServer {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths: Arc::new(paths),
            pending_writes: Arc::new(Mutex::new(HashMap::new())),
            fetch_state: FetchState::default(),
            tool_router: Self::tool_router(),
        }
    }

    fn issue_token(&self, pending: PendingWrite) -> String {
        let mut map = self.pending_writes.lock().expect("pending_writes lock poisoned");
        cleanup_expired(&mut map);
        let token = generate_token();
        map.insert(token.clone(), pending);
        token
    }

    fn consume_token(&self, token: &str) -> Option<PendingWrite> {
        let mut map = self.pending_writes.lock().expect("pending_writes lock poisoned");
        cleanup_expired(&mut map);
        map.remove(token)
    }
}

fn cleanup_expired(map: &mut HashMap<String, PendingWrite>) {
    let now = Utc::now();
    map.retain(|_, v| v.expires_at >= now);
}

fn generate_token() -> String {
    // 16 hex chars of timestamp nanos — uniquely identifies a propose call.
    // Cryptographic randomness isn't needed; the token only gates a local write
    // path that the same process has already authorized via the propose step.
    let ts = Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    format!("{ts:016x}")
}

// =================== Parameter / response types ===================

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ListParams {
    /// Filter to a single category bucket (e.g. "comms", "knowledge"). Case-insensitive.
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HelpParams {
    /// Server name. Use `"librarian"` for the librarian's own playbook.
    pub server: String,
    /// Optional topic for drill-down. Omit for the overview.
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Free-text query — matched fuzzily against tool names and descriptions.
    pub query: String,
    /// Maximum number of hits to return. Defaults to 10.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteParams {
    pub server: String,
    /// Specific tool the note applies to, if any.
    #[serde(default)]
    pub tool: Option<String>,
    /// Bucket the note shows up under in `librarian_help(server, topic)`.
    #[serde(default)]
    pub topic: Option<String>,
    pub kind: NoteKind,
    pub basis: NoteBasis,
    /// One sentence. Keep prose, not literal arg blobs.
    pub claim: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeedParams {
    pub server: String,
    /// One-line summary that shows up in `librarian_list`.
    #[serde(default)]
    pub summary: Option<String>,
    /// Category bucket for grouping in `librarian_list`.
    #[serde(default)]
    pub category: Option<String>,
    /// The tool list (as the agent sees it).
    pub tools: Vec<SeedTool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeedTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Optional list of required argument names (the agent already knows them
    /// from the deferred tool reminder).
    #[serde(default)]
    pub required: Vec<String>,
    /// Optional map of argument names → short hints.
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct RefreshParams {
    /// Optional: refresh only this server. Omit to refresh every probeable server.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ManifestWriteParams {
    /// Server name the manifest applies to (must match the name in your MCP config).
    pub server: String,
    /// **PREFERRED for non-trivial content.** The manifest as a single TOML string.
    /// Easier to produce than nested JSON — triple-quoted blocks (`\"\"\"...\"\"\"`)
    /// handle multi-line workflow/topic bodies cleanly, no escape-mania. Parsed via
    /// toml::from_str. Provide ONE of `manifest_toml` or `manifest` (not both).
    #[serde(default)]
    pub manifest_toml: Option<String>,
    /// Alternative: the manifest as a structured object. Use this only for short/simple
    /// content. For anything with multi-line bodies, prefer `manifest_toml` — many MCP
    /// clients hang or truncate when constructing deeply nested JSON with embedded newlines.
    #[serde(default)]
    pub manifest: Option<Manifest>,
    /// Required ONLY on the commit call. Get it from a prior propose call.
    /// Omit (or null) to perform a propose call — returns a preview + token.
    #[serde(default)]
    pub confirm_token: Option<String>,
    /// Set true if a manifest for this server already exists and you intend to replace it.
    /// Required on both propose AND commit when overwriting.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ManifestDiffParams {
    pub server: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ManifestRestoreParams {
    pub server: String,
    /// Required ONLY on the commit call. Get it from a prior propose call.
    /// Omit (or null) to perform a propose call — returns a preview + token.
    #[serde(default)]
    pub confirm_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchDocsParams {
    /// Documentation URL to fetch. Public docs only — no auth-gated pages.
    pub url: String,
    /// Optional additional URLs to fetch in the same call (e.g. multi-page docs).
    /// Each is fetched sequentially with rate-limiting applied.
    #[serde(default)]
    pub extra_urls: Vec<String>,
    /// Max characters to return per URL. Default 20000, capped at 50000.
    /// Cached full content lets you re-fetch with a larger value without going to the network.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

// =================== Tool implementations ===================

#[tool_router]
impl LibrarianServer {
    #[tool(
        name = "librarian_list",
        description = "Return a directory of every known MCP server, grouped by category. \
                       Add `category` to filter. One call, agent knows the landscape."
    )]
    async fn list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, ErrorData> {
        self.list_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_help",
        description = "Get a playbook for one MCP server. No topic = overview (categories, workflows, gotchas). \
                       With topic = focused drill-down. Use `server=\"librarian\"` for the librarian itself."
    )]
    async fn help(&self, Parameters(p): Parameters<HelpParams>) -> Result<String, ErrorData> {
        self.help_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_search",
        description = "Fuzzy-search across all known tool names and descriptions. \
                       Returns ranked (server, tool, summary) candidates — cheap to scan before paying for a full schema load."
    )]
    async fn search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, ErrorData> {
        self.search_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_note",
        description = "Append a learned observation about a server's behavior. \
                       Prefer `kind=\"workflow\"` (highest value) and `basis=\"observed\"` (witnessed, not speculated). \
                       This is how the librarian gets smarter with use."
    )]
    async fn note(&self, Parameters(p): Parameters<NoteParams>) -> Result<String, ErrorData> {
        self.note_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_seed_playbook",
        description = "Bootstrap a server entry from the tool list you already see in your context. \
                       Use this for remote/cloud servers the librarian can't probe directly."
    )]
    async fn seed(&self, Parameters(p): Parameters<SeedParams>) -> Result<String, ErrorData> {
        self.seed_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_refresh",
        description = "Reprobe one or all probeable servers. Updates schemas and flags learned notes whose underlying tool shape drifted. \
                       Cache is read-only on the hot path; refresh is always explicit."
    )]
    async fn refresh(&self, Parameters(p): Parameters<RefreshParams>) -> Result<String, ErrorData> {
        self.refresh_inner(p).await.map_err(internal)
    }

    #[tool(
        name = "librarian_manifest_write",
        description = "Write or replace a server's manifest (the authoritative curated playbook). \
                       PASS THE MANIFEST AS `manifest_toml` (a single TOML string) — NOT as nested JSON. \
                       Nested JSON with multi-line workflow/topic bodies causes many MCP clients to hang. \
                       TOML uses `\"\"\"...\"\"\"` for multi-line strings, no escape mania. \
                       TWO-STEP REQUIRED: \
                       (1) Call WITHOUT confirm_token to receive a structured preview + a confirm_token. \
                       (2) Show the preview to the user verbatim and ask them to type 'I agree' or 'yes'. \
                       (3) Once they approve, re-call this tool with the SAME manifest plus confirm_token. \
                       Overwriting an existing manifest requires overwrite=true on both steps. \
                       Empty manifests are refused. Tokens expire in 5 minutes and are single-use. \
                       Every successful write auto-backs up the prior manifest to <server>.toml.bak — \
                       see librarian_manifest_diff and librarian_manifest_restore."
    )]
    async fn manifest_write(
        &self,
        Parameters(p): Parameters<ManifestWriteParams>,
    ) -> Result<String, ErrorData> {
        self.manifest_write_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_manifest_diff",
        description = "Show what changed between a server's current manifest and the auto-backup (the prior version). \
                       Read-only — no commit gate. Output enumerates ADDED / REMOVED / CHANGED items per section \
                       (meta, tool_categories, workflows, topics, gotchas). Use this to inspect what librarian_manifest_write \
                       changed, before deciding whether to librarian_manifest_restore."
    )]
    async fn manifest_diff(
        &self,
        Parameters(p): Parameters<ManifestDiffParams>,
    ) -> Result<String, ErrorData> {
        self.manifest_diff_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_manifest_restore",
        description = "Swap a server's current manifest with its auto-backup. Reversible: calling restore twice in a row \
                       leaves you where you started. \
                       TWO-STEP REQUIRED: \
                       (1) Call WITHOUT confirm_token to see the diff between current and backup + receive a token. \
                       (2) Show the diff to the user and ask them to type 'I agree' or 'yes'. \
                       (3) Re-call with the same server name plus confirm_token. \
                       Errors if no backup exists for the server."
    )]
    async fn manifest_restore(
        &self,
        Parameters(p): Parameters<ManifestRestoreParams>,
    ) -> Result<String, ErrorData> {
        self.manifest_restore_inner(p).map_err(internal)
    }

    #[tool(
        name = "librarian_fetch_docs",
        description = "Fetch a public documentation URL, extract readable text, and return it for use \
                       when synthesizing a manifest. Built for bootstrapping playbooks for HOSTED MCP \
                       servers (Notion, Figma, Slack, HubSpot, Context7, etc.) where the librarian \
                       can't probe directly — the agent reads vendor docs, then constructs a Manifest, \
                       then calls librarian_manifest_write. \
                       Pass `extra_urls` to fetch multiple pages in one call (rate-limited). \
                       Default 20000 chars per URL, capped at 50000. Cached locally for 7 days. \
                       Per-domain rate limit: 1 req/sec. Per-session cap: 50 distinct URLs (cached \
                       re-fetches are free). Public docs only — no auth, no JS-only SPAs."
    )]
    async fn fetch_docs(
        &self,
        Parameters(p): Parameters<FetchDocsParams>,
    ) -> Result<String, ErrorData> {
        self.fetch_docs_inner(p).await.map_err(internal)
    }
}

#[tool_handler]
impl ServerHandler for LibrarianServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Librarian indexes your other MCP servers. Start with `librarian_list`. \
                 Drill into a server with `librarian_help(server)`. \
                 Hunt for a specific tool with `librarian_search(query)`. \
                 File observations with `librarian_note`. \
                 For the librarian's own playbook, call `librarian_help(\"librarian\")`."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

fn internal(err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{err:#}"), None)
}

/// Load a server's manifest with one special case: if it's the librarian itself
/// and no user-authored manifest exists, inject a synthetic default so the
/// librarian doesn't appear orphaned in its own list output.
fn manifest_for(paths: &Paths, server: &str) -> Option<Manifest> {
    let user_manifest = playbook::load_manifest(paths, server).ok().flatten();
    if user_manifest.is_some() {
        return user_manifest;
    }
    if server.eq_ignore_ascii_case("librarian") {
        return Some(playbook::synthetic_librarian_manifest());
    }
    None
}

// =================== Implementation bodies ===================

impl LibrarianServer {
    fn list_inner(&self, p: ListParams) -> Result<String> {
        let index = Index::load(&self.paths.cache_file)?;
        let mut pairs: Vec<(ServerEntry, Option<Manifest>)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Indexed (probed/seeded) servers.
        for entry in index.servers.values() {
            let manifest = manifest_for(&self.paths, &entry.name);
            seen.insert(entry.name.clone());
            pairs.push((entry.clone(), manifest));
        }

        // 2. If the index is empty, fall back to discovery so the agent at
        //    least sees what's *configured* even before a refresh.
        if pairs.is_empty()
            && let Ok(configs) = discovery::discover()
        {
            for cfg in configs {
                let entry = index::entry_from_unprobed(&cfg);
                let manifest = manifest_for(&self.paths, &entry.name);
                seen.insert(entry.name.clone());
                pairs.push((entry, manifest));
            }
        }

        // 3. Surface manifest-only servers — manifests authored before the
        //    corresponding server is installed. Without this they're invisible.
        for server in playbook::list_manifest_servers(&self.paths).unwrap_or_default() {
            if seen.contains(&server) {
                continue;
            }
            let entry = index::entry_manifest_only(&server);
            let manifest = playbook::load_manifest(&self.paths, &server).ok().flatten();
            pairs.push((entry, manifest));
        }

        Ok(playbook::render_list(&pairs, p.category.as_deref()))
    }

    fn help_inner(&self, p: HelpParams) -> Result<String> {
        if p.server.eq_ignore_ascii_case("librarian") {
            return Ok(playbook::render_self());
        }

        let index = Index::load(&self.paths.cache_file)?;
        let entry = match index.servers.get(&p.server) {
            Some(e) => e.clone(),
            None => {
                // Fall back to discovery — show what's *configured* even if not probed.
                if let Some(cfg) = discovery::discover()?
                    .into_iter()
                    .find(|c| c.name == p.server)
                {
                    index::entry_from_unprobed(&cfg)
                } else if playbook::load_manifest(&self.paths, &p.server)?.is_some() {
                    // Last fallback: a manifest exists for this server even though it's
                    // not currently installed/indexed. Synthesize a stub so the playbook
                    // is reachable. This is the "authored in advance" path.
                    index::entry_manifest_only(&p.server)
                } else {
                    anyhow::bail!(
                        "unknown server '{}' — call `librarian_list()` to see what's available",
                        p.server
                    );
                }
            }
        };
        let manifest = playbook::load_manifest(&self.paths, &p.server)?;
        let notes = index::read_notes(&self.paths, &p.server)?;
        Ok(playbook::render_help(
            &entry,
            manifest.as_ref(),
            &notes,
            p.topic.as_deref(),
        ))
    }

    fn search_inner(&self, p: SearchParams) -> Result<String> {
        let index = Index::load(&self.paths.cache_file)?;
        let limit = p.limit.unwrap_or(10);
        let q = p.query.to_lowercase();
        let q_tokens: Vec<&str> = q.split_whitespace().collect();

        let mut hits: Vec<(String, String, String, i64)> = Vec::new();

        // Pass 1: indexed servers — rank their probed tools by name + description.
        for entry in index.servers.values() {
            for tool in &entry.tools {
                let score = rank(&q, &q_tokens, &tool.name, &tool.description);
                if score > 0 {
                    hits.push((
                        entry.name.clone(),
                        tool.name.clone(),
                        tool.description.clone(),
                        score,
                    ));
                }
            }
        }

        // Pass 2: manifest-only servers — rank against manifest content so
        // pre-authored playbooks are findable even before the server is installed.
        for server in playbook::list_manifest_servers(&self.paths).unwrap_or_default() {
            if index.servers.contains_key(&server) {
                continue;
            }
            let manifest = match playbook::load_manifest(&self.paths, &server) {
                Ok(Some(m)) => m,
                _ => continue,
            };
            let haystack = manifest_haystack(&manifest);
            let score = rank(&q, &q_tokens, &server, &haystack);
            if score > 0 {
                let desc = format!(
                    "[NOT INSTALLED] {}",
                    manifest
                        .meta
                        .summary
                        .clone()
                        .unwrap_or_else(|| "manifest authored before install".to_string())
                );
                hits.push((server, "(overview)".to_string(), desc, score));
            }
        }

        hits.sort_by(|a, b| b.3.cmp(&a.3));
        hits.truncate(limit);
        Ok(playbook::render_search(&p.query, &hits))
    }

    fn note_inner(&self, p: NoteParams) -> Result<String> {
        let note = Note {
            timestamp: Utc::now(),
            session_id: None, // could be threaded from a future request meta
            server: p.server.clone(),
            tool: p.tool,
            topic: p.topic,
            kind: p.kind,
            basis: p.basis,
            claim: p.claim,
            tags: p.tags,
            possibly_stale: false,
        };
        index::append_note(&self.paths, &note)?;
        // Echo a preview of what was stored so the agent can verify the
        // recorded value matches its intent — guards against silent
        // misrecording (e.g. unexpected defaults at the client layer).
        let claim_preview: String = if note.claim.chars().count() > 80 {
            let mut s: String = note.claim.chars().take(80).collect();
            s.push('…');
            s
        } else {
            note.claim.clone()
        };
        Ok(format!(
            "noted ({:?}, {:?}) on `{}`\n→ {}",
            note.kind, note.basis, p.server, claim_preview
        ))
    }

    fn seed_inner(&self, p: SeedParams) -> Result<String> {
        let mut index = Index::load(&self.paths.cache_file)?;
        let now = Utc::now();
        let tool_count = p.tools.len();
        let tools: Vec<IndexedTool> = p
            .tools
            .into_iter()
            .map(|t| IndexedTool {
                name: t.name,
                description: t.description,
                arg_summary: if t.required.is_empty() && t.properties.is_empty() {
                    None
                } else {
                    Some(ArgSummary {
                        required: t.required,
                        properties: t.properties,
                    })
                },
            })
            .collect();
        let entry = ServerEntry {
            name: p.server.clone(),
            transport_descriptor: "seeded by agent".to_string(),
            probeable: false,
            probe_status: ProbeStatus::Seeded,
            indexed_at: now,
            tools,
            summary: p.summary,
            category: p.category,
        };
        index.servers.insert(p.server.clone(), entry);
        index.save(&self.paths.cache_file)?;
        Ok(format!("seeded `{}` with {tool_count} tools", p.server))
    }

    async fn refresh_inner(&self, p: RefreshParams) -> Result<String> {
        let configs = discovery::discover()?;
        let mut index = Index::load(&self.paths.cache_file)?;
        let prior = index.clone();

        let to_probe: Vec<_> = match &p.server {
            Some(name) => configs.into_iter().filter(|c| &c.name == name).collect(),
            None => configs,
        };

        if to_probe.is_empty() {
            anyhow::bail!(
                "no servers to refresh{}",
                p.server
                    .as_deref()
                    .map(|s| format!(" (no config entry for '{s}')"))
                    .unwrap_or_default()
            );
        }

        let mut probed_count = 0;
        let mut failed_count = 0;
        let mut remote_count = 0;
        let mut drifted: Vec<(String, Vec<String>)> = Vec::new();

        for entry in probe::probe_all(&to_probe).await {
            // Drift detection vs prior index entry
            if let Some(old) = prior.servers.get(&entry.name) {
                let mut drifted_tools = Vec::new();
                for new_tool in &entry.tools {
                    if let Some(old_tool) =
                        old.tools.iter().find(|t| t.name == new_tool.name)
                        && Index::arg_shape_drifted(&old_tool.arg_summary, &new_tool.arg_summary)
                    {
                        drifted_tools.push(new_tool.name.clone());
                    }
                }
                if !drifted_tools.is_empty() {
                    drifted.push((entry.name.clone(), drifted_tools));
                }
            }

            match &entry.probe_status {
                ProbeStatus::Ok => probed_count += 1,
                ProbeStatus::NotProbeable => remote_count += 1,
                _ => failed_count += 1,
            }
            index.servers.insert(entry.name.clone(), entry);
        }

        // Apply drift flags to learned notes
        let mut drift_note_total = 0;
        for (server, drifted_tools) in &drifted {
            let mut notes = index::read_notes(&self.paths, server)?;
            let mut changed = 0;
            for note in notes.iter_mut() {
                if let Some(tool) = &note.tool
                    && drifted_tools.contains(tool)
                    && !note.possibly_stale
                {
                    note.possibly_stale = true;
                    changed += 1;
                }
            }
            if changed > 0 {
                index::write_notes(&self.paths, server, &notes)?;
                drift_note_total += changed;
            }
        }

        index.save(&self.paths.cache_file)?;

        Ok(format!(
            "refreshed: {probed_count} probed, {failed_count} failed, {remote_count} remote (not probed). \
             drift flags set on {drift_note_total} notes across {} servers.",
            drifted.len()
        ))
    }
}

// =================== Manifest write ===================

impl LibrarianServer {
    fn manifest_write_inner(&self, p: ManifestWriteParams) -> Result<String> {
        // Guard 0: exactly one input form must be provided. Parse TOML if given;
        // otherwise use the structured value. TOML is preferred because nested JSON
        // with embedded newlines causes some MCP clients to hang during serialization.
        let manifest: Manifest = match (p.manifest_toml.as_deref(), p.manifest) {
            (Some(_), Some(_)) => anyhow::bail!(
                "Error: provide either `manifest_toml` (preferred) or `manifest`, not both. \
                 Action: pick one. `manifest_toml` accepts a single TOML string and is the \
                 recommended path for non-trivial content."
            ),
            (Some(toml_str), None) => toml::from_str::<Manifest>(toml_str).map_err(|e| {
                anyhow::anyhow!(
                    "Error: failed to parse `manifest_toml`: {e}. \
                     Action: TOML errors include line/col — fix the syntax and retry. \
                     Common pitfalls: \
                     (1) Root-level fields like `gotchas = [...]` MUST appear BEFORE any `[section]` or `[[section]]` header. \
                         Otherwise they get attached to the previous table. \
                     (2) Use `\"\"\"...\"\"\"` triple-quoted blocks for multi-line workflow/topic bodies — \
                         no escape mania, raw newlines OK. \
                     (3) `[[workflows]]`, `[[topics]]`, `[[tool_categories]]` use DOUBLE brackets (array-of-tables). \
                     (4) `gotchas` is a string array: `gotchas = [\"item 1\", \"item 2\"]`."
                )
            })?,
            (None, Some(m)) => m,
            (None, None) => anyhow::bail!(
                "Error: must provide either `manifest_toml` (preferred) or `manifest`. \
                 Action: pass `manifest_toml` with a TOML string."
            ),
        };

        // Guard 1: refuse empty manifests outright. This is the cheapest gate
        // against an agent calling with `Manifest::default()` and blowing away
        // a curated file.
        if playbook::manifest_is_empty(&manifest) {
            anyhow::bail!(
                "Error: manifest is empty (no meta, no categories, no workflows, no topics, no gotchas). \
                 Action: include at least one of `meta.category`, `meta.summary`, `tool_categories`, \
                 `workflows`, `topics`, or `gotchas` before writing. Empty manifests clobber existing \
                 content with nothing useful."
            );
        }

        let target = self.paths.manifest_path(&p.server);
        let existing = playbook::load_manifest(&self.paths, &p.server)?;

        match p.confirm_token {
            // ----- Propose mode: dry-run, return preview + token. -----
            None => {
                if existing.is_some() && !p.overwrite {
                    anyhow::bail!(
                        "Error: manifest for `{}` already exists at `{}`. \
                         Action: only re-call with `overwrite=true` AFTER reading the current \
                         manifest (via `librarian_help(\"{}\")`) and confirming with the user that \
                         replacement is intended. Pass `overwrite=true` without a token to preview \
                         the replacement.",
                        p.server,
                        target.display(),
                        p.server,
                    );
                }
                let pending = PendingWrite {
                    server: p.server.clone(),
                    action: PendingAction::Write {
                        fingerprint: playbook::manifest_fingerprint(&manifest),
                        overwrite: p.overwrite,
                    },
                    expires_at: Utc::now() + chrono::Duration::seconds(PENDING_WRITE_TTL_SECS),
                };
                let token = self.issue_token(pending);
                let preview = playbook::render_manifest_preview(
                    &p.server,
                    &manifest,
                    &target,
                    existing.as_ref(),
                );
                Ok(format!(
                    "{preview}\n\
                     ---\n\
                     **REVIEW REQUIRED.** Show the preview above to the user. \
                     Ask them to type **\"I agree\"** or **\"yes\"** to commit. \
                     Once they approve, re-call `librarian_manifest_write` with:\n\
                     - the **same** `manifest` content (any difference will reject),\n\
                     - `confirm_token=\"{token}\"`,\n\
                     - `overwrite={}` (must match the propose call).\n\n\
                     The token expires in {} minutes and is single-use. \
                     Do NOT commit on your own initiative — wait for explicit user approval.",
                    p.overwrite,
                    PENDING_WRITE_TTL_SECS / 60,
                ))
            }
            // ----- Commit mode: verify token + fingerprint, then write. -----
            Some(token) => {
                let pending = self.consume_token(&token).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Error: `confirm_token` is not recognized, already used, or expired. \
                         Action: re-call without `confirm_token` to get a fresh preview + token. \
                         Tokens are single-use and expire after {} minutes.",
                        PENDING_WRITE_TTL_SECS / 60,
                    )
                })?;
                if pending.server != p.server {
                    anyhow::bail!(
                        "Error: `confirm_token` was issued for server `{}`, but this commit is for `{}`. \
                         Action: re-call without `confirm_token` for the correct server to get a fresh token.",
                        pending.server,
                        p.server,
                    );
                }
                let (token_fingerprint, token_overwrite) = match &pending.action {
                    PendingAction::Write { fingerprint, overwrite } => (fingerprint, *overwrite),
                    PendingAction::Restore => anyhow::bail!(
                        "Error: `confirm_token` was issued for a restore call, not a write. \
                         Action: re-call `librarian_manifest_write` without `confirm_token` to \
                         get a write-specific token."
                    ),
                };
                if token_overwrite != p.overwrite {
                    anyhow::bail!(
                        "Error: `overwrite` flag changed between propose ({}) and commit ({}). \
                         Action: re-call without `confirm_token` with the intended `overwrite` value to \
                         get a fresh preview + token. The user must approve the actual flag value.",
                        token_overwrite,
                        p.overwrite,
                    );
                }
                let now_fp = playbook::manifest_fingerprint(&manifest);
                if &now_fp != token_fingerprint {
                    anyhow::bail!(
                        "Error: manifest content differs from what was proposed and approved. \
                         The user approved a specific manifest; you are now trying to commit \
                         different content. Action: re-call without `confirm_token` with the \
                         CURRENT manifest content to get a fresh preview + token, then have the \
                         user approve the new version."
                    );
                }
                // All checks passed. Re-verify existence under the same rules
                // as propose (paranoia: file could have been created between
                // propose and commit).
                if playbook::load_manifest(&self.paths, &p.server)?.is_some()
                    && !token_overwrite
                {
                    anyhow::bail!(
                        "Error: a manifest for `{}` was created since the propose call, and \
                         `overwrite=false` on this commit. Action: re-call without `confirm_token` \
                         and `overwrite=true` to preview the replacement.",
                        p.server,
                    );
                }
                playbook::write_manifest(&self.paths, &p.server, &manifest)?;
                Ok(format!(
                    "Committed manifest for `{}` → `{}` ({} categor{}, {} workflow{}, {} topic{}, {} gotcha{}).",
                    p.server,
                    target.display(),
                    manifest.tool_categories.len(),
                    if manifest.tool_categories.len() == 1 { "y" } else { "ies" },
                    manifest.workflows.len(),
                    if manifest.workflows.len() == 1 { "" } else { "s" },
                    manifest.topics.len(),
                    if manifest.topics.len() == 1 { "" } else { "s" },
                    manifest.gotchas.len(),
                    if manifest.gotchas.len() == 1 { "" } else { "s" },
                ))
            }
        }
    }
}

// =================== Manifest diff & restore ===================

impl LibrarianServer {
    fn manifest_diff_inner(&self, p: ManifestDiffParams) -> Result<String> {
        let current = playbook::load_manifest(&self.paths, &p.server)?;
        let backup = playbook::load_manifest_backup(&self.paths, &p.server)?;

        match (current, backup) {
            (None, None) => Ok(format!(
                "*(no manifest and no backup for `{}` — nothing to diff)*",
                p.server
            )),
            (Some(_), None) => Ok(format!(
                "*(no backup for `{}` — only the current manifest exists. Backups are created \
                 on every `librarian_manifest_write` commit.)*",
                p.server
            )),
            (None, Some(_)) => Ok(format!(
                "*(no current manifest for `{}` — only the backup exists. \
                 Call `librarian_manifest_restore` to bring the backup back as current.)*",
                p.server
            )),
            (Some(curr), Some(bak)) => {
                let mut out = String::new();
                let _ = writeln!(out, "# Manifest diff for `{}`", p.server);
                let _ = writeln!(
                    out,
                    "\n**Backup → Current** (what `librarian_manifest_write` changed)\n"
                );
                out.push_str(&playbook::diff_manifests(&bak, &curr));
                out.push_str(
                    "\n---\n*To revert these changes, use `librarian_manifest_restore`.*\n",
                );
                Ok(out)
            }
        }
    }

    fn manifest_restore_inner(&self, p: ManifestRestoreParams) -> Result<String> {
        let backup = playbook::load_manifest_backup(&self.paths, &p.server)?;
        if backup.is_none() {
            anyhow::bail!(
                "Error: no backup exists for `{}`. Action: backups are only created when a \
                 manifest is written via `librarian_manifest_write`. There's nothing to restore.",
                p.server,
            );
        }
        let current = playbook::load_manifest(&self.paths, &p.server)?;

        match p.confirm_token {
            // ----- Propose: show diff + token. -----
            None => {
                let pending = PendingWrite {
                    server: p.server.clone(),
                    action: PendingAction::Restore,
                    expires_at: Utc::now() + chrono::Duration::seconds(PENDING_WRITE_TTL_SECS),
                };
                let token = self.issue_token(pending);

                let mut preview = String::new();
                let _ = writeln!(
                    preview,
                    "## MANIFEST RESTORE — PREVIEW (NOT YET COMMITTED)\n\n**Server:** `{}`\n",
                    p.server
                );
                preview.push_str(
                    "Restore will swap the current manifest with the backup. \
                     Calling restore twice in a row leaves you where you started.\n\n",
                );
                match (&current, &backup) {
                    (Some(c), Some(b)) => {
                        preview.push_str("### What will change (current → backup)\n\n");
                        preview.push_str(&playbook::diff_manifests(c, b));
                    }
                    (None, Some(_)) => {
                        preview.push_str(
                            "### No current manifest exists; restore will bring the backup back as current.\n\n",
                        );
                    }
                    _ => {}
                }
                let _ = writeln!(
                    preview,
                    "\n---\n\
                     **REVIEW REQUIRED.** Show the diff above to the user. \
                     Ask them to type **\"I agree\"** or **\"yes\"** to commit. \
                     Once they approve, re-call `librarian_manifest_restore` with:\n\
                     - `server=\"{}\"`,\n\
                     - `confirm_token=\"{token}\"`.\n\n\
                     Token expires in {} minutes and is single-use. \
                     Do NOT commit on your own initiative — wait for explicit user approval.",
                    p.server,
                    PENDING_WRITE_TTL_SECS / 60,
                );
                Ok(preview)
            }
            // ----- Commit: verify token, then swap. -----
            Some(token) => {
                let pending = self.consume_token(&token).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Error: `confirm_token` is not recognized, already used, or expired. \
                         Action: re-call `librarian_manifest_restore` without `confirm_token` to \
                         get a fresh preview + token. Tokens are single-use and expire after {} minutes.",
                        PENDING_WRITE_TTL_SECS / 60,
                    )
                })?;
                if pending.server != p.server {
                    anyhow::bail!(
                        "Error: `confirm_token` was issued for server `{}`, but this commit is for `{}`. \
                         Action: re-call without `confirm_token` for the correct server to get a fresh token.",
                        pending.server,
                        p.server,
                    );
                }
                if !matches!(pending.action, PendingAction::Restore) {
                    anyhow::bail!(
                        "Error: `confirm_token` was issued for a different action (not a restore). \
                         Action: re-call `librarian_manifest_restore` without `confirm_token` to get \
                         a restore-specific token."
                    );
                }
                playbook::restore_manifest(&self.paths, &p.server)?;
                Ok(format!(
                    "Restored `{}` from backup. The previous current is now the backup, so a \
                     second `librarian_manifest_restore` call will undo this restore.",
                    p.server,
                ))
            }
        }
    }
}

// =================== Fetch docs ===================

impl LibrarianServer {
    async fn fetch_docs_inner(&self, p: FetchDocsParams) -> Result<String> {
        let max_chars = p.max_chars.unwrap_or(fetch::DEFAULT_MAX_CHARS);
        let mut out = String::new();

        let urls: Vec<String> = std::iter::once(p.url.clone())
            .chain(p.extra_urls.into_iter())
            .collect();

        for (i, url) in urls.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n---\n\n");
            }
            match fetch::fetch_docs(&self.paths, &self.fetch_state, url, max_chars).await {
                Ok(outcome) => {
                    let cache_status = if outcome.from_cache {
                        let age_min = outcome.cache_age_secs / 60;
                        if age_min < 1 {
                            "HIT (just now)".to_string()
                        } else if age_min < 60 {
                            format!("HIT (fetched {age_min}min ago)")
                        } else {
                            format!("HIT (fetched {}h ago)", age_min / 60)
                        }
                    } else {
                        "MISS (fresh fetch)".to_string()
                    };
                    let _ = writeln!(out, "**Fetched:** {}", outcome.url);
                    let _ = writeln!(
                        out,
                        "**Cache:** {} · **path:** `{}`",
                        cache_status,
                        outcome.cache_path.display()
                    );
                    let trunc_note = if outcome.truncated {
                        format!(
                            " *(TRUNCATED — pass `max_chars` up to {} to get more)*",
                            fetch::MAX_MAX_CHARS
                        )
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        out,
                        "**Source:** {} chars · **Returned:** {} chars{}",
                        outcome.source_size, outcome.returned_size, trunc_note,
                    );
                    out.push_str("\n---\n\n");
                    out.push_str(&outcome.content);
                }
                Err(err) => {
                    let _ = writeln!(out, "**Failed to fetch:** `{url}`");
                    let _ = writeln!(out, "{err:#}");
                }
            }
        }
        Ok(out)
    }
}

// =================== Fuzzy rank ===================

/// Concatenate searchable fields from a manifest for free-text matching.
fn manifest_haystack(m: &Manifest) -> String {
    let mut s = String::new();
    if let Some(c) = &m.meta.category {
        s.push_str(c);
        s.push(' ');
    }
    if let Some(sum) = &m.meta.summary {
        s.push_str(sum);
        s.push(' ');
    }
    for c in &m.tool_categories {
        s.push_str(&c.name);
        s.push(' ');
        for t in &c.tools {
            s.push_str(t);
            s.push(' ');
        }
    }
    for w in &m.workflows {
        s.push_str(&w.title);
        s.push(' ');
        s.push_str(&w.body);
        s.push(' ');
    }
    for t in &m.topics {
        s.push_str(&t.title);
        s.push(' ');
        s.push_str(&t.body);
        s.push(' ');
    }
    for g in &m.gotchas {
        s.push_str(g);
        s.push(' ');
    }
    s
}

fn rank(query: &str, query_tokens: &[&str], name: &str, description: &str) -> i64 {
    let name_lower = name.to_lowercase();
    let desc_lower = description.to_lowercase();
    let mut score: i64 = 0;
    // Whole-query substring matches are highest signal
    if name_lower.contains(query) {
        score += 100;
    }
    if desc_lower.contains(query) {
        score += 30;
    }
    // Token overlap
    for token in query_tokens {
        if token.is_empty() {
            continue;
        }
        if name_lower.contains(token) {
            score += 25;
        }
        if desc_lower.contains(token) {
            score += 8;
        }
    }
    // Exact name match dominates
    if name_lower == query {
        score += 200;
    }
    score
}

// Silence the brief-budget constant from config; we use it for future render trimming
// but not in the current MVP rendering paths.
#[allow(dead_code)]
const _BRIEF_BUDGET: usize = DEFAULT_BRIEF_TOKEN_BUDGET;

// Touch RequestContext so future hooks (session id, etc.) don't break the import.
#[allow(dead_code)]
fn _ctx_marker(_c: RequestContext<RoleServer>) {}
