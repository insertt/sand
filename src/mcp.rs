//! MCP servers. sbx owns the registry (`sbx mcp add`) and the per-sandbox
//! gateway; sand selects servers per sandbox with `--mcp a,b`.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::args;
use crate::config::Config;
use crate::log;
use crate::sbx::Sbx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpState {
    Ok,
    NeedsAuth,
    Error,
    /// Registered, but sbx did not report a state sand understands.
    Unknown,
}

impl McpState {
    pub fn as_str(self) -> &'static str {
        match self {
            McpState::Ok => "ok",
            McpState::NeedsAuth => "auth",
            McpState::Error => "error",
            McpState::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpStatus {
    pub name: String,
    pub state: McpState,
}

/// Normalizes `--mcp a,b --mcp c` into a de-duplicated list, keeping order.
pub fn normalize(names: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in names.iter().flat_map(|n| n.split(',')).map(str::trim).filter(|n| !n.is_empty()) {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
            bail!("invalid MCP server name '{name}' (letters, numbers, dots, hyphens and underscores only)");
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

fn registered(sbx: &Sbx) -> Result<BTreeSet<String>> {
    match sbx.capture(args!["mcp", "ls", "-q"])? {
        Some(out) => Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()),
        None => bail!("`sbx mcp ls` failed; cannot check which MCP servers are registered"),
    }
}

/// Makes sure every requested server is registered with sbx, registering the
/// ones defined under `[mcp.<name>]` in config.toml.
pub fn ensure_registered(sbx: &Sbx, config: &Config, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    log::step(format!("Checking MCP servers: {}", names.join(", ")));
    let known = registered(sbx)?;
    for name in names.iter().filter(|n| !known.contains(*n)) {
        let Some(def) = config.mcp.get(name) else {
            bail!(
                "MCP server '{name}' is not registered with sbx.\n\
                 Register it once with `sbx mcp add {name} --url <URL>` (or `--command <CMD>`), \
                 or define [mcp.{name}] in sand's config.toml so sand can register it for you."
            );
        };
        log::info(format!("'{name}' is not registered yet; registering it from config.toml"));
        let mut cmd = args!["mcp", "add", name];
        match (&def.url, &def.command) {
            (Some(url), None) => cmd.extend(args!["--url", url]),
            (None, Some(command)) => {
                cmd.extend(args!["--command", command]);
                if !def.args.is_empty() {
                    cmd.extend(args!["--args", def.args.join(",")]);
                }
            }
            _ => bail!("[mcp.{name}] in config.toml needs exactly one of `url` or `command`"),
        }
        sbx.run(cmd)?;
    }
    log::ok("all requested MCP servers are registered");
    Ok(())
}

/// Attaches a registered server to a running sandbox's gateway.
pub fn load(sbx: &Sbx, sandbox: &str, name: &str) -> Result<()> {
    sbx.run(args!["mcp", "load", name, "--sandbox", sandbox])
}

fn find_servers<'a>(value: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| find_servers(item, out)),
        Value::Object(map) => {
            if map.get("name").is_some_and(Value::is_string) {
                out.push(map);
            } else {
                map.values().for_each(|v| find_servers(v, out));
            }
        }
        _ => {}
    }
}

fn truthy(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Derives a state from one server entry of `sbx mcp ls --json`. The schema is
/// not documented, so this looks for the obvious signals and otherwise reports
/// the server as unknown rather than guessing.
fn interpret(entry: &serde_json::Map<String, Value>) -> McpState {
    // sbx names the command to run next; for a server that needs authorization
    // it is `sbx mcp auth <name>`, whatever the status is called.
    if entry.get("next_step").and_then(Value::as_str).is_some_and(|step| step.contains("sbx mcp auth")) {
        return McpState::NeedsAuth;
    }
    let mut state = McpState::Unknown;
    for (key, value) in entry {
        let key = key.to_ascii_lowercase();
        let text = value.as_str().map(str::to_ascii_lowercase).unwrap_or_default();
        if key.contains("auth") {
            let needs = match truthy(value) {
                Some(flag) => (key.contains("need") || key.contains("required")) == flag,
                None => ["need", "required", "unauth", "expired", "missing", "not authorized"]
                    .iter()
                    .any(|w| text.contains(w)),
            };
            if needs {
                return McpState::NeedsAuth;
            }
            if truthy(value) == Some(true) || ["authorized", "ok", "valid"].contains(&text.as_str()) {
                state = McpState::Ok;
            }
        } else if ["status", "state", "health"].contains(&key.as_str()) {
            if ["need", "auth", "unauth"].iter().any(|w| text.contains(w)) && !text.starts_with("authorized") {
                return McpState::NeedsAuth;
            }
            if ["error", "fail", "unsupported", "unavailable", "missing", "skipped"].iter().any(|w| text.contains(w)) {
                return McpState::Error;
            }
            if ["ok", "ready", "usable", "connected", "available", "authorized", "healthy", "active"]
                .contains(&text.as_str())
            {
                state = McpState::Ok;
            }
        } else if ["usable", "ready", "available", "ok"].contains(&key.as_str()) {
            match truthy(value) {
                Some(true) => state = McpState::Ok,
                Some(false) => return McpState::Error,
                None => {}
            }
        } else if key == "error" && !value.is_null() && !text.is_empty() {
            return McpState::Error;
        }
    }
    state
}

pub fn parse_status(json: &str, names: &[String]) -> Vec<McpStatus> {
    let parsed: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut servers = Vec::new();
    find_servers(&parsed, &mut servers);
    names
        .iter()
        .map(|name| {
            let entry = servers.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str()));
            let state = match entry {
                Some(entry) => interpret(entry),
                None if parsed.is_null() => McpState::Unknown,
                None => McpState::Error,
            };
            McpStatus { name: name.clone(), state }
        })
        .collect()
}

