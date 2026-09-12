//! Muse provider: Muse Code (Meta) sign-in and billing state.
//!
//! Muse exposes no quota or usage API that a polling daemon can use: the
//! Model API has no aggregate billing/usage endpoint (the obvious probe
//! paths all return 404), and subscription windows are only delivered as
//! SSE events on streaming turns, which cannot be observed by polling.
//! So instead of synthetic data, this provider reports what is observable
//! — CLI presence, sign-in state, and billing precedence — and returns
//! `Unsupported` for live quota numbers.
//!
//! Credential resolution mirrors the CLI itself:
//!
//! - `MUSE_AUTH_PATH`
//! - `$XDG_CONFIG_HOME/muse/auth.json`
//! - `~/.config/muse/auth.json`
//!
//! The file holds a `providers` map; `{"providers": {}}` means signed out,
//! so mere existence (or non-zero size) of the file is not enough.
//! Read-only: login/logout stay entirely with the `muse` CLI.

use super::{ProviderError, ProviderQuota, QuotaProvider};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, env, fs, path::PathBuf};

/// Why live Muse quota is unsupported: stated once, reused in messages.
const NO_QUOTA_API: &str = "Muse exposes no quota API for polling (subscription windows arrive only as SSE events on streaming turns)";

#[derive(Debug, Default)]
pub(crate) struct MuseState {
    pub cli_installed: bool,
    pub signed_in: bool,
    /// `META_API_KEY` moves Muse onto per-token Model API billing, ahead of
    /// any stored subscription session. (`META_MODEL_API_KEY` belongs to
    /// third-party BYOK harnesses and is deliberately ignored here.)
    pub meta_api_key: bool,
}

pub(crate) struct MuseProvider;

impl MuseProvider {
    /// Takes `demo` for a uniform constructor across providers, but Muse
    /// ignores it: there is no quota signal to synthesize, so demo mode would
    /// only produce misleading data. See [`status`].
    pub(crate) fn new(_demo: bool) -> Self {
        Self
    }
}

#[derive(Debug, Deserialize, Default)]
struct MuseAuthFile {
    #[serde(default)]
    providers: HashMap<String, Value>,
}

/// Best-effort session check: any non-trivial entry in the providers map.
/// Unknown shapes fail closed (treated as signed out) rather than crashing.
pub(crate) fn session_present(raw: &str) -> bool {
    let file: MuseAuthFile = match serde_json::from_str(raw) {
        Ok(file) => file,
        Err(_) => return false,
    };
    file.providers.values().any(|value| match value {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => !s.trim().is_empty(),
        _ => true,
    })
}

/// Resolve the credential path the way the CLI does. `get_env` is a
/// parameter (rather than reading the process env directly) so tests can
/// cover the precedence without mutating global state.
pub(crate) fn resolve_auth_path(get_env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(path) = get_env("MUSE_AUTH_PATH").filter(|s| !s.trim().is_empty()) {
        return Some(PathBuf::from(path));
    }
    let config_home = get_env("XDG_CONFIG_HOME")
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            get_env("HOME")
                .filter(|s| !s.trim().is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(config_home.join("muse").join("auth.json"))
}

fn cli_installed() -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join("muse").is_file()))
        .unwrap_or(false)
}

pub(crate) fn detect_state() -> MuseState {
    let get_env = |key: &str| env::var(key).ok();
    let signed_in = resolve_auth_path(get_env)
        .as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|raw| session_present(&raw))
        .unwrap_or(false);
    let meta_api_key = get_env("META_API_KEY")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    MuseState {
        cli_installed: cli_installed(),
        signed_in,
        meta_api_key,
    }
}

