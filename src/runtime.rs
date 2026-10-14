//! The per-sandbox runtime directory (mounted read-only at `~/.sand` inside the
//! sandbox): the status-line script and the status files it reads, plus the
//! banner sand prints before handing the terminal to the agent.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;

use crate::args;
use crate::harness::Profile;
use crate::mcp::{self, McpState, McpStatus};
use crate::mounts::MountSet;
use crate::paths::RUNTIME_DIR_NAME;
use crate::sbx::{Sbx, shell_quote};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiState {
    Ok,
    /// The sandbox network policy rejected the request.
    Blocked,
    Unreachable,
    /// The harness has no API endpoint configured to probe.
    NotConfigured,
}

/// Interprets the output of `curl -w '\n%{http_code}'` run inside the sandbox.
/// Any HTTP answer from the API (including 401) proves the connection works;
/// only proxy policy blocks, 5xx and transport failures count as down.
pub fn classify_api(output: &str) -> ApiState {
    let (body, code) = output.trim_end().rsplit_once('\n').unwrap_or(("", output.trim()));
    let code: u16 = code.trim().parse().unwrap_or(0);
    let policy_block = ["Blocked by", "Approval required"].iter().any(|p| body.trim_start().starts_with(p));
    match code {
        0 => ApiState::Unreachable,
        403 if policy_block => ApiState::Blocked,
        500.. => ApiState::Unreachable,
        _ => ApiState::Ok,
    }
}

pub fn check_api(sbx: &Sbx, sandbox: &str, profile: &Profile) -> ApiState {
    let Some(url) = &profile.api_check_url else { return ApiState::NotConfigured };
    let script = format!("curl -sS -m 6 -w '\\n%{{http_code}}' {} 2>/dev/null", shell_quote(url));
    match sbx.capture(args!["exec", sandbox, "bash", "-lc", script]) {
        Ok(Some(out)) => classify_api(&out),
        // curl exits non-zero on transport errors.
        _ => ApiState::Unreachable,
    }
}

/// Writes the files the in-sandbox status line reads.
pub fn write_info(runtime: &Path, sandbox: &str, profile: &Profile) -> Result<()> {
    fs::create_dir_all(runtime).with_context(|| format!("creating {}", runtime.display()))?;
    let info = format!(
        "sandbox={sandbox}\nharness={}\napi_url={}\n",
        profile.name,
        profile.api_check_url.as_deref().unwrap_or("")
    );
    fs::write(runtime.join("info"), info)?;
    fs::write(runtime.join("statusline.sh"), statusline_script())?;
    Ok(())
}

