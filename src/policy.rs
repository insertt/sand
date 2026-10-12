//! `sand allow`: grant a sandbox network access to hosts its policy blocked.

use std::io::IsTerminal;

use anyhow::{Context, Result, anyhow, bail};
use console::Term;
use dialoguer::MultiSelect;
use serde::Deserialize;

use crate::args;
use crate::cli::AllowArgs;
use crate::config::Config;
use crate::log;
use crate::paths::Dirs;
use crate::sbx::Sbx;
use crate::session;

/// `sbx policy log --json`.
#[derive(Debug, Default, Deserialize)]
struct PolicyLog {
    #[serde(default)]
    blocked_hosts: Vec<Entry>,
    #[serde(default)]
    allowed_hosts: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    host: String,
    #[serde(default)]
    vm_name: String,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    last_seen: String,
    #[serde(default)]
    count_since: u64,
    #[serde(default)]
    reason: String,
}

/// A host (with its port, when sbx logged one) the sandbox could not reach.
#[derive(Debug, PartialEq)]
struct Blocked {
    host: String,
    count: u64,
    last_seen: String,
    reason: String,
    /// Whether an allow rule lifts the block. Only default-deny blocks qualify;
    /// explicit deny rules and proxy-internal blocks are not undone by one.
    grantable: bool,
}

impl Blocked {
    fn describe(&self) -> String {
        let seen = self.last_seen.get(..19).unwrap_or(&self.last_seen).replace('T', " ");
        let requests = if self.count == 1 { "request" } else { "requests" };
        format!("{}  ({} {requests}, last {seen})", self.host, self.count)
    }
}

/// The sandbox's blocked hosts, most recent first. A host that was reached
/// after it was last blocked is left out: it has been allowed since.
fn blocked_hosts(log: PolicyLog, sandbox: &str) -> Vec<Blocked> {
    let mine = |e: &Entry| e.vm_name.is_empty() || e.vm_name == sandbox;
    let mut out: Vec<Blocked> = Vec::new();
    for entry in log.blocked_hosts.iter().filter(|e| mine(e)) {
        // sbx writes RFC 3339 timestamps in local time, which order as text.
        if log.allowed_hosts.iter().any(|a| mine(a) && a.host == entry.host && a.last_seen > entry.last_seen) {
            continue;
        }
        let grantable = entry.rule.starts_with("no applicable policies")
            || entry.reason.to_ascii_lowercase().contains("default deny");
        match out.iter_mut().find(|b| b.host == entry.host) {
            Some(seen) => {
                seen.count += entry.count_since;
                seen.grantable &= grantable;
                if entry.last_seen > seen.last_seen {
                    seen.last_seen = entry.last_seen.clone();
                    seen.reason = entry.reason.clone();
                }
            }
            None => out.push(Blocked {
                host: entry.host.clone(),
                count: entry.count_since,
                last_seen: entry.last_seen.clone(),
                reason: entry.reason.clone(),
                grantable,
            }),
        }
    }
    out.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    out
}

fn read_blocked(sbx: &Sbx, sandbox: &str) -> Result<Vec<Blocked>> {
    let text = sbx
        .capture(args!["policy", "log", sandbox, "--json"])?
        .ok_or_else(|| anyhow!("`sbx policy log {sandbox} --json` failed"))?;
    let log: PolicyLog = serde_json::from_str(&text).context("parsing the output of `sbx policy log --json`")?;
    Ok(blocked_hosts(log, sandbox))
}

fn validate_hosts(hosts: &[String]) -> Result<()> {
    for host in hosts {
        if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
            bail!("'{host}' is not a host; use a name like example.com, example.com:443 or *.example.com");
        }
    }
    Ok(())
}

/// Lets the user tick the hosts to allow; nothing is ticked to begin with.
fn choose(sandbox: &str, blocked: &[Blocked]) -> Result<Vec<String>> {
    let offered: Vec<&Blocked> = blocked.iter().filter(|b| b.grantable).collect();
    for other in blocked.iter().filter(|b| !b.grantable) {
        log::info(format!("not offered: {} — {} (an allow rule does not lift this)", other.host, other.reason));
    }
    if offered.is_empty() {
        log::info("nothing here can be allowed with a rule");
        return Ok(Vec::new());
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "sandbox '{sandbox}' has {} blocked host(s) but there is no terminal to choose on; \
             pass --host HOST[,HOST...] or see them with --list.",
            offered.len()
        );
    }
    let labels: Vec<String> = offered.iter().map(|b| b.describe()).collect();
    let picked = MultiSelect::new()
        .with_prompt(format!("Allow '{sandbox}' to reach (space selects, enter confirms)"))
        .items(&labels)
        .interact_on_opt(&Term::stderr())?;
    Ok(picked.unwrap_or_default().into_iter().map(|i| offered[i].host.clone()).collect())
}

