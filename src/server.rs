use anyhow::Result;
use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::config::{DEFAULT_BRIEF_TOKEN_BUDGET, Paths, validate_server_name};
use crate::discovery;
use crate::fetch::{self, FetchState};
use crate::index::{
    self, ArgSummary, Index, IndexedTool, Note, NoteBasis, NoteKind, ProbeStatus, ServerEntry,
};
use crate::lockfile;
use crate::playbook::{self, Manifest};
use crate::probe;

/// Time-to-live for pending manifest-write tokens. The agent must commit within
/// this window after the user approves; otherwise re-propose.
const PENDING_WRITE_TTL_SECS: i64 = 5 * 60;

/// Hard cap on `manifest_toml` input bytes. Real manifests for the busiest
/// hosted servers (Notion's 14 tools etc.) land well under 16 KiB authored.
/// 256 KiB is ~16x the worst real case, large enough that no honest workflow
/// hits it, small enough that an adversarial agent cannot exhaust parser
/// memory via a deeply-nested or pathological TOML payload. Enforced at the
/// `librarian_manifest_write` tool entry before `toml::from_str` runs.
const MAX_MANIFEST_TOML_BYTES: usize = 256 * 1024;

/// Hard cap on `librarian_note` `claim` bytes. Notes are convention-prose,
/// roughly one sentence. 8 KiB is generous; anything bigger is the agent
/// pasting raw blobs into the notes file (an anti-pattern the cap nudges
/// away from).
const MAX_CLAIM_BYTES: usize = 8 * 1024;

/// Hard cap on `extra_urls` count for `librarian_fetch_docs`. Each URL costs
/// a network round trip and up to `MAX_RESPONSE_BYTES`; capping the count
/// bounds the per-call wall time and memory footprint.
const MAX_EXTRA_URLS: usize = 16;

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
    Write {
        fingerprint: String,
        overwrite: bool,
    },
    /// Swap current ↔ backup for the named server.
    Restore,
    /// The user approved a specific list of servers to bulk-seed.
    /// Fingerprint pins the exact `Vec<SeedParams>` content.
    SeedBatch { fingerprint: String },
    /// The user approved removing an index entry for the named server.
    SeedRemove,
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
        let mut map = self
            .pending_writes
            .lock()
            .expect("pending_writes lock poisoned");
        cleanup_expired(&mut map);
        let token = generate_token();
        map.insert(token.clone(), pending);
        token
    }

    fn consume_token(&self, token: &str) -> Option<PendingWrite> {
        let mut map = self
            .pending_writes
            .lock()
            .expect("pending_writes lock poisoned");
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

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct OnboardingParams {}

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
    /// Bypass duplicate detection. Default false. By default an exact match
    /// on (server, tool, kind, normalized claim) is rejected with a pointer
    /// to the existing note so playbooks don't accumulate near-duplicates.
    /// Set true to file the note anyway (e.g. re-emphasizing a still-true
    /// observation in a new session).
    #[serde(default)]
    pub allow_duplicate: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeedRemoveParams {
    /// Server name to remove from the librarian index.
    pub server: String,
    /// Required ONLY on the commit call. Get it from a prior propose call.
    /// Omit (or null) to perform a propose call — returns a preview + token.
    #[serde(default)]
    pub confirm_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SeedBatchParams {
    /// One entry per server to seed. Same shape as `librarian_seed_playbook`.
    /// Designed for first-install onboarding: many hosted/cloud servers at once
    /// under a single user approval.
    pub servers: Vec<SeedParams>,
    /// Required ONLY on the commit call. Get it from a prior propose call.
    /// Omit (or null) to perform a propose call — returns a preview + token.
    #[serde(default)]
    pub confirm_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct RefreshParams {
    /// Optional: refresh only this server. Omit to refresh every probeable server.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
        self.list_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_onboarding",
        description = "Return a step-by-step bootstrap prompt for indexing every MCP server connected to this client. \
                       Call this once on first install: it tells the agent how to combine \
                       `librarian_refresh` (auto-discovers local stdio servers) with `librarian_seed_playbook` \
                       (manually-registers hosted/cloud servers visible only in the deferred-tools reminder). \
                       Output is a prompt — the agent reads it and performs the actions described."
    )]
    async fn onboarding(
        &self,
        Parameters(_): Parameters<OnboardingParams>,
    ) -> Result<String, ErrorData> {
        Ok(playbook::render_onboarding())
    }

    #[tool(
        name = "librarian_help",
        description = "Get a playbook for one MCP server. No topic = overview (categories, workflows, gotchas). \
                       With topic = focused drill-down. Use `server=\"librarian\"` for the librarian itself."
    )]
    async fn help(&self, Parameters(p): Parameters<HelpParams>) -> Result<String, ErrorData> {
        self.help_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_search",
        description = "Fuzzy-search across all known tool names and descriptions. \
                       Returns ranked (server, tool, summary) candidates — cheap to scan before paying for a full schema load."
    )]
    async fn search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, ErrorData> {
        self.search_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_note",
        description = "Append a learned observation about a server's behavior. \
                       Prefer `kind=\"workflow\"` (highest value) and `basis=\"observed\"` (witnessed, not speculated). \
                       This is how the librarian gets smarter with use."
    )]
    async fn note(&self, Parameters(p): Parameters<NoteParams>) -> Result<String, ErrorData> {
        self.note_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_seed_playbook",
        description = "Bootstrap a single server entry from the tool list you already see in your context. \
                       Use this for remote/cloud servers the librarian can't probe directly. \
                       For first-install onboarding of MANY hosted servers at once, prefer \
                       `librarian_seed_batch` — one user approval covers the whole batch."
    )]
    async fn seed(&self, Parameters(p): Parameters<SeedParams>) -> Result<String, ErrorData> {
        self.seed_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_seed_remove",
        description = "Remove a server entry from the librarian index. For cleaning up stale \
                       seeds (servers no longer connected to the client). \
                       TWO-STEP REQUIRED: \
                       (1) Call WITHOUT confirm_token to receive a preview of what will be \
                       removed + a confirm_token. \
                       (2) Show the preview to the user and ask them to type 'I agree' or 'yes'. \
                       (3) Re-call with the same server name plus confirm_token. \
                       Notes about the server (learned/<server>.jsonl) are NOT deleted — they \
                       persist on disk. Manifest files are NOT touched either. If a manifest \
                       exists OR the server is in the MCP client config, it will be re-surfaced \
                       on the next list/refresh."
    )]
    async fn seed_remove(
        &self,
        Parameters(p): Parameters<SeedRemoveParams>,
    ) -> Result<String, ErrorData> {
        self.seed_remove_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_seed_batch",
        description = "Bulk-seed many hosted/cloud server entries in one approved transaction. \
                       Designed for first-install onboarding where the agent has identified every \
                       hosted MCP server visible in its deferred-tools reminder. \
                       TWO-STEP REQUIRED: \
                       (1) Call WITHOUT confirm_token to receive a structured preview (every \
                       server, its category, its tool count, plus a warning if any names \
                       collide with existing entries) + a confirm_token. \
                       (2) Show the preview to the user and ask them to type 'I agree' or 'yes'. \
                       (3) Re-call with the SAME `servers` list plus confirm_token. \
                       All seeds land atomically under one write lock. Tokens expire in 5 \
                       minutes and are single-use."
    )]
    async fn seed_batch(
        &self,
        Parameters(p): Parameters<SeedBatchParams>,
    ) -> Result<String, ErrorData> {
        self.seed_batch_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }

    #[tool(
        name = "librarian_refresh",
        description = "Reprobe one or all probeable servers. Updates schemas and flags learned notes whose underlying tool shape drifted. \
                       Cache is read-only on the hot path; refresh is always explicit."
    )]
    async fn refresh(&self, Parameters(p): Parameters<RefreshParams>) -> Result<String, ErrorData> {
        self.refresh_inner(p)
            .await
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
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
        self.manifest_write_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
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
        self.manifest_diff_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
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
        self.manifest_restore_inner(p)
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
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
        self.fetch_docs_inner(p)
            .await
            .map_or_else(|e| Ok(format_diagnostic(e)), Ok)
    }
}

