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
- The socket is created `0600` (umask is tightened around `bind()` so there is
  no world-readable window) — only the owning user can connect.
- On `SIGINT`/`SIGTERM` the daemon shuts down gracefully and unlinks the
  socket file.

## Security & credentials

- **Access control is the socket mode.** Any process running as the socket
  owner can read `/quota` (and every token-derived number in it). Keep the
  socket on a per-user tmpfs; don't loosen the `0600`.
- **The daemon reads provider CLI credential files, read-only.** With no env
  override it will pick up `~/.codex/auth.json` (`codex login`),
  `~/.claude/.credentials.json` (`claude login`) and the `muse` session file.
  It never writes them and never refreshes tokens — that stays with each CLI.
- **Those tokens are sent to the provider's own API** (`api.openai.com`,
  `chatgpt.com`, `api.anthropic.com`) over TLS, and nowhere else. Nothing is
  written to disk: the cache is RAM-only and the socket is on tmpfs.
- Run it as **your** user (or a dedicated service user with its own copies of
  the credentials) — not root, and not a user other people can `su` to.

## Build

```bash
cargo build --release
# binary: ./target/release/ai-quota-tracker
```

## Run manually

```bash
# demo mode (default): providers with no credentials return stand-in metrics
# in their real response shape (billing for openai, subscription windows for
# codex/anthropic); muse has no quota API so it still reports `unsupported`
./target/release/ai-quota-tracker

# with real keys
OPENAI_API_KEY=sk-... OPENAI_GRANTED_USD=120 QUOTA_DEMO_MODE=0 \
  ./target/release/ai-quota-tracker

# tuning
QUOTA_SOCKET_PATH=/dev/shm/ai_quota_cache.sock QUOTA_CACHE_TTL_SECS=300 \
QUOTA_FETCH_TIMEOUT_SECS=15 ./target/release/ai-quota-tracker
```

`QUOTA_CACHE_TTL_SECS` and `QUOTA_FETCH_TIMEOUT_SECS` take integer seconds; a
set value that fails to parse logs a warning and falls back to the default.

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

Currently supported (the `providers` map keys in `/quota`):

| Key | Source | What it reports |
| --- | ------ | --------------- |
| `openai` | `OPENAI_API_KEY` (+ optional `OPENAI_GRANTED_USD`) | Platform billing: trailing-30d USD spend vs your configured grant |
| `codex` | `~/.codex/auth.json` (from `codex login`, ChatGPT flow) | ChatGPT subscription: plan, 5h + weekly windows, credits |
| `anthropic` | `CLAUDE_CODE_OAUTH_TOKEN` (or `ANTHROPIC_OAUTH_TOKEN`, or `~/.claude/.credentials.json` from `claude login`) | Claude Code subscription: all OAuth usage windows (`five_hour` → `5h`, `seven_day` → `weekly`, per-model windows, plus any future buckets) |
| `muse` | `muse login` session (`MUSE_AUTH_PATH` → `$XDG_CONFIG_HOME/muse/auth.json` → `~/.config/muse/auth.json`) | Sign-in / billing state only — see below |

Codex details:
- Reads the CLI-owned `auth.json` read-only; token refresh stays with the
  `codex` CLI (a 401 tells you to run `codex login` again).
- `CODEX_HOME` overrides `~/.codex`; `CODEX_ACCESS_TOKEN` /
  `CODEX_ACCOUNT_ID` override the file (multi-account setups, tests).
- `CODEX_BASE_URL` overrides `https://chatgpt.com/backend-api` (debugging).
- API-key mode (`OPENAI_API_KEY` in `auth.json`) has no usage endpoint and is
  reported as not configured.

Muse details:
- Muse exposes **no quota API** a polling daemon can use: the Model API has
  no aggregate billing/usage endpoint, and subscription windows are only
  delivered as SSE events on streaming turns. The provider therefore reports
  an explicit `unsupported` status instead of synthetic data.
- It does surface what is observable, read-only: whether the `muse` CLI is
  installed, whether a `muse login` session exists (the `providers` map in
  `auth.json` — `{"providers": {}}` means signed out), and billing
  precedence: `META_API_KEY` moves Muse onto per-token Model API billing
  ahead of any stored subscription session.

### Not yet supported

Candidates, roughly in order of how cleanly they'd fit. "Feasible" means a
polling daemon can read a usage/billing number without a browser session or
scraping.

| Provider | Feasible? | Notes |
| --- | --- | --- |
| Google Gemini / AI Studio | partial | Cloud Billing API gives spend for a GCP project (needs a service account + `roles/billing.viewer`); AI Studio free-tier keys have no usage endpoint. |
| Mistral (La Plateforme) | likely | Has a billing/usage area; needs confirmation of a stable API endpoint vs. dashboard-only. |
| xAI (Grok) | likely | Console exposes credits/usage; API surface not yet verified here. |
| OpenRouter | yes | `GET /api/v1/auth/key` returns `limit` / `usage` for the key — easy add. |
| DeepSeek | likely | `GET /user/balance` returns granted/available credits. |
| GitHub Copilot | no (individual) | No per-user quota API; org/enterprise billing is admin-only via the GitHub billing API. |
| Cursor | no | Usage lives behind the dashboard/session; no documented API. |
| AWS Bedrock / Azure OpenAI | project-level only | Spend comes from the cloud provider's Cost Explorer / Cost Management APIs, not the model endpoint. |

Contributions welcome — open an issue first (see [Contributing](#contributing)),
then implement `QuotaProvider` as in [Adding a provider](#adding-a-provider).

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
        "reset_timestamp": 0
      }
    }
  },
  "errors": {}
}
```

Providers that fail keep serving their last-known-good metrics; the failure is
surfaced in `errors` instead of failing the whole response.

Timestamp convention: a `resets_at` / `reset_timestamp` of `0` means the reset
time is unknown or there is no scheduled reset (e.g. a provider that did not
report one), as opposed to a window that just reset.

Note on the OpenAI billing block: `total_used` is trailing-30-day USD spend
(a rolling window — the costs API reports spend, not your billing cycle), so
`reset_timestamp` is `0`: a rolling window has no discrete reset and the true
monthly cycle start is not observable.

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
