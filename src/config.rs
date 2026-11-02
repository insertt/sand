//! User configuration (`config.toml`). Everything is optional.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::log;
use crate::paths::Dirs;

/// Written by `--open-config` when there is no config file yet.
const STARTER: &str = r#"# sand configuration. Everything is optional; the README describes every key.

# default_harness = "claude"
# nightly = true                       # create sandboxes from nightly harness images
# tern_native = false                  # keep the agent's terminal UI in Tern

# [harness.claude]
# template = "rust"
# overlay = { model = "opus" }         # merged over the built-in settings overlay

# auth = "vertex"                     # Claude on Vertex AI, with the [vertex] settings

# [vertex]
# project = "my-gcp-project"
# region = "global"

# [templates.rust]
# dockerfile = "~/sand/rust.Dockerfile"

# [mcp.linear]
# url = "https://mcp.linear.app/mcp"
"#;

/// Splits an editor command into words: separated by whitespace, with `"…"` or
/// `'…'` grouping words that contain spaces. A value that names an existing
/// file is a single word, so an unquoted path with spaces works too. Backslashes
/// are kept as they are, since they separate the parts of a Windows path.
fn split_command(value: &str) -> Vec<String> {
    let value = value.trim();
    if Path::new(value).is_file() {
        return vec![value.to_string()];
    }
    let (mut words, mut word, mut quote) = (Vec::new(), None::<String>, None);
    for c in value.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.get_or_insert_default().push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            None if c.is_whitespace() => words.extend(word.take()),
            None => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// Editor commands to try, in order: `$VISUAL`, `$EDITOR`, then what the platform has.
fn editors() -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|var| env::var(var).ok())
        .map(|value| split_command(&value))
        .filter(|command| !command.is_empty())
        .collect();
    let fallbacks: &[&[&str]] = if cfg!(windows) {
        &[&["notepad"]]
    } else if cfg!(target_os = "macos") {
        &[&["open", "-W", "-t"]]
    } else {
        &[&["sensible-editor"], &["nano"], &["vi"]]
    };
    out.extend(fallbacks.iter().map(|command| command.iter().map(|s| s.to_string()).collect()));
    out
}

/// `sand --open-config`: opens config.toml in an editor, then checks the result.
pub fn open_in_editor(dirs: &Dirs) -> Result<()> {
    let file = &dirs.config_file;
    if !file.exists() {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(file, STARTER).with_context(|| format!("writing {}", file.display()))?;
        log::info(format!("created {} with commented examples", file.display()));
    }
    log::step(format!("Opening {}", file.display()));
    let mut opened = false;
    for command in editors() {
        log::command(format!("{} {}", command.join(" "), file.display()));
        match Command::new(&command[0]).args(&command[1..]).arg(file).status() {
            Ok(status) if status.success() => {
                opened = true;
                break;
            }
            Ok(status) => bail!("{} exited with {status}", command[0]),
            Err(e) if e.kind() == ErrorKind::NotFound => log::info(format!("{} is not installed", command[0])),
            Err(e) => return Err(e).with_context(|| format!("running {}", command[0])),
        }
    }
    if !opened {
        bail!("no editor found; set $EDITOR, or edit {} yourself", file.display());
    }
    Config::load(dirs)?;
    log::ok("the config is valid");
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Harness used when none is named on the command line (default: claude).
    pub default_harness: Option<String>,
    /// Home directory of the agent user inside sandboxes (default: /home/agent).
    pub sandbox_home: Option<String>,
    /// Create sandboxes from the harness's nightly image (default: false).
    /// `[harness.<name>] nightly` overrides it for one harness.
    pub nightly: Option<bool>,
    /// Draw sessions natively in Tern when it can (default: true); false keeps
    /// the agent's terminal UI. `SAND_TERN_NATIVE` overrides it when set.
    pub tern_native: Option<bool>,
    /// Overrides for built-in harnesses and definitions of additional ones.
    #[serde(default)]
    pub harness: BTreeMap<String, HarnessOverride>,
    #[serde(default)]
    pub templates: BTreeMap<String, TemplateDef>,
    /// MCP servers sand may register with `sbx mcp add` when `--mcp` names
    /// one that is not registered yet.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpDef>,
    #[serde(default)]
    pub vertex: VertexDef,
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
    pub nightly: Option<bool>,
    pub import_skills: Option<bool>,
    /// Ask in the agent's UI whether to allow hosts the network policy blocked.
    pub network_prompts: Option<bool>,
    /// How the agent authenticates to its model API (default: sbx).
    pub auth: Option<Auth>,
    /// Settings merged over the overlay when `auth = "vertex"`. In strings,
    /// `{project}`, `{region}` and `{host}` come from `[vertex]` and `{token}`
    /// is the placeholder the sandbox proxy replaces with the access token.
    pub vertex_overlay: Option<toml::Table>,
    /// Environment variable that holds the token placeholder for the agent.
    pub vertex_token_env: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// Whatever sbx has for the agent (`sbx secret set`, OAuth).
    #[default]
    Sbx,
    /// Vertex AI with a Google Cloud access token (see `[vertex]`).
    Vertex,
}

