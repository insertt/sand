//! User configuration (`config.toml`). Everything is optional.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::Dirs;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Harness used when none is named on the command line (default: claude).
    pub default_harness: Option<String>,
    /// Home directory of the agent user inside sandboxes (default: /home/agent).
    pub sandbox_home: Option<String>,
    /// Overrides for built-in harnesses and definitions of additional ones.
    #[serde(default)]
    pub harness: BTreeMap<String, HarnessOverride>,
    #[serde(default)]
    pub templates: BTreeMap<String, TemplateDef>,
    /// MCP servers sand may register with `sbx mcp add` when `--mcp` names
    /// one that is not registered yet.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpDef>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOverride {
    pub agent: Option<String>,
    pub host_config_dir: Option<String>,
    pub sandbox_config_dir: Option<String>,
    pub copy: Option<Vec<String>>,
    pub extra_copy: Option<Vec<CopyPair>>,
    pub exclude: Option<Vec<String>>,
    pub settings_files: Option<Vec<String>>,
    pub settings_format: Option<SettingsFormat>,
    pub strip_keys: Option<Vec<String>>,
    /// Deep-merged over the built-in overlay.
    pub overlay: Option<toml::Table>,
    pub history: Option<Vec<String>>,
    pub bypass_args: Option<Vec<String>>,
    pub statusline: Option<StatuslineKind>,
    pub acp_command: Option<Vec<String>>,
    pub api_check_url: Option<String>,
    pub template: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyPair {
    /// Host path (`~` allowed).
    pub from: String,
    /// Destination relative to the sandbox home.
    pub to: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsFormat {
    Json,
    Toml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatuslineKind {
    /// The harness cannot run a status-line command; sand prints a banner instead.
    None,
    /// Claude Code's `statusLine` command setting.
    Claude,
    /// Codex's built-in status line (`tui.status_line`): it shows model, branch,
    /// context and limits, but cannot run a command, so sand also prints a banner.
    Codex,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateDef {
    /// Image reference to use as-is (pulled by the sandbox runtime).
    pub image: Option<String>,
    /// Dockerfile to build with `sand template build <name>`.
    pub dockerfile: Option<String>,
    /// Build context (default: the Dockerfile's directory).
    pub context: Option<String>,
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDef {
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Config {
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let file = &dirs.config_file;
        match fs::read_to_string(file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }

    pub fn default_harness(&self) -> &str {
        self.default_harness.as_deref().unwrap_or("claude")
    }

    pub fn sandbox_home(&self) -> &str {
        self.sandbox_home.as_deref().unwrap_or(crate::paths::DEFAULT_SANDBOX_HOME).trim_end_matches('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let config: Config = toml::from_str(
            r#"
            default_harness = "codex"

            [harness.claude]
            template = "rust"
            history = [".claude/projects"]
            overlay = { model = "opus" }

            [harness.aider]
            agent = "shell"
            host_config_dir = "~/.aider"
            sandbox_config_dir = ".aider"
            copy = ["conf.yml"]
            statusline = "none"
            settings_format = "json"

            [templates.rust]
            dockerfile = "~/sand/rust.Dockerfile"
            build_args = { RUST = "1.90" }

            [templates.corp]
            image = "ghcr.io/acme/agent:1"

            [mcp.atlassian]
            url = "https://mcp.atlassian.com/v1/mcp"

            [mcp.pw]
            command = "npx"
            args = ["@playwright/mcp@latest"]
            "#,
        )
        .unwrap();
        assert_eq!(config.default_harness(), "codex");
        assert_eq!(config.sandbox_home(), "/home/agent");
        assert_eq!(config.harness["claude"].template.as_deref(), Some("rust"));
        assert_eq!(config.harness["aider"].statusline, Some(StatuslineKind::None));
        assert_eq!(config.templates["rust"].build_args["RUST"], "1.90");
        assert_eq!(config.mcp["pw"].args, vec!["@playwright/mcp@latest"]);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("defualt_harness = \"x\"").is_err());
        assert!(toml::from_str::<Config>("[harness.claude]\nhistroy = []").is_err());
    }
}
