//! Asks an image registry which build a tag currently points at, so that a
//! sandbox created from a nightly image can tell when a newer one is published.

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

const TIMEOUT: Duration = Duration::from_secs(8);
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// An image reference split into the parts the registry API needs.
#[derive(Debug, PartialEq, Eq)]
struct Reference {
    /// Base URL of the registry API.
    registry: String,
    repository: String,
    tag: String,
}

impl Reference {
    fn parse(image: &str) -> Result<Self> {
        if image.contains('@') {
            bail!("'{image}' is pinned to a digest");
        }
        let (host, rest) = match image.split_once('/') {
            Some((first, rest)) if first.contains(['.', ':']) || first == "localhost" => (Some(first), rest),
            _ => (None, image),
        };
        let (repository, tag) = match rest.rsplit_once(':') {
            Some((repository, tag)) if !tag.contains('/') => (repository, tag),
            _ => (rest, "latest"),
        };
        if repository.is_empty() || tag.is_empty() {
            bail!("'{image}' is not an image reference");
        }
        let (registry, repository) = match host {
            None | Some("docker.io") | Some("index.docker.io") => {
                let official = if repository.contains('/') { "" } else { "library/" };
                ("https://registry-1.docker.io".to_string(), format!("{official}{repository}"))
            }
            Some(host) => (format!("https://{host}"), repository.to_string()),
        };
        Ok(Self { registry, repository, tag: tag.to_string() })
    }
}

/// Value of `key` in a `WWW-Authenticate: Bearer key="value",…` challenge.
fn challenge_param<'a>(challenge: &'a str, key: &str) -> Option<&'a str> {
    let params = challenge.strip_prefix("Bearer ").or_else(|| challenge.strip_prefix("bearer "))?;
    params.split(',').find_map(|part| part.trim().strip_prefix(key)?.strip_prefix("=\"")?.strip_suffix('"'))
}

/// Anonymous pull token for the registry that answered with `challenge`.
fn token(agent: &Agent, challenge: &str) -> Result<String> {
    let realm =
        challenge_param(challenge, "realm").ok_or_else(|| anyhow!("unsupported authentication: {challenge}"))?;
    let mut request = agent.get(realm);
    for key in ["service", "scope"] {
        if let Some(value) = challenge_param(challenge, key) {
            request = request.query(key, value);
        }
    }
    let mut response = request.call()?;
    if !response.status().is_success() {
        bail!("{realm} answered {}", response.status());
    }
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string()?)?;
    ["token", "access_token"]
        .iter()
        .find_map(|key| body[key].as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{realm} returned no token"))
}

/// Digest the tag of `image` points at in its registry right now.
pub fn digest(image: &str) -> Result<String> {
    let mut reference = Reference::parse(image)?;
    // `$SAND_REGISTRY` points sand at a registry mirror.
    if let Some(mirror) = env::var("SAND_REGISTRY").ok().filter(|m| !m.is_empty()) {
        reference.registry = mirror.trim_end_matches('/').to_string();
    }
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .build()
        .into();
    let url = format!("{}/v2/{}/manifests/{}", reference.registry, reference.repository, reference.tag);
    let head = |bearer: Option<&str>| {
        let mut request = agent.head(&url).header("Accept", MANIFEST_TYPES);
        if let Some(bearer) = bearer {
            request = request.header("Authorization", format!("Bearer {bearer}"));
        }
        request.call().with_context(|| format!("asking {} about {image}", reference.registry))
    };
    let mut response = head(None)?;
    if response.status().as_u16() == 401 {
        let challenge = response.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default();
        let token = token(&agent, challenge).context("getting a registry token")?;
        response = head(Some(&token))?;
    }
    if !response.status().is_success() {
        bail!("{} answered {} for {image}", reference.registry, response.status());
    }
    response
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} did not report a digest for {image}", reference.registry))
}

/// A digest shortened for display: `sha256:0f6658689635`.
pub fn short(digest: &str) -> &str {
    digest.get(..19).unwrap_or(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(image: &str) -> (String, String, String) {
        let r = Reference::parse(image).unwrap();
        (r.registry, r.repository, r.tag)
    }

    #[test]
    fn references_are_split_for_the_registry_api() {
        let hub = "https://registry-1.docker.io".to_string();
        assert_eq!(
            parts("docker.io/sbx/claude-image:latest"),
            (hub.clone(), "sbx/claude-image".into(), "latest".into())
        );
        assert_eq!(
            parts("docker/sandbox-templates:shell-docker-nightly"),
            (hub.clone(), "docker/sandbox-templates".into(), "shell-docker-nightly".into())
        );
        assert_eq!(parts("ubuntu"), (hub, "library/ubuntu".into(), "latest".into()));
        assert_eq!(
            parts("localhost:5000/team/agent"),
            ("https://localhost:5000".into(), "team/agent".into(), "latest".into())
        );
        assert_eq!(parts("ghcr.io/acme/agent:3"), ("https://ghcr.io".into(), "acme/agent".into(), "3".into()));
        assert!(Reference::parse("ghcr.io/acme/agent@sha256:abc").is_err());
    }

    /// Needs network access: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn docker_hub_reports_a_digest() {
        let digest = digest("docker.io/sbx/claude-image:latest").unwrap();
        assert!(digest.starts_with("sha256:") && digest.len() == 71, "{digest}");
    }

    #[test]
    fn bearer_challenges_are_parsed() {
        let challenge = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:sbx/claude-image:pull""#;
        assert_eq!(challenge_param(challenge, "realm"), Some("https://auth.docker.io/token"));
        assert_eq!(challenge_param(challenge, "scope"), Some("repository:sbx/claude-image:pull"));
        assert_eq!(challenge_param("Basic realm=\"x\"", "realm"), None);
        assert_eq!(short("sha256:0f6658689635aaaaaaaaaaaaaaaa"), "sha256:0f6658689635");
    }
}