/// Host-side view of the requested servers (registered? authorized?).
pub fn status(sbx: &Sbx, names: &[String]) -> Vec<McpStatus> {
    if names.is_empty() {
        return Vec::new();
    }
    if let Ok(Some(json)) = sbx.capture(args!["mcp", "ls", "--json"])
        && serde_json::from_str::<Value>(&json).is_ok()
    {
        return parse_status(&json, names);
    }
    // Fall back to "is it registered at all".
    let known = registered(sbx).ok();
    names
        .iter()
        .map(|name| McpStatus {
            name: name.clone(),
            state: match &known {
                Some(known) if known.contains(name) => McpState::Unknown,
                Some(_) => McpState::Error,
                None => McpState::Unknown,
            },
        })
        .collect()
}

/// Reauthorizes the servers a sandbox uses whose credentials sbx reports as
/// missing or expired. `sbx mcp auth` refreshes a credential on its own when
/// it can and only starts the OAuth sign-in when that needs the user, so it
/// gets the terminal. A server that stays unauthorized does not stop the launch.
pub fn ensure_authorized(sbx: &Sbx, names: &[String], interactive: bool) -> Result<()> {
    let pending: Vec<String> =
        status(sbx, names).into_iter().filter(|s| s.state == McpState::NeedsAuth).map(|s| s.name).collect();
    if pending.is_empty() {
        return Ok(());
    }
    log::step(format!("Authorizing MCP servers: {}", pending.join(", ")));
    if !interactive {
        log::warn(format!(
            "these need authorization, which may open a sign-in, and there is no terminal to do that on; run `sbx mcp auth {}` yourself",
            pending.join("`, `sbx mcp auth ")
        ));
        return Ok(());
    }
    for name in &pending {
        log::info(format!("{name}: refreshing the credential; sbx asks you to sign in if that needs your consent"));
        if sbx.interact(args!["mcp", "auth", name])?.success() {
            log::ok(format!("{name} is authorized"));
        } else {
            log::warn(format!(
                "could not authorize '{name}'; continuing without it (retry with `sbx mcp auth {name}`)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_split_trimmed_and_deduplicated() {
        let got = normalize(&["atlassian, gitlab".into(), "gitlab,notion".into(), "".into()]).unwrap();
        assert_eq!(got, vec!["atlassian", "gitlab", "notion"]);
        assert!(normalize(&["bad name".into()]).is_err());
        assert!(normalize(&[]).unwrap().is_empty());
    }

    fn states(json: &str, names: &[&str]) -> Vec<McpState> {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        parse_status(json, &names).into_iter().map(|s| s.state).collect()
    }

    #[test]
    fn status_parsing_tolerates_different_shapes() {
        let flat = r#"[
            {"name": "a", "type": "remote", "status": "ready"},
            {"name": "b", "auth_status": "needs authorization"},
            {"name": "c", "usable": false},
            {"name": "d", "type": "local"},
            {"name": "e", "authorized": false},
            {"name": "f", "needs_auth": true},
            {"name": "g", "needs_auth": false, "usable": true},
            {"name": "h", "status": "expired", "next_step": "sbx mcp auth h"}
        ]"#;
        assert_eq!(states(flat, &["h"]), vec![McpState::NeedsAuth]);
        assert_eq!(
            states(flat, &["a", "b", "c", "d", "e", "f", "g", "zz"]),
            vec![
                McpState::Ok,
                McpState::NeedsAuth,
                McpState::Error,
                McpState::Unknown,
                McpState::NeedsAuth,
                McpState::NeedsAuth,
                McpState::Ok,
                McpState::Error
            ]
        );
        let nested = r#"{"gateways": [{"gateway": "local", "servers": [{"name": "a", "state": "error: boom"}]}]}"#;
        assert_eq!(states(nested, &["a"]), vec![McpState::Error]);
        assert_eq!(states("not json", &["a"]), vec![McpState::Unknown]);
        // What sbx prints for a server waiting for `sbx mcp auth`.
        let real = r#"{
            "gateway": {"name": "LOCAL", "local": true, "operator": "managed by you", "decision": "local", "signed_in_as": "insertt"},
            "servers": [
                {"name": "notion", "transport": "remote http", "status": "needs-auth", "type": "remote", "next_step": "sbx mcp auth notion"}
            ]
        }"#;
        assert_eq!(states(real, &["notion", "gitlab"]), vec![McpState::NeedsAuth, McpState::Error]);
    }
}
