//! Harness profiles: everything sand needs to know about an agent CLI is data,
//! so supporting another harness means adding a profile (built in or in config.toml).

use std::env;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::{Auth, Config, CopyPair, HarnessOverride, SettingsFormat, StatuslineKind};
use crate::paths::RUNTIME_DIR_NAME;
use crate::sync::deep_merge;
use crate::vertex::{self, Vertex};

/// Agents sbx ships; used to tell a harness name from a path on the command line.
pub const SBX_AGENTS: &[&str] =
    &["claude", "codex", "copilot", "cursor", "devin", "docker-agent", "droid", "gemini", "kiro", "opencode", "shell"];

/// Image of `agent` that is rebuilt every night with the latest release of the
/// agent CLI, if one is published. Most agents have a daily `sbx/<agent>-image`
/// (the image their sbx kit runs on); the rest only have a nightly tag of the
/// sbx template.
pub fn nightly_image(agent: &str) -> Option<String> {
    match agent {
        "claude" | "codex" | "copilot" | "cursor" | "devin" | "docker-agent" | "droid" | "kiro" | "opencode" => {
            Some(format!("docker.io/sbx/{agent}-image:latest"))
        }
        "gemini" | "shell" => Some(format!("docker/sandbox-templates:{agent}-docker-nightly")),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    /// Agent (or kit reference) passed to `sbx create`.
    pub agent: String,
    /// User-level configuration directory on the host (`~` allowed).
    pub host_config_dir: Option<String>,
    /// Its location inside the sandbox, relative to the sandbox home.
    pub sandbox_config_dir: Option<String>,
    /// Entries of the host config directory copied into the sandbox.
    pub copy: Vec<String>,
    /// Additional host paths copied to locations relative to the sandbox home.
    pub extra_copy: Vec<CopyPair>,
    /// File or directory names never copied, wherever they appear.
    pub exclude: Vec<String>,
    /// Candidate settings files inside the config directory; the first one that
    /// exists on the host is merged (sandbox defaults < host settings < overlay).
    pub settings_files: Vec<String>,
    pub settings_format: SettingsFormat,
    /// Top-level keys dropped from the host settings before merging.
    pub strip_keys: Vec<String>,
    /// Settings sand enforces on top (bypass permissions, status line).
    pub overlay: Value,
    /// Directories (relative to the sandbox home) holding conversation history;
    /// backed by host directories so they survive `--recreate`.
    pub history: Vec<String>,
    /// Flags that put the agent in bypass-permissions mode.
    pub bypass_args: Vec<String>,
    pub statusline: StatuslineKind,
    /// Command that speaks ACP on stdio inside the sandbox.
    pub acp_command: Vec<String>,
    /// URL probed from inside the sandbox to check the agent API is reachable.
    pub api_check_url: Option<String>,
    /// Default template (name from config.toml or image reference).
    pub template: Option<String>,
    /// Without a template, create the sandbox from the agent's nightly image.
    pub nightly: bool,
    /// sbx shares host skills with this agent through its own store (which it
    /// mounts read-only over the skills directory), so sand runs
    /// `sbx skills import` instead of copying them.
    pub import_skills: bool,
    /// Settings that point the agent at Vertex AI, merged over the overlay when
    /// `auth = "vertex"` (placeholders: `{project}`, `{region}`, `{host}`, `{token}`).
    pub vertex_overlay: Option<Value>,
    /// Environment variable that holds the token placeholder for the agent.
    pub vertex_token_env: Option<String>,
    /// Set when the harness authenticates through Vertex AI.
    pub vertex: Option<Vertex>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Credential files that must never leave the host, whatever the profile says.
const ALWAYS_EXCLUDED: &[&str] = &[".credentials.json", "auth.json", ".DS_Store"];

impl Profile {
    /// Profile for a harness sand has no specific knowledge of: it runs in the
    /// sandbox, but nothing is copied and no history is preserved.
    fn generic(name: &str) -> Self {
        Self {
            name: name.to_string(),
            agent: name.to_string(),
            host_config_dir: None,
            sandbox_config_dir: None,
            copy: vec![],
            extra_copy: vec![],
            exclude: strings(ALWAYS_EXCLUDED),
            settings_files: vec![],
            settings_format: SettingsFormat::Json,
            strip_keys: vec![],
            overlay: json!({}),
            history: vec![],
            bypass_args: vec![],
            statusline: StatuslineKind::None,
            acp_command: vec![],
            api_check_url: None,
            template: None,
            nightly: false,
            import_skills: false,
            vertex_overlay: None,
            vertex_token_env: None,
            vertex: None,
        }
    }

    fn builtin(name: &str, sandbox_home: &str) -> Option<Self> {
        let base = Self::generic(name);
        Some(match name {
            "claude" => Self {
                host_config_dir: Some(env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| "~/.claude".into())),
                sandbox_config_dir: Some(".claude".into()),
                copy: strings(&[
                    "CLAUDE.md",
                    "commands",
                    "agents",
                    "plugins",
                    "output-styles",
                    "hooks",
                    "rules",
                    "keybindings.json",
                ]),
                settings_files: strings(&["settings.json"]),
                strip_keys: strings(&["apiKeyHelper", "awsAuthRefresh", "awsCredentialExport"]),
                overlay: json!({
                    "permissions": { "defaultMode": "bypassPermissions" },
                    "skipDangerousModePermissionPrompt": true,
                    "statusLine": {
                        "type": "command",
                        "command": format!("sh {sandbox_home}/{RUNTIME_DIR_NAME}/statusline.sh"),
                    },
                }),
                history: strings(&[".claude/projects", ".claude/todos", ".claude/file-history"]),
                bypass_args: strings(&["--dangerously-skip-permissions"]),
                statusline: StatuslineKind::Claude,
                acp_command: strings(&["npx", "-y", "@agentclientprotocol/claude-agent-acp"]),
                api_check_url: Some("https://api.anthropic.com/v1/models".into()),
                import_skills: true,
                // Claude Code leaves Google authentication alone and sends the
                // token placeholder as its bearer token.
                vertex_overlay: Some(json!({
                    "env": {
                        "CLAUDE_CODE_USE_VERTEX": "1",
                        "CLAUDE_CODE_SKIP_VERTEX_AUTH": "1",
                        "ANTHROPIC_VERTEX_PROJECT_ID": "{project}",
                        "CLOUD_ML_REGION": "{region}",
                        "ANTHROPIC_AUTH_TOKEN": "{token}",
                    },
                })),
                vertex_token_env: Some("ANTHROPIC_AUTH_TOKEN".into()),
                ..base
            },
            "codex" => Self {
                host_config_dir: Some(env::var("CODEX_HOME").unwrap_or_else(|_| "~/.codex".into())),
                sandbox_config_dir: Some(".codex".into()),
                copy: strings(&["AGENTS.md", "AGENTS.override.md", "prompts", "rules"]),
                settings_files: strings(&["config.toml"]),
                settings_format: SettingsFormat::Toml,
                // Host MCP servers are replaced by the sandbox MCP gateway (--mcp).
                strip_keys: strings(&["mcp_servers"]),
                overlay: json!({
                    "approval_policy": "never",
                    "sandbox_mode": "danger-full-access",
                    "tui": {
                        "status_line": [
                            "model-with-reasoning",
                            "git-branch",
                            "context-used",
                            "five-hour-limit",
                            "weekly-limit",
                        ],
                    },
                }),
                history: strings(&[".codex/sessions"]),
                bypass_args: strings(&["--dangerously-bypass-approvals-and-sandbox"]),
                statusline: StatuslineKind::Codex,
                acp_command: strings(&["npx", "-y", "@agentclientprotocol/codex-acp"]),
                api_check_url: Some("https://api.openai.com/v1/models".into()),
                import_skills: true,
                ..base
            },
            "opencode" => Self {
                host_config_dir: Some(match env::var("XDG_CONFIG_HOME") {
                    Ok(xdg) if !xdg.is_empty() => format!("{}/opencode", xdg.trim_end_matches('/')),
                    _ => "~/.config/opencode".into(),
                }),
                sandbox_config_dir: Some(".config/opencode".into()),
                copy: strings(&[
                    "AGENTS.md",
                    "agent",
                    "agents",
                    "command",
                    "commands",
                    "skill",
                    "skills",
                    "plugin",
                    "plugins",
                    "mode",
                    "modes",
                    "tool",
                    "tools",
                    "themes",
                ]),
                settings_files: strings(&["opencode.jsonc", "opencode.json"]),
                strip_keys: strings(&["mcp"]),
                overlay: json!({ "permission": "allow" }),
                history: strings(&[".local/share/opencode"]),
                acp_command: strings(&["opencode", "acp"]),
                ..base
            },
            _ => return None,
        })
    }

    fn apply(&mut self, o: &HarnessOverride) -> Result<()> {
        macro_rules! set {
            ($($field:ident),*) => { $( if let Some(v) = &o.$field { self.$field = v.clone(); } )* };
        }
        macro_rules! set_some {
            ($($field:ident),*) => { $( if let Some(v) = &o.$field { self.$field = Some(v.clone()); } )* };
        }
        set!(
            agent,
            copy,
            extra_copy,
            settings_files,
            settings_format,
            strip_keys,
            history,
            bypass_args,
            statusline,
            acp_command,
            nightly,
            import_skills
        );
        set_some!(host_config_dir, sandbox_config_dir, api_check_url, template, vertex_token_env);
        if let Some(overlay) = &o.vertex_overlay {
            self.vertex_overlay = Some(serde_json::to_value(overlay)?);
        }
        if let Some(exclude) = &o.exclude {
            self.exclude = strings(ALWAYS_EXCLUDED);
            self.exclude.extend(exclude.iter().cloned());
        }
        if let Some(overlay) = &o.overlay {
            deep_merge(&mut self.overlay, serde_json::to_value(overlay)?);
        }
        for dir in self.history.iter().chain(self.sandbox_config_dir.iter()) {
            if dir.starts_with('/') || dir.split('/').any(|part| part == "..") {
                bail!("harness '{}': '{dir}' must be a path relative to the sandbox home", self.name);
            }
        }
        Ok(())
    }

    pub fn resolve(name: &str, config: &Config) -> Result<Self> {
        let mut profile = Self::builtin(name, config.sandbox_home()).unwrap_or_else(|| Self::generic(name));
        profile.nightly = config.nightly.unwrap_or(false);
        let overrides = config.harness.get(name);
        if let Some(overrides) = overrides {
            profile.apply(overrides)?;
        }
        if overrides.and_then(|o| o.auth) == Some(Auth::Vertex) {
            if profile.vertex_overlay.is_none() && profile.vertex_token_env.is_none() {
                bail!(
                    "sand does not know how to point '{name}' at Vertex AI; set harness.{name}.vertex_overlay \
                     or harness.{name}.vertex_token_env in config.toml"
                );
            }
            let vertex = Vertex::from_config(config)?;
            if overrides.is_none_or(|o| o.api_check_url.is_none()) {
                profile.api_check_url = Some(format!("https://{}/", vertex.host()));
            }
            profile.vertex = Some(vertex);
        }
        Ok(profile)
    }

    /// Merges the Vertex AI settings into the overlay, with the token
    /// placeholder of `sandbox` in place of `{token}`.
    pub fn wire_vertex(&mut self, sandbox: &str) {
        if let (Some(vertex), Some(overlay)) = (&self.vertex, &self.vertex_overlay) {
            deep_merge(&mut self.overlay, vertex.render(overlay, &vertex::placeholder(sandbox)));
        }
    }

    /// True when sand only knows how to launch this harness, nothing more.
    pub fn is_basic(&self) -> bool {
        self.host_config_dir.is_none() && self.history.is_empty()
    }
}

/// Whether `word` names a harness (as opposed to being a path to mount).
pub fn is_known(word: &str, config: &Config) -> bool {
    Profile::builtin(word, "").is_some() || config.harness.contains_key(word) || SBX_AGENTS.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_enforce_bypass_and_never_copy_credentials() {
        let config = Config::default();
        for name in ["claude", "codex", "opencode"] {
            let p = Profile::resolve(name, &config).unwrap();
            assert!(!p.is_basic(), "{name}");
            assert!(p.exclude.iter().any(|e| e == ".credentials.json") && p.exclude.iter().any(|e| e == "auth.json"));
            assert!(!p.acp_command.is_empty() && !p.history.is_empty(), "{name}");
            assert!(p.overlay.as_object().is_some_and(|o| !o.is_empty()), "{name}");
            // Skills reach claude and codex through sbx's store, never through sand's copy.
            assert_eq!(p.import_skills, name != "opencode", "{name}");
            assert!(!p.import_skills || !p.copy.iter().any(|e| e == "skills"), "{name}");
        }
        let claude = Profile::resolve("claude", &config).unwrap();
        assert_eq!(claude.overlay["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(claude.overlay["statusLine"]["command"], "sh /home/agent/.sand/statusline.sh");
    }

    #[test]
    fn config_overrides_and_defines_harnesses() {
        let config: Config = toml::from_str(
            r#"
            [harness.claude]
            template = "rust"
            exclude = ["node_modules"]
            overlay = { model = "opus", permissions = { allow = ["Bash"] } }

            [harness.aider]
            agent = "shell"
            history = [".aider/history"]
            "#,
        )
        .unwrap();
        let claude = Profile::resolve("claude", &config).unwrap();
        assert_eq!(claude.template.as_deref(), Some("rust"));
        assert!(
            claude.exclude.contains(&"node_modules".to_string()) && claude.exclude.contains(&"auth.json".to_string())
        );
        assert_eq!(claude.overlay["model"], "opus");
        assert_eq!(claude.overlay["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(claude.overlay["permissions"]["allow"][0], "Bash");

        let aider = Profile::resolve("aider", &config).unwrap();
        assert_eq!(aider.agent, "shell");
        assert!(is_known("aider", &config) && is_known("gemini", &config) && !is_known("src", &config));

        let gemini = Profile::resolve("gemini", &config).unwrap();
        assert!(gemini.is_basic() && gemini.agent == "gemini");
    }

    #[test]
    fn nightly_is_global_with_a_harness_override() {
        let config: Config = toml::from_str("nightly = true\n[harness.codex]\nnightly = false").unwrap();
        assert!(Profile::resolve("claude", &config).unwrap().nightly);
        assert!(!Profile::resolve("codex", &config).unwrap().nightly);
        assert!(!Profile::resolve("claude", &Config::default()).unwrap().nightly);

        assert_eq!(nightly_image("claude").as_deref(), Some("docker.io/sbx/claude-image:latest"));
        assert_eq!(nightly_image("shell").as_deref(), Some("docker/sandbox-templates:shell-docker-nightly"));
        for agent in SBX_AGENTS {
            assert!(nightly_image(agent).is_some(), "{agent}");
        }
        assert_eq!(nightly_image("docker.io/sbx/pi-kit"), None);
    }

    #[test]
    fn vertex_auth_is_opt_in_and_wired_through_the_overlay() {
        let plain = Profile::resolve("claude", &Config::default()).unwrap();
        assert!(plain.vertex.is_none() && plain.overlay.get("env").is_none());

        let config: Config = toml::from_str(
            r#"
            [vertex]
            project = "acme"
            region = "us-east5"

            [harness.claude]
            auth = "vertex"
            overlay = { env = { DEBUG = "1" } }

            [harness.codex]
            auth = "vertex"

            [harness.opencode]
            auth = "vertex"
            api_check_url = "https://gw.corp/health"
            vertex_overlay = { provider = { vertex = { options = { apiKey = "{token}" } } } }
            "#,
        )
        .unwrap();
        let mut claude = Profile::resolve("claude", &config).unwrap();
        assert_eq!(claude.api_check_url.as_deref(), Some("https://us-east5-aiplatform.googleapis.com/"));
        assert!(claude.overlay["env"].get("CLAUDE_CODE_USE_VERTEX").is_none(), "wired once the sandbox is known");
        claude.wire_vertex("claude-proj");
        let env = &claude.overlay["env"];
        assert_eq!((&env["CLAUDE_CODE_USE_VERTEX"], &env["CLAUDE_CODE_SKIP_VERTEX_AUTH"]), (&json!("1"), &json!("1")));
        assert_eq!(
            (&env["ANTHROPIC_VERTEX_PROJECT_ID"], &env["CLOUD_ML_REGION"]),
            (&json!("acme"), &json!("us-east5"))
        );
        assert_eq!((&env["ANTHROPIC_AUTH_TOKEN"], &env["DEBUG"]), (&json!("sand-vertex-claude-proj"), &json!("1")));

        // No built-in wiring for codex; a harness that brings its own is accepted.
        assert!(Profile::resolve("codex", &config).unwrap_err().to_string().contains("vertex_overlay"));
        let mut opencode = Profile::resolve("opencode", &config).unwrap();
        opencode.wire_vertex("oc");
        assert_eq!(opencode.overlay["provider"]["vertex"]["options"]["apiKey"], "sand-vertex-oc");
        assert_eq!(opencode.api_check_url.as_deref(), Some("https://gw.corp/health"));

        let missing: Config = toml::from_str("[harness.claude]\nauth = \"vertex\"").unwrap();
        assert!(Profile::resolve("claude", &missing).unwrap_err().to_string().contains("project"));
    }

    #[test]
    fn history_must_stay_inside_the_sandbox_home() {
        let config: Config = toml::from_str("[harness.x]\nhistory = [\"../etc\"]").unwrap();
        assert!(Profile::resolve("x", &config).is_err());
    }
}