/// `sand allow`: show what the sandbox was blocked from reaching and add allow rules.
pub fn allow(args: AllowArgs, dirs: &Dirs, config: &Config) -> Result<i32> {
    let sbx = Sbx::new(true);
    let (name, _) = session::find_sandbox(&sbx, &args.targets, args.name.as_deref(), args.scratch, dirs, config)?;

    let hosts = if args.host.is_empty() {
        let blocked = read_blocked(&sbx, &name)?;
        if blocked.is_empty() {
            log::ok(format!("nothing is blocked for sandbox '{name}'"));
            return Ok(0);
        }
        if args.list {
            for entry in &blocked {
                let note = if entry.grantable { String::new() } else { format!("  [{}]", entry.reason) };
                println!("{}{note}", entry.describe());
            }
            return Ok(0);
        }
        log::step(format!("Hosts sandbox '{name}' was blocked from reaching"));
        choose(&name, &blocked)?
    } else {
        validate_hosts(&args.host)?;
        args.host
    };
    if hosts.is_empty() {
        log::info("nothing selected; the policy is unchanged");
        return Ok(0);
    }

    log::step(if args.global {
        "Allowing for every sandbox".to_string()
    } else {
        format!("Allowing for sandbox '{name}'")
    });
    let mut cmd = args!["policy", "allow", "network"];
    if !args.global {
        cmd.extend(args!["--sandbox", &name]);
    }
    cmd.push(hosts.join(",").into());
    sbx.run(cmd)?;
    log::ok(format!("allowed {}", hosts.join(", ")));
    log::info("requests that were blocked have already failed; ask the agent to try again");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<Blocked> {
        blocked_hosts(serde_json::from_str(json).unwrap(), "box")
    }

    #[test]
    fn blocked_hosts_are_filtered_merged_and_sorted() {
        let blocked = parse(
            r#"{
              "blocked_hosts": [
                {"host": "old.example.com:443", "vm_name": "box", "rule": "no applicable policies for op(x)",
                 "last_seen": "2026-10-12T12:00:00.5+02:00", "count_since": 2, "reason": "No matching allow rule (default deny)"},
                {"host": "new.example.com:443", "vm_name": "box", "rule": "no applicable policies for op(x)",
                 "last_seen": "2026-10-12T16:07:30.847426+02:00", "count_since": 104, "reason": "No matching allow rule (default deny)"},
                {"host": "new.example.com:443", "vm_name": "box", "proxy_type": "network", "rule": "no applicable policies for op(y)",
                 "last_seen": "2026-10-12T16:07:31.1+02:00", "count_since": 1, "reason": "No matching allow rule (default deny)"},
                {"host": "elsewhere.example.com:443", "vm_name": "other", "rule": "no applicable policies for op(x)",
                 "last_seen": "2026-10-12T17:00:00+02:00", "count_since": 9, "reason": "No matching allow rule (default deny)"},
                {"host": "gateway.internal", "vm_name": "box", "rule": "<dns proxy policy>",
                 "last_seen": "2026-10-12T12:51:16.790945+02:00", "count_since": 2, "reason": "DNS lookup blocked by proxy policy"},
                {"host": "fixed.example.com:443", "vm_name": "box", "rule": "no applicable policies for op(x)",
                 "last_seen": "2026-10-12T13:00:00+02:00", "count_since": 3, "reason": "No matching allow rule (default deny)"}
              ],
              "allowed_hosts": [
                {"host": "fixed.example.com:443", "vm_name": "box", "rule": "", "last_seen": "2026-10-12T13:05:00+02:00", "count_since": 1},
                {"host": "old.example.com:443", "vm_name": "other", "rule": "", "last_seen": "2026-10-12T18:00:00+02:00", "count_since": 1}
              ]
            }"#,
        );
        let hosts: Vec<&str> = blocked.iter().map(|b| b.host.as_str()).collect();
        assert_eq!(hosts, ["new.example.com:443", "gateway.internal", "old.example.com:443"]);
        assert_eq!(blocked[0].count, 105);
        assert_eq!(blocked[0].describe(), "new.example.com:443  (105 requests, last 2026-10-12 16:07:31)");
        assert!(blocked[0].grantable && !blocked[1].grantable && blocked[2].grantable);
        assert_eq!(blocked[1].reason, "DNS lookup blocked by proxy policy");
    }

    #[test]
    fn an_empty_log_has_nothing_blocked() {
        assert!(parse("{}").is_empty());
        assert!(parse(r#"{"blocked_hosts": [], "allowed_hosts": []}"#).is_empty());
    }

    #[test]
    fn hosts_given_by_hand_are_checked() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        validate_hosts(&s(&["example.com", "example.com:443", "*.example.com", "**"])).unwrap();
        for bad in ["", "--sandbox", "a b"] {
            assert!(validate_hosts(&s(&[bad])).is_err(), "{bad:?}");
        }
    }
}
