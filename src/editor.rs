//! `--install-acp`: registers sand as an ACP agent in an editor's or terminal's configuration.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde_json::{Map, Value, json};

use crate::log;
use crate::paths::Dirs;
use crate::tern;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AcpClient {
    Zed,
    Jetbrains,
    Tern,
}

/// The command an editor or terminal runs to start the agent.
pub struct AgentCommand {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

impl AgentCommand {
    /// `sand [harness] --acp`, using the absolute path of this binary because
    /// editors started from a desktop launcher often lack the shell's PATH.
    pub fn for_harness(harness: Option<&str>) -> Result<Self> {
        let program = env::current_exe().context("locating the sand executable")?;
        let program = dunce::canonicalize(&program).unwrap_or(program).to_string_lossy().into_owned();
        Ok(match harness {
            Some(h) => Self { label: format!("sand ({h})"), program, args: vec![h.to_string(), "--acp".to_string()] },
            None => Self { label: "sand".to_string(), program, args: vec!["--acp".to_string()] },
        })
    }
}

fn zed_settings_path(dirs: &Dirs) -> Result<PathBuf> {
    if cfg!(windows) {
        let appdata = env::var_os("APPDATA").ok_or_else(|| anyhow!("%APPDATA% is not set"))?;
        return Ok(PathBuf::from(appdata).join("Zed").join("settings.json"));
    }
    let config = match env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => dirs.home.join(".config"),
    };
    Ok(config.join("zed").join("settings.json"))
}

fn jetbrains_acp_path(dirs: &Dirs) -> PathBuf {
    dirs.home.join(".jetbrains").join("acp.json")
}

fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".sand-backup");
        let copy = path.with_file_name(name);
        fs::copy(path, &copy).with_context(|| format!("backing up {}", path.display()))?;
        log::info(format!("backed up the previous file to {}", copy.display()));
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Adds or replaces `agent_servers.<label>` in Zed's settings, leaving the rest
/// of the file (comments, formatting) untouched.
pub fn zed_settings_with_agent(existing: &str, agent: &AgentCommand) -> Result<String> {
    let source = if existing.trim().is_empty() { "{}\n" } else { existing };
    let root = CstRootNode::parse(source, &Default::default())
        .map_err(|e| anyhow!("Zed's settings.json could not be parsed: {e}"))?;
    let entry = CstInputValue::Object(vec![
        ("type".to_string(), CstInputValue::String("custom".to_string())),
        ("command".to_string(), CstInputValue::String(agent.program.clone())),
        (
            "args".to_string(),
            CstInputValue::Array(agent.args.iter().map(|a| CstInputValue::String(a.clone())).collect()),
        ),
        ("env".to_string(), CstInputValue::Object(vec![])),
    ]);
    let servers = root.object_value_or_set().object_value_or_set("agent_servers");
    match servers.get(&agent.label) {
        Some(prop) => prop.set_value(entry),
        None => {
            servers.append(&agent.label, entry);
        }
    }
    Ok(root.to_string())
}

/// Adds or replaces `agent_servers.<label>` in JetBrains' `acp.json`.
pub fn jetbrains_config_with_agent(existing: &str, agent: &AgentCommand) -> Result<String> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        jsonc_parser::parse_to_serde_value(existing, &Default::default())
            .map_err(|e| anyhow!("acp.json could not be parsed: {e}"))?
    };
    let Some(object) = root.as_object_mut() else { bail!("acp.json must contain a JSON object") };
    let servers = object.entry("agent_servers").or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else { bail!("`agent_servers` in acp.json must be an object") };
    servers.insert(agent.label.clone(), json!({ "command": agent.program, "args": agent.args, "env": {} }));
    Ok(format!("{}\n", serde_json::to_string_pretty(&root)?))
}

