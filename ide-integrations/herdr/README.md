# Herdr sidebar integration

Surfaces `ai-quota-tracker`'s `/quota` data directly in the
[Herdr](https://herdr.dev) sidebar, next to each live `claude` or `codex`
agent pane — so you can see 5h/weekly usage and the next reset without
leaving your terminal workspace manager.

Herdr has no plugin system for custom sidebar widgets. This works entirely
through two things Herdr's CLI/config already expose:

- `herdr pane report-metadata <pane_id> --token name=value` — attaches
  arbitrary key/value "tokens" to a pane, which Herdr keeps until they
  expire (`--ttl-ms`) or are overwritten.
- `[ui.sidebar.agents]` in `config.toml` — lets you add a custom row of
  tokens (built-in or `$name` ones from pane metadata) to each agent's
  sidebar entry.

## How it works

`herdr-quota-poller.sh` runs in a loop (default: every 60s). Each cycle it:

1. Curls the daemon's `/quota` endpoint over its Unix socket.
2. Formats a compact per-provider string with `jq`, e.g.
   `5h 43% (↻2h4m)  wk 48%` — used% and time until the 5h window resets.
3. Lists live agents with `herdr agent list`, and for each pane running a
   `claude` or `codex` agent, pushes that provider's string as the pane's
   `quota` token via `herdr pane report-metadata`.

Because the token is scoped per-pane, a `claude` pane shows Anthropic's
numbers and a `codex` pane shows Codex's numbers automatically — no
per-pane configuration needed.

## Setup

1. Copy (or symlink) the script somewhere on `PATH`, e.g.:

   ```bash
   cp herdr-quota-poller.sh ~/.local/bin/
   chmod +x ~/.local/bin/herdr-quota-poller.sh
   ```

2. Add a sidebar row for the `$quota` token to `~/.config/herdr/config.toml`:

   ```toml
   [ui.sidebar.agents]
   rows = [["state_icon", "machine", "workspace", "tab"], ["agent"], ["$quota"]]
   ```

   (Adjust the other rows to match your existing config — only the
   `["$quota"]` row is required for this integration. Putting it on its own
   row, rather than appending it to the `agent` row, keeps it readable.)

3. Reload Herdr's config without restarting your session:

   ```bash
   herdr server reload-config
   ```

4. Make sure `ai-quota-tracker` itself is running (see the top-level
   [README](../../README.md#run-manually)), then start the poller:

   ```bash
   nohup ~/.local/bin/herdr-quota-poller.sh > /tmp/herdr-quota-poller.log 2>&1 &
   disown
   ```

Neither the daemon nor the poller is a systemd/user service by default —
both are plain background processes here and won't survive a reboot. Wrap
either in a systemd `--user` unit if you want that.

## Configuration

| Env var | Default | Purpose |
| --- | --- | --- |
| `QUOTA_SOCKET_PATH` | `/dev/shm/ai_quota_cache.sock` | Matches the daemon's own `QUOTA_SOCKET_PATH`. |
| `HERDR_QUOTA_POLL_INTERVAL` | `60` (seconds) | How often to poll and republish. The pane token TTL is set to this plus 30s, so a token goes stale automatically if the poller stops. |

## Customizing the displayed string

Edit the `JQ_FILTER` at the top of `herdr-quota-poller.sh` — it's the only
place the sidebar string is built. Current output shape:

```
5h <used%> (↻<hours>h<minutes>m)  wk <used%>
```

Common tweaks:

- Drop the weekly figure: use only `fmt5h("anthropic")` / `fmt5h("codex")`.
- Drop the reset countdown: remove the `relm(...)` call from `fmt5h`.
- Change labels/symbols (`5h`, `wk`, `↻`) to taste.
- Add a warning marker above a threshold, e.g. prefix with `⚠` when
  `used_percent >= 90`.

After editing, restart the poller (`pkill -f herdr-quota-poller.sh`, then
relaunch as in step 4). No `herdr server reload-config` is needed for
wording-only changes — that's only required when the `config.toml` row
layout itself changes.
