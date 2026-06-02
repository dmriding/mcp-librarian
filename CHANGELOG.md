# Changelog

All notable changes to this project will be documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims for [SemVer](https://semver.org/spec/v2.0.0.html) once it has a public release surface to break.

## [Unreleased]

### Changed
- Rewrote the rmcp server `instructions` string (surfaced to agents in session-init context by Claude Desktop / Claude Code / Codex) to open with directive framing: *"Orient before acting. Before calling any indexed MCP server's tools, call `librarian_help(server)`..."*. Closes the cold-start discoverability gap from the 2026-06-02 Claude Desktop feedback — the prior text was informational and left the orient-first behavior opt-in.
- `librarian_search` ranking now folds curated **intent phrases** into the score. The internal `rank()` function gained an `aliases` arm sized between description (30) and tool name (100) — curated phrases beat auto-descriptions but the tool's own name still wins for direct lookups. Closes the round-2 failure where `"search code for where a function is defined"` returned hosted servers' `*_search_*` tools with zero codeview results.

### Added
- `librarian_manifest_write` propose preview now surfaces a soft nudge when a manifest has zero gotchas: *"no gotchas listed. Real-world usage patterns and footguns are typically the highest-signal part of a playbook. Consider adding 2-3 before committing."* Not a hard reject — some servers legitimately have none. Mirrors the existing "always show `Gotchas: 0 entries`" discipline by making the absence's *cost* visible alongside its count.
- **`tool_aliases` field on manifests** — manifest authors can attach intent phrases per tool name (e.g. `outline` aliased to `["find function", "locate definition", "where is X defined", "symbol lookup"]`). Phrases are folded into `librarian_search` ranking so agents describing intent in natural language surface the right tool even when it shares no lexical tokens with the query. Backward-compatible (`#[serde(default)]`); existing manifests parse unchanged. Documented in `librarian_help("librarian", "manifest_schema")` with a worked example and an inert-when-mistargeted guarantee for stale alias entries.

### Fixed
- Typo in `librarian_help("librarian")` workflow text — `tools_dump` → `tools` (matches the actual `librarian_seed_playbook` parameter name).

## [0.1.0] - 2026-05-16

First public version. Everything below is shipped on `main` and exercised against Claude Code, Claude Desktop, and Codex.

### Added — core MCP server

- **Thirteen tools** exposed over stdio MCP:
  - `librarian_list` — directory of every known MCP server, grouped by category
  - `librarian_help` — playbook for one server (overview or topic drill-down). `server="librarian"` returns the librarian's own playbook. `topic="manifest_schema"` returns the TOML reference
  - `librarian_search` — fuzzy match across all tool names and descriptions
  - `librarian_note` — agent appends a learned observation about a server
  - `librarian_seed_playbook` — register a single hosted/cloud server from the deferred-tools reminder
  - `librarian_seed_batch` — bulk-register many hosted servers in one approved transaction
  - `librarian_seed_remove` — explicit cleanup of stale index entries
  - `librarian_onboarding` — returns a step-by-step prompt for first-install bootstrapping
  - `librarian_refresh` — reprobe local stdio servers; flag drifted notes
  - `librarian_manifest_write` — author or replace a server's curated manifest (propose/commit gated)
  - `librarian_manifest_diff` — show changes between current manifest and auto-backup
  - `librarian_manifest_restore` — swap current ↔ backup (propose/commit gated)
  - `librarian_fetch_docs` — fetch public vendor docs for bootstrapping manifests
- **Manifest authoring** via `manifest_toml` parameter (single TOML string, preferred over nested JSON which hangs some MCP clients)
- **Learned-notes system** with `kind` (workflow / arg_shape / behavior / error_pattern / tip / example) and `basis` (observed / inferred), append-only JSONL per server
- **Manifest-only servers** — entries whose manifest exists on disk but server isn't currently installed are surfaced in list/help/search
- **Drift detection** — `librarian_refresh` flags notes whose underlying tool's arg shape changed, marking them `⚠possibly stale` instead of deleting
- **CLI subcommands** — `serve`, `list`, `print`, `refresh`, `compact` (compact is a stub)

### Added — safety and security

- **Propose/commit gate** on `librarian_manifest_write`, `librarian_manifest_restore`, `librarian_seed_batch`, `librarian_seed_remove`: every write tool requires a two-step dance. First call returns a structured preview + a single-use, content-fingerprinted token; second call with that token commits. Tokens expire in 5 minutes. Code-enforced, not a social rule
- **Three-layer defense against the TOML misplaced-`gotchas` footgun**: preview always shows every section count (zero included), parse-time detector rejects misplaced root keys with the offending parent named, `manifest_schema` topic warns up front
- **Server name validation** — `[A-Za-z0-9_.-]+`, no leading dot, max 64 chars. Blocks path-traversal writes via names like `../../foo`
- **SSRF guards on `librarian_fetch_docs`** — rejects non-http(s) schemes, IP literals in loopback/private/link-local/multicast/CGNAT/IPv6 unique-local ranges (plus IPv4-mapped equivalents), `localhost` aliases as hostnames, hostnames that DNS-resolve into any of the above. Redirect policy re-validates each hop (max 5)
- **Response body cap** — `librarian_fetch_docs` truncates at 5 MiB to prevent OOM from misbehaving servers
- **Cross-process write lock** — multiple MCP clients (Claude Code + Codex side-by-side) writing simultaneously are serialized via an advisory file lock at `config/.librarian.lock`. Held only during writes, sub-millisecond in practice
- **Atomic file writes** — manifests, restores, index saves, and notes all use write-temp-then-rename. Readers see fully-old or fully-new content, never a partial write
- **Note dedup at write time** — `librarian_note` rejects an exact normalized match on (server, tool, kind, claim) with a pointer to the existing note's timestamp. `allow_duplicate=true` bypasses for intentional re-emphasis
- **Strict deserialization** of `kind` and `basis` — null and missing fields rejected at the JSON boundary, no silent defaulting
- **`invalid_params` JSON-RPC error code** for validation failures so Claude Desktop surfaces the message instead of swallowing it

### Added — UX polish

- `librarian_help` returns a manifest-authoring footer for seeded entries (no manifest), pointing at `librarian_fetch_docs` and `manifest_schema`
- Auto-prefix tool grouping recurses one level when all tools share a top prefix (Notion's 14 `notion-*` tools split into `create` / `update` / `get` / etc. instead of one flat bucket)
- `(seeded)` marker in `librarian_list` includes the tool count, e.g. `(seeded — 18 tools)` so handshake-only stubs read distinctly from rich seeds
- `librarian_manifest_diff` labels each side with file mtime (older / newer) so post-restore direction is unambiguous
- Virtual topics for `gotchas` / `workflows` / `categories` on every server (not just manifest-authored ones)
- Search ranking filters stop-words and tokens under 3 chars to avoid false-positive matches on common words
- Manifest-write preview is intentionally compact (counts + names, not full body content) to dodge a Claude Desktop rendering hang on dense markdown
- Seed-remove preview is character-conservative (ASCII hyphens only, no angle-bracket placeholders, no backticks around punctuation) to dodge the same hang
- `sanitize_for_preview()` strips em-dash / en-dash / ellipsis / NBSP from agent-supplied content flowing into preview chrome

### Fixed

- `librarian_search` no longer false-positives on stop words like "in", "the", "for"
- `librarian_help` no longer duplicates learned notes between virtual topics and the Related Notes section
- `librarian_help` renders manifest-declared tool categories when the server hasn't been probed yet (was previously blank)
- `librarian_list` honors `entry.category` for seeded entries when no manifest exists (previously bucketed them all under "uncategorized")
- `librarian_note` echoes the recorded claim in its ack so the agent can verify what landed
- `librarian_manifest_write` accepts `manifest_toml` as a flat string parameter (works around MCP clients that hang on nested JSON with multi-line bodies)

### Known limitations

- Windows-first; macOS/Linux paths exist but unverified
- Local stdio probing only; remote/cloud servers must be seeded
- No CLI playbook recursion (paired-CLI is manifest-driven; no auto-walk)
- No compaction of learned notes (`mcp-librarian compact` is a stub); soft-dedup at write time limits growth
- DNS rebinding is not mitigated (would require IP pinning that `reqwest` doesn't expose cleanly)
- Single-host only; no clustering, no shared state across machines

[Unreleased]: https://github.com/dmriding/mcp-librarian/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/dmriding/mcp-librarian/releases/tag/v0.1.0
