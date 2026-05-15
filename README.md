# mcp-librarian

An MCP server that indexes your other MCP servers and emits playbooks on demand. The agent calls one tool, gets the landscape; drills into a single server when needed. Designed to get smarter as you use it — agents file notes about what they learn, and future sessions inherit the knowledge.

I built this for me. Patches welcome; feature requests without patches will be politely declined.

## What it does

A single Rust MCP server exposing six tools to the agent:

| Tool | What it does |
|---|---|
| `librarian_list` | Directory of all known MCP servers, grouped by category |
| `librarian_help` | Playbook for one server. No `topic` = overview; with `topic` = drill-down |
| `librarian_search` | Fuzzy-match across every known tool's name + description |
| `librarian_note` | Agent appends an observation about a server's behavior |
| `librarian_seed_playbook` | Bootstrap a server entry from the tool list the agent already sees |
| `librarian_refresh` | Reprobe local stdio servers; flag notes whose schemas drifted |

The agent's discovery cost collapses from N tool schemas to one call returning prose.

## Why

I have a lot of MCP servers connected to Claude Code. Deferred loading means the agent doesn't actually see tool schemas until it explicitly searches for them, and most MCP servers ship no `help`-style orientation tool, so the agent has to flail. This solves that for me — every server gets a uniform overview / drill-down playbook surface, whether or not the server itself was designed for that.

## How it stays useful over time

Two-layer storage, both human-readable, both editable:

- **`<config>/manifests/<server>.toml`** — *your* canon. Hand-authored TOML, takes priority in rendering. Define categories, workflows, topics, gotchas — like a curated `help` for that server.
- **`<data>/learned/<server>.jsonl`** — *agent* append-only observations. Every note carries a `kind` (`workflow` / `arg_shape` / `behavior` / `error_pattern` / `tip` / `example`) and a `basis` (`observed` for things just witnessed, `inferred` for speculation — rendered weaker).

After a `librarian_refresh`, any note whose underlying tool's arg shape has drifted gets flagged `⚠possibly stale` — not deleted, just marked. You read and decide.

## Install

```powershell
cargo install --path .
```

Or build locally:

```powershell
cargo build --release
.\target\release\mcp-librarian.exe --help
```

## Configure

Add it to your Claude Code MCP config (`~/.claude.json` on Windows: `C:\Users\<you>\.claude.json`):

```json
{
  "mcpServers": {
    "librarian": {
      "command": "C:\\path\\to\\mcp-librarian.exe",
      "args": ["serve"]
    }
  }
}
```

Then in a Claude Code session: `librarian_list()` to see what it found, `librarian_refresh()` once to probe schemas.

### Paths

Resolved via the [`directories`](https://docs.rs/directories) crate. On Windows:

- Cache: `%LOCALAPPDATA%\netviper\mcp-librarian\cache\index.json`
- Config (manifests): `%APPDATA%\netviper\mcp-librarian\config\manifests\<server>.toml`
- Learned notes: `%LOCALAPPDATA%\netviper\mcp-librarian\data\learned\<server>.jsonl`

macOS/Linux paths follow the same crate's conventions but are marked `TODO: verify` in the source — I'm on Windows; patches welcome.

### Environment overrides

- `MCP_LIBRARIAN_CONFIG` — point at a JSON file with an `{ "mcpServers": { ... } }` block, used in addition to the standard locations. Useful for tests and non-standard setups.
- `RUST_LOG` — standard `tracing_subscriber` filter; defaults to `warn,mcp_librarian=info`.

## CLI

```
mcp-librarian serve                # MCP server on stdio (the mode Claude Code uses)
mcp-librarian list [--category X]  # print the list as markdown
mcp-librarian print <server> [--topic T]
mcp-librarian refresh [--server X] # reprobe schemas
mcp-librarian compact <server>     # stub: will compact learned/<server>.jsonl, not yet implemented
```

## Manifest format

`<config>/manifests/<server>.toml`:

```toml
[meta]
category = "comms"
summary  = "Team chat — channels, threads, reactions."
# paired_cli = "team-chat-cli"  # optional, future use

# Optional manually-curated tool groupings (overrides auto-prefix grouping)
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

# Drill-down topics
[[topics]]
name  = "rate_limits"
title = "Rate limits & retry semantics"
body  = """
Posting is gated at ~1 message per second per channel.
On 429, back off for the value of the Retry-After header.
"""

# Surprising behaviors that would trip up future agents
gotchas = [
  "Channel IDs are not the same as channel names — resolve via chat_search first",
  "Bots cannot post in channels they haven't been invited to",
]
```

## Known limitations

- **Windows-first.** macOS/Linux paths exist but I haven't verified them. Patches welcome.
- **Local stdio probing only.** Remote/cloud MCP servers (the `claude.ai_*` family) can't be probed by spawning. Use `librarian_seed_playbook` to bootstrap them from the tool list the agent already sees in its context.
- **No CLI playbook recursion.** Paired-CLI detection is manifest-driven; the recursive `--help` walker is a future feature.
- **No compaction of learned notes.** Files grow append-only. `mcp-librarian compact <server>` is a stub.
- **No PII scrubbing.** Convention: `claim` is prose, not literal arg blobs. No customer data in, no customer data out.

## Licenses

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE). Pick whichever fits.

## Contributing

Patches welcome. Feature requests without patches will be politely declined — this is a personal tool, OSS'd because someone else might want it. Open an issue if you want to discuss before sending a PR.
