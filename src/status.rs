//! `sand status`: everything sand recorded about a sandbox, what sbx says it is
//! doing, and (unless `--no-probe`) whether the agent API and the MCP servers
//! can be reached right now.

use std::path::{Path, PathBuf};

use anyhow::Result;
use console::style;
use serde::Serialize;

use crate::cli::StatusArgs;
use crate::config::Config;
use crate::harness::Profile;
use crate::lock;
use crate::log;
use crate::mcp::{self, McpStatus};
use crate::paths::Dirs;
use crate::runtime::{self, ApiState};
use crate::sbx::Sbx;
use crate::session;
use crate::state::{self, Kind, SandboxState};

#[derive(Debug, Serialize)]
struct Status {
    name: String,
    /// As reported by `sbx ls --json` (running, stopped, …).
    sbx_state: Option<String>,
    /// Who holds the session lock, while a sand session is attached.
    session: Option<String>,
    data_dir: PathBuf,
    /// sand's record; `None` for a sandbox sand did not create.
    record: Option<SandboxState>,
    /// The agent API probe; `None` when it was not run.
    api: Option<ApiState>,
    /// The recorded MCP servers as the gateway reports them; empty when not probed.
    mcp: Vec<McpStatus>,
}

/// Why the live checks did not run, if they did not.
fn skip_reason(args: &StatusArgs, sbx_state: Option<&str>) -> Option<&'static str> {
    if args.no_probe {
        Some("not checked (--no-probe)")
    } else if sbx_state.is_some_and(|s| s != "running") {
        // `sbx exec` would start it; a status query should not.
        Some("not checked (the sandbox is not running)")
    } else {
        None
    }
}

pub fn status(args: StatusArgs, dirs: &Dirs, config: &Config) -> Result<i32> {
    let sbx = Sbx::connect(true)?;
    let (name, harness) = session::find_sandbox(&sbx, &args.targets, args.name.as_deref(), args.scratch, dirs, config)?;
    let record = state::load(dirs, &name)?;
    let profile = Profile::resolve(record.as_ref().map_or(&harness, |r| &r.harness), config)?;
    let sbx_state = sbx.sandbox_state(&name)?;
    let skipped = skip_reason(&args, sbx_state.as_deref());

    let api = skipped.is_none().then(|| runtime::check_api(&sbx, &name, &profile));
    // The gateway runs on the host, so a stopped sandbox does not stop this one.
    let mcp = match &record {
        Some(r) if !args.no_probe => mcp::status(&sbx, &r.mcp),
        _ => Vec::new(),
    };
    let status = Status {
        sbx_state,
        session: lock::holder(dirs, &name),
        data_dir: dirs.sandbox_dir(&name),
        record,
        api,
        mcp,
        name,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(0);
    }
    if status.record.is_none() {
        log::warn(format!("'{}' was not created by sand; only what sbx reports is known", status.name));
    }
    print(&status, &harness, &profile, skipped, state::now());
    Ok(0)
}

/// The banner helpers style for stderr; this prints to stdout.
fn for_stdout(text: String) -> String {
    if console::colors_enabled() { text } else { console::strip_ansi_codes(&text).into_owned() }
}

