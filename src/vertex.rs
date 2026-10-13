//! Vertex AI authentication (`auth = "vertex"`): the agent talks to Vertex AI
//! with a Google Cloud access token it never sees. The token is an sbx custom
//! secret: sbx runs a host command for it, caches the result, and its proxy
//! swaps the placeholder the sandbox holds for the token on requests to Vertex.

use std::ffi::OsString;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::args;
use crate::config::Config;

const DEFAULT_REGION: &str = "global";
const DEFAULT_TOKEN_COMMAND: &str = "gcloud auth print-access-token";
/// Google access tokens live for an hour; sbx asks for a new one before that.
const DEFAULT_REFRESH: &str = "45m";

/// The `[vertex]` table with its defaults filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vertex {
    pub project: String,
    pub region: String,
    /// Hosts the token is sent to; the first one serves the API.
    pub hosts: Vec<String>,
    pub token_command: String,
    pub refresh: String,
}

/// Vertex AI endpoints of a region. Requests for some models go to the global
/// endpoint whatever the region, so it is always covered.
fn hosts(region: &str) -> Vec<String> {
    let global = "aiplatform.googleapis.com".to_string();
    match region {
        "global" => vec![global],
        "us" | "eu" => vec![format!("aiplatform.{region}.rep.googleapis.com"), global],
        _ => vec![format!("{region}-aiplatform.googleapis.com"), global],
    }
}

impl Vertex {
    pub fn from_config(config: &Config) -> Result<Self> {
        let v = &config.vertex;
        let filled = |value: &Option<String>| value.clone().filter(|s| !s.trim().is_empty());
        let Some(project) = filled(&v.project) else {
            bail!("auth = \"vertex\" needs the Google Cloud project: set `project` under [vertex] in config.toml");
        };
        let region = filled(&v.region).unwrap_or_else(|| DEFAULT_REGION.into());
        let hosts = match &v.hosts {
            Some(hosts) if !hosts.is_empty() => hosts.clone(),
            _ => hosts(&region),
        };
        Ok(Self {
            project,
            hosts,
            token_command: filled(&v.token_command).unwrap_or_else(|| DEFAULT_TOKEN_COMMAND.into()),
            refresh: filled(&v.refresh).unwrap_or_else(|| DEFAULT_REFRESH.into()),
            region,
        })
    }

    pub fn host(&self) -> &str {
        &self.hosts[0]
    }

    /// Replaces `{project}`, `{region}`, `{host}` and `{token}` in every string of `overlay`.
    pub fn render(&self, overlay: &Value, placeholder: &str) -> Value {
        match overlay {
            Value::String(s) => Value::String(
                s.replace("{project}", &self.project)
                    .replace("{region}", &self.region)
                    .replace("{host}", self.host())
                    .replace("{token}", placeholder),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.render(item, placeholder)).collect()),
            Value::Object(map) => {
                Value::Object(map.iter().map(|(key, value)| (key.clone(), self.render(value, placeholder))).collect())
            }
            other => other.clone(),
        }
    }

    /// `sbx secret set-custom` arguments that create or update the sandbox's token secret.
    pub fn secret_args(&self, sandbox: &str, env: Option<&str>) -> Vec<OsString> {
        let mut out = args!["secret", "set-custom", "--sandbox", sandbox];
        for host in &self.hosts {
            out.extend(args!["--host", host]);
        }
        if let Some(env) = env {
            out.extend(args!["--env", env]);
        }
        out.extend(args![
            "--placeholder",
            placeholder(sandbox),
            "--command",
            &self.token_command,
            "--refresh",
            &self.refresh
        ]);
        out
    }
}

/// What the sandbox holds instead of the token. It is fixed per sandbox, so the
/// settings that carry it stay valid when the secret is registered again.
pub fn placeholder(sandbox: &str) -> String {
    format!("sand-vertex-{sandbox}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn vertex(toml: &str) -> Result<Vertex> {
        Vertex::from_config(&toml::from_str::<Config>(toml).unwrap())
    }

    #[test]
    fn defaults_cover_the_region_and_refresh_before_the_token_expires() {
        let v = vertex("[vertex]\nproject = \"acme\"").unwrap();
        assert_eq!((v.region.as_str(), v.host()), ("global", "aiplatform.googleapis.com"));
        assert_eq!((v.token_command.as_str(), v.refresh.as_str()), ("gcloud auth print-access-token", "45m"));

        let v = vertex("[vertex]\nproject = \"acme\"\nregion = \"us-east5\"").unwrap();
        assert_eq!(v.hosts, ["us-east5-aiplatform.googleapis.com", "aiplatform.googleapis.com"]);
        assert_eq!(
            vertex("[vertex]\nproject = \"p\"\nregion = \"eu\"").unwrap().host(),
            "aiplatform.eu.rep.googleapis.com"
        );

        let v = vertex("[vertex]\nproject = \"p\"\nhosts = [\"gw.corp\"]\nrefresh = \"on-demand\"").unwrap();
        assert_eq!((v.hosts, v.refresh.as_str()), (vec!["gw.corp".to_string()], "on-demand"));

        assert!(vertex("").unwrap_err().to_string().contains("[vertex]"));
        assert!(vertex("[vertex]\nproject = \" \"").is_err());
    }

    #[test]
    fn overlays_are_rendered_and_the_secret_is_scoped_to_the_sandbox() {
        let v = vertex("[vertex]\nproject = \"acme\"\nregion = \"us-east5\"\ntoken_command = \"/opt/token --json\"")
            .unwrap();
        let overlay = json!({"env": {"P": "{project}", "T": "{token}"}, "urls": ["https://{host}/{region}"], "n": 1});
        assert_eq!(
            v.render(&overlay, "ph"),
            json!({"env": {"P": "acme", "T": "ph"}, "urls": ["https://us-east5-aiplatform.googleapis.com/us-east5"], "n": 1})
        );

        let args: Vec<String> =
            v.secret_args("claude-proj", Some("TOKEN")).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args.join(" "),
            "secret set-custom --sandbox claude-proj --host us-east5-aiplatform.googleapis.com \
             --host aiplatform.googleapis.com --env TOKEN --placeholder sand-vertex-claude-proj \
             --command /opt/token --json --refresh 45m"
        );
        assert_eq!(args[args.iter().position(|a| a == "--command").unwrap() + 1], "/opt/token --json");
        assert!(!v.secret_args("x", None).iter().any(|a| a == "--env"));
    }
}
