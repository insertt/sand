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

/// `sbx policy log --json`. Newer sbx versions write `blocked`, `allowed`,
/// `sandbox_id` and `count`; older ones the names below. Lists may be `null`.
#[derive(Debug, Default, Deserialize)]
struct PolicyLog {
    #[serde(default, alias = "blocked", deserialize_with = "null_as_empty")]
    blocked_hosts: Vec<Entry>,
    #[serde(default, alias = "allowed", deserialize_with = "null_as_empty")]
    allowed_hosts: Vec<Entry>,
}

const LOG_KEYS: [&str; 4] = ["blocked", "allowed", "blocked_hosts", "allowed_hosts"];

fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Entry>, D::Error> {
    Ok(Option::<Vec<Entry>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct Entry {
    host: String,
    #[serde(default, alias = "sandbox_id")]
    vm_name: String,
    #[serde(default)]
    last_seen: String,
    #[serde(default, alias = "count")]
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
    /// Whether the sandbox reached the host after it was last blocked.
    reached_since: bool,
}

impl Blocked {
    fn describe(&self) -> String {
        let seen = self.last_seen.get(..19).unwrap_or(&self.last_seen).replace('T', " ");
        let requests = if self.count == 1 { "request" } else { "requests" };
        let reached = if self.reached_since { ", reached since" } else { "" };
        // Default deny is the usual case; any other reason (an explicit deny
        // rule, org policy) is shown because an allow rule may not lift it.
        let note = if self.reason.is_empty() || self.reason.to_ascii_lowercase().contains("default deny") {
            String::new()
        } else {
            format!("  [{}]", self.reason)
        };
        format!("{}  ({} {requests}, last {seen}{reached}){note}", self.host, self.count)
    }
}

/// An RFC 3339 timestamp as UTC seconds and nanoseconds since the epoch, so
/// that times written with different offsets or precision order correctly.
fn instant(ts: &str) -> Option<(i64, u32)> {
    let num = |range: std::ops::Range<usize>| ts.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let rest = &ts[19..];
    let (nanos, zone) = match rest.strip_prefix('.') {
        Some(frac) => {
            let digits = frac.find(|c: char| !c.is_ascii_digit()).unwrap_or(frac.len());
            let padded = format!("{:0<9}", &frac[..digits.min(9)]);
            (padded.parse().ok()?, &frac[digits..])
        }
        None => (0, rest),
    };
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = match zone.get(..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let n = |range: std::ops::Range<usize>| zone.get(range)?.parse::<i64>().ok();
            sign * (n(1..3)? * 3600 + n(4..6)? * 60)
        }
    };
    // Civil date to days since the epoch (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset, nanos))
}

/// Every host the sandbox was blocked from reaching, oldest first. Hosts the
/// sandbox reached after their last block are kept and marked as such: a host
/// can be reached one way and still be blocked another.
fn blocked_hosts(log: PolicyLog, sandbox: &str) -> Vec<Blocked> {
    let mine = |e: &Entry| e.vm_name.is_empty() || e.vm_name == sandbox;
    let mut out: Vec<Blocked> = Vec::new();
    for entry in log.blocked_hosts.iter().filter(|e| mine(e)) {
        match out.iter_mut().find(|b| b.host == entry.host) {
            Some(seen) => {
                seen.count += entry.count_since;
                if instant(&entry.last_seen) > instant(&seen.last_seen) {
                    seen.last_seen = entry.last_seen.clone();
                    seen.reason = entry.reason.clone();
                }
            }
            None => out.push(Blocked {
                host: entry.host.clone(),
                count: entry.count_since,
                last_seen: entry.last_seen.clone(),
                reason: entry.reason.clone(),
                reached_since: false,
            }),
        }
    }
    for blocked in &mut out {
        let last = instant(&blocked.last_seen);
        blocked.reached_since =
            log.allowed_hosts.iter().any(|a| mine(a) && a.host == blocked.host && instant(&a.last_seen) > last);
    }
    out.sort_by_key(|b| instant(&b.last_seen));
    out
}

fn read_blocked(sbx: &Sbx, sandbox: &str) -> Result<Vec<Blocked>> {
    let text = sbx
        .capture(args!["policy", "log", sandbox, "--json"])?
        .ok_or_else(|| anyhow!("`sbx policy log {sandbox} --json` failed"))?;
    Ok(blocked_hosts(parse_log(&text)?, sandbox))
}

/// Whether sbx logged `host` blocked for `sandbox`: as logged, or by name when
/// `host` has no port (a proxy message may leave it out).
pub fn was_blocked(sbx: &Sbx, sandbox: &str, host: &str) -> Result<bool> {
    Ok(read_blocked(sbx, sandbox)?.iter().any(|b| logged_as(&b.host, host)))
}

fn logged_as(logged: &str, host: &str) -> bool {
    let logged = logged.to_ascii_lowercase();
    logged == host || (!host.contains(':') && logged.split_once(':').is_some_and(|(name, _)| name == host))
}

/// Refuses output in a shape sand does not know rather than reading it as an
/// empty log, which would report that nothing is blocked.
fn parse_log(text: &str) -> Result<PolicyLog> {
    let context = "parsing the output of `sbx policy log --json`";
    let value: serde_json::Value = serde_json::from_str(text).context(context)?;
    if let Some(object) = value.as_object()
        && !object.is_empty()
        && !LOG_KEYS.iter().any(|k| object.contains_key(*k))
    {
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        bail!("`sbx policy log --json` printed fields sand does not know ({}); expected `blocked`", keys.join(", "));
    }
    serde_json::from_value(value).context(context)
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
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "sandbox '{sandbox}' has {} blocked host(s) but there is no terminal to choose on; \
             pass --host HOST[,HOST...] or see them with --list.",
            blocked.len()
        );
    }
    let labels: Vec<String> = blocked.iter().map(|b| b.describe()).collect();
    let picked = MultiSelect::new()
        .with_prompt(format!("Allow '{sandbox}' to reach (space selects, enter confirms)"))
        .items(&labels)
        .interact_on_opt(&Term::stderr())?;
    Ok(picked.unwrap_or_default().into_iter().map(|i| blocked[i].host.clone()).collect())
}