/// Vertex AI settings shared by every harness with `auth = "vertex"`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VertexDef {
    /// Google Cloud project ID (required).
    pub project: Option<String>,
    /// Vertex AI region (default: global).
    pub region: Option<String>,
    /// Hosts that receive the token (default: the region's Vertex AI endpoints).
    pub hosts: Option<Vec<String>>,
    /// Host command that prints an access token (default: gcloud auth
    /// print-access-token). sbx runs it through the host shell.
    pub token_command: Option<String>,
    /// How long sbx keeps a token before running the command again:
    /// a duration or "on-demand" (default: 45m).
    pub refresh: Option<String>,
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
            nightly = true
            tern_native = false

            [harness.claude]
            template = "rust"
            nightly = false
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

            [vertex]
            project = "acme"
            refresh = "30m"
            "#,
        )
        .unwrap();
        assert_eq!(config.default_harness(), "codex");
        assert_eq!(config.sandbox_home(), "/home/agent");
        assert_eq!(config.harness["claude"].template.as_deref(), Some("rust"));
        assert_eq!((config.nightly, config.harness["claude"].nightly), (Some(true), Some(false)));
        assert_eq!(config.tern_native, Some(false));
        assert_eq!(config.harness["aider"].statusline, Some(StatuslineKind::None));
        assert_eq!(config.templates["rust"].build_args["RUST"], "1.90");
        assert_eq!(config.mcp["pw"].args, vec!["@playwright/mcp@latest"]);
        assert_eq!((config.vertex.project.as_deref(), config.vertex.refresh.as_deref()), (Some("acme"), Some("30m")));
        assert_eq!(config.harness["claude"].auth, None);
    }

    #[test]
    fn editor_commands_are_split_like_a_shell_would() {
        assert_eq!(split_command("vim"), ["vim"]);
        assert_eq!(split_command("  code --wait "), ["code", "--wait"]);
        assert_eq!(
            split_command(r#""C:\Program Files\Microsoft VS Code\bin\code.cmd" --wait"#),
            [r"C:\Program Files\Microsoft VS Code\bin\code.cmd", "--wait"]
        );
        assert_eq!(split_command("'/opt/my editor/ed' -n 'a b'"), ["/opt/my editor/ed", "-n", "a b"]);
        assert_eq!(split_command("ed ''"), ["ed", ""]);
        assert!(split_command("  ").is_empty());

        // An existing file is one word, quoted or not.
        let tmp = tempfile::tempdir().unwrap();
        let editor = tmp.path().join("my editor.exe");
        fs::write(&editor, "").unwrap();
        let path = editor.to_string_lossy().into_owned();
        assert_eq!(split_command(&path), [path.as_str()]);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("defualt_harness = \"x\"").is_err());
        assert!(toml::from_str::<Config>("[harness.claude]\nhistroy = []").is_err());
        assert!(toml::from_str::<Config>("[harness.claude]\nauth = \"bedrock\"").is_err());
        assert!(toml::from_str::<Config>("[vertex]\nprojct = \"x\"").is_err());
    }
}