fn print(d: &Status, harness: &str, profile: &Profile, skipped: Option<&str>, now: u64) {
    let row = |label: &str, value: String| println!("  {} {value}", style(format!("{label:<9}")).dim());
    let rows = |label: &str, values: Vec<String>| {
        for (i, value) in values.into_iter().enumerate() {
            row(if i == 0 { label } else { "" }, value);
        }
    };
    println!();
    row("sandbox", style(&d.name).cyan().bold().to_string());
    row("state", describe_sbx_state(d.sbx_state.as_deref()));
    row("session", d.session.clone().unwrap_or_else(|| "none attached".to_string()));
    let api = || match (d.api, skipped) {
        (Some(api), _) => for_stdout(runtime::describe_api(api, profile)),
        (None, reason) => style(reason.unwrap_or("not checked")).dim().to_string(),
    };
    match &d.record {
        None => {
            row("harness", format!("{harness} (assumed; not recorded)"));
            row("API", api());
        }
        Some(r) => {
            let agent = if r.agent == r.harness { String::new() } else { format!(" (sbx agent {})", r.agent) };
            row("harness", format!("{}{agent}", r.harness));
            row("kind", describe_kind(r.kind).to_string());
            row("created", describe_time(r.created_at, now));
            row("template", r.template.clone().unwrap_or_else(|| "none (the agent's default image)".to_string()));
            if let Some(digest) = &r.image_digest {
                row("image", format!("nightly {}", short_digest(digest)));
            }
            if let Some(digest) = &r.declined_digest {
                row("", format!("newer build {} declined", short_digest(digest)));
            }
            rows("mounts", r.mounts.describe());
            let mcp = if !d.mcp.is_empty() {
                for_stdout(runtime::describe_mcp(&d.mcp))
            } else if r.mcp.is_empty() {
                "none (gateway discovery)".to_string()
            } else {
                format!("{} {}", r.mcp.join(", "), style("(not checked)").dim())
            };
            row("API", api());
            row("MCP", mcp);
            if !r.internal_mounts.is_empty() {
                // Host directories under the data directory are shown relative to it.
                let host = |h: &str| match Path::new(h).strip_prefix(&d.data_dir) {
                    Ok(rel) => rel.display().to_string(),
                    Err(_) => h.to_string(),
                };
                let internal = r
                    .internal_mounts
                    .iter()
                    .map(|m| format!("{} → {} ({})", host(&m.host), m.target, m.mode))
                    .collect();
                rows("internal", internal);
            }
        }
    }
    row("data", d.data_dir.display().to_string());
    println!();
}

fn describe_sbx_state(state: Option<&str>) -> String {
    match state {
        Some("running") => style("running").green().to_string(),
        Some(other) => style(other).yellow().to_string(),
        None => style("unknown (`sbx ls --json` did not say)").dim().to_string(),
    }
}

fn describe_kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Project => "project",
        Kind::Scratch => "shared scratch (--scratch)",
        Kind::Dedicated => "throwaway scratch (--dedicated-scratch)",
    }
}

fn short_digest(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algo, hex)) => format!("{algo}:{}", &hex[..hex.len().min(12)]),
        None => digest[..digest.len().min(12)].to_string(),
    }
}

/// `2026-10-22 14:03 UTC (2 days ago)`; 0 means sand did not record it.
fn describe_time(secs: u64, now: u64) -> String {
    if secs == 0 {
        return "unknown".to_string();
    }
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hour, minute) = (secs % 86_400 / 3600, secs % 3600 / 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC ({})", ago(now.saturating_sub(secs)))
}

fn ago(secs: u64) -> String {
    let (amount, unit) = match secs {
        0..60 => return "just now".to_string(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{amount} {unit}{} ago", if amount == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_civil_dates_with_a_relative_age() {
        assert_eq!(describe_time(0, 100), "unknown");
        assert_eq!(describe_time(1, 30), "1970-01-01 00:00 UTC (just now)");
        // 2024-02-29 12:34:56 UTC, a leap day.
        assert_eq!(describe_time(1_709_210_096, 1_709_210_096 + 7200), "2024-02-29 12:34 UTC (2 hours ago)");
        assert_eq!(describe_time(1_798_761_600, 1_798_761_600 + 86_400), "2027-01-01 00:00 UTC (1 day ago)");
        assert_eq!(ago(61), "1 minute ago");
    }

    #[test]
    fn digests_are_shortened() {
        assert_eq!(short_digest("sha256:0123456789abcdef0123"), "sha256:0123456789ab");
        assert_eq!(short_digest("abc"), "abc");
    }
}
