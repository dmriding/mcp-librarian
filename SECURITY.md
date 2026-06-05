# Security policy

## Scope

`mcp-librarian` is a single-user, local-only tool that runs over stdio MCP. It has no network listening surface, manages no credentials, and is run with the local user's privileges. The threat model below covers what this tool defends against, what it deliberately doesn't, and how to report a security issue.

## Threat model

### What we defend against

| Vector | Defense |
|---|---|
| Agent supplies a path-traversal `server` name (`"../../foo"`) to write a file outside the data dir | `validate_server_name()` rejects non-`[A-Za-z0-9_.-]+` characters, leading dots, and oversize names. Applied at every tool entry that accepts `server`. |
| Agent calls `librarian_fetch_docs` against a localhost / private-IP / cloud-metadata URL (SSRF) | Sync URL screen rejects non-http(s) schemes, IP literals in loopback / link-local / private / CGNAT / IPv6 unique-local ranges (plus IPv4-mapped equivalents) and `localhost` aliases. DNS resolve then re-checks. Redirect policy re-validates each hop, max 5 hops. |
| Malicious server returns a 5 GiB response body to exhaust memory | Response body cap (5 MiB) enforced via streamed-and-limited read in `fetch.rs`. |
| Agent commits a manifest different from what the user approved | `librarian_manifest_write` / `librarian_manifest_restore` / `librarian_seed_batch` / `librarian_seed_remove` use a propose/commit gate with content fingerprinting and 5-minute single-use tokens. Any content drift between propose and commit rejects. |
| Concurrent writes from two MCP clients (Claude Code + Codex + Claude Desktop running side-by-side) corrupt the shared on-disk state | Cross-process advisory file lock (`config/.librarian.lock`) serializes write-class operations. All file writes use write-temp-then-rename so a reader during a write sees fully-old or fully-new content, never partial. |
| Agent silently submits invalid input (null `kind`, null `basis`) and the tool defaults | Strict serde validation rejects null and missing required fields at the JSON boundary. `invalid_params` JSON-RPC error code is used so MCP clients surface the actual message instead of swallowing it. |
| Agent's TOML has `gotchas` misplaced under `[meta]` and the array silently vanishes | Three-layer defense: preview always shows `Gotchas: N entries` (zero included), parse-time detector rejects misplaced root keys with the offending parent named, `manifest_schema` topic warns up front. |
| Note dedup races: two clients writing the same observation concurrently both pass the dedup check | Read-check-write held atomically under the write lock. |

### What we deliberately don't defend against

| Threat | Why not |
|---|---|
| **DNS rebinding** — a domain that returns a public IP during our resolve check but a private IP when `reqwest` actually dials | Would require IP pinning that `reqwest` doesn't expose cleanly. The redirect-policy URL re-validation catches the most common attack shape. Mitigation would be a custom DNS resolver + connector wiring; not worth the complexity for the realistic threat (agent hallucinates or is prompt-injected with a localhost URL — that's covered). |
| **At-rest encryption** of stored data | Manifests / index / notes don't contain credentials, only references to env-var *names* like `POLYMARKET_API_KEY`. The fetch-docs cache holds public docs only (the SSRF guard ensures this). Any file-adjacent reversible scheme is obfuscation, not security. If you need at-rest protection, use BitLocker / FileVault / dm-crypt at the volume level. |
| **Disk theft of unrelated data via the librarian process** | The librarian only reads files it's pointed at (configs, manifests, notes, fetched docs in cache). It doesn't enumerate `%USERPROFILE%`. If an attacker has local code execution, they have far better tools than this MCP server. |
| **Sandbox escape from probed child processes** | `librarian_refresh` spawns stdio MCP servers from your client config. Those subprocesses run with your user privileges. Trust your MCP config like you trust any other config you opt into. |
| **PII scrubbing in learned notes** | Convention is that `claim` is prose, not literal arg blobs. No automated redaction; agents shouldn't paste secrets into notes. If they do, the user can `rm <data>/learned/<server>.jsonl` to clear. |
| **Cloud-sync exposure of `%APPDATA%\netviper\`** | If you sync your AppData to OneDrive Personal or similar, manifests and notes go along. The librarian doesn't choose where AppData lives. Be aware. |
| **Same-nanosecond token collision** | `generate_token()` uses 16 hex chars of `Utc::now().timestamp_nanos_opt()`. Not cryptographically random by design. The token only gates a write the same process already authorized via the propose step; for a single-user local tool, the realistic adversary cannot observe the timestamp window. Two propose calls in the same nanosecond would collide and the second supplants the first — visible failure mode, user re-proposes. |
| **Adversarial filesystem TOCTOU on manifest write** | The atomic write-temp-then-rename path assumes the manifests directory isn't under attacker control. If another local user already has write access to your `%APPDATA%\netviper\`, that implies full state compromise; we don't defend the symlink-swap variant of that case. |
| **Backup history depth = 1** | Only one `.toml.bak` per server. Two consecutive bad commits lose the original. Trade-off: keeps on-disk state visible and simple. If you need deeper history, version `%APPDATA%\netviper\manifests\` with git. |
| **TOML parser nesting depth** | We cap `manifest_toml` input size at 256 KiB at the tool boundary; beyond that, recursion bounds are the `toml` crate's responsibility. We don't impose a custom nesting cap. |
| **Tool-boundary panics** | A panic in any inner function terminates the librarian process. Not wrapped in `catch_unwind` by design — a panic represents a logic bug, and surfacing it loudly is more useful than masking it. The MCP client will see the connection drop and reconnect. |

## Reporting a security issue

If you find a real vulnerability — something an adversary could actually exploit, not a hardening suggestion — please:

1. **Don't** open a public GitHub issue describing the exploit.
2. **Do** email the maintainer at the address listed on the GitHub profile, with subject prefix `[mcp-librarian security]`. Include: the affected version (commit hash is best), repro steps, and a minimal PoC if you have one.
3. **Expect** a response within ~7 days. This is a personal project; response times depend on bandwidth.

For hardening suggestions, behavior questions, or "what about X attack class" discussions, a public GitHub issue is fine and welcomed.

## What we'll do with a report

- Acknowledge it within 7 days
- Investigate, reproduce, and patch on a feature branch
- Disclose the fix in the [CHANGELOG](CHANGELOG.md) once shipped on `main`, crediting the reporter unless asked not to
- For high-severity issues, coordinate disclosure timing if requested

This is a small project. There's no "embargo until coordinated disclosure across vendors" process; the librarian has no upstream consumers to coordinate with.
