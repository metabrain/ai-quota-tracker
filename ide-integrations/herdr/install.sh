#!/usr/bin/env bash
# Installs/refreshes the Herdr sidebar integration from this repo checkout:
#   1. copies herdr-quota-poller.sh into ~/.local/bin
#   2. ensures ~/.config/herdr/config.toml has the [ui.sidebar.agents] $quota row
#   3. (re)starts the poller and reloads the Herdr server config
#
# Safe to re-run any time you pull repo changes to this integration —
# it always re-copies from this directory rather than editing in place.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
POLLER_SRC="${SCRIPT_DIR}/herdr-quota-poller.sh"
POLLER_DEST="${BIN_DIR}/herdr-quota-poller.sh"
HERDR_CONFIG="${HOME}/.config/herdr/config.toml"
STATE_DIR="${HOME}/.local/state/herdr-quota-poller"
LOG_FILE="${STATE_DIR}/poller.log"

echo "==> Installing poller script"
mkdir -p "$BIN_DIR"
cp "$POLLER_SRC" "$POLLER_DEST"
chmod +x "$POLLER_DEST"
echo "    $POLLER_DEST"

echo "==> Checking Herdr config for the \$quota sidebar row"
mkdir -p "$(dirname "$HERDR_CONFIG")"
touch "$HERDR_CONFIG"
if grep -q '\[ui\.sidebar\.agents\]' "$HERDR_CONFIG"; then
  if grep -q '\$quota' "$HERDR_CONFIG"; then
    echo "    already configured, leaving as-is"
  else
    echo "    [ui.sidebar.agents] exists but has no \$quota token — not editing it automatically."
    echo "    Add \"\$quota\" to a row yourself, e.g.:"
    echo '      rows = [["state_icon", "machine", "workspace", "tab"], ["agent"], ["$quota"]]'
  fi
else
  {
    echo ""
    echo "[ui.sidebar.agents]"
    echo 'rows = [["state_icon", "machine", "workspace", "tab"], ["agent"], ["$quota"]]'
  } >> "$HERDR_CONFIG"
  echo "    appended [ui.sidebar.agents] block to $HERDR_CONFIG"
fi

echo "==> Restarting poller"
pkill -f "$POLLER_DEST" 2>/dev/null || true
mkdir -p "$STATE_DIR"
nohup "$POLLER_DEST" > "$LOG_FILE" 2>&1 &
disown
echo "    running (pid $!), logging to $LOG_FILE"

echo "==> Reloading Herdr server config"
if command -v herdr >/dev/null 2>&1 && herdr status >/dev/null 2>&1; then
  herdr server reload-config
else
  echo "    herdr server not reachable, skipped — reload manually with 'herdr server reload-config'"
fi

echo "==> Checking ai-quota-tracker daemon"
SOCKET="${QUOTA_SOCKET_PATH:-/dev/shm/ai_quota_cache.sock}"
if [ -S "$SOCKET" ]; then
  echo "    socket present at $SOCKET"
else
  echo "    WARNING: no socket at $SOCKET — start the daemon first (see top-level README)"
fi

echo "==> Done"
