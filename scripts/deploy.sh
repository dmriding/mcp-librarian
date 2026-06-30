#!/usr/bin/env bash
# mcp-librarian deploy (macOS / Linux).
#
# Builds release and installs the binary to the canonical local MCP-server
# location, so Claude Desktop / Claude Code pick up the new build on restart.
#
#   Binary: ~/.local/share/mcp-servers/<name>/<name>   (override: $MCP_SERVERS_ROOT)
#   Config: platform config dir (dev/netviper/mcp-librarian), or $LIBRARIAN_CONFIG
#
# Run from anywhere: ./scripts/deploy.sh
set -euo pipefail

NAME="mcp-librarian"
SERVERS_ROOT="${MCP_SERVERS_ROOT:-$HOME/.local/share/mcp-servers}"
DEST_DIR="$SERVERS_ROOT/$NAME"
DEST="$DEST_DIR/$NAME"

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_DIR/target/release/$NAME"

echo "==> [1/4] Stopping any running $NAME instances (spawned by Claude)..."
if pkill -x "$NAME" 2>/dev/null; then echo "    killed running instance(s)"; else echo "    none running"; fi

echo "==> [2/4] Building release..."
( cd "$REPO_DIR" && cargo build --release )

echo "==> [3/4] Installing to $DEST ..."
mkdir -p "$DEST_DIR"
install -m 755 "$SRC" "$DEST"

echo "==> [4/4] Health check..."
"$DEST" --version

cat <<EOF

Deploy complete.
  Binary: $DEST
  Config: \${LIBRARIAN_CONFIG:-platform config dir (dev/netviper/mcp-librarian)}

Restart Claude Desktop and Claude Code so they respawn the cached MCP child process.
EOF