pub fn write_mcp_status(runtime: &Path, statuses: &[McpStatus]) -> Result<()> {
    let mut text = String::new();
    for status in statuses {
        text.push_str(&format!("{}\t{}\n", status.name, status.state.as_str()));
    }
    // Written via rename so the status line never reads a half-written file.
    let tmp = runtime.join("mcp-status.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, runtime.join("mcp-status"))?;
    Ok(())
}

/// Keeps `mcp-status` fresh while a session runs.
pub struct Refresher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Refresher {
    pub fn start(sbx: Sbx, runtime: std::path::PathBuf, servers: Vec<String>) -> Option<Self> {
        if servers.is_empty() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::spawn(move || {
            let mut ticks = 0u32;
            while !flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                ticks += 1;
                if ticks.is_multiple_of(120) {
                    let _ = write_mcp_status(&runtime, &mcp::status(&sbx, &servers));
                }
            }
        });
        Some(Self { stop, handle: Some(handle) })
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn mark(ok: bool) -> console::StyledObject<&'static str> {
    if ok { style("✓").green().for_stderr() } else { style("✗").red().for_stderr() }
}

pub fn describe_mcp(statuses: &[McpStatus]) -> String {
    if statuses.is_empty() {
        return "none (the agent can discover registered servers through the gateway)".to_string();
    }
    statuses
        .iter()
        .map(|s| match s.state {
            McpState::Ok => format!("{} {}", s.name, mark(true)),
            McpState::NeedsAuth => {
                format!("{} {} needs auth (sbx mcp auth {})", s.name, style("⚠").yellow().for_stderr(), s.name)
            }
            McpState::Error => format!("{} {}", s.name, mark(false)),
            McpState::Unknown => format!("{} {}", s.name, style("?").dim().for_stderr()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn describe_api(api: ApiState, profile: &Profile) -> String {
    let url = profile.api_check_url.as_deref().unwrap_or("");
    let host = url.split("//").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url);
    match api {
        ApiState::Ok => format!("{} {host} reachable", mark(true)),
        ApiState::Blocked => {
            format!("{} {host} blocked by the sandbox network policy (see `sbx policy log`)", mark(false))
        }
        ApiState::Unreachable => format!("{} {host} unreachable from the sandbox", mark(false)),
        ApiState::NotConfigured => format!("not checked (set harness.{}.api_check_url to enable)", profile.name),
    }
}

/// The status summary shown before the agent starts and by `sand status`.
pub fn banner(sandbox: &str, profile: &Profile, mounts: &MountSet, api: ApiState, mcp: &[McpStatus]) {
    let label = |text: &str| style(format!("{text:<8}")).dim().for_stderr();
    eprintln!();
    eprintln!("  {} {} ({})", label("sandbox"), style(sandbox).cyan().bold().for_stderr(), profile.name);
    for (i, line) in mounts.describe().iter().enumerate() {
        eprintln!("  {} {line}", label(if i == 0 { "mounts" } else { "" }));
    }
    eprintln!("  {} {}", label("API"), describe_api(api, profile));
    eprintln!("  {} {}", label("MCP"), describe_mcp(mcp));
    eprintln!();
}

/// POSIX shell status line, two rows:
///
/// ```text
/// 📦 <sandbox> │ ⎇ <branch> │ API ✓ │ MCP a ✓ b ✗
/// 🤖 <model> │ ctx ████░░░░░░ 42% │ 5h 23% │ 7d 41%
/// ```
///
/// The second row comes from the session JSON the harness pipes to stdin
/// (Claude Code's schema); parts the harness does not report are left out.
/// It must return immediately, so the API and MCP gateway probes run in the
/// background and the line shows the most recent cached result.
pub fn statusline_script() -> String {
    format!(
        r#"#!/bin/sh
# Generated by sand. Status line for agent sessions running in a Docker Sandbox.
DIR="${{HOME:-/home/agent}}/{RUNTIME_DIR_NAME}"
CACHE="${{TMPDIR:-/tmp}}/sand-status"
TTL=30
mkdir -p "$CACHE" 2>/dev/null
input=$(cat 2>/dev/null | tr -d '\n') # session JSON piped in by the harness

# field OBJECT KEY: the value of the first KEY after OBJECT in the session JSON.
# The objects read here are flat, so this needs no JSON parser in the sandbox.
field() {{
  printf '%s\n' "$input" | awk -v obj="\"$1\"" -v key="\"$2\"" '{{
    i = index($0, obj); if (!i) exit
    s = substr($0, i + length(obj)); i = index(s, key); if (!i) exit
    s = substr(s, i + length(key)); sub(/^[ \t]*:[ \t]*/, "", s)
    if (s ~ /^"/) {{ s = substr(s, 2); sub(/".*/, "", s) }} else sub(/[,}} \t].*/, "", s)
    print s
  }}' 2>/dev/null
}}
# percent OBJECT: its used_percentage as a whole number, or nothing.
percent() {{
  pct=$(field "$1" used_percentage); pct=${{pct%%.*}}
  case "$pct" in ''|*[!0-9]*) return 1 ;; esac
  echo "$pct"
}}

value() {{ sed -n "s/^$1=//p" "$DIR/info" 2>/dev/null | head -n 1; }}
name=$(value sandbox)
[ -n "$name" ] || name="${{SANDBOX_NAME:-sandbox}}"
api_url=$(value api_url)

now=$(date +%s)
age() {{
  modified=$(stat -c %Y "$1" 2>/dev/null) || {{ echo 999999; return; }}
  echo $((now - modified))
}}

probe_api() {{
  out=$(curl -sS -m 6 -w '\n%{{http_code}}' "$api_url" 2>/dev/null) || out=000
  code=$(printf '%s\n' "$out" | tail -n 1)
  case "$code" in
    000|5??) state=down ;;
    403) case "$out" in "Blocked by"*|"Approval required"*) state=blocked ;; *) state=ok ;; esac ;;
    *) state=ok ;;
  esac
  printf '%s\n' "$state" > "$CACHE/api.tmp" && mv "$CACHE/api.tmp" "$CACHE/api"
}}

