# Changelog

All notable changes to this project will be documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims for [SemVer](https://semver.org/spec/v2.0.0.html) once it has a public release surface to break.

## [Unreleased]

## [0.2.2] - 2026-06-29

Pre-public hardening pass. A full adversarial review surfaced two reachable SSRF gaps, a single-call denial-of-service, an ungated destructive write, and a red dependency gate; all are closed below, plus the should-fix and documentation-accuracy items found alongside them.

### Security
- **SSRF: connection-time IP validation on every hop.** Outbound `librarian_fetch_docs` now uses a custom `reqwest` DNS resolver that re-runs the block-list against every IP the client actually connects to — the initial host and each redirect hop — and the client dials exactly those IPs. This closes two gaps: (1) a redirect to a hostname that *resolves* into a private/loopback/metadata range (previously only literal-IP redirects were screened), and (2) the DNS-rebinding window where the one-shot resolve check and the real connection could see different IPs. The previous "we don't defend against DNS rebinding" deferral in `SECURITY.md` is removed — it's defended now.
- **SSRF: IPv6 embedded-IPv4 forms blocked.** The IP block-list now also rejects NAT64 (`64:ff9b::/96`), 6to4 (`2002::/16`), and deprecated IPv4-compatible (`::a.b.c.d`) addresses by decoding and re-checking the embedded IPv4, in addition to the existing IPv4-mapped handling.
- **DoS: deeply-nested `manifest_toml` is rejected before parsing.** The `toml` crate is recursive-descent with no depth limit; a ~260-byte payload of nested brackets could overflow the thread stack — an unrecoverable abort (not a catchable panic) that killed the whole process, and which the 256 KiB byte cap did not prevent. `librarian_manifest_write` now scans bracket/brace nesting (ignoring strings and comments) and rejects past 32 levels before any parse.
- **`librarian_seed_playbook` overwrites are now gated.** Adding a *new* entry is still a one-shot fast path, but *overwriting* an existing entry (`overwrite=true`) now goes through the same propose/commit flow as the other write tools: a preview + single-use, content-fingerprinted token, with a commit that rejects on any content drift. Previously `overwrite=true` replaced an entry in a single ungated call — a prompt-injected agent could silently clobber a real probed/curated entry.
- **Config-derived server names are path-validated.** Server names read from the MCP client config (`mcpServers` keys) now pass the same `validate_server_name` check as agent-supplied names before becoming filesystem paths; invalid keys are skipped with a warning rather than escaping the data dir.
- **Seed size caps.** Per-server tool count, name/description/summary sizes, and servers-per-batch are bounded, plus a hard ceiling on the serialized `index.json` at save time — `index.json` is re-parsed on nearly every call, so this stops an agent from bloating it.
- **Token generation no longer collides or reads as a guessable clock.** `generate_token()` was a bare wall-clock nanosecond; two proposes in the same tick could collide and silently evict the earlier pending write. It now appends a process-lifetime monotonic counter (guaranteeing uniqueness) and mixes in an OS-CSPRNG-seeded salt.

### Fixed
- Dependency advisories cleared so the blocking `cargo deny` CI gate is green: `anyhow` bumped to ≥1.0.103 (RUSTSEC-2026-0190), and RUSTSEC-2026-0189 (rmcp Streamable-HTTP DNS rebinding) is documented as not-reachable in `deny.toml`/CI — the librarian serves over stdio only and never enables the HTTP server transport.
- `SECURITY.md` and `README.md` corrected to match the implementation: which writes are content-fingerprinted vs server+action-bound; the credential note now states the `env` block is read only to forward it to the spawned child (never stored/logged/returned); learned notes live under `%APPDATA%` (Roaming), not `%LOCALAPPDATA%`, and the on-disk tree shows the cache correctly under `%LOCALAPPDATA%`; the atomic-write claim is scoped to the paths that actually use temp-then-rename.
- Security-disclosure channel fixed: `SECURITY.md` now points at GitHub private vulnerability reporting and a concrete email rather than a profile address that wasn't published.

### Changed
- CI: least-privilege `permissions: contents: read` on both workflows; the advisory `cargo audit` job ignores the two accepted advisories explicitly; added `.github/dependabot.yml` to keep Actions + Cargo dependencies current and surface new advisories off-PR.
- `compact` CLI subcommand prints a clean "not implemented yet" notice instead of a `TODO:` line.
- Test fixtures use neutral example server names.
- Platform support docs updated: **macOS (Apple Silicon) is now confirmed working** by the maintainer (previously "compiles, exercised only at release-tag CI"). Windows remains the primary target; Linux stays CI-only / not hands-on verified.

## [0.2.1] - 2026-06-05

Hardening pass: closes the one ungated overwrite path on the librarian surface, routes CLI refresh through the same write-lock as the MCP handler, and turns `extra_urls` rate-limit failures into waits so multi-page same-domain fetches work as documented.

### Security
- **`librarian_seed_playbook` refuses to overwrite an existing index entry by default.** Without this gate, a confused or prompt-injected agent could call `librarian_seed_playbook(server="<existing_name>", tools=[])` and silently replace a real probed entry with arbitrary seed content — the only ungated write path on the librarian surface. New `overwrite: bool` parameter (default false) opts into deliberate replacement; rejection points the agent at `librarian_seed_batch` for multi-server overwrites under a single user approval.

### Fixed
- CLI `mcp-librarian refresh` now routes its index merge + write through `with_write_lock`. The MCP `librarian_refresh` handler already did; running CLI refresh while a Claude client was writing concurrently could lose updates. Probing still happens outside the lock so it isn't held across slow child-process spawns.
- `librarian_fetch_docs` now **waits** for the per-domain rate-limit slot instead of bailing with `Error: rate-limited`. Multi-page same-domain `extra_urls` batches (the documented use case for vendor docs) work end-to-end. The wait is capped at 30 seconds total per call so a pathological queue can't stall indefinitely.
- `SECURITY.md` no longer references the 0.1.0-era `invalid_params` JSON-RPC error code for validation failures; the diagnostics-as-content shape shipped in 0.2.0 is now documented in the threat model too.
- `README.md` headline security claim (formerly "single-use tokens with content fingerprints on every write") is now accurate about which write paths are token-gated and which (`seed_playbook`, `note`) are not.
- CI now runs `cargo deny check` as a blocking job, matching the dependency policy in `CONTRIBUTING.md`. Previously the policy was advertised but only enforced locally.

## [0.2.0] - 2026-06-05

First public release. Intent-aware search and directive session orientation; manifests gain an additive `tool_aliases` field; tool failures arrive as `Ok` content so no client renderer can swallow them; hard input caps at every write tool entry; expanded documentation of threat-model deferrals and dependency hygiene.

### Added
- **`tool_aliases` field on manifests** — manifest authors can attach intent phrases per tool (e.g. `outline` aliased to `["find function", "locate definition", "where is X defined"]`). Phrases are folded into `librarian_search` ranking so agents describing intent in natural language surface the right tool even when it shares no lexical tokens with the query. Backward-compatible (`#[serde(default)]`); existing manifests parse unchanged. Documented in `librarian_help("librarian", "manifest_schema")`.
- `librarian_manifest_write` propose preview now surfaces a soft nudge when a manifest has zero gotchas: *"no gotchas listed. Real-world usage patterns and footguns are typically the highest-signal part of a playbook. Consider adding 2-3 before committing."* Not a hard reject — some servers legitimately have none.
- **Hard input caps at tool boundary** to bound parser memory against adversarial agent input: 256 KiB for `manifest_toml`, 8 KiB for `librarian_note` `claim`, 16 for `librarian_fetch_docs` `extra_urls` count. All emit explicit "Error: X bytes exceeds cap of Y. Action: …" responses, well above realistic input sizes.
- GitHub Actions: per-push Windows `cargo fmt + clippy + test + doc` workflow, plus tag-triggered Linux/macOS/Windows matrix workflow for release-time cross-platform verification. Advisory `cargo audit` job runs alongside.
- `cargo deny check` configuration ([deny.toml](deny.toml)) — license allow-list, wildcard-version ban, source restrictions.
- `CODE_OF_CONDUCT.md`, GitHub issue + PR templates, security-disclosure contact link in issue config.
- `SECURITY.md` now documents the previously-implicit threat-model deferrals: same-nanosecond token collision (by design for a local single-user tool), adversarial filesystem TOCTOU, backup history depth = 1, TOML nesting cap (handled via input size cap), tool-boundary panics (terminate process by design).
- README "Tested platforms" subsection — Windows primary, Linux/macOS via `directories` crate exercised on tag-time matrix CI.
- Concurrency stress test: 8 concurrent `librarian_note` writes; asserts every write succeeds, every note lands, no orphaned `.tmp` files.
- Unicode trap test pinning that `validate_server_name` continues to reject combining marks, ZWJ, BOM, emoji, control bytes.
- Empty-manifest round-trip test (default `Manifest` → TOML → parse → render with explicit zero-gotcha nudge).

### Changed
- Rewrote the rmcp server `instructions` string (surfaced at session init by Claude Desktop / Claude Code / Codex) to open with directive framing: *"Orient before acting. Before calling any indexed MCP server's tools, call `librarian_help(server)`..."*. The prior text was informational and left the orient-first behavior opt-in.
- `librarian_search` ranking now folds curated **intent phrases** into the score (see Added: `tool_aliases`). Alias hits rank between description and tool-name weight — curated phrases beat auto-descriptions but the tool's own name still wins for direct lookups.
- `librarian_search` ranking adds a **per-phrase token-overlap bonus** on top of haystack-level alias scoring: when an alias phrase has ≥2 meaningful tokens and ≥50% of them appear in the query, the tool gets a phrase-level bonus so multi-word intent matches outrank tools that only share a single common token via their name.
- Project-wide `cargo fmt` pass; no semantic changes.

### Fixed
- Tool failures now return `Ok` MCP content with a leading `Error: …` prefix instead of JSON-RPC `error` envelopes. Claude Desktop swallows JSON-RPC errors on some renderer paths, hiding the actionable diagnostic; returning failures as normal tool output makes them visible to the agent and the user. Supersedes the `invalid_params` approach shipped in 0.1.0 for the user-facing cases.
- `playbook::manifest_fingerprint` no longer silently collapses to an empty string on serialization failure (`unwrap_or_default` → `expect("manifest serialization is infallible")`). Empty fingerprints would mean two distinct manifests collide and the propose/commit gate could wave a content-drifted commit through.
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

[Unreleased]: https://github.com/dmriding/mcp-librarian/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/dmriding/mcp-librarian/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/dmriding/mcp-librarian/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dmriding/mcp-librarian/releases/tag/v0.1.0