#[tool_handler]
impl ServerHandler for LibrarianServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            // Surfaced by MCP clients (Claude Code, Claude Desktop, Codex) in
            // the agent's session-init context. The directive opener converts
            // librarian from "opt-in if you know to ask" into "the first thing
            // the agent reads before touching any other indexed server." This
            // is the cheapest hook for solving cold-start discoverability.
            instructions: Some(
                "Orient before acting. Before calling any indexed MCP server's tools, \
                 call `librarian_help(server)` to load that server's workflows, gotchas, \
                 and tool categorizations.\n\n\
                 Start with `librarian_list()` for the landscape; `librarian_search(query)` \
                 finds tools across all servers; `librarian_note(...)` files an observation \
                 for future sessions. Self-playbook: `librarian_help(\"librarian\")`."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

/// Render an `anyhow::Error` as the text content of a successful tool
/// response. The agent reads it as the call's output and self-corrects.
///
/// Why not return a JSON-RPC error instead: every realistic failure in this
/// codebase is a user-correctable validation problem (bad token, misplaced
/// gotchas, unknown server, SSRF-blocked URL, expired propose, etc.), and
/// multiple MCP client harnesses have been observed to swallow the error
/// `message` field — surfacing only a generic "Tool execution failed"
/// without the diagnostic. Returning the diagnostic as text content means
/// the message ALWAYS reaches the agent, regardless of client behavior.
///
/// Every `anyhow::bail!()` in this crate starts its message with "Error: ".
/// That prefix is the signal to the agent that the response is a corrective
/// message, not a normal one. Agents can pattern-match on it.
///
/// `internal()` below is kept for any genuinely-internal error path that
/// should escalate at the JSON-RPC layer — but no such path currently
/// exists in the tool surface.
fn format_diagnostic(err: anyhow::Error) -> String {
    format!("{err:#}")
}

#[allow(dead_code)]
fn internal(err: anyhow::Error) -> ErrorData {
    ErrorData::invalid_params(format!("{err:#}"), None)
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
            return Ok(match p.topic.as_deref() {
                None => playbook::render_self(),
                Some(t) => playbook::render_librarian_topic(t),
            });
        }
        validate_server_name(&p.server)?;

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
        // Per-call manifest cache so the indexed-server pass doesn't re-read
        // each manifest once per tool. Keyed by server name; value is the
        // optional Manifest (None means "checked, no manifest exists").
        let mut manifest_cache: std::collections::HashMap<String, Option<Manifest>> =
            std::collections::HashMap::new();

        // Pass 1: indexed servers — rank their probed tools by name + description,
        // boosted by any tool_aliases attached to the tool in the server's manifest.
        for entry in index.servers.values() {
            let manifest = manifest_cache.entry(entry.name.clone()).or_insert_with(|| {
                playbook::load_manifest(&self.paths, &entry.name)
                    .ok()
                    .flatten()
            });
            for tool in &entry.tools {
                let phrases = collect_alias_phrases_for_tool(manifest.as_ref(), &tool.name);
                let aliases_haystack = phrases.join(" ");
                let mut score = rank(
                    &q,
                    &q_tokens,
                    &tool.name,
                    &tool.description,
                    &aliases_haystack,
                );
                // Per-phrase bonus: a phrase whose meaningful tokens densely
                // match the query is stronger signal than scattered token hits
                // across the cat'd haystack. Closes the noisy-query gap where
                // alias-only tools (e.g. `outline`) got squeezed below hosted
                // `*_search_*` tools that share a single common name token
                // ("search") with a multi-intent query.
                score += phrase_overlap_bonus(&phrases, &q);
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
        // Aliases flow into `manifest_haystack` here (catch-all), since there's
        // no specific tool to attribute them to.
        for server in playbook::list_manifest_servers(&self.paths).unwrap_or_default() {
            if index.servers.contains_key(&server) {
                continue;
            }
            let manifest = match playbook::load_manifest(&self.paths, &server) {
                Ok(Some(m)) => m,
                _ => continue,
            };
            let haystack = manifest_haystack(&manifest);
            let score = rank(&q, &q_tokens, &server, &haystack, "");
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

        hits.sort_by_key(|h| std::cmp::Reverse(h.3));
        hits.truncate(limit);
        Ok(playbook::render_search(&p.query, &hits))
    }

    fn note_inner(&self, p: NoteParams) -> Result<String> {
        validate_server_name(&p.server)?;
        if p.claim.len() > MAX_CLAIM_BYTES {
            anyhow::bail!(
                "Error: `claim` is {} bytes; cap is {} bytes. \
                 Action: `claim` is convention-prose (a sentence or two describing one observation), \
                 not a paste of raw arg blobs. If you really need to file a long claim, split it \
                 into multiple focused notes.",
                p.claim.len(),
                MAX_CLAIM_BYTES,
            );
        }
        // The read-check-write sequence below MUST be atomic across processes,
        // or two clients writing the same observation concurrently can both
        // pass the dedup check before either commits and produce duplicates —
        // the exact pollution dedup was added to prevent.
        let note = lockfile::with_write_lock(&self.paths, || {
            // Dedup gate: by default reject a note that's equivalent to an existing
            // one (same server, tool, kind, normalized claim). This is the cheapest
            // way to keep playbooks from accumulating near-duplicates as fresh
            // sessions re-discover known facts. `allow_duplicate=true` is the
            // escape hatch for intentional re-emphasis.
            if !p.allow_duplicate {
                let existing = index::read_notes(&self.paths, &p.server).unwrap_or_default();
                let new_norm = normalize_claim(&p.claim);
                if let Some(dup) = existing.iter().find(|n| {
                    n.tool == p.tool && n.kind == p.kind && normalize_claim(&n.claim) == new_norm
                }) {
                    let tool_clause = p
                        .tool
                        .as_deref()
                        .map(|t| format!(" / `{t}`"))
                        .unwrap_or_default();
                    let existing_preview: String = if dup.claim.chars().count() > 120 {
                        let mut s: String = dup.claim.chars().take(120).collect();
                        s.push('…');
                        s
                    } else {
                        dup.claim.clone()
                    };
                    anyhow::bail!(
                        "Error: duplicate note. An equivalent note already exists for `{}`{} (kind: {:?}) \
                         filed at {}. Existing claim: \"{}\". \
                         Action: if your new note adds genuinely different content, rephrase it. \
                         If you want to record this observation anyway (e.g. recency bump or strong \
                         re-confirmation), re-call with `allow_duplicate=true`.",
                        p.server,
                        tool_clause,
                        p.kind,
                        dup.timestamp.format("%Y-%m-%d %H:%M UTC"),
                        existing_preview,
                    );
                }
            }

            let note = Note {
                timestamp: Utc::now(),
                session_id: None, // could be threaded from a future request meta
                server: p.server.clone(),
                tool: p.tool.clone(),
                topic: p.topic.clone(),
                kind: p.kind.clone(),
                basis: p.basis.clone(),
                claim: p.claim.clone(),
                tags: p.tags.clone(),
                possibly_stale: false,
            };
            index::append_note(&self.paths, &note)?;
            Ok(note)
        })?;
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
        validate_server_name(&p.server)?;
        let now = Utc::now();
        let tool_count = p.tools.len();
        let server_name = p.server.clone();
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
            name: server_name.clone(),
            transport_descriptor: "seeded by agent".to_string(),
            probeable: false,
            probe_status: ProbeStatus::Seeded,
            indexed_at: now,
            tools,
            summary: p.summary,
            category: p.category,
        };

        // Lock around load-modify-save so a concurrent seed/refresh doesn't
        // produce a lost update.
        lockfile::with_write_lock(&self.paths, || {
            let mut index = Index::load(&self.paths.cache_file)?;
            index.servers.insert(server_name.clone(), entry);
            index.save(&self.paths.cache_file)?;
            Ok(())
        })?;
        Ok(format!("seeded `{}` with {tool_count} tools", server_name))
    }

    fn seed_remove_inner(&self, p: SeedRemoveParams) -> Result<String> {
        validate_server_name(&p.server)?;

        match p.confirm_token {
            // ----- Propose: show what'll be removed + token. -----
            None => {
                let index = Index::load(&self.paths.cache_file)?;
                let entry = index.servers.get(&p.server).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Error: `{}` is not in the librarian index. \
                         Action: call `librarian_list()` to see what's actually indexed. \
                         If a manifest file exists for this name but the entry isn't in the \
                         index, the entry is surfaced via the manifest-only fallback — delete \
                         the manifest file at `<config>/manifests/{}.toml` to remove it.",
                        p.server,
                        p.server,
                    )
                })?;

                // Detect cases where removal won't have the intended permanent effect,
                // so the preview can warn the user.
                let manifest_exists = playbook::load_manifest(&self.paths, &p.server)
                    .ok()
                    .flatten()
                    .is_some();
                let in_client_config = discovery::discover()
                    .ok()
                    .map(|cs| cs.iter().any(|c| c.name == p.server))
                    .unwrap_or(false);

                let pending = PendingWrite {
                    server: p.server.clone(),
                    action: PendingAction::SeedRemove,
                    expires_at: Utc::now() + chrono::Duration::seconds(PENDING_WRITE_TTL_SECS),
                };
                let token = self.issue_token(pending);

                // IMPORTANT: this preview is rendered by the MCP client. Claude
                // Desktop has been observed to hang for minutes on previews that
                // combine em-dashes, angle-bracket placeholders, and nested
                // backticks. Keep this body character-conservative: ASCII hyphens
                // only, no `<x>` placeholders, no backticks around content that
                // already contains punctuation. The regression tests in
                // `tests/integration.rs` assert no known hang-triggering glyphs
                // appear in this preview.
                let safe_summary = sanitize_for_preview(entry.summary.as_deref());

                let mut preview = String::new();
                preview.push_str("## SEED REMOVAL - PREVIEW (NOT YET COMMITTED)\n\n");
                let _ = writeln!(preview, "Server: {}", entry.name);
                let _ = writeln!(preview, "State:  {:?}", entry.probe_status);
                if let Some(s) = &safe_summary {
                    let _ = writeln!(preview, "Summary: {s}");
                }
                if let Some(c) = &entry.category {
                    let _ = writeln!(preview, "Category: {c}");
                }
                let _ = writeln!(preview, "Tools:  {}", entry.tools.len());

                preview.push('\n');
                if manifest_exists || in_client_config {
                    preview.push_str("Warning - removal will not be permanent in this case:\n");
                    if manifest_exists {
                        preview.push_str(
                            "  - A manifest file exists for this server. After index removal it \
                             will re-surface as a manifest-only entry in librarian_list. To \
                             remove fully, delete the manifest file too.\n",
                        );
                    }
                    if in_client_config {
                        preview.push_str(
                            "  - This server is in your MCP client config. The next \
                             librarian_refresh will re-add it. To remove permanently, also \
                             remove the entry from your client config.\n",
                        );
                    }
                    preview.push('\n');
                }

                preview.push_str(
                    "Learned notes (the per-server jsonl file under data/learned) are NOT \
                     deleted. They persist on disk and reappear if the server is re-seeded \
                     under the same name.\n\n",
                );

                let _ = writeln!(
                    preview,
                    "---\n\
                     REVIEW REQUIRED. Show the preview above to the user. Ask them to type \
                     \"I agree\" or \"yes\" to commit. Once they approve, re-call \
                     librarian_seed_remove with:\n  \
                     server=\"{}\",\n  \
                     confirm_token=\"{token}\".\n\n\
                     Token expires in {} minutes and is single-use.",
                    p.server,
                    PENDING_WRITE_TTL_SECS / 60,
                );
                Ok(preview)
            }
            // ----- Commit: verify token, then remove. -----
            Some(token) => {
                let pending = self.consume_token(&token).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Error: `confirm_token` is not recognized, already used, or expired. \
                         Action: re-call `librarian_seed_remove` without `confirm_token` to \
                         get a fresh preview + token."
                    )
                })?;
                if pending.server != p.server {
                    anyhow::bail!(
                        "Error: `confirm_token` was issued for server `{}`, but this commit is for `{}`. \
                         Action: re-call without `confirm_token` for the correct server.",
                        pending.server,
                        p.server,
                    );
                }
                if !matches!(pending.action, PendingAction::SeedRemove) {
                    anyhow::bail!(
                        "Error: `confirm_token` was issued for a different action (not a seed removal). \
                         Action: re-call `librarian_seed_remove` without `confirm_token` to get a \
                         removal-specific token."
                    );
                }

                let server_name = p.server.clone();
                lockfile::with_write_lock(&self.paths, || {
                    let mut index = Index::load(&self.paths.cache_file)?;
                    if index.servers.remove(&server_name).is_none() {
                        anyhow::bail!(
                            "Error: `{}` no longer in the index — removed by another process between \
                             propose and commit.",
                            server_name,
                        );
                    }
                    index.save(&self.paths.cache_file)?;
                    Ok(())
                })?;

                Ok(format!("Removed `{}` from the librarian index.", p.server))
            }
        }
    }

    fn seed_batch_inner(&self, p: SeedBatchParams) -> Result<String> {
        // Validate up front: non-empty, no name collisions within the batch,
        // every server name passes the path-traversal validator.
        if p.servers.is_empty() {
            anyhow::bail!(
                "Error: `servers` is empty. Action: pass at least one server entry. \
                 For zero servers, just don't call this tool."
            );
        }
        let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for item in &p.servers {
            validate_server_name(&item.server)?;
            if !seen_names.insert(item.server.as_str()) {
                anyhow::bail!(
                    "Error: server name `{}` appears more than once in the batch. \
                     Action: each server may appear at most once. Deduplicate the list.",
                    item.server,
                );
            }
        }

        let fingerprint = seed_batch_fingerprint(&p.servers);

        match p.confirm_token {
            // ----- Propose -----
            None => {
                // Load the current index to identify collisions for the preview.
                let index = Index::load(&self.paths.cache_file)?;
                let collisions: Vec<&str> = p
                    .servers
                    .iter()
                    .filter(|s| index.servers.contains_key(&s.server))
                    .map(|s| s.server.as_str())
                    .collect();

                let pending = PendingWrite {
                    server: "(batch)".to_string(),
                    action: PendingAction::SeedBatch { fingerprint },
                    expires_at: Utc::now() + chrono::Duration::seconds(PENDING_WRITE_TTL_SECS),
                };
                let token = self.issue_token(pending);

                let mut preview = String::new();
                preview.push_str("## SEED BATCH — PREVIEW (NOT YET COMMITTED)\n\n");
                let _ = writeln!(
                    preview,
                    "The agent proposes to add the following {} server{} to the librarian index:\n",
                    p.servers.len(),
                    if p.servers.len() == 1 { "" } else { "s" },
                );
                let mut total_tools = 0usize;
                for item in &p.servers {
                    total_tools += item.tools.len();
                    let _ = writeln!(
                        preview,
                        "  - {} ({}) — {} tool{}",
                        item.server,
                        item.category.as_deref().unwrap_or("uncategorized"),
                        item.tools.len(),
                        if item.tools.len() == 1 { "" } else { "s" },
                    );
                }
                let _ = writeln!(
                    preview,
                    "\nTotal: {} server{}, {total_tools} tool{}.",
                    p.servers.len(),
                    if p.servers.len() == 1 { "" } else { "s" },
                    if total_tools == 1 { "" } else { "s" },
                );

                if collisions.is_empty() {
                    preview.push_str("\nNo existing entries will be overwritten.\n");
                } else {
                    preview.push_str("\n**Existing entries that will be OVERWRITTEN:**\n");
                    for name in &collisions {
                        let _ = writeln!(preview, "  - {name}");
                    }
                    preview.push_str(
                        "\nIf any of these were curated, abandon this batch and seed the new ones \
                         individually with `librarian_seed_playbook` instead.\n",
                    );
                }

                let _ = writeln!(
                    preview,
                    "\n---\n\
                     **REVIEW REQUIRED.** Show the preview above to the user. \
                     Ask them to type **\"I agree\"** or **\"yes\"** to commit. \
                     Once they approve, re-call `librarian_seed_batch` with:\n\
                     - the **same** `servers` list (any difference will reject),\n\
                     - `confirm_token=\"{token}\"`.\n\n\
                     The token expires in {} minutes and is single-use. \
                     Do NOT commit on your own initiative — wait for explicit user approval.",
                    PENDING_WRITE_TTL_SECS / 60,
                );
                Ok(preview)
            }
            // ----- Commit -----
            Some(token) => {
                let pending = self.consume_token(&token).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Error: `confirm_token` is not recognized, already used, or expired. \
                         Action: re-call `librarian_seed_batch` without `confirm_token` to get \
                         a fresh preview + token. Tokens are single-use and expire after {} minutes.",
                        PENDING_WRITE_TTL_SECS / 60,
                    )
                })?;
                let token_fp = match &pending.action {
                    PendingAction::SeedBatch { fingerprint } => fingerprint,
                    _ => anyhow::bail!(
                        "Error: `confirm_token` was issued for a different action (not a seed batch). \
                         Action: re-call `librarian_seed_batch` without `confirm_token` to get a \
                         batch-specific token."
                    ),
                };
                if &fingerprint != token_fp {
                    anyhow::bail!(
                        "Error: batch content differs from what was proposed and approved. \
                         The user approved a specific list of servers; you are now trying to \
                         commit a different list. Action: re-call without `confirm_token` with \
                         the CURRENT list to get a fresh preview + token, then have the user \
                         approve the new version."
                    );
                }

                let count = p.servers.len();
                let total_tools: usize = p.servers.iter().map(|s| s.tools.len()).sum();

                // All seeds happen under a single write lock so the index file
                // is updated atomically. Partial failure of any one entry
                // shouldn't be possible since seed is in-memory mutation
                // followed by one save.
                lockfile::with_write_lock(&self.paths, || {
                    let mut index = Index::load(&self.paths.cache_file)?;
                    let now = Utc::now();
                    for item in &p.servers {
                        let tools: Vec<IndexedTool> = item
                            .tools
                            .iter()
                            .map(|t| IndexedTool {
                                name: t.name.clone(),
                                description: t.description.clone(),
                                arg_summary: if t.required.is_empty() && t.properties.is_empty() {
                                    None
                                } else {
                                    Some(ArgSummary {
                                        required: t.required.clone(),
                                        properties: t.properties.clone(),
                                    })
                                },
                            })
                            .collect();
                        let entry = ServerEntry {
                            name: item.server.clone(),
                            transport_descriptor: "seeded by agent (batch)".to_string(),
                            probeable: false,
                            probe_status: ProbeStatus::Seeded,
                            indexed_at: now,
                            tools,
                            summary: item.summary.clone(),
                            category: item.category.clone(),
                        };
                        index.servers.insert(item.server.clone(), entry);
                    }
                    index.save(&self.paths.cache_file)?;
                    Ok(())
                })?;

                Ok(format!(
                    "Seeded {count} server{} ({total_tools} tool{}) in one batch.",
                    if count == 1 { "" } else { "s" },
                    if total_tools == 1 { "" } else { "s" },
                ))
            }
        }
    }

    async fn refresh_inner(&self, p: RefreshParams) -> Result<String> {
        if let Some(name) = p.server.as_deref() {
            validate_server_name(name)?;
        }
        let configs = discovery::discover()?;
        // Snapshot the prior index *before* probing for drift detection. We
        // don't hold the lock during probe (it can take seconds per server)
        // — we re-load and merge under the lock once probing completes.
        let prior = Index::load(&self.paths.cache_file)?;

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

        let probed_entries = probe::probe_all(&to_probe).await;

        // Now under the lock: re-load index (catch any concurrent writes),
        // apply our probe results, compute drift, persist notes + index. The
        // re-load means another process's seed/refresh that happened during
        // our probe isn't lost.
        let (probed_count, failed_count, remote_count, drift_note_total, drifted_servers) =
            lockfile::with_write_lock(&self.paths, || {
                let mut index = Index::load(&self.paths.cache_file)?;
                let mut probed_count = 0;
                let mut failed_count = 0;
                let mut remote_count = 0;
                let mut drifted: Vec<(String, Vec<String>)> = Vec::new();

                for entry in probed_entries {
                    if let Some(old) = prior.servers.get(&entry.name) {
                        let mut drifted_tools = Vec::new();
                        for new_tool in &entry.tools {
                            if let Some(old_tool) =
                                old.tools.iter().find(|t| t.name == new_tool.name)
                                && Index::arg_shape_drifted(
                                    &old_tool.arg_summary,
                                    &new_tool.arg_summary,
                                )
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
                Ok((
                    probed_count,
                    failed_count,
                    remote_count,
                    drift_note_total,
                    drifted.len(),
                ))
            })?;

        Ok(format!(
            "refreshed: {probed_count} probed, {failed_count} failed, {remote_count} remote (not probed). \
             drift flags set on {drift_note_total} notes across {drifted_servers} servers.",
        ))
    }
}

// =================== Manifest write ===================

impl LibrarianServer {
    fn manifest_write_inner(&self, p: ManifestWriteParams) -> Result<String> {
        validate_server_name(&p.server)?;
        if let Some(toml_str) = p.manifest_toml.as_deref()
            && toml_str.len() > MAX_MANIFEST_TOML_BYTES
        {
            anyhow::bail!(
                "Error: `manifest_toml` is {} bytes; cap is {} bytes. \
                 Action: real manifests for the busiest known servers land under 16 KiB. \
                 If you're hitting 256 KiB you likely embedded raw content that belongs \
                 elsewhere (a `librarian_note`, a `topic.body` excerpt, or no manifest at all). \
                 Trim and retry.",
                toml_str.len(),
                MAX_MANIFEST_TOML_BYTES,
            );
        }
        // Guard 0: exactly one input form must be provided. Parse TOML if given;
        // otherwise use the structured value. TOML is preferred because nested JSON
        // with embedded newlines causes some MCP clients to hang during serialization.
        let manifest: Manifest = match (p.manifest_toml.as_deref(), p.manifest) {
            (Some(_), Some(_)) => anyhow::bail!(
                "Error: provide either `manifest_toml` (preferred) or `manifest`, not both. \
                 Action: pick one. `manifest_toml` accepts a single TOML string and is the \
                 recommended path for non-trivial content."
            ),
            (Some(toml_str), None) => {
                let manifest = toml::from_str::<Manifest>(toml_str).map_err(|e| {
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
                })?;
                // Catch the silent-data-loss footgun: the TOML parsed cleanly,
                // but a `gotchas` key was misplaced under a sub-table and got
                // scoped there instead of root. The struct field is empty but
                // the source contained the data — silently dropping it would
                // commit an incomplete manifest. Reject loudly.
                if let Some(misplaced) = detect_misplaced_gotchas(toml_str) {
                    anyhow::bail!(
                        "Error: `gotchas` key was misplaced in your TOML and got silently dropped \
                         by TOML's table-scoping rules. {misplaced} \
                         Action: move the `gotchas = [...]` array to BEFORE the first `[section]` \
                         or `[[section]]` header in your TOML. Root-level keys must appear before \
                         any table header. See `librarian_help(\"librarian\", \"manifest_schema\")` \
                         for the canonical ordering."
                    );
                }
                manifest
            }
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
                    PendingAction::Write {
                        fingerprint,
                        overwrite,
                    } => (fingerprint, *overwrite),
                    PendingAction::Restore
                    | PendingAction::SeedBatch { .. }
                    | PendingAction::SeedRemove => anyhow::bail!(
                        "Error: `confirm_token` was issued for a different action (not a manifest write). \
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
                // All checks passed. The existence re-check + write must be
                // atomic across processes — otherwise another writer can create
                // a manifest between our check and our write, defeating the
                // overwrite=false guard. Lock-protected.
                lockfile::with_write_lock(&self.paths, || {
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
                    playbook::write_manifest(&self.paths, &p.server, &manifest)
                })?;
                Ok(format!(
                    "Committed manifest for `{}` → `{}` ({} categor{}, {} workflow{}, {} topic{}, {} gotcha{}).",
                    p.server,
                    target.display(),
                    manifest.tool_categories.len(),
                    if manifest.tool_categories.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    manifest.workflows.len(),
                    if manifest.workflows.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
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
        validate_server_name(&p.server)?;
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
                let (curr_t, bak_t) = playbook::manifest_mtimes(&self.paths, &p.server);
                let (curr_label, bak_label) = playbook::format_mtime_pair(curr_t, bak_t);
                let mut out = String::new();
                let _ = writeln!(out, "# Manifest diff for `{}`", p.server);
                let _ = writeln!(
                    out,
                    "\n**Backup → Current** (what `librarian_manifest_write` changed)"
                );
                let _ = writeln!(out, "- Backup:  {bak_label}");
                let _ = writeln!(out, "- Current: {curr_label}");
                // After a `librarian_manifest_restore` the backup is the file with
                // the newer mtime — the diff direction is the same, but what looks
                // like an "added" change is what restore would re-undo. Surface this
                // explicitly so it can't be misread.
                if matches!((curr_t, bak_t), (Some(c), Some(b)) if b > c) {
                    out.push_str(
                        "\n> *Note: backup is newer than current — looks like \
                         a restore just happened. Items listed below are what a \
                         second `librarian_manifest_restore` would re-introduce.*\n",
                    );
                }
                out.push('\n');
                out.push_str(&playbook::diff_manifests(&bak, &curr));
                out.push_str(
                    "\n---\n*To revert these changes, use `librarian_manifest_restore`.*\n",
                );
                Ok(out)
            }
        }
    }

    fn manifest_restore_inner(&self, p: ManifestRestoreParams) -> Result<String> {
        validate_server_name(&p.server)?;
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
                        let (curr_t, bak_t) = playbook::manifest_mtimes(&self.paths, &p.server);
                        let (curr_label, bak_label) = playbook::format_mtime_pair(curr_t, bak_t);
                        preview.push_str("### What will change (current → backup)\n\n");
                        let _ = writeln!(preview, "- Current: {curr_label}");
                        let _ = writeln!(preview, "- Backup:  {bak_label}");
                        preview.push('\n');
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
                // Lock-protected: restore involves a read-then-two-writes
                // sequence that would interleave badly with a concurrent write.
                lockfile::with_write_lock(&self.paths, || {
                    playbook::restore_manifest(&self.paths, &p.server)
                })?;
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
        if p.extra_urls.len() > MAX_EXTRA_URLS {
            anyhow::bail!(
                "Error: `extra_urls` has {} entries; cap is {}. \
                 Action: fetch in batches. The cap exists because each URL costs a network \
                 round trip and up to 5 MiB of response memory; uncapped, a single call could \
                 stall for minutes.",
                p.extra_urls.len(),
                MAX_EXTRA_URLS,
            );
        }
        let max_chars = p.max_chars.unwrap_or(fetch::DEFAULT_MAX_CHARS);
        let mut out = String::new();

        let urls: Vec<String> = std::iter::once(p.url.clone())
            .chain(p.extra_urls)
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

/// Natural-language stop words that produce noise in tool-search ranking
/// (they appear inside unrelated tool names/descriptions and dominate the
/// score for queries like "files in a repo"). The list is intentionally
/// short — only the highest-noise triggers — to avoid over-filtering
/// technical queries.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "in", "of", "on", "at", "to", "for", "with", "by", "from", "and", "or",
    "but", "is", "are", "was", "were", "be", "been", "this", "that", "these", "those", "it", "its",
    "show", "find", "get", "want", "need", "me", "my", "you", "your", "which", "what", "when",
    "where", "who", "why", "how", "can", "could", "should", "would", "will",
];

/// Canonical fingerprint of a seed batch — used to verify that a commit-mode
/// call carries the exact list the user approved. Same pattern as
/// `manifest_fingerprint`: serialize to JSON and compare strings.
fn seed_batch_fingerprint(items: &[SeedParams]) -> String {
    serde_json::to_string(items).unwrap_or_default()
}

/// Scan a raw TOML string for `gotchas` keys that landed inside a sub-table or
/// array-of-tables element instead of at the document root. Returns a
/// descriptive message naming the offending parent (e.g. "found inside [meta]"
/// or "found inside [[topics]] item 2"), or None if there's nothing misplaced.
///
/// Background: in TOML, once a `[section]` or `[[section]]` header opens, every
/// subsequent key belongs to that section until another header arrives. There's
/// no way to "close" a section and return to root. So writing `gotchas = [...]`
/// after `[meta]` silently scopes the array to the meta table — and our
/// `Manifest` struct expects gotchas at root, so it ends up as an empty Vec
/// with no parse error. This detector is the loud-failure version of that
/// silent acceptance.
fn detect_misplaced_gotchas(toml_str: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(toml_str).ok()?;
    let toml::Value::Table(root) = value else {
        return None;
    };
    let mut found: Vec<String> = Vec::new();
    for (key, sub) in &root {
        if key == "gotchas" {
            continue; // correctly placed at root
        }
        match sub {
            toml::Value::Table(t) if t.contains_key("gotchas") => {
                found.push(format!("found inside `[{key}]`"));
            }
            toml::Value::Array(arr) => {
                for (i, item) in arr.iter().enumerate() {
                    if let toml::Value::Table(t) = item
                        && t.contains_key("gotchas")
                    {
                        found.push(format!("found inside `[[{key}]]` item {i}"));
                    }
                }
            }
            _ => {}
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found.join("; "))
    }
}

/// Conservative ASCII normalization for content that flows verbatim into a
/// propose-preview response. Claude Desktop has been observed to hang on
/// previews containing em-dashes (U+2014) and en-dashes (U+2013) embedded in
/// otherwise-normal text — its incremental markdown renderer appears to wait
/// for tokens it never receives. Other MCP clients (Codex, Claude Code) are
/// not affected. Cheap to apply defensively at the boundary.
///
/// Strips: em-dash → hyphen, en-dash → hyphen, horizontal ellipsis → "...",
/// non-breaking space → space. Leaves regular Unicode (accented letters etc.)
/// untouched. Returns None if input was None.
fn sanitize_for_preview(s: Option<&str>) -> Option<String> {
    s.map(|raw| {
        raw.chars()
            .map(|c| match c {
                '\u{2014}' | '\u{2013}' => "-".to_string(),
                '\u{2026}' => "...".to_string(),
                '\u{00A0}' => " ".to_string(),
                other => other.to_string(),
            })
            .collect()
    })
}

/// Normalize a note claim for duplicate detection. Collapses internal
/// whitespace, trims, and lowercases. Two claims with the same prose
/// content but different casing or stray double-spaces compare equal.
fn normalize_claim(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn is_meaningful_token(t: &str) -> bool {
    // Filter: must be ≥3 chars (eliminates "a", "an", "in", "of", "to", "or"
    // mid-word matches) AND not in the stop-word list. Short technical terms
    // like "ci" or "pr" do get filtered — acceptable; users searching for
    // those will typically include more context.
    t.len() >= 3 && !STOP_WORDS.iter().any(|s| s.eq_ignore_ascii_case(t))
}

/// Stricter cousin of `is_meaningful_token` for ALIAS PHRASES. Keeps
/// wh-words ("where", "what", "how") and action verbs ("find", "get",
/// "locate", "show") because those are precisely the intent signals an
/// author curates an alias around — filtering them via the full STOP_WORDS
/// list strips the phrase to nothing. Drops only pure grammatical filler.
fn is_meaningful_phrase_token(t: &str) -> bool {
    if t.len() < 3 {
        return false;
    }
    !matches!(
        t.to_lowercase().as_str(),
        "the"
            | "this"
            | "that"
            | "these"
            | "those"
            | "and"
            | "but"
            | "for"
            | "with"
            | "from"
            | "are"
            | "was"
            | "were"
            | "been"
            | "has"
            | "had"
            | "have"
            | "you"
            | "your"
            | "its"
    )
}

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
    // Tool-aliases: phrases authored to make a tool findable by intent
    // even when its name/description don't carry the intent's words.
    // For the manifest-only-server search pass these flow into the
    // catch-all haystack; for indexed servers, aliases get a per-tool
    // boost in the rank function via `collect_aliases_for_tool` below.
    for alias in &m.tool_aliases {
        for phrase in &alias.phrases {
            s.push_str(phrase);
            s.push(' ');
        }
    }
    s
}

/// Collect alias phrases attached to `tool_name` from a manifest. Returned
/// as `Vec<String>` (one entry per phrase) so callers can both iterate
/// per-phrase for structured scoring (see `phrase_overlap_bonus`) and
/// `.join(" ")` to recover the flat haystack the existing `rank()` expects.
/// Returns an empty Vec if no manifest, no aliases for this tool, or the
/// matching aliases entries have no phrases.
fn collect_alias_phrases_for_tool(manifest: Option<&Manifest>, tool_name: &str) -> Vec<String> {
    let Some(m) = manifest else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for alias in &m.tool_aliases {
        if alias.tool == tool_name {
            for phrase in &alias.phrases {
                out.push(phrase.clone());
            }
        }
    }
    out
}

/// Phrase-level overlap bonus. Awarded on top of the haystack-level scoring
/// in `rank()` so a phrase whose meaningful tokens densely match the query
/// outranks tools that only share a single common token via their name.
///
/// A phrase must have ≥2 meaningful tokens AND at least 2 of them must
/// appear in the query (≥50% overlap) for the bonus to fire. This is
/// the right calibration for the alias-vs-name-collision failure mode:
/// an alias phrase like `"where is X defined"` (meaningful tokens after
/// stop-word/placeholder filtering: [`where`, `defined`]) fully matched
/// against the query lifts the tool above unrelated single-name-token
/// matches — without false-positive boosting on incidental single-token
/// overlap, which the per-token `rank()` arm already weights at +15.
///
/// Each qualifying phrase contributes `matched_tokens * 20`, capped at +80
/// per phrase so a single long phrase can't dominate; multiple qualifying
/// phrases sum.
fn phrase_overlap_bonus(phrases: &[String], query: &str) -> i64 {
    let q_lower = query.to_lowercase();
    let mut bonus: i64 = 0;
    for phrase in phrases {
        let phrase_lower = phrase.to_lowercase();
        let phrase_tokens: Vec<&str> = phrase_lower
            .split_whitespace()
            .filter(|t| is_meaningful_phrase_token(t))
            .collect();
        if phrase_tokens.len() < 2 {
            continue;
        }
        let matched: usize = phrase_tokens
            .iter()
            .filter(|t| q_lower.contains(*t))
            .count();
        if matched < 2 {
            continue;
        }
        let fraction = matched as f32 / phrase_tokens.len() as f32;
        if fraction >= 0.5 {
            bonus += (matched as i64 * 20).min(80);
        }
    }
    bonus
}

fn rank(query: &str, query_tokens: &[&str], name: &str, description: &str, aliases: &str) -> i64 {
    let name_lower = name.to_lowercase();
    let desc_lower = description.to_lowercase();
    let aliases_lower = aliases.to_lowercase();
    let mut score: i64 = 0;
    // Whole-query substring matches are highest signal
    if name_lower.contains(query) {
        score += 100;
    }
    // Curated intent phrases beat auto-descriptions but lose to the tool's
    // own name. Sized so a manifest with a literal-phrase match outranks
    // an unrelated tool whose name happens to contain a common search term.
    if !aliases_lower.is_empty() && aliases_lower.contains(query) {
        score += 60;
    }
    if desc_lower.contains(query) {
        score += 30;
    }
    // Token overlap — only meaningful tokens contribute to avoid false
    // positives from short common words ("in", "a") substring-matching
    // unrelated tool names.
    for token in query_tokens {
        if !is_meaningful_token(token) {
            continue;
        }
        if name_lower.contains(token) {
            score += 25;
        }
        if !aliases_lower.is_empty() && aliases_lower.contains(token) {
            score += 15;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;
    use tempfile::TempDir;

    fn test_paths() -> (TempDir, Paths) {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        let paths = Paths {
            cache_dir: base.join("cache"),
            config_dir: base.join("config"),
            manifest_dir: base.join("config/manifests"),
            learned_dir: base.join("data/learned"),
            cache_file: base.join("cache/index.json"),
            docs_cache_dir: base.join("cache/docs"),
        };
        paths.ensure_dirs().unwrap();
        (dir, paths)
    }

    #[test]
    fn normalize_claim_handles_whitespace_and_case() {
        assert_eq!(
            normalize_claim("  Foo   bar BAZ "),
            normalize_claim("foo bar baz")
        );
        assert_eq!(normalize_claim("Foo"), "foo");
        assert_eq!(normalize_claim("a  b"), "a b");
    }

    #[test]
    fn note_dedup_rejects_equivalent_repeat() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let mk = |claim: &str| NoteParams {
            server: "demo".into(),
            tool: Some("demo_tool".into()),
            topic: None,
            kind: NoteKind::Tip,
            basis: NoteBasis::Observed,
            claim: claim.into(),
            tags: vec![],
            allow_duplicate: false,
        };
        // First note: accepted.
        server.note_inner(mk("API key must be set")).unwrap();
        // Same claim normalized (whitespace, case): rejected.
        let err = server.note_inner(mk("api  key  MUST be set")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate note") && msg.contains("allow_duplicate=true"),
            "expected dedup error mentioning the escape hatch, got: {msg}"
        );
    }

    #[test]
    fn note_dedup_distinguishes_by_kind_and_tool() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        // Same claim text, different kind → both accepted.
        server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: Some("foo".into()),
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: "shared text".into(),
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap();
        server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: Some("foo".into()),
                topic: None,
                kind: NoteKind::Behavior,
                basis: NoteBasis::Observed,
                claim: "shared text".into(),
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap();
        // Same kind, different tool → also accepted.
        server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: Some("bar".into()),
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: "shared text".into(),
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap();
    }

    #[test]
    fn detect_misplaced_gotchas_catches_under_meta() {
        let bad = r#"
[meta]
category = "comms"

gotchas = ["this gets scoped to meta"]
"#;
        let found = detect_misplaced_gotchas(bad);
        assert!(found.is_some(), "should detect gotchas under [meta]");
        assert!(found.unwrap().contains("[meta]"));
    }

    #[test]
    fn detect_misplaced_gotchas_catches_under_array_of_tables() {
        let bad = r#"
[[topics]]
name = "auth"
title = "Auth"
body = "..."

gotchas = ["this gets scoped to topics[0]"]
"#;
        let found = detect_misplaced_gotchas(bad);
        assert!(found.is_some(), "should detect gotchas under [[topics]]");
        let msg = found.unwrap();
        assert!(msg.contains("[[topics]]"));
        assert!(msg.contains("item 0"));
    }

    #[test]
    fn detect_misplaced_gotchas_passes_correct_ordering() {
        let good = r#"
gotchas = ["item 1", "item 2"]

[meta]
category = "comms"

[[topics]]
name = "auth"
title = "Auth"
body = "..."
"#;
        assert!(
            detect_misplaced_gotchas(good).is_none(),
            "correctly placed gotchas should not be flagged"
        );
    }

    #[test]
    fn detect_misplaced_gotchas_passes_when_absent() {
        let no_gotchas = r#"
[meta]
category = "comms"
summary = "..."

[[topics]]
name = "auth"
title = "Auth"
body = "..."
"#;
        assert!(detect_misplaced_gotchas(no_gotchas).is_none());
    }

    #[test]
    fn manifest_write_rejects_misplaced_gotchas() {
        // End-to-end: the agent submits TOML with gotchas under [meta]. We
        // catch it before issuing a token. The user is told what's wrong.
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let bad_toml = r#"[meta]
category = "comms"
summary = "test"

gotchas = ["this is lost"]
"#;
        let err = server
            .manifest_write_inner(ManifestWriteParams {
                server: "demo".into(),
                manifest_toml: Some(bad_toml.into()),
                manifest: None,
                confirm_token: None,
                overwrite: false,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("misplaced") || msg.contains("silently dropped"),
            "should mention the silent-drop failure mode: {msg}"
        );
        assert!(
            msg.contains("[meta]"),
            "should name the offending parent: {msg}"
        );
        assert!(
            msg.contains("manifest_schema"),
            "should point at the schema topic"
        );
    }

    #[test]
    fn manifest_write_accepts_correct_ordering() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let good_toml = r#"gotchas = ["fine"]

[meta]
category = "comms"
summary = "test"
"#;
        // Propose mode — should succeed and return a preview
        let preview = server
            .manifest_write_inner(ManifestWriteParams {
                server: "demo".into(),
                manifest_toml: Some(good_toml.into()),
                manifest: None,
                confirm_token: None,
                overwrite: false,
            })
            .unwrap();
        assert!(preview.contains("MANIFEST WRITE"));
        assert!(preview.contains("Gotchas: 1 entries"));
    }

    #[test]
    fn manifest_preview_shows_zero_counts_for_empty_sections() {
        // The agent could authour a manifest with only meta, no other content.
        // The preview must show "Gotchas: 0 entries" / "Workflows (0):" etc.
        // so an unexpected zero is visible at a glance.
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let toml = r#"[meta]
category = "comms"
summary = "minimal"
"#;
        let preview = server
            .manifest_write_inner(ManifestWriteParams {
                server: "demo".into(),
                manifest_toml: Some(toml.into()),
                manifest: None,
                confirm_token: None,
                overwrite: false,
            })
            .unwrap();
        // Every section should have a count line, even when zero.
        assert!(preview.contains("Tool categories (0):"));
        assert!(preview.contains("Workflows (0):"));
        assert!(preview.contains("Topics (0):"));
        assert!(preview.contains("Gotchas: 0 entries"));
    }

    #[test]
    fn server_instructions_are_directive_about_orientation() {
        // Asserts the session-init instructions string (surfaced to agents by
        // MCP clients at server attach) directs orientation through librarian
        // BEFORE the agent calls any indexed server's tool. This is the
        // load-bearing fix for cold-start discoverability.
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let info = server.get_info();
        let text = info
            .instructions
            .expect("server must surface instructions to MCP clients");
        // Directive opener — the imperative that converts "opt-in" into
        // "first thing the agent does."
        assert!(
            text.contains("Orient before acting"),
            "instructions must open with directive framing: {text}"
        );
        // Primary action.
        assert!(
            text.contains("librarian_help(server)"),
            "must name the primary orientation tool"
        );
        // Other read-surface tools stay surfaced for completeness.
        assert!(text.contains("librarian_list"));
        assert!(text.contains("librarian_search"));
        assert!(text.contains("librarian_note"));
        // Context-budget cap. The instructions ride in every session's init
        // payload; bloat here costs the agent's working budget per session.
        // 600 chars is comfortably more than the current text but stops a
        // future contributor from turning this into a wall of prose.
        assert!(
            text.len() <= 600,
            "instructions exceed 600 chars ({}); keep them tight: {text}",
            text.len()
        );
    }

    #[test]
    fn tool_methods_return_diagnostic_as_content_not_error() {
        // Regression guard: validation failures must surface as Ok(content)
        // so MCP clients that swallow JSON-RPC error.message fields still
        // see the diagnostic. The "Error:" prefix on bail!() messages is the
        // signal to the agent.
        use rmcp::handler::server::wrapper::Parameters;
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);

        // Bad input: gotchas misplaced under [meta]. The bail!() in
        // manifest_write_inner produces "Error: `gotchas` key was misplaced..."
        // The tool method must convert this to Ok(content) so the harness
        // displays it.
        let bad = ManifestWriteParams {
            server: "demo".into(),
            manifest_toml: Some("[meta]\ncategory = \"x\"\n\ngotchas = [\"lost\"]\n".into()),
            manifest: None,
            confirm_token: None,
            overwrite: false,
        };
        let response = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { server.manifest_write(Parameters(bad)).await });
        match response {
            Ok(content) => {
                assert!(
                    content.starts_with("Error:"),
                    "diagnostic should be prefixed with `Error:`, got: {content}"
                );
                assert!(
                    content.contains("misplaced"),
                    "should explain the failure: {content}"
                );
            }
            Err(e) => panic!("validation failure must be returned as Ok(content), not Err: {e:?}"),
        }
    }

    #[test]
    fn sanitize_for_preview_strips_em_dashes_and_friends() {
        assert_eq!(
            sanitize_for_preview(Some("foo — bar")).unwrap(),
            "foo - bar"
        );
        assert_eq!(sanitize_for_preview(Some("a–b")).unwrap(), "a-b",);
        assert_eq!(sanitize_for_preview(Some("yes…")).unwrap(), "yes...",);
        // Non-breaking space → regular space
        assert_eq!(
            sanitize_for_preview(Some("foo\u{00A0}bar")).unwrap(),
            "foo bar",
        );
        // ASCII passes through untouched
        assert_eq!(
            sanitize_for_preview(Some("foo - bar")).unwrap(),
            "foo - bar",
        );
        // Regular Unicode (accents, emoji) is preserved
        assert_eq!(sanitize_for_preview(Some("café 🎉")).unwrap(), "café 🎉",);
        assert_eq!(sanitize_for_preview(None), None);
    }

    #[test]
    fn seed_remove_preview_has_no_known_hang_triggers() {
        // Regression guard: the seed_remove preview must not contain the
        // character/markup combinations that hung Claude Desktop's renderer
        // (em-dashes, angle-bracket placeholders, backticks around content
        // with punctuation). If a future edit reintroduces any of these,
        // this test catches it before deploy.
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        // Seed an entry with an em-dash in its summary to ensure the
        // sanitizer pipeline strips it from the rendered preview.
        server
            .seed_inner(SeedParams {
                server: "claude.ai_Probe".into(),
                summary: Some("Probe MCP via claude.ai mediator — exposes auth tools.".into()),
                category: Some("comms".into()),
                tools: vec![SeedTool {
                    name: "auth".into(),
                    description: "".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            })
            .unwrap();

        let preview = server
            .seed_remove_inner(SeedRemoveParams {
                server: "claude.ai_Probe".into(),
                confirm_token: None,
            })
            .unwrap();

        assert!(
            !preview.contains('\u{2014}'),
            "em-dash in preview: {preview}"
        );
        assert!(!preview.contains('\u{2013}'), "en-dash in preview");
        assert!(
            !preview.contains("<data>") && !preview.contains("<server>"),
            "angle-bracket placeholders in preview: {preview}"
        );
        // Confirm the actual content still serves its purpose
        assert!(preview.contains("SEED REMOVAL"));
        assert!(preview.contains("claude.ai_Probe"));
        assert!(preview.contains("confirm_token="));
    }

    #[test]
    fn seed_remove_round_trip() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        // Seed an entry to remove
        server
            .seed_inner(SeedParams {
                server: "stale_server".into(),
                summary: Some("to be removed".into()),
                category: Some("comms".into()),
                tools: vec![],
            })
            .unwrap();
        assert!(
            Index::load(&paths.cache_file)
                .unwrap()
                .servers
                .contains_key("stale_server")
        );

        // Propose
        let preview = server
            .seed_remove_inner(SeedRemoveParams {
                server: "stale_server".into(),
                confirm_token: None,
            })
            .unwrap();
        assert!(preview.contains("SEED REMOVAL"));
        assert!(preview.contains("stale_server"));
        let token = extract_token(&preview);

        // Commit
        let msg = server
            .seed_remove_inner(SeedRemoveParams {
                server: "stale_server".into(),
                confirm_token: Some(token),
            })
            .unwrap();
        assert!(msg.contains("Removed"));
        assert!(
            !Index::load(&paths.cache_file)
                .unwrap()
                .servers
                .contains_key("stale_server")
        );
    }

    #[test]
    fn seed_remove_rejects_unknown_server() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let err = server
            .seed_remove_inner(SeedRemoveParams {
                server: "never_seeded".into(),
                confirm_token: None,
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("not in the librarian index"));
    }

    #[test]
    fn seed_remove_warns_when_manifest_exists() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        // Seed an entry
        server
            .seed_inner(SeedParams {
                server: "with_manifest".into(),
                summary: Some("seeded".into()),
                category: None,
                tools: vec![],
            })
            .unwrap();
        // And write a manifest for the same name
        crate::playbook::write_manifest(
            &paths,
            "with_manifest",
            &Manifest {
                meta: crate::playbook::ManifestMeta {
                    summary: Some("real manifest".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let preview = server
            .seed_remove_inner(SeedRemoveParams {
                server: "with_manifest".into(),
                confirm_token: None,
            })
            .unwrap();
        assert!(
            preview.contains("manifest file exists"),
            "should warn about manifest re-surfacing: {preview}"
        );
    }

    #[test]
    fn seed_remove_rejects_cross_server_token() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        // Seed two entries
        for name in ["aaa", "bbb"] {
            server
                .seed_inner(SeedParams {
                    server: name.into(),
                    summary: None,
                    category: None,
                    tools: vec![],
                })
                .unwrap();
        }
        // Propose removal of "aaa"
        let propose = server
            .seed_remove_inner(SeedRemoveParams {
                server: "aaa".into(),
                confirm_token: None,
            })
            .unwrap();
        let token = extract_token(&propose);
        // Try to use that token to commit removal of "bbb" — must reject.
        let err = server
            .seed_remove_inner(SeedRemoveParams {
                server: "bbb".into(),
                confirm_token: Some(token),
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("issued for server"));
    }

    #[test]
    fn seed_batch_propose_returns_preview_and_token() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let params = SeedBatchParams {
            servers: vec![
                SeedParams {
                    server: "claude.ai_Foo".into(),
                    summary: Some("Foo MCP".into()),
                    category: Some("comms".into()),
                    tools: vec![SeedTool {
                        name: "foo_send".into(),
                        description: "send a foo".into(),
                        required: vec![],
                        properties: Default::default(),
                    }],
                },
                SeedParams {
                    server: "claude.ai_Bar".into(),
                    summary: Some("Bar MCP".into()),
                    category: Some("knowledge".into()),
                    tools: vec![],
                },
            ],
            confirm_token: None,
        };
        let preview = server.seed_batch_inner(params).unwrap();
        assert!(preview.contains("SEED BATCH"));
        assert!(preview.contains("claude.ai_Foo"));
        assert!(preview.contains("claude.ai_Bar"));
        assert!(preview.contains("Total: 2 servers"));
        assert!(preview.contains("confirm_token="));
    }

    fn extract_token(preview: &str) -> String {
        let line = preview
            .lines()
            .find(|l| l.contains("confirm_token=\""))
            .expect("token line");
        let start = line.find("confirm_token=\"").unwrap() + 15;
        let end = line[start..].find('"').unwrap() + start;
        line[start..end].to_string()
    }

    #[test]
    fn seed_batch_commit_persists_all_servers() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        let payload = vec![
            SeedParams {
                server: "claude.ai_One".into(),
                summary: Some("One".into()),
                category: Some("comms".into()),
                tools: vec![SeedTool {
                    name: "x_y".into(),
                    description: "".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            },
            SeedParams {
                server: "claude.ai_Two".into(),
                summary: Some("Two".into()),
                category: Some("knowledge".into()),
                tools: vec![],
            },
        ];
        let propose = server
            .seed_batch_inner(SeedBatchParams {
                servers: payload.clone(),
                confirm_token: None,
            })
            .unwrap();
        let token = extract_token(&propose);

        let commit_msg = server
            .seed_batch_inner(SeedBatchParams {
                servers: payload,
                confirm_token: Some(token),
            })
            .unwrap();
        assert!(commit_msg.contains("Seeded 2 servers"));

        let index = Index::load(&paths.cache_file).unwrap();
        assert!(index.servers.contains_key("claude.ai_One"));
        assert!(index.servers.contains_key("claude.ai_Two"));
    }

    #[test]
    fn seed_batch_rejects_tampered_content() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let original = vec![SeedParams {
            server: "claude.ai_X".into(),
            summary: Some("X".into()),
            category: None,
            tools: vec![],
        }];
        let propose = server
            .seed_batch_inner(SeedBatchParams {
                servers: original,
                confirm_token: None,
            })
            .unwrap();
        let token = extract_token(&propose);

        // Submit a DIFFERENT list with the same token — fingerprint should reject.
        let tampered = vec![SeedParams {
            server: "claude.ai_Different".into(),
            summary: Some("Different".into()),
            category: None,
            tools: vec![],
        }];
        let err = server
            .seed_batch_inner(SeedBatchParams {
                servers: tampered,
                confirm_token: Some(token),
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("differs from what was proposed"));
    }

    #[test]
    fn seed_batch_rejects_duplicate_names_within_batch() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let err = server
            .seed_batch_inner(SeedBatchParams {
                servers: vec![
                    SeedParams {
                        server: "dup".into(),
                        summary: None,
                        category: None,
                        tools: vec![],
                    },
                    SeedParams {
                        server: "dup".into(),
                        summary: None,
                        category: None,
                        tools: vec![],
                    },
                ],
                confirm_token: None,
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("appears more than once"));
    }

    #[test]
    fn seed_batch_rejects_path_traversal_in_any_item() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let err = server
            .seed_batch_inner(SeedBatchParams {
                servers: vec![
                    SeedParams {
                        server: "valid".into(),
                        summary: None,
                        category: None,
                        tools: vec![],
                    },
                    SeedParams {
                        server: "../escape".into(),
                        summary: None,
                        category: None,
                        tools: vec![],
                    },
                ],
                confirm_token: None,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("starts with `.`") || msg.contains("disallowed character"));
    }

    #[test]
    fn seed_batch_propose_flags_collisions_with_existing_entries() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        // Pre-seed an existing entry.
        server
            .seed_inner(SeedParams {
                server: "claude.ai_Existing".into(),
                summary: Some("existing".into()),
                category: Some("comms".into()),
                tools: vec![],
            })
            .unwrap();
        // Now propose a batch that collides with it.
        let preview = server
            .seed_batch_inner(SeedBatchParams {
                servers: vec![
                    SeedParams {
                        server: "claude.ai_Existing".into(),
                        summary: Some("new content".into()),
                        category: Some("comms".into()),
                        tools: vec![],
                    },
                    SeedParams {
                        server: "claude.ai_New".into(),
                        summary: Some("brand new".into()),
                        category: None,
                        tools: vec![],
                    },
                ],
                confirm_token: None,
            })
            .unwrap();
        assert!(preview.contains("will be OVERWRITTEN"));
        assert!(preview.contains("claude.ai_Existing"));
    }

    #[test]
    fn tools_reject_path_traversal_in_server_name() {
        // Every write-class tool that accepts `server` must reject names that
        // would escape the data directory. We don't enumerate every path; the
        // dedicated validate_server_name tests cover that. Here we just check
        // that the wiring is in place at each tool entry.
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let bad = "../escape".to_string();

        // librarian_note
        let err = server
            .note_inner(NoteParams {
                server: bad.clone(),
                tool: None,
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: "x".into(),
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );

        // librarian_seed_playbook
        let err = server
            .seed_inner(SeedParams {
                server: bad.clone(),
                summary: None,
                category: None,
                tools: vec![],
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );

        // librarian_manifest_write
        let err = server
            .manifest_write_inner(ManifestWriteParams {
                server: bad.clone(),
                manifest_toml: Some("[meta]\nsummary=\"x\"\n".into()),
                manifest: None,
                confirm_token: None,
                overwrite: false,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );

        // librarian_manifest_diff
        let err = server
            .manifest_diff_inner(ManifestDiffParams {
                server: bad.clone(),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );

        // librarian_manifest_restore
        let err = server
            .manifest_restore_inner(ManifestRestoreParams {
                server: bad.clone(),
                confirm_token: None,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );

        // librarian_help — should reject too (read tool but still uses paths).
        let err = server
            .help_inner(HelpParams {
                server: bad.clone(),
                topic: None,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("starts with `.`") || msg.contains("disallowed character"),
            "expected server-name rejection, got: {msg}"
        );
    }

    #[test]
    fn concurrent_dedup_wins_exactly_once() {
        // Two threads file the same note simultaneously. Without the write
        // lock, both would read-then-write past the dedup check. With it,
        // one succeeds first and the other sees the existing note and
        // rejects. Exactly one Ok, exactly one Err.
        let (_tmp, paths) = test_paths();
        let server = Arc::new(LibrarianServer::new(paths.clone()));
        let barrier = Arc::new(Barrier::new(2));

        let h1 = {
            let server = server.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                server.note_inner(NoteParams {
                    server: "demo".into(),
                    tool: None,
                    topic: None,
                    kind: NoteKind::Tip,
                    basis: NoteBasis::Observed,
                    claim: "shared observation".into(),
                    tags: vec![],
                    allow_duplicate: false,
                })
            })
        };
        let h2 = {
            let server = server.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                server.note_inner(NoteParams {
                    server: "demo".into(),
                    tool: None,
                    topic: None,
                    kind: NoteKind::Tip,
                    basis: NoteBasis::Observed,
                    claim: "shared observation".into(),
                    tags: vec![],
                    allow_duplicate: false,
                })
            })
        };

        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let errs = [&r1, &r2].iter().filter(|r| r.is_err()).count();
        assert_eq!(
            oks, 1,
            "exactly one of the concurrent writers should succeed; got r1={r1:?} r2={r2:?}"
        );
        assert_eq!(errs, 1, "the other should be rejected with a dedup error");

        // Verify only one note actually landed on disk.
        let notes = index::read_notes(&paths, "demo").unwrap();
        assert_eq!(notes.len(), 1, "should have exactly one note in storage");
    }

    #[test]
    fn concurrent_manifest_writes_serialize() {
        // Two threads write a manifest for the same server. With the lock,
        // they serialize: final state matches one of the two writers exactly,
        // and the backup (if any) is the OTHER writer's content — not a
        // half-written file.
        let (_tmp, paths) = test_paths();
        let server = Arc::new(LibrarianServer::new(paths.clone()));
        let barrier = Arc::new(Barrier::new(2));

        let make_params = |summary: &str| ManifestWriteParams {
            server: "demo".into(),
            manifest_toml: Some(format!(
                "[meta]\ncategory = \"data\"\nsummary  = \"{summary}\"\n"
            )),
            manifest: None,
            confirm_token: None,
            overwrite: true,
        };

        // Pre-create a manifest so both writers go through the overwrite path
        // (without this they'd hit the overwrite=false error on the propose
        // before we even get to the lock).
        crate::playbook::write_manifest(
            &paths,
            "demo",
            &Manifest {
                meta: crate::playbook::ManifestMeta {
                    summary: Some("initial".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        // Two-stage dance: each thread proposes, then commits with the token.
        let run = |label: &'static str| {
            let server = server.clone();
            let barrier = barrier.clone();
            let p = make_params(label);
            thread::spawn(move || -> Result<(String, &'static str), anyhow::Error> {
                barrier.wait();
                let propose = server.manifest_write_inner(p.clone())?;
                // Extract the token from the propose response.
                let token_line = propose
                    .lines()
                    .find(|l| l.contains("confirm_token=\""))
                    .ok_or_else(|| anyhow::anyhow!("no token in: {propose}"))?;
                let start = token_line.find("confirm_token=\"").unwrap() + 15;
                let end = token_line[start..].find('"').unwrap() + start;
                let token = token_line[start..end].to_string();

                let mut commit = p.clone();
                commit.confirm_token = Some(token);
                server.manifest_write_inner(commit).map(|s| (s, label))
            })
        };

        let h1 = run("alpha");
        let h2 = run("beta");
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert!(
            r1.is_ok() && r2.is_ok(),
            "both writers should commit: r1={r1:?} r2={r2:?}"
        );

        // Final manifest content must match exactly one of the writers
        // (no half-written or merged state).
        let final_m = crate::playbook::load_manifest(&paths, "demo")
            .unwrap()
            .unwrap();
        let summary = final_m.meta.summary.unwrap();
        assert!(
            summary == "alpha" || summary == "beta",
            "final summary should be one of the two writers' values, got: {summary}"
        );
    }

    #[test]
    fn lock_file_is_created_on_first_write() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: None,
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: "test".into(),
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap();
        let lock_path = crate::lockfile::lock_path_for(&paths);
        assert!(
            lock_path.exists(),
            "lock file should exist at {}",
            lock_path.display()
        );
    }

    #[test]
    fn note_dedup_allow_duplicate_bypass() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let mk = |allow| NoteParams {
            server: "demo".into(),
            tool: None,
            topic: None,
            kind: NoteKind::Tip,
            basis: NoteBasis::Observed,
            claim: "same exact thing".into(),
            tags: vec![],
            allow_duplicate: allow,
        };
        server.note_inner(mk(false)).unwrap();
        // Repeat with bypass: accepted, even though it's an exact duplicate.
        server.note_inner(mk(true)).unwrap();
    }

    #[test]
    fn stop_words_and_short_tokens_filtered() {
        assert!(!is_meaningful_token("a"));
        assert!(!is_meaningful_token("an"));
        assert!(!is_meaningful_token("in"));
        assert!(!is_meaningful_token("the"));
        assert!(!is_meaningful_token("for"));
        assert!(!is_meaningful_token("which"));
        assert!(is_meaningful_token("files"));
        assert!(is_meaningful_token("repo"));
        assert!(is_meaningful_token("ingest"));
    }

    #[test]
    fn rank_ignores_stop_words_in_query() {
        // "files in a repo" should NOT score against a name/desc that only
        // contains "in" or "a" — those tokens get filtered.
        let q = "files in a repo";
        let tokens: Vec<&str> = q.split_whitespace().collect();
        // "forge_sprint_status" doesn't contain "files" or "repo" — should score 0
        let score = rank(q, &tokens, "forge_sprint_status", "Check sprint state", "");
        assert_eq!(
            score, 0,
            "stop-word and short-token matches must not contribute to score"
        );
    }

    #[test]
    fn rank_credits_meaningful_token_matches() {
        let q = "files in a repo";
        let tokens: Vec<&str> = q.split_whitespace().collect();
        // A description that literally mentions "files" and "repo" should score.
        let score = rank(
            q,
            &tokens,
            "github_search",
            "Search files across the repo",
            "",
        );
        assert!(score > 0, "meaningful tokens should still score");
    }

    #[test]
    fn rank_alias_phrase_boosts_above_unrelated_name_match() {
        // Alias-routing failure mode: a query whose intent words don't
        // match the right tool's name, but DO match an authored alias.
        // The aliased tool must outrank a tool whose name happens to
        // contain a generic search term.
        let q = "where is a function defined";
        let tokens: Vec<&str> = q.split_whitespace().collect();
        // `outline` has no description and no token overlap with the query.
        // But its alias says "find function" / "locate definition" which
        // shares two meaningful tokens with the query ("function", "defined"
        // matches "definition" only as a substring of the alias).
        let aliased_score = rank(
            q,
            &tokens,
            "outline",
            "",
            "find function locate definition where is X defined symbol lookup",
        );
        // `slack_search` has no alias but its name contains "search" — no
        // token overlap with the actual query either, so score should be 0
        // or low.
        let unaliased_score = rank(q, &tokens, "slack_search", "search Slack channels", "");
        assert!(
            aliased_score > unaliased_score,
            "aliased tool ({aliased_score}) must outrank unaliased lexical-noise hit ({unaliased_score})"
        );
        assert!(aliased_score > 0, "aliased tool should score above 0");
    }

    #[test]
    fn search_with_tool_aliases_surfaces_codeview_above_lexical_noise() {
        // End-to-end repro of the alias-routing failure: an intent-style
        // query whose meaningful tokens don't overlap with the right tool's
        // name. With an authored alias on the right tool, search must rank
        // it above unrelated tools whose names happen to contain a token
        // from the query.
        use rmcp::handler::server::wrapper::Parameters;
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());

        // Seed two servers: codeview (with grep + outline) and slack
        // (with a search-named tool that shouldn't win on intent).
        server
            .seed_inner(SeedParams {
                server: "codeview".into(),
                summary: Some("read-only code inspection".into()),
                category: Some("developer-tools".into()),
                tools: vec![
                    SeedTool {
                        name: "grep".into(),
                        description: "regex content search".into(),
                        required: vec![],
                        properties: Default::default(),
                    },
                    SeedTool {
                        name: "outline".into(),
                        description: "symbol-level outline of a single file".into(),
                        required: vec![],
                        properties: Default::default(),
                    },
                ],
            })
            .unwrap();
        server
            .seed_inner(SeedParams {
                server: "slack".into(),
                summary: Some("team chat".into()),
                category: Some("comms".into()),
                tools: vec![SeedTool {
                    name: "slack_search_channels".into(),
                    description: "find channels by name".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            })
            .unwrap();

        // Author a manifest for codeview aliasing `outline` to intent phrases
        // that DO share tokens with the failing query.
        crate::playbook::write_manifest(
            &paths,
            "codeview",
            &Manifest {
                tool_aliases: vec![crate::playbook::ToolAlias {
                    tool: "outline".into(),
                    phrases: vec![
                        "find function".into(),
                        "locate definition".into(),
                        "where is X defined".into(),
                        "symbol lookup".into(),
                    ],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        // Run the failing query through the public search tool method.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(async {
                server
                    .search(Parameters(SearchParams {
                        query: "search code for where a function is defined".into(),
                        limit: Some(10),
                    }))
                    .await
            })
            .unwrap();

        // codeview/outline should appear BEFORE any slack_* tool.
        let outline_pos = result
            .find("codeview / outline")
            .expect("codeview/outline should be in results");
        let slack_pos = result.find("slack_search_channels");
        if let Some(sp) = slack_pos {
            assert!(
                outline_pos < sp,
                "codeview/outline must rank above slack search:\n{result}"
            );
        }
    }

    #[test]
    fn search_aliases_inert_when_query_doesnt_match_phrases() {
        // Aliases must boost ONLY when the query content actually overlaps
        // with a phrase. A slack query shouldn't surface codeview just
        // because codeview HAS some aliases.
        use rmcp::handler::server::wrapper::Parameters;
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        server
            .seed_inner(SeedParams {
                server: "codeview".into(),
                summary: Some("code".into()),
                category: Some("dev".into()),
                tools: vec![SeedTool {
                    name: "outline".into(),
                    description: "".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            })
            .unwrap();
        server
            .seed_inner(SeedParams {
                server: "slack".into(),
                summary: Some("chat".into()),
                category: Some("comms".into()),
                tools: vec![SeedTool {
                    name: "slack_send_message".into(),
                    description: "post a message to a channel".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            })
            .unwrap();
        crate::playbook::write_manifest(
            &paths,
            "codeview",
            &Manifest {
                tool_aliases: vec![crate::playbook::ToolAlias {
                    tool: "outline".into(),
                    phrases: vec!["find function".into()],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(async {
                server
                    .search(Parameters(SearchParams {
                        query: "post message to slack channel".into(),
                        limit: Some(10),
                    }))
                    .await
            })
            .unwrap();

        let slack_pos = result.find("slack_send_message");
        if let Some(sp) = slack_pos {
            // Codeview entries may or may not be present; if present they
            // must rank below slack.
            if let Some(cv_pos) = result.find("codeview / outline") {
                assert!(
                    sp < cv_pos,
                    "slack must rank above codeview when query is slack-shaped:\n{result}"
                );
            }
        }
    }

    #[test]
    fn aliases_for_nonexistent_tool_are_inert() {
        // An alias whose `tool` field names a tool the server doesn't have
        // must not surface any tool in search just because the alias phrases
        // matched the query. Guards against manifest drift (tool was removed
        // but alias entry left behind).
        use rmcp::handler::server::wrapper::Parameters;
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        server
            .seed_inner(SeedParams {
                server: "codeview".into(),
                summary: Some("code".into()),
                category: Some("dev".into()),
                tools: vec![SeedTool {
                    name: "grep".into(),
                    description: "regex search".into(),
                    required: vec![],
                    properties: Default::default(),
                }],
            })
            .unwrap();
        // Alias points at `ghost_tool` which isn't in the seeded tool list.
        // Phrases share tokens with the query, but should NOT surface any
        // codeview tool because of this alias.
        crate::playbook::write_manifest(
            &paths,
            "codeview",
            &Manifest {
                tool_aliases: vec![crate::playbook::ToolAlias {
                    tool: "ghost_tool".into(),
                    phrases: vec!["nuclear fusion reactor".into()],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(async {
                server
                    .search(Parameters(SearchParams {
                        query: "nuclear fusion reactor".into(),
                        limit: Some(10),
                    }))
                    .await
            })
            .unwrap();

        // No codeview tool should appear in the results — the alias is
        // attached to a tool that doesn't exist, so it has no host to boost.
        assert!(
            !result.contains("codeview /"),
            "alias attached to a non-existent tool must not surface unrelated tools:\n{result}"
        );
    }

    #[test]
    fn phrase_overlap_bonus_fires_on_dense_match() {
        // Canonical outline alias set used as a fixture in alias tests.
        // After stop-word and short-token filtering:
        // "where is X defined" -> [where, defined].
        // Query "where is a function defined" contains both -> 2/2 -> +40.
        // "find function" -> [find, function]; only "function" in query
        // -> 1/2 -> below the matched>=2 floor -> 0.
        let phrases = vec![
            "find function".to_string(),
            "locate definition".to_string(),
            "where is X defined".to_string(),
            "symbol lookup".to_string(),
        ];
        let bonus = phrase_overlap_bonus(&phrases, "where is a function defined");
        assert_eq!(
            bonus, 40,
            "expected +40 from fully-matched 2-token phrase, got {bonus}"
        );
    }

    #[test]
    fn phrase_overlap_bonus_inert_on_zero_overlap() {
        // No phrase token appears in the query -> 0 bonus. Guards against
        // any tool with aliases getting a phantom boost on unrelated queries.
        let phrases = vec!["find function".to_string(), "locate definition".to_string()];
        let bonus = phrase_overlap_bonus(&phrases, "slack channel message");
        assert_eq!(bonus, 0);
    }

    #[test]
    fn phrase_overlap_bonus_below_matched_floor_is_inert() {
        // A single matched token does NOT trigger the phrase bonus — that
        // signal is already covered by the per-token alias arm in rank()
        // at +15. The phrase bonus exists specifically for multi-token
        // dense matches.
        let phrases = vec!["find function".to_string()];
        // "function" in query, "find" not -> 1/2 matched, fails matched>=2.
        let bonus = phrase_overlap_bonus(&phrases, "what function does this serve");
        assert_eq!(bonus, 0);
    }

    #[test]
    fn phrase_overlap_bonus_scales_with_match_count() {
        // 3-token phrase fully matched -> 3 * 20 = +60.
        // Caps at +80 so no single phrase dominates the score budget.
        let phrases = vec!["regex search files".to_string()];
        let bonus = phrase_overlap_bonus(&phrases, "regex search files in repo");
        assert_eq!(bonus, 60);
    }

    #[test]
    fn search_outline_above_grep_in_pure_intent_query() {
        // Isolated alias-effect test: when the query is pure intent (no
        // "search"/"grep" lexical noise), outline must win on its alias
        // phrase match. Counterpart to the noisy-query test above where
        // outline rides on aliases alone and competes with multiple
        // name-token matches — the phrase bonus is the mechanism that
        // keeps it in contention even when noise is mixed in.
        use rmcp::handler::server::wrapper::Parameters;
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths.clone());
        server
            .seed_inner(SeedParams {
                server: "codeview".into(),
                summary: Some("code".into()),
                category: Some("dev".into()),
                tools: vec![
                    SeedTool {
                        name: "grep".into(),
                        description: "regex content search".into(),
                        required: vec![],
                        properties: Default::default(),
                    },
                    SeedTool {
                        name: "outline".into(),
                        description: "symbol-level outline of a single file".into(),
                        required: vec![],
                        properties: Default::default(),
                    },
                ],
            })
            .unwrap();
        crate::playbook::write_manifest(
            &paths,
            "codeview",
            &Manifest {
                tool_aliases: vec![
                    crate::playbook::ToolAlias {
                        tool: "outline".into(),
                        phrases: vec![
                            "find function".into(),
                            "locate definition".into(),
                            "where is X defined".into(),
                            "symbol lookup".into(),
                        ],
                    },
                    crate::playbook::ToolAlias {
                        tool: "grep".into(),
                        phrases: vec!["regex search files".into(), "find pattern".into()],
                    },
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(async {
                server
                    .search(Parameters(SearchParams {
                        query: "where is a function defined".into(),
                        limit: Some(5),
                    }))
                    .await
            })
            .unwrap();
        let outline_pos = result
            .find("codeview / outline")
            .expect("outline should appear in results");
        if let Some(grep_pos) = result.find("codeview / grep") {
            assert!(
                outline_pos < grep_pos,
                "outline (full phrase match) must rank above grep on pure intent query:\n{result}"
            );
        }
    }

    #[test]
    fn rank_alias_does_not_boost_unrelated_queries() {
        // Aliases must only boost queries whose content actually matches an
        // alias phrase. A query about Slack channels shouldn't surface a
        // codeview tool just because that tool has *any* aliases.
        let q = "slack channel message";
        let tokens: Vec<&str> = q.split_whitespace().collect();
        let codeview_alias_score = rank(
            q,
            &tokens,
            "outline",
            "",
            "find function locate definition symbol lookup",
        );
        assert_eq!(
            codeview_alias_score, 0,
            "alias must not boost a tool when query has zero overlap with phrases"
        );
    }

    // --- Phase 5: input-size caps at the tool boundary ---

    #[test]
    fn note_rejects_oversized_claim() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let oversized = "x".repeat(MAX_CLAIM_BYTES + 1);
        let err = server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: Some("foo".into()),
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: oversized,
                tags: vec![],
                allow_duplicate: false,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Error:") && msg.contains("claim") && msg.contains("cap is"),
            "expected actionable size-cap error, got: {msg}"
        );
    }

    #[test]
    fn note_accepts_claim_at_cap_boundary() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        // Exactly at the cap (not over) must succeed.
        let at_cap = "y".repeat(MAX_CLAIM_BYTES);
        server
            .note_inner(NoteParams {
                server: "demo".into(),
                tool: Some("foo".into()),
                topic: None,
                kind: NoteKind::Tip,
                basis: NoteBasis::Observed,
                claim: at_cap,
                tags: vec![],
                allow_duplicate: false,
            })
            .expect("claim at the cap boundary should be accepted");
    }

    #[test]
    fn manifest_write_rejects_oversized_toml() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        // Build a TOML body whose total length exceeds the cap. Padding the
        // summary string with `x`s is the simplest way to overshoot.
        let padding = "x".repeat(MAX_MANIFEST_TOML_BYTES + 1);
        let oversized_toml = format!("[meta]\nsummary = \"{padding}\"\n");
        let err = server
            .manifest_write_inner(ManifestWriteParams {
                server: "demo".into(),
                manifest_toml: Some(oversized_toml),
                manifest: None,
                confirm_token: None,
                overwrite: false,
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Error:") && msg.contains("manifest_toml") && msg.contains("cap is"),
            "expected actionable size-cap error, got: {msg}"
        );
    }

    #[test]
    fn fetch_docs_rejects_too_many_extra_urls() {
        let (_tmp, paths) = test_paths();
        let server = LibrarianServer::new(paths);
        let urls: Vec<String> = (0..(MAX_EXTRA_URLS + 1))
            .map(|i| format!("https://example.com/{i}"))
            .collect();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(async move {
            server
                .fetch_docs_inner(FetchDocsParams {
                    url: "https://example.com/main".into(),
                    extra_urls: urls,
                    max_chars: None,
                })
                .await
                .unwrap_err()
        });
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Error:") && msg.contains("extra_urls") && msg.contains("cap is"),
            "expected actionable count-cap error, got: {msg}"
        );
    }

    #[test]
    fn manifest_fingerprint_is_non_empty_for_default() {
        // The propose/commit gate relies on a stable, non-empty fingerprint
        // to detect content drift between propose and commit. An empty
        // fingerprint would mean two distinct manifests collide.
        let fp = crate::playbook::manifest_fingerprint(&Manifest::default());
        assert!(
            !fp.is_empty(),
            "manifest_fingerprint must never return empty; got {fp:?}"
        );
    }

    // --- Phase 7: concurrency stress + unicode + empty-manifest robustness ---

    #[test]
    fn concurrent_note_writes_no_dupes_no_orphan_tmp() {
        // Stress: 8 threads each file a DISTINCT note for the same server,
        // hitting the write-lock + atomic-rename paths in tight succession.
        // Asserts: every write succeeds, all 8 notes land on disk, and no
        // stray `.tmp` artifacts are left behind in the learned dir.
        const N: usize = 8;
        let (_tmp, paths) = test_paths();
        let server = Arc::new(LibrarianServer::new(paths.clone()));
        let barrier = Arc::new(Barrier::new(N));

        let handles: Vec<_> = (0..N)
            .map(|i| {
                let server = server.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    server.note_inner(NoteParams {
                        server: "demo".into(),
                        tool: Some(format!("tool_{i}")),
                        topic: None,
                        kind: NoteKind::Tip,
                        basis: NoteBasis::Observed,
                        claim: format!("distinct observation #{i}"),
                        tags: vec![],
                        allow_duplicate: false,
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let oks = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, N,
            "all {N} distinct concurrent notes should succeed; results={results:?}"
        );

        let notes = index::read_notes(&paths, "demo").unwrap();
        assert_eq!(
            notes.len(),
            N,
            "all {N} notes should land in storage; found {}",
            notes.len()
        );

        // No `.tmp` orphans in the learned dir. The write-temp-then-rename
        // path completes atomically; a leftover .tmp would indicate a race
        // or a panic mid-write.
        if paths.learned_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&paths.learned_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            let orphans: Vec<_> = entries
                .iter()
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e == "tmp")
                        .unwrap_or(false)
                })
                .collect();
            assert!(
                orphans.is_empty(),
                "found orphaned tmp files after concurrent writes: {orphans:?}"
            );
        }
    }

    #[test]
    fn validate_server_name_rejects_unicode_traps() {
        use crate::config::validate_server_name;
        // Even though validate_server_name's regex is already ASCII-only,
        // pin the behavior with explicit test cases. These are the inputs
        // most likely to slip past an ad-hoc loosening someone might try
        // ("just allow basic Latin-1!") — the test fires immediately when
        // the regex is widened.
        let traps = [
            "caf\u{00E9}",      // é (precomposed)
            "cafe\u{0301}",     // é (decomposed: e + combining acute)
            "demo\u{200D}name", // zero-width joiner
            "demo\u{FEFF}name", // byte-order mark
            "\u{0301}leading",  // combining mark at start
            "demo\u{0008}",     // backspace
            "demo\u{0000}",     // null byte
            "\u{1F600}",        // emoji
        ];
        for input in traps {
            let res = validate_server_name(input);
            assert!(
                res.is_err(),
                "validate_server_name should reject Unicode trap {input:?}, but accepted it"
            );
        }
    }

    #[test]
    fn empty_manifest_round_trips_through_toml() {
        // A fully-empty Manifest should serialize to TOML, parse back, and
        // re-render without error. Guards against any subsystem assuming
        // at least one tool category / workflow / topic / etc.
        let original = Manifest::default();
        let toml_str = toml::to_string(&original).expect("empty manifest serializes");
        let parsed: Manifest = toml::from_str(&toml_str).expect("empty manifest TOML parses back");
        // Default round-trip equality: every section count must be zero.
        assert_eq!(parsed.workflows.len(), 0);
        assert_eq!(parsed.topics.len(), 0);
        assert_eq!(parsed.tool_categories.len(), 0);
        assert_eq!(parsed.gotchas.len(), 0);
        assert_eq!(parsed.tool_aliases.len(), 0);
        // Preview rendering must not panic on an empty manifest, and must
        // surface the zero-gotcha nudge per the Phase 5 schema (a manifest
        // with zero gotchas is the canonical case the nudge addresses).
        let preview = crate::playbook::render_manifest_preview(
            "demo",
            &original,
            std::path::Path::new("dummy.toml"),
            None,
        );
        assert!(
            preview.contains("Gotchas") && preview.contains("0"),
            "preview must show explicit zero-gotcha count; got:\n{preview}"
        );
    }
}
