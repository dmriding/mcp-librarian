# mcp-librarian

> An MCP server that indexes your other MCP servers and emits playbooks on demand.

I built this for me, and I run it daily with Claude Code and Codex. Patches welcome; feature requests without patches will be politely declined.

---

## Contents

1. [The problem](#the-problem)
2. [The solution](#the-solution)
3. [What this is](#what-this-is)
4. [What this is not](#what-this-is-not)
5. [How it works (mental model)](#how-it-works-mental-model)
6. [A 60-second tour from the agent's POV](#a-60-second-tour-from-the-agents-pov)
7. [Install](#install)
8. [Configure (add to your MCP client)](#configure-add-to-your-mcp-client)
9. [First run — index everything (Claude Code, Claude Desktop, Codex)](#first-run--index-everything)
10. [The thirteen tools](#the-thirteen-tools)
11. [Manifests — your canonical playbooks](#manifests--your-canonical-playbooks)
12. [Workflow recipes](#workflow-recipes)
13. [Storage paths](#storage-paths)
14. [Environment & CLI](#environment--cli)
15. [Security & storage](#security--storage)
16. [Known limitations](#known-limitations)
17. [Project docs](#project-docs)
18. [Licenses & contributing](#licenses--contributing)

---

## The problem

You connect a coding agent (Claude Code, Codex, Claude Desktop, etc.) to half a dozen MCP servers. Each server exposes 5–50 tools. Most modern clients use **deferred loading** — the agent only sees tool *names* up front, and has to explicitly request a full schema to actually call a tool.

In practice this means:

- The agent **doesn't know what's available** without paying for schemas it hasn't seen
- Most MCP servers ship **no `help`-style orientation tool**, so the agent flails: random tool calls, schema errors, wasted context
- There's **no shared memory** between sessions — an observation one session pays a price to learn is lost the next time
- **Hosted/cloud MCP servers** (the `claude.ai_*` family, Notion, Slack, Figma) can't be probed by spawning. Your agent literally cannot inspect them — only the docs the vendor provides on the web
- **Authoring a curated playbook** for a server is something every team needs but no one has a standard place for

The agent's discovery cost scales with the number of MCP servers, and the wasted-token cost compounds with every new session.

## The solution

One small MCP server that indexes all your *other* MCP servers and serves curated playbooks on demand. The agent makes **one tool call** (`librarian_list`) and gets the entire landscape grouped by category. Drilling into a single server (`librarian_help`) returns a hand-curated workflow + categories + gotchas summary, plus learned observations from prior sessions. Searching across tools (`librarian_search`) is fuzzy and cheap.

The playbook for each server lives in two layers:

- **Manifests** — *your* canonical TOML. Hand-authored or AI-authored-then-approved-by-you.
- **Learned notes** — agent-appended observations, append-only, tagged with `kind` and `basis`. The librarian re-renders the playbook with manifest content first and observed notes underneath.

For hosted servers the librarian can't probe, there's `librarian_fetch_docs` (SSRF-guarded) plus `librarian_seed_playbook` — read the vendor's public docs, synthesize a manifest, persist it once, and now every session has the playbook.

The agent's discovery cost collapses from "N schemas × M tools each" to **one tool call returning prose**.

## What this is

- **An MCP server that indexes other MCP servers.** It runs locally over stdio, like every other MCP server.
- **A two-layer knowledge store.** Manifests are your canon. Learned notes accrete from agent observations across sessions.
- **A bootstrapping tool for hosted MCP servers.** Fetches public docs, lets the agent synthesize a playbook, then commits it via a propose/commit gate so you've reviewed before it lands on disk.
- **Concurrent-safe.** Multiple clients (Claude Code + Codex side-by-side) writing simultaneously are serialized via a file lock; manifest writes are atomic.
- **Security-conscious.** SSRF guards on outbound fetch, path-traversal validation on every `server` parameter, single-use tokens with content fingerprints on every write.
- **Rust, no runtime dependencies.** Single statically-linked binary. ~12 MB.
- **Designed for the worst-case AI.** Gates are *code rules*, not social rules in tool descriptions. An agent that ignores instructions still can't write a manifest without producing a structured preview first.
- **Diagnostics-as-content.** Tool failures return `Ok` MCP content with a leading `Error: …` prefix, not JSON-RPC `error` envelopes. Agents see schema violations, expired tokens, and validation failures inline in the same response stream they read on success — no client transport quirk can hide them from the model.

## What this is not

- **It is NOT a proxy** for your MCP servers. It does not invoke their tools. It only describes them.
- **It does NOT manage credentials.** API keys / tokens live in your MCP client config (`.claude.json` / `claude_desktop_config.json`) and get passed to spawned servers as env vars. The librarian never reads them.
- **It is NOT a multi-user or cloud service.** Single-user, local-only. Your data stays on your disk.
- **It is NOT a replacement** for the MCP servers it indexes. It's a directory/playbook layer on top.
- **It is NOT a generic notes system.** Notes are scoped to (server, tool, kind, basis). It's a knowledge graph for MCP behavior, not for arbitrary prose.
- **It does NOT scrub PII.** Convention: notes are prose, not literal arg blobs. No customer data in, no customer data out.
- **It does NOT auto-encrypt** stored data. Manifests/notes aren't sensitive (see [Security & storage](#security--storage) for the threat model and why volume-level encryption is the right answer if you need it).

## How it works (mental model)

Three pieces of state live on disk, all human-readable, all editable:

```
%APPDATA%\netviper\mcp-librarian\            (Windows; macOS/Linux follow `directories` crate)
├── config/
│   ├── manifests/
│   │   ├── slack.toml               # your canon for slack
│   │   ├── slack.toml.bak           # auto-backup written before every commit
│   │   └── playwright.toml          # …
│   └── .librarian.lock              # advisory file lock for cross-process writes
├── cache/
│   ├── index.json                   # probed tool names + descriptions (the index)
│   └── docs/<hash>.json             # cached responses from librarian_fetch_docs
└── data/
    └── learned/
        ├── slack.jsonl              # agent-appended observations, append-only
        └── playwright.jsonl
```

**Rendering precedence** when an agent calls `librarian_help("slack")`:

1. The manifest's `meta.summary`, `workflows`, `tool_categories`, `topics`, `gotchas` render first — they're *your* curated content
2. The indexed tools (from probing the live server) are grouped under the manifest's `tool_categories`, or auto-grouped by name prefix if no categories defined
3. Learned notes with `basis="observed"` render under their relevant sections (workflows → Key Workflows, behavior/tip/error_pattern → Gotchas)
4. Learned notes with `basis="inferred"` render in a weaker, separately-labeled section so a future agent knows to trust them less

**Search is intent-aware.** `librarian_search` matches across tool names, descriptions, and any `tool_aliases` phrases declared in the manifest. A query like `"post to channel"` ranks Slack's `chat_send_message` first when the author has wired that intent into `tool_aliases`, even if the word "post" never appears in the tool's description.

When you run `librarian_refresh`, the librarian re-probes every stdio MCP server, compares each tool's argument shape against the prior index, and **flags learned notes about tools whose schemas drifted** with a `⚠possibly stale` marker. Stale notes are not deleted — you read them and decide.

## A 60-second tour from the agent's POV

Imagine an agent in a fresh Claude Code session with eight MCP servers configured. Without the librarian:

```
agent thinks: I need to send a Slack message. What tools does Slack have?
agent calls: slack.send_message → schema error, missing required `channel_id`
agent calls: slack.list_channels → ok, picks one
agent calls: slack.send_message with channel name → fails, channels need IDs
agent: gives up or wastes 2000 tokens loading every Slack schema in full
```

With the librarian:

```
agent calls: librarian_list()
  → 8 servers grouped by category, one-line summary each
agent calls: librarian_help("slack")
  → category breakdown, workflows ("post a threaded reply", "resolve names before acting"),
    gotchas ("channel IDs ≠ channel names"), all in <1 KB
agent calls: slack.list_channels then slack.send_message correctly first try
```

This is **the entire value proposition**. Everything else (manifest authoring, notes, fetch_docs, propose/commit gates) is supporting infrastructure to make sure that experience stays accurate as servers and your knowledge of them evolve.

## Install

```powershell
# from source
cargo build --release
.\target\release\mcp-librarian.exe --help
```

Or install globally:

```powershell
cargo install --path .
```

The result is one self-contained ~12 MB binary. No runtime dependencies, no Node, no Python.

## Configure (add to your MCP client)

### Claude Code (`~/.claude.json` on every platform)

```jsonc
{
  "mcpServers": {
    "librarian": {
      "command": "C:\\path\\to\\mcp-librarian.exe",
      "args": ["serve"]
    }
  }
}
```

### Claude Desktop (`%APPDATA%\Claude\claude_desktop_config.json` on Windows; `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS)

Same shape:

```jsonc
{
  "mcpServers": {
    "librarian": {
      "command": "C:\\path\\to\\mcp-librarian.exe",
      "args": ["serve"]
    }
  }
}
```

After saving the config, **fully quit and relaunch Claude Desktop** (closing the window isn't enough — use Quit from the menu or kill it from the system tray). The librarian will appear under the hammer/tools icon in the input area on next chat.

### Codex (or any other MCP client)

The MCP transport is plain stdio. Add an entry pointing `command` at the binary with `args: ["serve"]` and you're in.

## First run — index everything

After installing the librarian into one of the clients above, run this **once** to populate the index with every connected MCP server.

### The one-call path (recommended)

```
librarian_onboarding()
```

Returns a step-by-step bootstrap prompt the agent reads and executes. It handles:

1. `librarian_refresh()` — auto-discovers and probes every **local stdio** MCP server (the ones in `~/.claude.json` / `claude_desktop_config.json`)
2. Diffs that result against the MCP servers the agent can see in its own deferred-tools reminder
3. For each **hosted/cloud** MCP visible to the agent but missing from the index (e.g. `claude.ai_Slack`, `claude.ai_Notion`, OAuth-connected servers), runs `librarian_seed_playbook` with the right name, category, and tool list
4. Verifies with a final `librarian_list()`

The hosted/cloud distinction matters because the librarian *cannot probe hosted MCPs* by spawning — they only exist inside the client's mediator. The agent has to seed them from what it can see in its own context.

### The manual path

If you'd rather drive it yourself:

```
librarian_refresh()            # local stdio servers
librarian_list()               # see what got indexed
# Build a single batch of every hosted server still missing, then:
librarian_seed_batch(
    servers=[
        {
            "server": "claude.ai_Slack",
            "summary": "Slack workspace MCP via claude.ai mediator — …",
            "category": "comms",
            "tools": [{"name": "slack_send_message", "description": "..."}, ...]
        },
        { ...next server... },
        ...
    ]
)
# Returns a preview + a confirm_token. After reading the preview and saying "yes":
librarian_seed_batch(servers=[...same as above...], confirm_token="…")
```

One approval covers everything. After the first run you have one tool call (`librarian_list`) that returns the full landscape of every connected MCP, including the hosted ones.

### Running both Claude Code AND Claude Desktop?

That works — they each spawn their own librarian process sharing the same data files. Cross-process writes are serialized via an advisory file lock (see [Concurrency](#security--storage)), so concurrent `librarian_note` / `librarian_manifest_write` / `librarian_refresh` calls won't corrupt state. Each client should still run `librarian_onboarding()` once on first install so the hosted servers visible to *that specific client* get seeded under names matching its prefixes.

## The thirteen tools

| Tool | What it does |
|---|---|
| `librarian_onboarding` | Returns a one-shot bootstrap prompt for first-install setup — refresh local stdio + seed every hosted server visible in the agent's deferred-tools reminder. |
| `librarian_list` | Directory of all known MCP servers, grouped by category. Optional `category` filter. |
| `librarian_help` | Playbook for one server. No `topic` = overview; with `topic` = drill-down. `server="librarian"` returns the librarian's own playbook. `topic="manifest_schema"` returns the TOML reference. |
| `librarian_search` | Intent-aware fuzzy match across every known tool's name, description, and manifest `tool_aliases` phrases. Stop-word filtered. |
| `librarian_note` | Agent appends an observation about a server's behavior. Soft-dedup at write time on (server, tool, kind, normalized claim). `allow_duplicate=true` to bypass. |
| `librarian_seed_playbook` | Bootstrap a single hosted/cloud server entry from the tool list the agent already sees in its deferred-tools reminder. No approval gate — for fast incremental "I just noticed a new server" additions. |
| `librarian_seed_batch` | Bulk-seed many hosted servers in one approved transaction. Two-step propose/commit gate (one user approval covers the whole batch). The recommended path for first-install onboarding when there are 5+ hosted MCPs to register. |
| `librarian_seed_remove` | Remove a server entry from the index. For cleaning up stale seeds (servers no longer connected). Two-step propose/commit gate. Manifests and learned notes are not deleted; warning surfaces if removal won't be permanent. |
| `librarian_refresh` | Reprobe local stdio servers. Flags notes whose schemas drifted. |
| `librarian_manifest_write` | Author or replace a manifest. Two-step propose/commit gate: first call returns a structured preview + a single-use token; second call with that token + the same content (content-fingerprinted) commits. |
| `librarian_manifest_diff` | Show what changed between current and the auto-backup. Read-only. mtime-labeled to disambiguate post-restore direction. |
| `librarian_manifest_restore` | Swap current ↔ backup. Two-step propose/commit gate. Reversible: a second restore undoes the first. |
| `librarian_fetch_docs` | Fetch public documentation pages (HTTP/HTTPS only, SSRF-guarded, 5 MiB response cap, 7-day cache) for bootstrapping a manifest from vendor docs. |

For the agent-facing version of this with workflows and gotchas, call `librarian_help("librarian")` — that's the librarian's own playbook.

## Manifests — your canonical playbooks

A manifest is a single TOML file at `<config>/manifests/<server>.toml`. Five sections, all optional:

```toml
# Root-level (must come BEFORE any [section] header — TOML grammar requirement)
gotchas = [
    "Channel IDs are not the same as channel names — resolve via chat_search first.",
    "Bots cannot post in channels they haven't been invited to.",
]

# Top-level summary, used in librarian_list
[meta]
category = "comms"
summary  = "Team chat — channels, threads, reactions."
# paired_cli = "team-chat-cli"  # optional, future use

# Manually-curated tool groupings (overrides the auto-prefix grouping
# the librarian falls back to when no categories are defined)
[[tool_categories]]
name  = "Read"
tools = ["chat_read_channel", "chat_read_thread", "chat_search"]

[[tool_categories]]
name  = "Write"
tools = ["chat_send_message", "chat_add_reaction"]

# Long-form workflows surfaced in the overview
[[workflows]]
title = "Post a threaded reply"
body  = """
1. chat_search to find the parent message
2. chat_send_message with `thread_ts` set to the parent's `ts`
"""

# Drill-down topics — accessible via librarian_help(server, topic=name)
[[topics]]
name  = "rate_limits"
title = "Rate limits & retry semantics"
body  = """
Posting is gated at ~1 message per second per channel.
On 429, back off for the value of the Retry-After header.
"""

# Optional: per-tool intent phrases, folded into librarian_search ranking
# so natural-language queries find the right tool even when the wording
# doesn't appear in the tool's name or description.
[[tool_aliases]]
tool    = "chat_send_message"
phrases = ["post to channel", "send a message", "reply in thread"]
```

For the full schema reference (every field documented, plus the TOML grammar trap with root keys vs sections), have the agent call:

```
librarian_help("librarian", topic="manifest_schema")
```

That's how the librarian explains its own format — the answer to "how do I structure this?" is one tool call away.

## Workflow recipes

Three patterns cover most of how I use the librarian day-to-day.

### 1. Cold orientation (the canonical agent flow)

```
agent calls librarian_list()                # 1 call, full landscape
agent identifies the server it needs
agent calls librarian_help(server)          # workflows + categories + gotchas
agent calls librarian_help(server, topic)   # drill into a specific workflow if needed
agent calls the actual tool from that server, correctly first try
```

### 2. Bootstrapping a hosted/cloud server (e.g. Notion, Slack-claude-mediated, Figma)

The librarian can't probe these by spawning. You bootstrap a manifest from vendor docs:

```
1. agent calls librarian_fetch_docs(url="https://docs.vendor.com/mcp",
                                    extra_urls=["https://docs.vendor.com/mcp/auth"])
   → cleaned text from the docs (HTML stripped, 5 MiB cap, public hosts only)
2. agent synthesizes a Manifest in TOML based on what it read
3. agent calls librarian_manifest_write(server="vendor",
                                        manifest_toml="<toml string>")
   → returns a structured preview + a confirm_token
4. You read the preview. You type "I agree" / "yes".
5. agent calls librarian_manifest_write(...same content..., confirm_token="...")
   → commits to disk. Future sessions see it via librarian_list.
```

The propose/commit gate is **code-enforced**, not social-rule. The token is single-use, expires in 5 minutes, and is fingerprinted to the exact manifest content — committing a different shape rejects.

### 3. Filing what an agent learned

When an agent discovers a non-obvious behavior, it files a note:

```
librarian_note(
    server="slack",
    tool="chat_send_message",
    kind="behavior",              # or workflow / arg_shape / error_pattern / tip / example
    basis="observed",             # or "inferred" — renders weaker
    claim="Posting to a channel the bot hasn't been invited to silently no-ops, no error.",
    tags=["permissions", "footgun"]
)
```

Next session's `librarian_help("slack")` shows this note under Gotchas, dated. If a future `librarian_refresh` detects the underlying `chat_send_message` schema changed, the note gets flagged `⚠possibly stale`.

Dedup at write time prevents the same observation from accumulating across sessions; `allow_duplicate=true` is the escape hatch for intentional re-emphasis.

## Storage paths

Resolved via the [`directories`](https://docs.rs/directories) crate. On Windows:

- Cache: `%LOCALAPPDATA%\netviper\mcp-librarian\cache\index.json`
- Doc cache: `%LOCALAPPDATA%\netviper\mcp-librarian\cache\docs\<hash>.json`
- Manifests: `%APPDATA%\netviper\mcp-librarian\config\manifests\<server>.toml` (+ `.bak`)
- Lock: `%APPDATA%\netviper\mcp-librarian\config\.librarian.lock`
- Learned notes: `%LOCALAPPDATA%\netviper\mcp-librarian\data\learned\<server>.jsonl`

macOS/Linux paths follow the same crate's conventions but are marked `TODO: verify` in the source — I'm on Windows; patches welcome.

## Environment & CLI

### Environment overrides

- `MCP_LIBRARIAN_CONFIG` — point at a JSON file with an `{ "mcpServers": { ... } }` block, used in addition to the standard locations. Useful for tests and non-standard setups.
- `RUST_LOG` — standard `tracing_subscriber` filter; defaults to `warn,mcp_librarian=info`.

### CLI

```
mcp-librarian serve                # MCP server on stdio (the mode clients use)
mcp-librarian list [--category X]  # print the list as markdown
mcp-librarian print <server> [--topic T]
mcp-librarian refresh [--server X] # reprobe schemas
mcp-librarian compact <server>     # stub: will compact learned/<server>.jsonl, not yet implemented
```

The non-`serve` subcommands are for local inspection — quick "what does this look like?" without going through an MCP client.

## Security & storage

**What the librarian stores on disk** (paths above):

- `cache/index.json` — probed tool names + descriptions. No values.
- `config/manifests/<server>.toml` — your hand-authored playbooks. Plain TOML.
- `config/manifests/<server>.toml.bak` — one-step backup written before every commit.
- `data/learned/<server>.jsonl` — agent-appended observations. Prose claims plus `kind` / `basis` metadata.
- `cache/docs/<hash>.json` — cached responses from `librarian_fetch_docs` (7-day TTL).

**What it doesn't store.** No API keys, tokens, or session credentials. Those live in your MCP client config (`.claude.json`, `claude_desktop_config.json`) and are passed to spawned servers as env vars; the librarian never reads them. Convention for `librarian_note` claims: prose, not literal arg blobs.

**Why no app-level encryption.** The data above isn't sensitive (playbooks, references to env-var *names*, public-docs cache). Any reversible scheme would need its key on disk next to the file — that's obfuscation, not security. If you want at-rest protection, BitLocker / FileVault / dm-crypt at the volume level is the right tool, not application-level.

**Don't sync `%APPDATA%\netviper\` to a less-trusted location** (OneDrive Personal, etc.) — manifests and notes go with it. The directory is created with whatever ACLs your `%APPDATA%` inherits, which is usually user-only.

**Concurrency.** If you run multiple MCP clients (e.g. Claude Code + Codex) simultaneously, each spawns its own librarian process sharing the same files. Cross-process writes are serialized via an advisory file lock at `config/.librarian.lock`. Manifest writes use write-temp-then-rename so readers always see fully-old or fully-new content. The lock is held briefly (sub-ms per write) and never during long operations like probing.

**SSRF guards on `librarian_fetch_docs`.** Rejects:

- Non-`http(s)` schemes (`file://`, `ftp://`, etc.)
- IP literals in loopback / private / link-local / multicast / CGNAT / IPv6 unique-local / IPv6 link-local ranges, plus IPv4-mapped equivalents
- `localhost` and aliases as hostnames
- Hostnames that DNS-resolve into any of the above
- Redirects to any of the above (re-validated per hop, max 5 hops)

Response bodies are capped at 5 MiB to prevent OOM.

**Known SSRF limitation.** DNS rebinding (a domain that returns a public IP during our resolve check but a private IP for reqwest's actual dial) is not mitigated — would require IP-pinning that reqwest doesn't expose cleanly. For the realistic threat model (agent hallucinates or is prompt-injected with a localhost / metadata URL), the existing guards cover it.

**Server-name validation.** The `server` argument in every tool that takes one is validated to `[A-Za-z0-9_.-]+` with no leading dot, preventing path-traversal writes via names like `../../foo`.

**Propose/commit gates.** Every write tool (`librarian_manifest_write`, `librarian_manifest_restore`, `librarian_seed_batch`, `librarian_seed_remove`) requires a two-step dance: the propose call returns a structured preview + a single-use token; the commit call must include that token AND the same content (fingerprinted). Tokens expire in 5 minutes. This is the code-rule that prevents an agent from auto-committing destructive changes — designed for the worst-case AI, not the best.

## Known limitations

- **Windows-first.** macOS/Linux paths exist but I haven't verified them. Patches welcome.
- **Local stdio probing only.** Remote/cloud MCP servers (the `claude.ai_*` family) can't be probed by spawning. Use `librarian_seed_playbook` or `librarian_fetch_docs` + `librarian_manifest_write` to bootstrap them from the tool list the agent already sees or from vendor docs.
- **No CLI playbook recursion.** Paired-CLI detection is manifest-driven; the recursive `--help` walker is a future feature.
- **No compaction of learned notes.** Files grow append-only. `mcp-librarian compact <server>` is a stub. Dedup-at-write-time substantially limits growth in practice.
- **No PII scrubbing.** Convention: `claim` is prose, not literal arg blobs.
- **Single-host only.** No clustering, no shared state across machines. Each machine has its own librarian state.

## Project docs

- [CHANGELOG.md](CHANGELOG.md) — release history, [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format
- [CONTRIBUTING.md](CONTRIBUTING.md) — dev loop, style, philosophy (code-rule gates, loud failures, atomic writes)
- [SECURITY.md](SECURITY.md) — threat model, what we defend against, what we deliberately don't, how to report

## Licenses & contributing

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE). Pick whichever fits.

Patches welcome. Feature requests without patches will be politely declined — this is a personal tool, OSS'd because someone else might want it. See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop and house style.
