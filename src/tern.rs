//! Tern (a terminal with its own ACP chat): registering sand as one of its
//! custom agents, and finding that agent so that a session started in a Tern
//! pane can open in Tern's chat instead of the agent's terminal UI.

use std::env;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::editor::AgentCommand;
use crate::paths::Dirs;

/// Tern's executable, which its shells export while typed agent CLIs open as
/// ACP chats (Settings › Agents › Open agent commands as ACP chats).
pub fn acp_executable(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup("TERN_ACP").filter(|v| !v.is_empty())
}

pub fn settings_path(dirs: &Dirs) -> Result<PathBuf> {
    if let Some(dir) = env::var_os("TERN_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    let dir = if cfg!(target_os = "macos") {
        dirs.home.join("Library").join("Application Support").join("Tern")
    } else if cfg!(windows) {
        PathBuf::from(env::var_os("APPDATA").ok_or_else(|| anyhow!("%APPDATA% is not set"))?).join("Tern")
    } else {
        match env::var_os("XDG_CONFIG_HOME") {
            Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("tern"),
            _ => dirs.home.join(".config").join("tern"),
        }
    };
    Ok(dir.join("settings.json"))
}

/// A Tern agent id: lowercase letters, digits, `.`, `_` and `-`, up to 64.
pub fn agent_id(harness: Option<&str>) -> String {
    let id: String = match harness {
        Some(h) => format!("sand-{h}"),
        None => "sand".to_string(),
    }
    .to_lowercase()
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
    .collect();
    id.chars().take(64).collect()
}

/// The agent's name in Tern's agent picker, e.g. `Claude Code (sand)`.
pub fn agent_name(harness: Option<&str>) -> String {
    let product = match harness {
        None => return "sand".to_string(),
        Some("claude") => "Claude Code",
        Some("codex") => "Codex",
        Some("opencode") => "OpenCode",
        Some(other) => other,
    };
    format!("{product} (sand)")
}

/// Names earlier versions of sand gave the agent, like `sand (claude)`.
fn is_legacy_name(name: &str) -> bool {
    name == "sand" || (name.starts_with("sand (") && name.ends_with(')'))
}

fn parse(existing: &str) -> Result<Value> {
    if existing.trim().is_empty() {
        return Ok(json!({}));
    }
    jsonc_parser::parse_to_serde_value(existing, &Default::default())
        .map_err(|e| anyhow!("Tern's settings.json could not be parsed: {e}"))
}

/// Adds or replaces the custom agent `id` in Tern's settings, and lists it in
/// `own_tools`: otherwise Tern hands the agent its own file and terminal tools,
/// which run on the host and so outside the sandbox.
pub fn settings_with_agent(existing: &str, id: &str, agent: &AgentCommand) -> Result<String> {
    let mut root = parse(existing)?;
    let Some(object) = root.as_object_mut() else { bail!("Tern's settings.json must contain a JSON object") };
    let agents = object.entry("agents").or_insert_with(|| Value::Object(Map::new()));
    let Some(agents) = agents.as_object_mut() else { bail!("`agents` in Tern's settings.json must be an object") };

    let entry = json!({ "id": id, "name": agent.label, "command": agent.program, "args": agent.args, "env": {} });
    let custom = agents.entry("custom").or_insert_with(|| Value::Array(Vec::new()));
    let Some(custom) = custom.as_array_mut() else { bail!("`agents.custom` in Tern's settings.json must be a list") };
    match custom.iter_mut().find(|a| a["id"] == id) {
        // The name may have been changed in Tern; everything else is sand's.
        Some(existing) => {
            let name = existing["name"].as_str().filter(|n| !is_legacy_name(n)).map(str::to_string);
            *existing = entry;
            if let Some(name) = name {
                existing["name"] = json!(name);
            }
        }
        None => custom.push(entry),
    }

    let own_tools = agents.entry("own_tools").or_insert_with(|| Value::Array(Vec::new()));
    let Some(own_tools) = own_tools.as_array_mut() else {
        bail!("`agents.own_tools` in Tern's settings.json must be a list")
    };
    if !own_tools.iter().any(|t| t == id) {
        own_tools.push(json!(id));
    }
    Ok(format!("{}\n", serde_json::to_string_pretty(&root)?))
}

fn is_sand(agent: &Value) -> bool {
    let program = agent["command"].as_str().unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default();
    program == "sand" || program.eq_ignore_ascii_case("sand.exe")
}

/// Whether any custom agent runs sand, for any harness.
pub fn has_sand_agent(settings: &str) -> bool {
    parse(settings)
        .ok()
        .and_then(|root| root["agents"]["custom"].as_array().map(|a| a.iter().any(is_sand)))
        .unwrap_or(false)
}