probe_gateway() {{
  state=down
  if [ -n "${{MCP_GATEWAY_URL:-}}" ]; then
    code=$(curl -s -m 4 -o /dev/null -w '%{{http_code}}' "$MCP_GATEWAY_URL" 2>/dev/null) || code=000
    case "$code" in 000|5??) state=down ;; *) state=ok ;; esac
  fi
  printf '%s\n' "$state" > "$CACHE/gateway.tmp" && mv "$CACHE/gateway.tmp" "$CACHE/gateway"
}}

# Refresh stale results in the background; "busy" stops probes from piling up.
if [ "$(age "$CACHE/stamp")" -gt "$TTL" ] && [ "$(age "$CACHE/busy")" -gt 10 ]; then
  : > "$CACHE/busy"
  ( {{ [ -n "$api_url" ] && probe_api; [ -s "$DIR/mcp-status" ] && probe_gateway; : > "$CACHE/stamp"; }} >/dev/null 2>&1 & )
fi

green=$(printf '\033[32m'); red=$(printf '\033[31m'); yellow=$(printf '\033[33m')
dim=$(printf '\033[2m'); cyan=$(printf '\033[36m'); reset=$(printf '\033[0m')
sep=" ${{dim}}│${{reset}} "

line="📦 ${{cyan}}${{name}}${{reset}}"

cwd=$(field workspace current_dir)
branch=$(git --no-optional-locks -C "${{cwd:-.}}" symbolic-ref --quiet --short HEAD 2>/dev/null) ||
  branch=$(git --no-optional-locks -C "${{cwd:-.}}" rev-parse --short HEAD 2>/dev/null)
[ -n "$branch" ] && line="${{line}}${{sep}}⎇ ${{branch}}"

if [ -n "$api_url" ]; then
  case "$(cat "$CACHE/api" 2>/dev/null)" in
    ok) api="${{green}}✓${{reset}}" ;;
    blocked) api="${{red}}✗ blocked${{reset}}" ;;
    down) api="${{red}}✗${{reset}}" ;;
    *) api="${{dim}}…${{reset}}" ;;
  esac
  line="${{line}}${{sep}}API ${{api}}"
fi

if [ -s "$DIR/mcp-status" ]; then
  gateway=$(cat "$CACHE/gateway" 2>/dev/null)
  mcp=""
  while IFS="$(printf '\t')" read -r server state; do
    [ -n "$server" ] || continue
    if [ "$gateway" = down ]; then
      icon="${{red}}✗${{reset}}"
    else
      case "$state" in
        ok) icon="${{green}}✓${{reset}}" ;;
        auth) icon="${{yellow}}⚠ auth${{reset}}" ;;
        error) icon="${{red}}✗${{reset}}" ;;
        *) icon="${{dim}}?${{reset}}" ;;
      esac
    fi
    mcp="${{mcp}} ${{server}} ${{icon}}"
  done < "$DIR/mcp-status"
  line="${{line}}${{sep}}MCP${{mcp}}"
else
  line="${{line}}${{sep}}MCP ${{dim}}none${{reset}}"
fi

printf '%s\n' "$line"

# Second row: model, context window and subscription limits.
level() {{
  if [ "$1" -ge 90 ]; then printf '%s' "$red"; elif [ "$1" -ge 70 ]; then printf '%s' "$yellow"; else printf '%s' "$green"; fi
}}
bar() {{
  filled=$((($1 + 5) / 10)); [ "$filled" -gt 10 ] && filled=10
  cells=""; i=0
  while [ "$i" -lt 10 ]; do
    if [ "$i" -lt "$filled" ]; then cells="${{cells}}█"; else cells="${{cells}}░"; fi
    i=$((i + 1))
  done
  printf '%s' "$cells"
}}
add() {{ usage="${{usage:+${{usage}}${{sep}}}}$1"; }}

