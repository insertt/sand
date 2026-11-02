//! Subscription limits for the native surface, asked of Anthropic from the host.
//!
//! Claude Code over ACP reports a rate limit's state but not how much of it is
//! spent, so sand reads the person's Claude login on the host and asks the
//! usage endpoint itself. The token never leaves this process: it is read for
//! each request, sent to Anthropic over HTTPS from the host (not through the
//! sandbox's proxy), and dropped. It is not logged, cached, written anywhere
//! or passed to the sandbox, which gets only the percentages.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use crate::native::Limits;

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
const TIMEOUT: Duration = Duration::from_secs(8);
/// The keychain item Claude Code keeps its login in on macOS.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Where Claude Code keeps the person's login on the host.
#[derive(Clone, Debug)]
pub struct Credentials {
    /// `.credentials.json` in Claude Code's config directory.
    file: PathBuf,
}

impl Credentials {
    pub fn new(config_dir: &Path) -> Self {
        Self { file: config_dir.join(".credentials.json") }
    }

    /// The OAuth access token, unless there is none or it has expired. Sand
    /// does not refresh it: that would write the person's login; Claude Code
    /// on the host does whenever it runs.
    fn token(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.file).ok().or_else(keychain)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
        access_token(&text, now)
    }
}

#[cfg(target_os = "macos")]
fn keychain() -> Option<String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(not(target_os = "macos"))]
fn keychain() -> Option<String> {
    None
}

/// The subscription's access token in Claude Code's credentials, unless it
/// expires within a minute of `now_ms`.
fn access_token(credentials: &str, now_ms: u64) -> Option<String> {
    let value: Value = serde_json::from_str(credentials).ok()?;
    let oauth = &value["claudeAiOauth"];
    let token = oauth["accessToken"].as_str().filter(|t| !t.is_empty())?;
    match oauth["expiresAt"].as_u64() {
        Some(expires) if expires <= now_ms + 60_000 => None,
        _ => Some(token.to_string()),
    }
}

/// The limits the endpoint reports; `None` when there is no login to ask with.
pub fn fetch(credentials: &Credentials) -> Option<Result<Limits>> {
    let token = credentials.token()?;
    Some(request(&token))
}

fn request(token: &str) -> Result<Limits> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .build()
        .into();
    let mut response = agent
        .get(URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", BETA)
        .header("Accept", "application/json")
        .call()
        .context("asking Anthropic for the subscription usage")?;
    let status = response.status();
    if !status.is_success() {
        bail!("the usage endpoint answered {status}");
    }
    let text = response.body_mut().read_to_string().context("reading the subscription usage")?;
    let body: Value = serde_json::from_str(&text).context("reading the subscription usage")?;
    parse(&body).context("the usage endpoint reported no limits")
}

/// The 5-hour and weekly windows' utilization, which the endpoint gives in percent.
fn parse(body: &Value) -> Option<Limits> {
    let percent = |window: &str| body[window]["utilization"].as_f64().map(|p| p.clamp(0.0, 100.0));
    let limits = Limits { five_hour: percent("five_hour"), seven_day: percent("seven_day") };
    (limits.five_hour.is_some() || limits.seven_day.is_some()).then_some(limits)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_token_comes_from_the_subscription_login_until_it_expires() {
        let creds = r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat", "refreshToken": "r", "expiresAt": 2000000}}"#;
        assert_eq!(access_token(creds, 1_000_000).as_deref(), Some("sk-ant-oat"));
        assert_eq!(access_token(creds, 1_950_000), None, "expires within a minute");
        assert_eq!(access_token(r#"{"claudeAiOauth": {"accessToken": "t"}}"#, 5).as_deref(), Some("t"));
        assert_eq!(access_token(r#"{"other": {}}"#, 0), None);
        assert_eq!(access_token("not json", 0), None);
    }

    #[test]
    fn missing_credentials_mean_no_request() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Credentials::new(tmp.path()).token().is_none() || cfg!(target_os = "macos"));
    }

    #[test]
    fn the_windows_are_read_in_percent() {
        let body = json!({
            "five_hour": {"utilization": 23.0, "resets_at": "2026-11-02T18:00:00Z"},
            "seven_day": {"utilization": 41.5, "resets_at": "2026-11-06T09:00:00Z"},
            "seven_day_opus": {"utilization": 3.0, "resets_at": null},
        });
        assert_eq!(parse(&body), Some(Limits { five_hour: Some(23.0), seven_day: Some(41.5) }));
        let body = json!({"five_hour": null, "seven_day": {"utilization": 120, "resets_at": null}});
        assert_eq!(parse(&body), Some(Limits { five_hour: None, seven_day: Some(100.0) }));
        assert_eq!(parse(&json!({"five_hour": null, "seven_day": null})), None);
    }
}
