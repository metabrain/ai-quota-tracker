# ai-quota-tracker

A hyper-lightweight, **RAM-only** background daemon that centralises and caches
AI provider quota / subscription metrics. Clients pull JSON over a Unix domain
socket — no TCP, no disk, no fuss.

## How it works

- State lives entirely in process memory. The socket lives on a tmpfs mount
  (`/dev/shm/ai_quota_cache.sock` by default), so nothing ever touches disk.
- **Lazy / pull-based caching:** provider APIs are only contacted when a local
  request arrives *and* the in-memory TTL has expired. Otherwise the daemon
  idles on the socket.
- The socket is `chmod 600` — only the owning user can connect.
- On `SIGINT`/`SIGTERM` the daemon shuts down gracefully and unlinks the
  socket file.

## Build

```bash
cargo build --release
# binary: ./target/release/ai-quota-tracker
```

## Run manually

```bash
# demo mode (default): providers return synthetic metrics without API keys
./target/release/ai-quota-tracker

# with real keys
OPENAI_API_KEY=sk-... OPENAI_GRANTED_USD=120 QUOTA_DEMO_MODE=0 \
  ./target/release/ai-quota-tracker

# tuning
QUOTA_SOCKET_PATH=/dev/shm/ai_quota_cache.sock QUOTA_CACHE_TTL_SECS=300 \
QUOTA_FETCH_TIMEOUT_SECS=15 ./target/release/ai-quota-tracker
```

## Query

```bash
curl --unix-socket /dev/shm/ai_quota_cache.sock http://localhost/quota
curl --unix-socket /dev/shm/ai_quota_cache.sock http://localhost/quota/refresh  # force refresh
curl --unix-socket /dev/shm/ai_quota_cache.sock http://localhost/health
```

Note: curl must run as the same user that owns the socket (`chmod 600`).

## Install as a systemd service

```bash
sudo useradd -r -s /usr/sbin/nologin aiquota
sudo install -m 755 target/release/ai-quota-tracker /usr/local/bin/
sudo mkdir -p /etc/ai-quota-tracker
sudo install -m 600 keys.env.example /etc/ai-quota-tracker/keys.env
# ... edit /etc/ai-quota-tracker/keys.env with real keys ...
sudo install -m 644 ai-quota-tracker.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ai-quota-tracker
systemctl status ai-quota-tracker
```

## Providers

| Key | Source | What it reports |
| --- | ------ | --------------- |
| `openai` | `OPENAI_API_KEY` (+ optional `OPENAI_GRANTED_USD`) | Platform billing: trailing-30d USD spend vs your configured grant |
| `codex` | `~/.codex/auth.json` (from `codex login`, ChatGPT flow) | ChatGPT subscription: plan, 5h + weekly windows, credits |
| `anthropic` | `CLAUDE_CODE_OAUTH_TOKEN` (or `ANTHROPIC_OAUTH_TOKEN`, or `~/.claude/.credentials.json` from `claude login`) | Claude Code subscription: all OAuth usage windows (`five_hour` → `5h`, `seven_day` → `weekly`, per-model windows, plus any future buckets) |

Codex details:
- Reads the CLI-owned `auth.json` read-only; token refresh stays with the
  `codex` CLI (a 401 tells you to run `codex login` again).
- `CODEX_HOME` overrides `~/.codex`; `CODEX_ACCESS_TOKEN` /
  `CODEX_ACCOUNT_ID` override the file (multi-account setups, tests).
- `CODEX_BASE_URL` overrides `https://chatgpt.com/backend-api` (debugging).
- API-key mode (`OPENAI_API_KEY` in `auth.json`) has no usage endpoint and is
  reported as not configured.

## Response shape

`GET /quota` returns per-provider **billing** (USD spend) and/or
**subscription** windows (e.g. Claude's 5-hour and weekly limits):

```json
{
  "updated_at": 1757546400,
  "providers": {
    "anthropic": {
      "subscription": {
        "plan": "claude-code",
        "windows": [
          { "window": "5h", "used": 23.5, "used_percent": 23.5, "resets_at": 1757553600 },
          { "window": "weekly", "used": 41.2, "used_percent": 41.2, "resets_at": 1757805600 }
        ]
      }
    },
    "codex": {
      "subscription": {
        "plan": "pro",
        "windows": [
          { "window": "5h", "used": 15.0, "used_percent": 15.0, "resets_at": 1757553600, "window_seconds": 18000 },
          { "window": "weekly", "used": 5.0, "used_percent": 5.0, "resets_at": 1757805600, "window_seconds": 604800 }
        ]
      }
    },
    "openai": {
      "billing": {
        "total_granted": 120.0,
        "total_used": 45.22,
        "remaining_balance": 74.78,
        "reset_timestamp": 1758842400
      }
    }
  },
  "errors": {}
}
```

Providers that fail keep serving their last-known-good metrics; the failure is
surfaced in `errors` instead of failing the whole response.

## Adding a provider

Add a module under `src/providers/` implementing the `QuotaProvider` trait
(see `src/providers/codex.rs` for a full example):

```rust
struct MyProvider { /* api key, etc. */ }

#[async_trait]
impl QuotaProvider for MyProvider {
    fn name(&self) -> &'static str { "myprovider" }
    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
        // hit the provider API with the shared reqwest client, map to ProviderQuota
        // (BillingQuota and/or SubscriptionQuota with 5h/weekly UsageWindows)
    }
}
```

then add it to the `providers` vec in `main()` and cover the parsing with
unit tests (`cargo test`).

## Development

```bash
cargo test                          # unit tests: parsing, auth loading, cache TTL
cargo clippy --all-targets          # must be warning-free
cargo fmt --check                   # rustfmt clean
```

## Contributing

This README is the source of truth for how the daemon behaves — code and docs
are kept in sync in the same change. All work is tracked by a pre-existing
GitHub Issue. See [AGENTS.md](AGENTS.md) for the full rules (they apply to human
and AI contributors alike).