/// The id of the custom agent that runs `sand <harness> --acp` (or bare
/// `sand --acp` when the harness is the default one), unless the user keeps
/// that agent's terminal UI (`agents.keep_tui`).
pub fn find_agent(settings: &str, harness: &str, default_harness: bool) -> Option<String> {
    let root = parse(settings).ok()?;
    let agents = root.get("agents")?;
    if agents.get("auto_acp").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let kept = |id: &str| agents["keep_tui"].as_array().is_some_and(|k| k.iter().any(|t| t == id));
    let runs_harness = |agent: &Value| {
        let args: Vec<&str> =
            agent["args"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        is_sand(agent) && (args == [harness, "--acp"] || (default_harness && args == ["--acp"]))
    };
    agents["custom"]
        .as_array()?
        .iter()
        .filter(|a| runs_harness(a))
        .filter_map(|a| a["id"].as_str())
        .find(|id| !kept(id))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentCommand {
        AgentCommand {
            label: "sand (claude)".into(),
            program: "/usr/local/bin/sand".into(),
            args: vec!["claude".into(), "--acp".into()],
        }
    }

    #[test]
    fn names_follow_the_product() {
        assert_eq!(agent_name(Some("claude")), "Claude Code (sand)");
        assert_eq!(agent_name(Some("codex")), "Codex (sand)");
        assert_eq!(agent_name(Some("aider")), "aider (sand)");
        assert_eq!(agent_name(None), "sand");
    }

    #[test]
    fn legacy_names_are_replaced() {
        let existing =
            r#"{"agents": {"custom": [{"id": "sand-claude", "name": "sand (claude)", "command": "/old/sand"}]}}"#;
        let mut agent = agent();
        agent.label = "Claude Code (sand)".into();
        let value: Value =
            serde_json::from_str(&settings_with_agent(existing, "sand-claude", &agent).unwrap()).unwrap();
        assert_eq!(value["agents"]["custom"][0]["name"], "Claude Code (sand)");
    }

    #[test]
    fn ids_are_valid_tern_ids() {
        assert_eq!(agent_id(Some("claude")), "sand-claude");
        assert_eq!(agent_id(None), "sand");
        assert_eq!(agent_id(Some("My Agent")), "sand-my-agent");
        assert_eq!(agent_id(Some(&"x".repeat(80))).len(), 64);
    }

    #[test]
    fn agent_is_added_with_own_tools_and_kept_idempotent() {
        let existing = r#"{"theme": "Dark", "agents": {"auto_acp": true, "own_tools": ["claude"],
            "custom": [{"id": "other", "name": "Other", "command": "x"}]}}"#;
        let updated = settings_with_agent(existing, "sand-claude", &agent()).unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["theme"], "Dark");
        assert_eq!(value["agents"]["auto_acp"], true);
        assert_eq!(value["agents"]["own_tools"], json!(["claude", "sand-claude"]));
        assert_eq!(value["agents"]["custom"][0]["id"], "other");
        let entry = &value["agents"]["custom"][1];
        assert_eq!(entry["id"], "sand-claude");
        assert_eq!(entry["command"], "/usr/local/bin/sand");
        assert_eq!(entry["args"], json!(["claude", "--acp"]));
        assert_eq!(settings_with_agent(&updated, "sand-claude", &agent()).unwrap(), updated);
    }

    #[test]
    fn reinstalling_keeps_a_renamed_agent_name() {
        let existing = r#"{"agents": {"custom": [{"id": "sand-claude", "name": "Mine", "command": "/old/sand"}]}}"#;
        let value: Value =
            serde_json::from_str(&settings_with_agent(existing, "sand-claude", &agent()).unwrap()).unwrap();
        let custom = value["agents"]["custom"].as_array().unwrap();
        assert_eq!(custom.len(), 1);
        assert_eq!(custom[0]["name"], "Mine");
        assert_eq!(custom[0]["command"], "/usr/local/bin/sand");
    }

    #[test]
    fn settings_are_created_and_bad_shapes_rejected() {
        let value: Value = serde_json::from_str(&settings_with_agent("", "sand", &agent()).unwrap()).unwrap();
        assert_eq!(value["agents"]["custom"][0]["id"], "sand");
        assert!(settings_with_agent("[]", "sand", &agent()).is_err());
        assert!(settings_with_agent(r#"{"agents": {"custom": {}}}"#, "sand", &agent()).is_err());
    }

    #[test]
    fn the_agent_running_a_harness_is_found() {
        let settings = r#"{"agents": {"custom": [
            {"id": "mine", "name": "Mine", "command": "/bin/other", "args": ["claude", "--acp"]},
            {"id": "sandboxed", "name": "S", "command": "/Users/me/.cargo/bin/sand", "args": ["claude", "--acp"]},
            {"id": "plain", "name": "P", "command": "C:\\bin\\sand.exe", "args": ["--acp"]}]}}"#;
        assert_eq!(find_agent(settings, "claude", false).as_deref(), Some("sandboxed"));
        assert_eq!(find_agent(settings, "codex", true).as_deref(), Some("plain"));
        assert_eq!(find_agent(settings, "codex", false), None);
        assert_eq!(find_agent("", "claude", false), None);
        assert_eq!(find_agent("not json", "claude", false), None);
        assert!(has_sand_agent(settings));
        assert!(!has_sand_agent(r#"{"agents": {"custom": [{"id": "x", "command": "/bin/other"}]}}"#));
        assert!(!has_sand_agent("{}"));
    }

    #[test]
    fn keep_tui_and_auto_acp_off_are_respected() {
        let agent = r#"{"id": "sand-claude", "name": "S", "command": "sand", "args": ["claude", "--acp"]}"#;
        let kept = format!(r#"{{"agents": {{"keep_tui": ["sand-claude"], "custom": [{agent}]}}}}"#);
        assert_eq!(find_agent(&kept, "claude", false), None);
        let off = format!(r#"{{"agents": {{"auto_acp": false, "custom": [{agent}]}}}}"#);
        assert_eq!(find_agent(&off, "claude", false), None);
    }
}
