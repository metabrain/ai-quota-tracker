#!/usr/bin/env bash
# Polls ai-quota-tracker over its Unix socket and republishes per-provider
# usage as Herdr *pane* metadata on each live agent's pane, so it shows up
# next to that specific agent in the sidebar via the $quota token configured
# under [ui.sidebar.agents] in config.toml. A claude pane gets the Anthropic
# figure, a codex pane gets the Codex figure, including a countdown to the
# next 5h-window reset.
set -u

SOCKET="${QUOTA_SOCKET_PATH:-/dev/shm/ai_quota_cache.sock}"
INTERVAL="${HERDR_QUOTA_POLL_INTERVAL:-60}"
TTL_MS=$(( (INTERVAL + 30) * 1000 ))

# Emits "claude=<str>\tcodex=<str>" (tab-separated) from a /quota payload.
JQ_FILTER='
  def w(name; win): (.providers[name].subscription.windows[]? | select(.window==win));
  def relm(win):
    (win.resets_at // 0) as $ts |
    if $ts == 0 then ""
    else ( ($ts - now) as $d |
      if $d <= 0 then " (resetting)"
      else " (↻\($d/3600|floor)h\(($d % 3600 / 60)|floor)m)"
      end )
    end;
  def fmt5h(name):
    (w(name;"5h")) as $x |
    if $x == null then "-" else "\($x.used_percent|floor)%\(relm($x))" end;
  def fmtwk(name):
    (w(name;"weekly")) as $x |
    if $x == null then "-" else "\($x.used_percent|floor)%" end;
  ( "5h " + fmt5h("anthropic") + "  wk " + fmtwk("anthropic") ) as $claude |
  ( "5h " + fmt5h("codex") + "  wk " + fmtwk("codex") ) as $codex |
  "\($claude)\t\($codex)"
'

while true; do
  quota_line=$(curl -sf --max-time 5 --unix-socket "$SOCKET" http://localhost/quota | jq -r "$JQ_FILTER" 2>/dev/null)
  if [ -n "$quota_line" ]; then
    claude_quota="${quota_line%%$'\t'*}"
    codex_quota="${quota_line#*$'\t'}"
  else
    claude_quota="offline"
    codex_quota="offline"
  fi

  herdr agent list 2>/dev/null | jq -r '.result.agents[] | "\(.agent)\t\(.pane_id)"' 2>/dev/null |
  while IFS=$'\t' read -r kind pane_id; do
    case "$kind" in
      claude) value="$claude_quota" ;;
      codex)  value="$codex_quota" ;;
      *)      continue ;;
    esac
    herdr pane report-metadata "$pane_id" --source ai-quota-poller \
      --token "quota=$value" --ttl-ms "$TTL_MS" >/dev/null 2>&1
  done

  sleep "$INTERVAL"
done