usage=""
model=$(field model display_name)
[ -n "$model" ] && add "🤖 $model"
used=$(percent context_window) && add "ctx $(level "$used")$(bar "$used")${{reset}} ${{used}}%"
used=$(percent five_hour) && add "5h $(level "$used")${{used}}%${{reset}}"
used=$(percent seven_day) && add "7d $(level "$used")${{used}}%${{reset}}"
[ -n "$usage" ] && printf '%s\n' "$usage"
exit 0
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_classification() {
        assert_eq!(classify_api("{\"data\":[]}\n200"), ApiState::Ok);
        assert_eq!(classify_api("{\"error\":\"unauthorized\"}\n401"), ApiState::Ok);
        assert_eq!(classify_api("{\"error\":\"forbidden\"}\n403"), ApiState::Ok);
        assert_eq!(
            classify_api("Blocked by network policy: domain api.openai.com\n  detail: x\n403"),
            ApiState::Blocked
        );
        assert_eq!(classify_api("Approval required for api.anthropic.com:443.\n\n403"), ApiState::Blocked);
        assert_eq!(classify_api("oops\n502"), ApiState::Unreachable);
        assert_eq!(classify_api("\n000"), ApiState::Unreachable);
        assert_eq!(classify_api(""), ApiState::Unreachable);
    }

    #[cfg(unix)]
    fn run_statusline(home: &Path, input: &str) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("sh")
            .arg(home.join(RUNTIME_DIR_NAME).join("statusline.sh"))
            .env("HOME", home)
            .env("TMPDIR", home)
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
        console::strip_ansi_codes(&out).into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn statusline_shows_model_context_and_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = Profile::resolve("codex", &crate::config::Config::default()).unwrap();
        let profile = Profile { api_check_url: None, ..profile };
        write_info(&tmp.path().join(RUNTIME_DIR_NAME), "claude-proj", &profile).unwrap();

        let input = r#"{
            "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
            "workspace": {"current_dir": "/nonexistent", "project_dir": "/nonexistent"},
            "context_window": {"context_window_size": 200000, "used_percentage": 42.4, "remaining_percentage": 57.6,
                "current_usage": {"input_tokens": 8500}},
            "rate_limits": {"five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 91, "resets_at": 1738857600}}
        }"#;
        assert_eq!(
            run_statusline(tmp.path(), input),
            "📦 claude-proj │ MCP none\n🤖 Opus 5.5 │ ctx ████░░░░░░ 42% │ 5h 23% │ 7d 91%\n"
        );

        // API-key sessions have no rate limits, and the context is unknown until the first response.
        let input = r#"{"model":{"id":"x","display_name":"Sonnet"},"context_window":{"used_percentage":null}}"#;
        assert_eq!(run_statusline(tmp.path(), input), "📦 claude-proj │ MCP none\n🤖 Sonnet\n");
        assert_eq!(run_statusline(tmp.path(), ""), "📦 claude-proj │ MCP none\n");
    }

    #[cfg(unix)]
    #[test]
    fn statusline_shows_the_git_branch() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let profile = Profile::resolve("codex", &crate::config::Config::default()).unwrap();
        write_info(&tmp.path().join(RUNTIME_DIR_NAME), "claude-proj", &Profile { api_check_url: None, ..profile })
            .unwrap();
        let repo = tmp.path().join("repo");
        let init = Command::new("git").args(["init", "-q", "-b", "feature/x"]).arg(&repo).status();
        if !init.is_ok_and(|s| s.success()) {
            return; // no git on this machine
        }
        let input = format!(r#"{{"workspace":{{"current_dir":"{}"}}}}"#, repo.display());
        assert_eq!(run_statusline(tmp.path(), &input), "📦 claude-proj │ ⎇ feature/x │ MCP none\n");
    }

    #[test]
    fn status_files_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = Profile::resolve("claude", &crate::config::Config::default()).unwrap();
        write_info(tmp.path(), "claude-proj", &profile).unwrap();
        let info = fs::read_to_string(tmp.path().join("info")).unwrap();
        assert!(
            info.contains("sandbox=claude-proj\n") && info.contains("api_url=https://api.anthropic.com/v1/models\n")
        );
        write_mcp_status(
            tmp.path(),
            &[
                McpStatus { name: "atlassian".into(), state: McpState::Ok },
                McpStatus { name: "gitlab".into(), state: McpState::NeedsAuth },
            ],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join("mcp-status")).unwrap(), "atlassian\tok\ngitlab\tauth\n");
    }
}