/// `sand allow`: show what the sandbox was blocked from reaching and add allow rules.
pub fn allow(args: AllowArgs, dirs: &Dirs, config: &Config) -> Result<i32> {
    let sbx = Sbx::connect(true)?;
    let (name, _) = session::find_sandbox(&sbx, &args.targets, args.name.as_deref(), args.scratch, dirs, config)?;

    let hosts = if args.host.is_empty() {
        let blocked = read_blocked(&sbx, &name)?;
        if blocked.is_empty() {
            log::ok(format!("nothing is blocked for sandbox '{name}'"));
            return Ok(0);
        }
        if args.list {
            for entry in &blocked {
                println!("{}", entry.describe());
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
        blocked_hosts(parse_log(json).unwrap(), "box")
    }

    #[test]
    fn blocked_hosts_are_filtered_merged_and_sorted_oldest_first() {
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
        assert_eq!(hosts, ["old.example.com:443", "gateway.internal", "fixed.example.com:443", "new.example.com:443"]);
        let new = &blocked[3];
        assert_eq!(new.count, 105);
        assert_eq!(new.describe(), "new.example.com:443  (105 requests, last 2026-10-12 16:07:31)");
        assert_eq!(
            blocked[1].describe(),
            "gateway.internal  (2 requests, last 2026-10-12 12:51:16)  [DNS lookup blocked by proxy policy]"
        );
        // Reached after it was blocked: still listed, and marked.
        assert!(blocked[2].reached_since && !blocked[0].reached_since);
        assert_eq!(
            blocked[2].describe(),
            "fixed.example.com:443  (3 requests, last 2026-10-12 13:00:00, reached since)"
        );
    }

    #[test]
    fn blocks_are_ordered_by_when_they_happened_not_by_their_text() {
        let blocked = parse(
            r#"{
              "blocked_hosts": [
                {"host": "a.dev:443", "vm_name": "box", "last_seen": "2026-10-12T13:00:00+02:00", "reason": "default deny"},
                {"host": "b.dev:443", "vm_name": "box", "last_seen": "2026-10-12T12:30:00Z", "reason": "default deny"},
                {"host": "c.dev:443", "vm_name": "box", "last_seen": "2026-10-12T12:30:00.25Z", "reason": "default deny"}
              ],
              "allowed_hosts": [
                {"host": "a.dev:443", "vm_name": "box", "last_seen": "2026-10-12T11:30:00Z"}
              ]
            }"#,
        );
        let hosts: Vec<&str> = blocked.iter().map(|b| b.host.as_str()).collect();
        assert_eq!(hosts, ["a.dev:443", "b.dev:443", "c.dev:443"]);
        // 11:30Z is after 13:00+02:00 (11:00Z), although it sorts before it as text.
        assert!(blocked[0].reached_since);
    }

    #[test]
    fn timestamps_are_read_as_instants() {
        assert_eq!(instant("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(instant("1970-01-01T02:00:00+02:00"), Some((0, 0)));
        assert_eq!(instant("2024-02-29T12:34:56.5-01:30"), Some((1_709_210_096 + 5400, 500_000_000)));
        assert_eq!(instant("2026-10-12T16:07:30.8474261234+02:00").map(|t| t.1), Some(847_426_123));
        assert_eq!(instant("yesterday"), None);
        assert_eq!(instant(""), None);
    }

    #[test]
    fn the_current_sbx_log_format_is_read() {
        let blocked = parse(
            r#"{
              "allowed": [
                {"sandbox_id": "box", "host": "api.anthropic.com:443", "proxy_type": "forward", "count": 99,
                 "first_seen": "2026-10-22T11:40:17.4639872+02:00", "last_seen": "2026-10-22T14:43:15.1938052+02:00"}
              ],
              "blocked": [
                {"sandbox_id": "box", "host": "http-intake.logs.us5.datadoghq.com:443", "proxy_type": "forward",
                 "rule": "no applicable policies for op(action=net:connect:tcp, resource=net:domain:http-intake.logs.us5.datadoghq.com:443)",
                 "reason": "No matching allow rule (default deny)", "count": 32,
                 "first_seen": "2026-10-22T11:40:39.8457741+02:00", "last_seen": "2026-10-22T14:43:00.1694221+02:00"},
                {"sandbox_id": "other", "host": "elsewhere.example.com:443", "reason": "default deny", "count": 1,
                 "last_seen": "2026-10-22T14:00:00+02:00"}
              ]
            }"#,
        );
        assert_eq!(blocked.len(), 1);
        assert_eq!(
            blocked[0].describe(),
            "http-intake.logs.us5.datadoghq.com:443  (32 requests, last 2026-10-22 14:43:00)"
        );
        assert!(!blocked[0].reached_since);
    }

    #[test]
    fn null_lists_are_empty_and_unknown_shapes_are_refused() {
        assert!(parse(r#"{"blocked": null, "allowed": null}"#).is_empty());
        assert!(parse_log("{}").is_ok());
        let err = parse_log(r#"{"denied": [{"host": "a.dev"}]}"#).unwrap_err().to_string();
        assert!(err.contains("fields sand does not know (denied)"), "{err}");
    }

    #[test]
    fn an_empty_log_has_nothing_blocked() {
        assert!(parse("{}").is_empty());
        assert!(parse(r#"{"blocked_hosts": [], "allowed_hosts": []}"#).is_empty());
    }

    #[test]
    fn a_host_is_matched_with_or_without_its_port() {
        assert!(logged_as("PyPI.org:443", "pypi.org"));
        assert!(logged_as("pypi.org:443", "pypi.org:443"));
        assert!(!logged_as("pypi.org:443", "pypi.org:80"));
        assert!(!logged_as("evil.pypi.org:443", "pypi.org"));
        assert!(!logged_as("pypi.org.evil:443", "pypi.org"));
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