/// Pure status decision, factored out for testing. Never fabricates quota:
/// there is no Muse quota endpoint, so this only ever returns `Unsupported`
/// (configured but nothing to report) or `NotConfigured` (no session) — demo
/// mode included.
pub(crate) fn status(state: &MuseState) -> Result<ProviderQuota, ProviderError> {
    if state.signed_in || state.meta_api_key {
        let mut detail = NO_QUOTA_API.to_string();
        if state.meta_api_key {
            detail.push_str(
                "; META_API_KEY is set, so Muse bills per-token (Model API) instead of using the subscription",
            );
        } else if state.signed_in {
            detail.push_str("; signed-in session detected");
        }
        return Err(ProviderError::Unsupported(detail));
    }
    if !state.cli_installed {
        return Err(ProviderError::NotConfigured(
            "muse CLI not found on PATH and no session detected: install from https://dev.meta.ai/install.sh, then run `muse login`",
        ));
    }
    Err(ProviderError::NotConfigured(
        "no Muse session detected: run `muse login`",
    ))
}

#[async_trait]
impl QuotaProvider for MuseProvider {
    fn name(&self) -> &'static str {
        "muse"
    }

    async fn fetch(&self, _client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
        status(&detect_state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |key: &str| map.get(key).map(|s| s.to_string())
    }

    #[test]
    fn auth_path_precedence() {
        // MUSE_AUTH_PATH wins over everything.
        assert_eq!(
            resolve_auth_path(env_from(&[
                ("MUSE_AUTH_PATH", "/custom/auth.json"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from("/custom/auth.json"))
        );
        // XDG_CONFIG_HOME beats ~/.config.
        assert_eq!(
            resolve_auth_path(env_from(&[
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/u")
            ])),
            Some(PathBuf::from("/xdg/muse/auth.json"))
        );
        // Falls back to ~/.config/muse/auth.json.
        assert_eq!(
            resolve_auth_path(env_from(&[("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.config/muse/auth.json"))
        );
        // Empty values are ignored; no HOME at all means no path.
        assert_eq!(
            resolve_auth_path(env_from(&[("XDG_CONFIG_HOME", "  ")])),
            None
        );
        assert_eq!(resolve_auth_path(env_from(&[])), None);
    }

    #[test]
    fn session_detection_reads_provider_map() {
        // The signed-out skeleton: file exists, non-empty, but no session.
        assert!(!session_present(r#"{"providers": {}}"#));
        assert!(!session_present(r#"{"providers": {"meta": {}}}"#));
        assert!(!session_present(r#"{"providers": {"meta": null}}"#));
        // A real session entry, whatever its shape.
        assert!(session_present(
            r#"{"providers": {"meta": {"token": "abc"}}}"#
        ));
        assert!(session_present(r#"{"providers": {"meta": "tok"}}"#));
        // Malformed or unexpected JSON fails closed.
        assert!(!session_present("not json"));
        assert!(!session_present(r#"{"unrelated": true}"#));
    }

    fn state(signed_in: bool, meta_api_key: bool, cli_installed: bool) -> MuseState {
        MuseState {
            cli_installed,
            signed_in,
            meta_api_key,
        }
    }

    #[test]
    fn status_messages() {
        // Signed in but no quota endpoint: explicit unsupported, no fake data.
        let err = status(&state(true, false, true)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("unsupported: Muse exposes no quota API"),
            "{msg}"
        );
        assert!(msg.contains("signed-in session detected"), "{msg}");

        // META_API_KEY billing precedence is surfaced.
        let err = status(&state(false, true, true)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported:"), "{msg}");
        assert!(msg.contains("META_API_KEY"), "{msg}");
        assert!(msg.contains("per-token"), "{msg}");

        // Nothing configured: actionable hints, distinct cases.
        let err = status(&state(false, false, false)).unwrap_err();
        assert!(err.to_string().contains("muse login"), "{err}");
        assert!(err.to_string().contains("install"), "{err}");
        let err = status(&state(false, false, true)).unwrap_err();
        assert!(err.to_string().contains("muse login"), "{err}");
        assert!(!err.to_string().contains("install"), "{err}");
    }

    #[test]
    fn demo_mode_never_fabricates_muse_quota() {
        // Muse has no quota endpoint, so demo mode can't stand anything in:
        // every state is an error regardless of the daemon's demo flag.
        assert!(status(&state(false, false, false)).is_err());
        assert!(status(&state(false, false, true)).is_err());
        assert!(status(&state(true, false, true)).is_err());
        assert!(status(&state(false, true, true)).is_err());
    }
}
