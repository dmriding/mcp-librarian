# Contributing

This is a personal tool, open-sourced because someone else might want it. The policy is opinionated:

- **Patches welcome.** PRs that fix real bugs, harden security, or improve agent ergonomics are likely to land.
- **Feature requests without patches will be politely declined.** Open an issue if you want to discuss before sending a PR — that's helpful and won't be ignored. But please don't expect new capabilities to materialize because they'd be nice; if you want it, build it.
- **Scope discipline matters.** This is not a generic notes system, a proxy for MCP servers, a credential manager, or a multi-user service. See the [README](README.md#what-this-is-not) for what this isn't.

---

## Dependency policy

Use the shortest caret range that pins what you need — `"0.8"` rather than `"0.8.0"` when nothing in the patch line matters, and never `"0.8.*"` or other wildcards. `Cargo.lock` is committed for reproducibility. New dependencies need a one-line "why" in the PR description. `cargo audit` runs as an advisory CI job — surfaces CVEs without blocking every push. `cargo deny check` (config in [deny.toml](deny.toml)) enforces the license allow-list and bans wildcard versions.

---

## Quick dev loop

```powershell
cargo test --all-targets                     # 69 unit + 72 integration
cargo clippy --all-targets -- -D warnings    # zero-warnings policy
cargo fmt --all -- --check                   # formatting clean
cargo build --release                        # binary at target/release/mcp-librarian.exe
```

A PR is ready when all four pass. CI runs the same set.

For interactive testing against a live MCP client:

1. Stop the running MCP server process (the client's child process for `mcp-librarian`)
2. `cargo build --release`
3. Copy `target/release/mcp-librarian.exe` to wherever your client config points
4. Restart your MCP client (Claude Code / Claude Desktop / Codex) so it spawns the new binary

A scripted version of this loop lives outside the repo — deploy paths are user-specific.

---

## Where things live

```
src/
├── main.rs           CLI entry — serve / list / print / refresh / compact
├── lib.rs            module registry
├── config.rs         Paths struct (platform-specific dirs) + server name validator
├── discovery.rs      reads MCP client configs to find servers
├── probe.rs          spawns stdio MCP servers and asks for their tool lists
├── index.rs          the on-disk index struct + learned-notes JSONL I/O
├── playbook.rs       Manifest struct + rendering (list, help, topics, previews, diffs)
├── fetch.rs          librarian_fetch_docs — HTTP w/ SSRF guards + cache
├── lockfile.rs       cross-process advisory write lock
└── server.rs         the 13 MCP tools, all gated paths, all sanitization

tests/integration.rs  shared-fixture integration tests
                      (unit tests live inline `#[cfg(test)]` in their respective src/ files)
```

User-facing docs live in this repo root: [README](README.md), [CHANGELOG](CHANGELOG.md), [SECURITY](SECURITY.md), [CODE_OF_CONDUCT](CODE_OF_CONDUCT.md), and this file. Design rationale and the running tech-debt notebook are author-private and don't ship in the repo.

---

## Tests

Two kinds:

- **Unit tests** — `#[cfg(test)] mod tests { ... }` in `src/*.rs`. Run with `cargo test --lib`. Closer to internals; can reach private helpers.
- **Integration tests** — `tests/integration.rs`. Run with `cargo test --test integration`. Public API surface only.

Concurrency-sensitive code lives in `src/server.rs` unit tests (specifically `concurrent_dedup_wins_exactly_once`, `concurrent_manifest_writes_serialize`) — these spawn threads against a real `LibrarianServer` and the file lock to verify cross-process behavior. If you add a new write-class tool, add a similar concurrency test.

If a fix is a regression guard against a specific behavior (e.g. "this output must not contain em-dashes"), make the test name say so explicitly: `seed_remove_preview_has_no_known_hang_triggers`, not `seed_remove_preview_works`. Future-you will thank you.

---

## Commit style

Short. One line where possible:

```
feat(area): one-line description
fix(area): one-line description
docs(area): one-line description
```

Areas track the rough subsystem touched (e.g. `manifest`, `seed`, `note`, `search`, `fetch`, `security`, `dx`, `readme`). Add a new area if none of the existing ones fit — keep it lowercase and one word.

If a commit really needs a body (multi-step refactor, non-obvious tradeoff), keep it under 8 lines and explain the *why*, not the *what*. The diff shows what changed.

The repo's git policy:
- Never amend or force-push to `main`
- Never commit changes the human didn't review or ask for
- Never skip hooks (`--no-verify`) without explicit user direction

---

## Style and philosophy

These aren't pedantic; they're load-bearing for the way the tool stays useful.

### Code-rule gates beat social-rule prompts

Every destructive write is gated with a propose/commit token that pins the exact content via fingerprint. **Don't add a new write-class tool without the same gate.** The whole point is that the tool refuses to misbehave even if the calling agent ignores its tool description. Compromised LLM, prompt injection, agent confusion — the gate doesn't care. Re-read `librarian_manifest_write` in `src/server.rs` as the reference implementation.

### Make the failure visible

If you find yourself writing "silent fallback" or "default to empty when unclear", stop. The librarian's design rule is **loud rejection > silent acceptance**. LLMs can't observe a silent error, so they can't learn from it. See the TOML misplaced-`gotchas` defense in `manifest_write_inner` as the canonical example — three layers of "this is wrong and here's where" rather than one layer of "we made it work somehow."

If you must accept the input, *also* tell the agent what was unexpected (echo the recorded value, or surface a `(none)` count) so they can verify.

### Diagnostics ride in the response content, not JSON-RPC error fields

Tool methods return `Ok(content)` even on validation failures. The diagnostic text (prefixed `Error: ...`) sits in the response body. **Don't return `Err(ErrorData)` from a tool method for a user-correctable failure.**

Why: multiple MCP client harnesses have been observed to swallow JSON-RPC `error.message` fields and display only "Tool execution failed" — eating the diagnostic. Returning content guarantees the message reaches the agent. The `Error:` prefix on every `anyhow::bail!()` message is the signal to the agent that this is a corrective response.

The pattern in every tool method:

```rust
async fn list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, ErrorData> {
    self.list_inner(p).map_or_else(|e| Ok(format_diagnostic(e)), Ok)
}
```

`format_diagnostic` lives in `src/server.rs`. `internal()` (kept for genuinely-internal errors that should escalate at the JSON-RPC layer) is currently unused — there are no such paths in the tool surface.

### Atomic writes, never partial state

All writes go through write-temp-then-rename. Don't add a write that does `fs::write(target, content)` directly. The rename is atomic on every supported OS; partial-write recovery isn't.

### Preview chrome stays ASCII-conservative

Claude Desktop's renderer hangs on certain markdown/Unicode combos (em-dashes + angle brackets in backticks, especially). Preview output uses ASCII hyphens, no angle-bracket placeholders, no backticks around content with punctuation. The `sanitize_for_preview()` helper exists for agent-supplied content that flows into chrome — use it. Regression tests assert each preview contains no known triggers; don't reintroduce them.

### Don't reach for unsafe; don't reach for async if sync works

The codebase is small and predictable. Almost everything is sync, with `async fn` only at MCP tool boundaries. If a new piece of code is tempted to grow a custom async/threading model, push back — usually the sync version is fine, and the cross-process file lock handles real concurrency.

### Test against the worst-case AI, not the best

When designing or reviewing a new tool, ask: "what does this do when the calling agent is confused, malicious, or has been prompt-injected?" If the answer is "it's fine because the tool description says X", that's not enough. The gate needs to be in the code, not the prose.

---

## Reporting bugs and security issues

- **Bugs**: open an issue with steps to reproduce. If it involves the agent's behavior, paste the exact tool call (server name, args) and the exact error or output. A minimal repro >> a description.
- **Security**: see [SECURITY.md](SECURITY.md) for the threat model and reporting path. Don't put credentials or org-internal data in a public issue.

---

## License

By contributing, you agree your contributions are dual-licensed under MIT and Apache-2.0, matching the project's licensing.