pub fn install(client: AcpClient, dirs: &Dirs, harness: Option<&str>) -> Result<()> {
    let mut agent = AgentCommand::for_harness(harness)?;
    if client == AcpClient::Tern {
        agent.label = tern::agent_name(harness);
    }
    let (path, name) = match client {
        AcpClient::Zed => (zed_settings_path(dirs)?, "Zed"),
        AcpClient::Jetbrains => (jetbrains_acp_path(dirs), "JetBrains"),
        AcpClient::Tern => (tern::settings_path(dirs)?, "Tern"),
    };
    log::step(format!("Installing ACP agent '{}' for {name}", agent.label));
    log::info(format!("config file: {}", path.display()));
    log::info(format!("command: {} {}", agent.program, agent.args.join(" ")));
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let tern_id = tern::agent_id(harness);
    let updated = match client {
        AcpClient::Zed => zed_settings_with_agent(&existing, &agent)?,
        AcpClient::Jetbrains => jetbrains_config_with_agent(&existing, &agent)?,
        AcpClient::Tern => tern::settings_with_agent(&existing, &tern_id, &agent)?,
    };
    if updated == existing {
        log::ok("already installed; nothing to change");
        return Ok(());
    }
    backup(&path)?;
    write(&path, &updated)?;
    log::ok(format!("added agent server '{}'", agent.label));
    match client {
        AcpClient::Zed => log::info(format!("In Zed: open the Agent Panel, click +, and pick '{}'.", agent.label)),
        AcpClient::Jetbrains => {
            log::info(format!("In the IDE: open AI Chat, choose the agent selector, and pick '{}'.", agent.label))
        }
        AcpClient::Tern => {
            log::info(format!(
                "In Tern: pick '{}' in the agent picker, or run `{}` in a Tern pane to open it in Tern's chat.",
                agent.label,
                harness.map_or_else(|| "sand".to_string(), |h| format!("sand {h}"))
            ));
            log::info(format!("'{tern_id}' uses its own file and terminal tools, so they run inside the sandbox."));
            return Ok(());
        }
    }
    log::info("The editor starts the agent in the project directory, which becomes the sandbox's primary workspace.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentCommand {
        AgentCommand {
            label: "sand (codex)".into(),
            program: "/usr/local/bin/sand".into(),
            args: vec!["codex".into(), "--acp".into()],
        }
    }

    fn parse(text: &str) -> Value {
        jsonc_parser::parse_to_serde_value(text, &Default::default()).unwrap()
    }

    #[test]
    fn zed_settings_keep_comments_and_other_agents() {
        let existing = r#"// my settings
{
  "theme": "One Dark", // keep me
  "agent_servers": {
    "other": { "type": "custom", "command": "x" },
  },
}
"#;
        let updated = zed_settings_with_agent(existing, &agent()).unwrap();
        assert!(updated.contains("// my settings") && updated.contains("// keep me"), "{updated}");
        let value = parse(&updated);
        assert_eq!(value["theme"], "One Dark");
        assert_eq!(value["agent_servers"]["other"]["command"], "x");
        let entry = &value["agent_servers"]["sand (codex)"];
        assert_eq!(entry["type"], "custom");
        assert_eq!(entry["command"], "/usr/local/bin/sand");
        assert_eq!(entry["args"], json!(["codex", "--acp"]));

        // Installing again is idempotent.
        assert_eq!(parse(&zed_settings_with_agent(&updated, &agent()).unwrap()), value);
    }

    #[test]
    fn zed_settings_are_created_when_missing() {
        let value = parse(&zed_settings_with_agent("", &agent()).unwrap());
        assert_eq!(value["agent_servers"]["sand (codex)"]["args"], json!(["codex", "--acp"]));
    }

    #[test]
    fn jetbrains_config_is_merged() {
        let existing =
            r#"{"default_mcp_settings": {"use_idea_mcp": true}, "agent_servers": {"other": {"command": "x"}}}"#;
        let value = parse(&jetbrains_config_with_agent(existing, &agent()).unwrap());
        assert_eq!(value["default_mcp_settings"]["use_idea_mcp"], true);
        assert_eq!(value["agent_servers"]["other"]["command"], "x");
        assert_eq!(value["agent_servers"]["sand (codex)"]["command"], "/usr/local/bin/sand");
        let fresh = parse(&jetbrains_config_with_agent("", &agent()).unwrap());
        assert_eq!(fresh["agent_servers"]["sand (codex)"]["args"], json!(["codex", "--acp"]));
        assert!(jetbrains_config_with_agent("[]", &agent()).is_err());
    }
}
