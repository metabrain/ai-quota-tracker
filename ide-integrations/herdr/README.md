# Herdr sidebar integration

Surfaces `ai-quota-tracker`'s `/quota` data directly in the
[Herdr](https://herdr.dev) sidebar, next to each live `claude` or `codex`
agent pane — so you can see 5h/weekly usage and the next reset without
leaving your terminal workspace manager.

Herdr does have a plugin system (`herdr plugin install`/`link`), but as of
v0.9.0 it's confined to invokable actions and a narrow set of lifecycle
events (e.g. `worktree.created`) — its own docs say "native non-terminal
plugin UI" and runtime action registration are explicitly out of scope for
plugin v1. There's no plugin hook for sidebar UI, no periodic/poll hook, and
no plugin-side equivalent of `report-metadata`. So this integration instead
works entirely through two things Herdr's CLI/config already expose:

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

Make sure `ai-quota-tracker` itself is running first (see the top-level
[README](../../README.md#run-manually)), then run the installer:

```bash
./install.sh
```

It's idempotent — re-run it any time after pulling repo changes to this
integration, or after editing `herdr-quota-poller.sh` in place here. Each
run:

1. Copies `herdr-quota-poller.sh` into `~/.local/bin/` (overwriting any
   previous copy — this directory is the source of truth, not `~/.local/bin`).
2. Appends the `[ui.sidebar.agents]` `$quota` row to
   `~/.config/herdr/config.toml` if it isn't already configured. It never
   edits an existing `[ui.sidebar.agents]` block for you — if one exists
   without `$quota`, it prints the snippet to add by hand.
3. Restarts the poller in the background, logging to
   `~/.local/state/herdr-quota-poller/poller.log`.
4. Runs `herdr server reload-config`.
5. Warns if the daemon's socket isn't present.

Neither the daemon nor the poller is a systemd/user service by default —
both are plain background processes and won't survive a reboot. Wrap either
in a systemd `--user` unit if you want that; `install.sh` doesn't do this.

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

After editing, just re-run `./install.sh` to redeploy and restart the
poller. No `herdr server reload-config` is needed for wording-only changes
— that's only required when the `config.toml` row layout itself changes,
which `install.sh` also handles on first install.
