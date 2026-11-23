#!/usr/bin/env bash
# M0 spike (d): Claude Code as the Orchestrator, driven headlessly via stream-json.
# Usage: spikes/orchestrator-cc/run.sh [turns.jsonl] [--resume <session-id>]
# Spends real tokens on your Claude Code login (~$0.08 for the bundled 5 turns).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
TURNS="${1:-$HERE/turns.jsonl}"; shift || true
OUT="${OUT:-$(mktemp -d -t orch-spike)}"
mkdir -p "$OUT/home"
cat > "$OUT/settings.json" <<JSON
{
  "permissions": { "allow": ["mcp__orch__ws_read_file", "mcp__orch__queue_list", "mcp__orch__delegate_task"] },
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "HOOK_DIR=$OUT python3 -I $HERE/hook.py" }] }],
    "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "HOOK_DIR=$OUT INLINE_BUDGET=${INLINE_BUDGET:-3} python3 -I $HERE/hook.py" }] }]
  }
}
JSON
cat > "$OUT/mcp.json" <<JSON
{ "mcpServers": { "orch": { "command": "$ROOT/target/debug/mcp-stub", "env": { "MCP_LOG": "$OUT/mcp.log" } } } }
JSON
cd "$OUT/home"
# NOTE: queued stream-json messages are merged into one turn; the real service must
# send one prompt and wait for the `result` message before sending the next.
claude -p --input-format stream-json --output-format stream-json --verbose \
  --model claude-opus-5-5 --effort medium \
  --tools "" --strict-mcp-config --mcp-config "$OUT/mcp.json" \
  --setting-sources "" --settings "$OUT/settings.json" \
  --system-prompt "$(cat "$HERE/system.md")" --permission-prompts none "$@" \
  < "$TURNS" > "$OUT/out.jsonl"
echo "transcript: $OUT/out.jsonl  hooks: $OUT/hooks.log  mcp: $OUT/mcp.log"
