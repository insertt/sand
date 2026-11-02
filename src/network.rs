//! Network and mount prompts: the `sand` mod shipped into Claude Code
//! sandboxes asks in the agent's UI whether to allow a host the network policy
//! blocked, or to mount a host directory the agent asked for, and this side
//! answers. The mod only files a request; sand confirms each one
//! in a native dialog on the host, which the agent cannot click, before it
//! changes the policy with sbx.
//!
//! Requests and answers go through two directories per sandbox, mounted in
//! that sandbox alone, so the sandbox a request came from is the directory it
//! appeared in, never anything the request says. The agent can write only the
//! first, the root of its mount, where sand reads files and removes entries
//! without following links; what sand writes is in the second, read-only to it:
//!
//! ```text
//! ~/.sand-requests/<id>.json      the mod: {"host": ..., "approval": ..., "option": ...},
//!                                 or {"kind": "mount", "path": ..., "mode": ..., "reason": ...}
//! ~/.sand-policy/pending.json     sand: the sbx approvals waiting for this sandbox
//! ~/.sand-policy/results/<id>.json  sand: {"ok": ..., "allowed": ..., "message": ...}
//! ```

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::args;
use crate::mounts::{self, Mode, Mount, MountSet};
use crate::paths::{Dirs, POLICY_DIR_NAME, REQUESTS_DIR_NAME, container_path};
use crate::sbx::Sbx;
use crate::session;

/// Where the mod is copied, relative to the sandbox home.
pub const MOD_DIR: &str = ".local/share/sand/mods/sand";

/// The mod's files, relative to its folder.
const MOD_FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../assets/claude-mod/sand/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../assets/claude-mod/sand/hooks/hooks.json")),
    ("hooks/register.tsx", include_str!("../assets/claude-mod/sand/hooks/register.tsx")),
    ("types/index.d.ts", include_str!("../assets/claude-mod/sand/types/index.d.ts")),
];

/// Where the MCP server that gives an ACP agent `request_mount` is copied,
/// relative to the sandbox home: the mod gives it to Claude Code's own UI,
/// which a session sand draws itself does not run.
pub const MCP_SERVER: &str = ".local/share/sand/mcp/sand.mjs";

/// The mod's files, and the MCP server's, as the sandbox gets them: paths
/// relative to the sandbox home.
pub fn mod_files() -> Vec<(String, &'static str)> {
    let mut files: Vec<(String, &'static str)> =
        MOD_FILES.iter().map(|(rel, content)| (format!("{MOD_DIR}/{rel}"), *content)).collect();
    files.push((MCP_SERVER.to_string(), include_str!("../assets/mcp/sand.mjs")));
    files
}

/// The MCP server, as ACP's `session/new` takes it: run by the agent in the
/// sandbox, with the bridge directory it files requests in.
pub fn mcp_server(sandbox_home: &str) -> Value {
    json!({
        // Not `sand`: the mod's hooks claim `mcp__sand__request_mount`, should anything load it.
        "name": "sand-acp",
        "command": "node",
        "args": [format!("{sandbox_home}/{MCP_SERVER}")],
        "env": [
            {"name": "SAND_POLICY_DIR", "value": format!("{sandbox_home}/{POLICY_DIR_NAME}")},
            {"name": "SAND_REQUESTS_DIR", "value": format!("{sandbox_home}/{REQUESTS_DIR_NAME}")},
        ],
    })
}

/// Settings that make the extra mounts working directories of Claude Code, so
/// that `@` suggests their files; `None` without extra mounts. The list stands
/// in for any the host settings name: inside the sandbox only mounted host
/// directories exist.
pub fn working_directories(mounts: &MountSet) -> Option<Value> {
    let dirs: Vec<String> = mounts.extras.iter().map(|m| container_path(&m.path.to_string_lossy())).collect();
    (!dirs.is_empty()).then(|| json!({ "permissions": { "additionalDirectories": dirs } }))
}

/// Settings that load the mod and tell it where the bridge directory is.
/// Claude Code reads `CLAUDE_CODE_PLUGIN_DIRS` from the user settings' `env`.
pub fn settings(sandbox_home: &str) -> Value {
    json!({
        "env": {
            "CLAUDE_CODE_PLUGIN_DIRS": format!("{sandbox_home}/{MOD_DIR}"),
            "SAND_POLICY_DIR": format!("{sandbox_home}/{POLICY_DIR_NAME}"),
            "SAND_REQUESTS_DIR": format!("{sandbox_home}/{REQUESTS_DIR_NAME}"),
        },
    })
}

const POLL: Duration = Duration::from_millis(500);
const PENDING_EVERY: Duration = Duration::from_secs(3);
/// A request this young that does not parse may still be being written.
const WRITE_GRACE: Duration = Duration::from_secs(2);
/// Results the mod had time to read are removed after this long.
const RESULT_TTL: Duration = Duration::from_secs(600);
/// A host declined on the host is not asked about again for this long: long
/// enough to swallow a burst of retries, short enough that asking again on
/// purpose shows the dialog again.
const DECLINE_COOLDOWN: Duration = Duration::from_secs(10);
/// Requests answered per round, so that a flood cannot keep the loop from stopping.
const PER_ROUND: usize = 10;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

/// An approval the sbx daemon published for this sandbox.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Approval {
    pub id: String,
    pub host: String,
    pub options: Vec<Choice>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default)]
    host: String,
    #[serde(default)]
    approval: Option<String>,
    #[serde(default)]
    option: Option<String>,
    /// For a mount: the host directory as the agent wrote it, and the mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    /// For a mount: why the agent asks, shown in the dialog as its words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Outcome {
    pub ok: bool,
    pub allowed: bool,
    pub message: String,
}

impl Outcome {
    fn allowed(message: String) -> Self {
        Self { ok: true, allowed: true, message }
    }
    fn done(message: String) -> Self {
        Self { ok: true, allowed: false, message }
    }
    fn failed(message: String) -> Self {
        Self { ok: false, allowed: false, message }
    }
}

/// What the person answered in the host dialog.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Allow,
    Deny,
    /// No dialog could be shown, and why.
    Unavailable(String),
}

/// What the network policy blocked, as the proxy's 403 bodies say it.
#[derive(Debug, Default, PartialEq)]
pub struct Blocks {
    /// Hosts no rule allows, or held for an approval.
    pub hosts: Vec<String>,
    /// Hosts a local deny rule blocks (an earlier refusal).
    pub ruled: Vec<String>,
    /// The organization's policy blocked something; nothing to ask about.
    pub org: bool,
}

/// The blocks a tool's output reports, each host once, plain and lowercase.
pub fn blocks_in(text: &str) -> Blocks {
    fn hosts(text: &str, marker: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || ".-:".contains(c))).unwrap_or(rest.len());
            // A sentence ending right after the host ("...for example.com.") keeps its period.
            let host = rest[..end].trim_end_matches(['.', ':']).to_ascii_lowercase();
            if valid_host(&host) && !found.contains(&host) {
                found.push(host);
            }
        }
        found
    }
    let ruled = hosts(text, "Blocked by local rule for ");
    let mut open = Vec::new();
    for host in
        hosts(text, "Approval required for ").into_iter().chain(hosts(text, "Blocked by network policy: domain "))
    {
        if !ruled.contains(&host) && !open.contains(&host) {
            open.push(host);
        }
    }
    Blocks { hosts: open, ruled, org: text.contains("Blocked by org policy") }
}

/// Whether `host` is one plain host, as the policy log writes them: a name
/// with an optional port. No wildcards and no lists, so a request cannot widen
/// what it asks for past what the dialog shows.
pub fn valid_host(host: &str) -> bool {
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.parse::<u16>().is_ok_and(|n| n > 0));
    !name.is_empty() && name.len() <= 253 && name.split('.').all(label_ok) && port_ok
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn is_deny(choice: &Choice) -> bool {
    let text = format!("{} {}", choice.id, choice.label).to_ascii_lowercase();
    ["deny", "reject", "block", "decline"].iter().any(|word| text.contains(word))
}

/// The rule id in what `sbx policy check network` prints for a host a local
/// deny rule blocks: `Reason: denied by local rule rule "local:<id>"`.
fn parse_deny_rule(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.to_ascii_lowercase().contains("denied by local rule"))?;
    let id: String =
        line.split("local:").nth(1)?.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    valid_id(&id).then_some(id)
}

/// Reads the approvals for `sandbox` from `sbx policy approval ls --json`. The
/// schema is not documented, so this takes any list of objects with an id and
/// a host, and keeps only those that name this sandbox: one that names none
/// cannot be told apart from another sandbox's.
pub fn parse_approvals(json: &str, sandbox: &str) -> Option<Vec<Approval>> {
    fn text<'a>(map: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
        keys.iter().find_map(|k| map.get(*k).and_then(Value::as_str)).filter(|s| !s.is_empty())
    }
    fn collect<'a>(value: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            Value::Object(map) if map.contains_key("id") || map.contains_key("approval_id") => out.push(map),
            Value::Object(map) => map.values().for_each(|v| collect(v, out)),
            _ => {}
        }
    }
    let parsed: Value = serde_json::from_str(json).ok()?;
    let mut entries = Vec::new();
    collect(&parsed, &mut entries);
    let approvals = entries
        .into_iter()
        .filter(|map| text(map, &["sandbox", "sandbox_name", "sandbox_id", "sandboxName", "vm_name"]) == Some(sandbox))
        .filter_map(|map| {
            let id = text(map, &["id", "approval_id"])?.to_string();
            let host = text(map, &["host", "domain", "resource", "target"])?.to_string();
            let options = map
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|o| match o {
                            Value::String(id) => Some(Choice { id: id.clone(), label: id.clone() }),
                            Value::Object(o) => {
                                let id = text(o, &["id", "option_id", "name"])?.to_string();
                                let label = text(o, &["label", "description", "name"]).unwrap_or(&id).to_string();
                                Some(Choice { id, label })
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Approval { id, host, options })
        })
        .collect();
    Some(approvals)
}

/// Writes `value` as JSON through a temporary file, so the mod never reads half of it.
fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(&tmp, path)
}

/// Asks the person on the host. `SAND_CONFIRM` replaces the native dialog: a
/// command run with the question as its last argument, exit status 0 allowing.
pub fn confirm(question: &str) -> Answer {
    if let Some(cmd) = std::env::var_os("SAND_CONFIRM").filter(|c| !c.is_empty()) {
        return run_dialog(Command::new(cmd).arg(question));
    }
    native_dialog(question)
}

fn run_dialog(cmd: &mut Command) -> Answer {
    match cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output() {
        // osascript answers "button returned:Allow", or "gave up:true" on its timeout.
        Ok(out) if out.status.success() => {
            let said = String::from_utf8_lossy(&out.stdout);
            if said.contains("gave up:true") { Answer::Deny } else { Answer::Allow }
        }
        Ok(_) => Answer::Deny,
        Err(err) => Answer::Unavailable(format!("{}: {err}", cmd.get_program().to_string_lossy())),
    }
}

#[cfg(target_os = "macos")]
fn native_dialog(question: &str) -> Answer {
    // The question goes in as an argument, never into the script's text.
    run_dialog(Command::new("osascript").args([
        "-e",
        "on run argv",
        "-e",
        "display dialog (item 1 of argv) with title \"sand\" buttons {\"Deny\", \"Allow\"} \
         default button \"Deny\" cancel button \"Deny\" with icon caution giving up after 300",
        "-e",
        "end run",
        question,
    ]))
}

#[cfg(windows)]
fn native_dialog(question: &str) -> Answer {
    // Read from the environment so that nothing in the question is parsed as PowerShell.
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
        $r = [System.Windows.Forms.MessageBox]::Show($env:SAND_QUESTION, 'sand', 'YesNo', 'Warning', 'Button2'); \
        if ($r -eq 'Yes') { exit 0 } else { exit 1 }";
    run_dialog(
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("SAND_QUESTION", question),
    )
}

#[cfg(not(any(target_os = "macos", windows)))]
fn native_dialog(question: &str) -> Answer {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Answer::Unavailable("there is no graphical session on the host".into());
    }
    let zenity = Command::new("zenity")
        .args(["--question", "--title=sand", "--no-markup", "--default-cancel", "--timeout=300"])
        .args(["--ok-label=Allow", "--cancel-label=Deny"])
        .arg(format!("--text={question}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match zenity {
        Ok(status) => return if status.success() { Answer::Allow } else { Answer::Deny },
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Answer::Unavailable(format!("zenity: {err}"));
        }
        Err(_) => {}
    }
    match run_dialog(
        Command::new("kdialog")
            .args(["--title", "sand", "--yes-label", "Allow", "--no-label", "Deny", "--warningyesno"])
            .arg(question),
    ) {
        Answer::Unavailable(_) => Answer::Unavailable("neither zenity nor kdialog is installed on the host".into()),
        answer => answer,
    }
}

/// What a session's bridge did, for the summary when the session ends.
#[derive(Default)]
pub struct Summary {
    pub allowed: Vec<String>,
    pub mounted: Vec<String>,
}

/// Mounts a directory into the running sandbox; answers whether anything changed.
type Mounter = Box<dyn Fn(&Mount) -> anyhow::Result<bool> + Send>;

/// Asks the person on the host whether to allow something.
pub type Confirm = Box<dyn Fn(&str) -> Answer + Send>;

/// Told the approvals sbx has pending for the sandbox, whenever they change.
pub type Watch = Box<dyn Fn(&[Approval]) + Send>;

/// What an outcome answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    /// Network access to a host (empty when the approval was gone).
    Host(String),
    /// A host directory the agent asked to mount, as it wrote it.
    Mount(String),
}

/// Told how an answer given in the session's own UI, or a mount the agent
/// asked for, turned out.
pub type Told = Box<dyn Fn(&Asked, &Outcome) + Send>;

/// What the person chose in a UI the agent cannot reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answered {
    /// An option of one of sbx's approvals (approval id, option id).
    Approval(String, String),
    /// A host the policy blocked without an approval, allowed for this sandbox.
    Allow(String),
    /// A host directory to mount (`DIR`, `DIR:ro` or `DIR:rw`), asked for by the person.
    Mount(String),
}

/// How a session that draws its own UI takes part: it asks the questions in
/// place of the host dialog, sees the approvals sbx has pending for the
/// sandbox, and answers them and the hosts it saw blocked, already confirmed
/// by the person in that UI, hearing back how each answer went.
pub struct Hooks {
    pub confirm: Confirm,
    pub pending: Watch,
    pub answers: Receiver<Answered>,
    pub told: Told,
}

/// Answers the requests of one sandbox while its session runs.
pub struct Bridge {
    stop: Arc<AtomicBool>,
    summary: Arc<Mutex<Summary>>,
}

impl Bridge {
    pub fn start(sbx: Sbx, dirs: Dirs, sandbox: String) -> Self {
        Self::start_with(sbx, dirs, sandbox, None)
    }

    /// Like `start`, asking and showing through `hooks` when given.
    pub fn start_with(sbx: Sbx, dirs: Dirs, sandbox: String, hooks: Option<Hooks>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let summary = Arc::new(Mutex::new(Summary::default()));
        let mounter: Mounter = {
            let (sbx, dirs, sandbox) = (sbx.clone(), dirs.clone(), sandbox.clone());
            Box::new(move |mount| session::mount_in_session(&sbx, &dirs, &sandbox, mount))
        };
        let mut worker = Worker {
            sbx,
            dir: dirs.policy_dir(&sandbox),
            inbox: dirs.requests_dir(&sandbox),
            log: dirs.sandbox_dir(&sandbox).join("network-prompts.log"),
            sandbox,
            dirs,
            mounter,
            pending: Vec::new(),
            pending_at: None,
            declined: HashMap::new(),
            summary: summary.clone(),
            confirm: Box::new(confirm),
            watch: None,
            answers: None,
            told: None,
        };
        if let Some(hooks) = hooks {
            worker.confirm = hooks.confirm;
            worker.watch = Some(hooks.pending);
            worker.answers = Some(hooks.answers);
            worker.told = Some(hooks.told);
        }
        let flag = stop.clone();
        // Not joined: it may be waiting on a dialog nobody answers.
        thread::spawn(move || {
            worker.reset();
            while !flag.load(Ordering::Relaxed) {
                worker.round(&flag);
                thread::sleep(POLL);
            }
        });
        Self { stop, summary }
    }

    /// Stops answering and says what was allowed.
    pub fn finish(self) -> Summary {
        self.stop.store(true, Ordering::Relaxed);
        std::mem::take(&mut *self.summary.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

struct Worker {
    sbx: Sbx,
    dirs: Dirs,
    sandbox: String,
    /// What sand writes back; read-only in the sandbox.
    dir: PathBuf,
    /// Where the sandbox files requests; anything in it is the agent's.
    inbox: PathBuf,
    log: PathBuf,
    pending: Vec<Approval>,
    pending_at: Option<Instant>,
    declined: HashMap<String, Instant>,
    summary: Arc<Mutex<Summary>>,
    confirm: Confirm,
    mounter: Mounter,
    /// Told whenever the pending approvals change.
    watch: Option<Watch>,
    /// Approvals and hosts answered in the session's own UI, and where their outcomes go.
    answers: Option<Receiver<Answered>>,
    told: Option<Told>,
}

impl Worker {
    /// Starts from a clean directory: requests filed while no session ran are
    /// answered now, but a pending list from an earlier session would be stale.
    fn reset(&self) {
        let _ = fs::create_dir_all(&self.inbox);
        let _ = fs::create_dir_all(self.dir.join("results"));
        let _ = fs::remove_file(self.dir.join("pending.json"));
    }

    fn round(&mut self, stop: &AtomicBool) {
        if self.pending_at.is_none_or(|at| at.elapsed() >= PENDING_EVERY) {
            self.refresh_pending();
        }
        let answered: Vec<Answered> = self.answers.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for answer in answered {
            let (asked, outcome, request) = match answer {
                Answered::Approval(id, option) => {
                    let host = self.pending.iter().find(|a| a.id == id).map(|a| a.host.clone()).unwrap_or_default();
                    let outcome = self.answer_approval(&id, Some(&option), true);
                    let request = Request {
                        kind: Some("approval".into()),
                        host: host.clone(),
                        approval: Some(id),
                        option: Some(option),
                        path: None,
                        mode: None,
                        reason: None,
                    };
                    (Asked::Host(host), outcome, request)
                }
                Answered::Allow(host) => {
                    let outcome = self.answer_allow(&host, true);
                    let request = Request {
                        kind: None,
                        host: host.clone(),
                        approval: None,
                        option: None,
                        path: None,
                        mode: None,
                        reason: None,
                    };
                    (Asked::Host(host), outcome, request)
                }
                Answered::Mount(path) => {
                    let outcome = self.answer_mount(&path, None, None, true);
                    let request = Request {
                        kind: Some("mount".into()),
                        host: String::new(),
                        approval: None,
                        option: None,
                        path: Some(path.clone()),
                        mode: None,
                        reason: None,
                    };
                    (Asked::Mount(path), outcome, request)
                }
            };
            self.record(&request, &outcome);
            if let Some(told) = &self.told {
                told(&asked, &outcome);
            }
        }
        for (id, path) in self.requests().into_iter().take(PER_ROUND) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(outcome) = self.take(&path) else { continue };
            let _ = write_json(&self.dir.join("results").join(format!("{id}.json")), &outcome);
        }
        self.expire_results();
    }

    fn refresh_pending(&mut self) {
        self.pending_at = Some(Instant::now());
        let listed = self.sbx.capture(args!["policy", "approval", "ls", "--json"]).ok().flatten();
        // An sbx without approvals leaves the list out: the mod then offers allow rules alone.
        if let Some(approvals) = listed.and_then(|json| parse_approvals(&json, &self.sandbox)) {
            if let Some(watch) = self.watch.as_ref().filter(|_| approvals != self.pending) {
                watch(&approvals);
            }
            self.pending = approvals;
            let _ = write_json(&self.dir.join("pending.json"), &json!({ "approvals": self.pending }));
        }
    }

    /// Request files by age, oldest first; anything else in the directory is
    /// removed. Nothing here follows a link: the agent makes the entries.
    fn requests(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = fs::read_dir(&self.inbox) else { return Vec::new() };
        let mut found: Vec<(SystemTime, String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // `DirEntry::metadata` does not follow links, unlike `Path::metadata`.
            let meta = entry.metadata().ok();
            match name.strip_suffix(".json").filter(|id| valid_id(id)) {
                Some(id) if meta.as_ref().is_some_and(fs::Metadata::is_file) => {
                    let modified = meta.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
                    found.push((modified, id.to_string(), path));
                }
                _ => {
                    let _ = crate::state::remove_any(&path);
                }
            }
        }
        found.sort();
        found.into_iter().map(|(_, id, path)| (id, path)).collect()
    }

    /// Reads, answers and removes one request; `None` while it may still be being written.
    fn take(&mut self, path: &Path) -> Option<Outcome> {
        let text = read_plain_file(path).unwrap_or_default();
        let request = match serde_json::from_str::<Request>(&text) {
            Ok(request) => request,
            Err(_) => {
                let age = fs::symlink_metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                if age.is_some_and(|age| age < WRITE_GRACE) {
                    return None;
                }
                let _ = fs::remove_file(path);
                return Some(Outcome::failed("sand could not read the request".into()));
            }
        };
        let _ = fs::remove_file(path);
        let outcome = self.answer(&request);
        self.record(&request, &outcome);
        // No mod reads the result back in a session sand draws: it tells the agent.
        if let Some(told) = self.told.as_ref().filter(|_| request.kind.as_deref() == Some("mount")) {
            told(&Asked::Mount(request.path.clone().unwrap_or_default()), &outcome);
        }
        Some(outcome)
    }

    fn answer(&mut self, request: &Request) -> Outcome {
        if request.kind.as_deref() == Some("mount") {
            let path = request.path.as_deref().unwrap_or_default();
            return self.answer_mount(path, request.mode.as_deref(), request.reason.as_deref(), false);
        }
        match &request.approval {
            Some(approval) => self.answer_approval(approval, request.option.as_deref(), false),
            None => self.answer_allow(&request.host, false),
        }
    }

    /// Mounts `path`; `confirmed` when the person asked for it in a UI the
    /// agent cannot reach (`/mount`), which needs no second question.
    fn answer_mount(&mut self, path: &str, mode: Option<&str>, reason: Option<&str>, confirmed: bool) -> Outcome {
        // `DIR:ro` / `DIR:rw`, as on sand's command line, wins over `mode`.
        let path = path.trim();
        let (raw, suffix) = mounts::split_mode(path);
        let mode = match (raw.len() < path.len(), mode) {
            (true, _) => suffix,
            (false, Some("ro")) => Mode::Ro,
            (false, Some("rw") | None) => Mode::Rw,
            (false, Some(other)) => return Outcome::failed(format!("'{other}' is not a mount mode (ro or rw)")),
        };
        let path = raw;
        if path.trim().is_empty() || path.chars().any(char::is_control) {
            return Outcome::failed("that is not a directory path".into());
        }
        // Relative paths are taken from the primary workspace, as `sand mount` takes them from where it runs.
        let state = crate::state::load(&self.dirs, &self.sandbox).ok().flatten();
        let cwd = state.as_ref().and_then(|s| s.mounts.primary.as_ref()).map(|m| m.path.clone());
        let cwd = cwd.unwrap_or_else(|| self.dirs.home.clone());
        let mount = match Mount::parse(&format!("{path}:{mode}"), &cwd, &self.dirs.home) {
            Ok(mount) if mount.path.is_dir() => mount,
            Ok(mount) => return Outcome::failed(format!("{} is not a directory", mount.path.display())),
            Err(err) => return Outcome::failed(format!("{err:#}")),
        };
        let shown = mount.path.display().to_string();
        if mounts::exposes(&mount.path, &self.dirs.sandboxes_dir()) {
            return Outcome::failed(format!(
                "{shown} holds sand's data for every sandbox; it is never mounted on request"
            ));
        }
        // Below a read-write mount the agent can swap the directory for a link
        // while the dialog is open, and have another one mounted than was shown.
        // It sees what is there already; only the person can ask for it (`/mount`).
        let writable = state.iter().flat_map(|s| {
            let user = s.mounts.all().filter(|m| m.mode == Mode::Rw).map(|m| m.path.clone());
            let own = s.internal_mounts.iter().filter(|m| m.mode == Mode::Rw).map(|m| PathBuf::from(&m.host));
            user.chain(own).collect::<Vec<_>>()
        });
        if !confirmed && let Some(within) = writable.into_iter().find(|w| mount.path.starts_with(w) && mount.path != *w)
        {
            return Outcome::failed(format!(
                "{shown} is inside {}, which the sandbox can already change; sand does not mount it on the agent's request",
                within.display()
            ));
        }
        let key = format!("mount:{shown}");
        if !confirmed && self.declined.get(&key).is_some_and(|at| at.elapsed() < DECLINE_COOLDOWN) {
            return Outcome::done(format!("mounting {shown}: declined on the host a moment ago"));
        }
        let access = match mode {
            Mode::Ro => "read-only".to_string(),
            Mode::Rw => "READ-WRITE: the agent can change and delete files in it".to_string(),
        };
        let mut question =
            format!("Sandbox '{}' asks to mount a directory of this computer:\n\n{shown}\n\n{access}", self.sandbox);
        if let Some(reason) = reason.map(one_line).filter(|r| !r.is_empty()) {
            question.push_str(&format!("\n\nThe agent says: \u{201c}{reason}\u{201d}"));
        }
        question.push_str("\n\nMount it?");
        match if confirmed { Answer::Allow } else { (self.confirm)(&question) } {
            Answer::Allow => {}
            Answer::Deny => {
                self.declined.insert(key, Instant::now());
                return Outcome::done(format!("mounting {shown}: declined on the host"));
            }
            Answer::Unavailable(why) => {
                return Outcome::failed(format!(
                    "sand could not ask on the host ({why}); mount it there with `sand mount {shown}`"
                ));
            }
        }
        // Still the directory the question named, not a link put there since.
        if dunce::canonicalize(&mount.path).ok().as_deref() != Some(mount.path.as_path()) {
            return Outcome::failed(format!("{shown} changed while sand was asking; it was not mounted"));
        }
        let target = container_path(&shown);
        match (self.mounter)(&mount) {
            Ok(true) => {
                self.summary.lock().unwrap_or_else(PoisonError::into_inner).mounted.push(mount.to_string());
                Outcome::allowed(format!("mounted {shown} ({}) at {target}", mount.mode))
            }
            Ok(false) => Outcome::done(format!("{shown} is already mounted ({}) at {target}", mount.mode)),
            Err(err) => Outcome::failed(format!("mounting {shown} failed: {err:#}")),
        }
    }

    /// Answers approval `id` with `option`; `confirmed` when the person chose
    /// it in a UI the agent cannot reach, which needs no second question.
    fn answer_approval(&mut self, id: &str, option: Option<&str>, confirmed: bool) -> Outcome {
        self.refresh_pending();
        // The host and options come from sbx, not from the request.
        let Some(approval) = self.pending.iter().find(|a| a.id == id).cloned() else {
            return Outcome::failed("that approval is no longer pending".into());
        };
        let Some(option) = option else { return Outcome::failed("no option was chosen".into()) };
        let choice = match approval.options.iter().find(|c| c.id == option) {
            Some(choice) => choice.clone(),
            None if approval.options.is_empty() => Choice { id: option.into(), label: option.into() },
            None => return Outcome::failed(format!("'{option}' is not an option of that approval")),
        };
        let host = &approval.host;
        // Refusing needs no confirmation: it can only take access away.
        if !is_deny(&choice) && !confirmed {
            let question = format!(
                "Sandbox '{}' asks for network access to {host}.\n\nAnswer: {}\n\nAllow it?",
                self.sandbox, choice.label
            );
            match (self.confirm)(&question) {
                Answer::Allow => {}
                Answer::Deny => return Outcome::done(format!("{host}: declined on the host")),
                Answer::Unavailable(why) => return unavailable(&why, host),
            }
        }
        let respond = args!["policy", "approval", "respond", &approval.id, "--option", &choice.id];
        let responded = self.sbx_quiet(respond);
        // Answered: it is no longer pending, wherever it shows.
        self.pending.retain(|a| a.id != approval.id);
        if let Some(watch) = &self.watch {
            watch(&self.pending);
        }
        match responded {
            Ok(()) if is_deny(&choice) => Outcome::done(format!("{host}: {}", choice.label)),
            Ok(()) => {
                self.allowed(host);
                Outcome::allowed(format!("{host}: {}", choice.label))
            }
            Err(err) => Outcome::failed(format!("{host}: sbx refused: {err}")),
        }
    }

    /// Allows `host` for this sandbox; `confirmed` when the person chose it in
    /// a UI the agent cannot reach, which needs no second question.
    fn answer_allow(&mut self, host: &str, confirmed: bool) -> Outcome {
        if !valid_host(host) {
            return Outcome::failed(format!("'{host}' is not a single host sand can allow"));
        }
        if !confirmed && self.declined.get(host).is_some_and(|at| at.elapsed() < DECLINE_COOLDOWN) {
            return Outcome::done(format!("{host}: declined on the host a moment ago"));
        }
        // A deny rule (an earlier refusal) wins over any allow, and sbx refuses an
        // allow beside a deny of the same scope, so allowing has to remove it.
        let deny = self.deny_rule(host);
        let question = match &deny {
            None => {
                format!("Sandbox '{}' asks for network access to {host}.\n\nAllow it for this sandbox?", self.sandbox)
            }
            Some(_) => format!(
                "Sandbox '{}' asks for network access to {host}, which a deny rule blocks (it was refused before).\n\n\
                 Remove that rule and allow it for this sandbox?",
                self.sandbox
            ),
        };
        match if confirmed { Answer::Allow } else { (self.confirm)(&question) } {
            Answer::Allow => {}
            Answer::Deny => {
                self.declined.insert(host.to_string(), Instant::now());
                return Outcome::done(format!("{host}: declined on the host"));
            }
            Answer::Unavailable(why) => return unavailable(&why, host),
        }
        if let Some(id) = &deny {
            let remove = args!["policy", "rm", "network", "--sandbox", &self.sandbox, "--id", id, "--force"];
            if let Err(err) = self.sbx_quiet(remove) {
                // Not this sandbox's own rule: it is global (or the organization's), not sand's to lift.
                return Outcome::failed(format!(
                    "{host} is blocked by deny rule {id}, which is not this sandbox's own ({err}); \
                     if it applies to every sandbox, remove it on the host with `sbx policy rm network --id {id}`"
                ));
            }
        }
        match self.sbx_quiet(args!["policy", "allow", "network", "--sandbox", &self.sandbox, host]) {
            Ok(()) => {
                self.allowed(host);
                Outcome::allowed(format!("{host}: allowed for this sandbox"))
            }
            Err(err) => Outcome::failed(format!("{host}: sbx refused: {err}")),
        }
    }

    /// The id of the local deny rule that blocks `host` for this sandbox, from
    /// `sbx policy check network`; `None` when nothing denies it explicitly.
    fn deny_rule(&self, host: &str) -> Option<String> {
        // The check needs a port; the proxy reports the ones it blocked with theirs.
        let target = if host.contains(':') { host.to_string() } else { format!("{host}:443") };
        let out = self.sbx.output(args!["policy", "check", "network", "--sandbox", &self.sandbox, target]).ok()?;
        parse_deny_rule(&format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    /// Runs sbx without a word on the terminal, which the agent owns.
    fn sbx_quiet(&self, args: Vec<OsString>) -> Result<(), String> {
        self.sbx.run_quiet(args).map_err(|err| format!("{err:#}"))
    }

    fn allowed(&self, host: &str) {
        self.summary.lock().unwrap_or_else(PoisonError::into_inner).allowed.push(host.to_string());
    }

    /// Appends the request and its outcome to the sandbox's log on the host.
    fn record(&self, request: &Request, outcome: &Outcome) {
        let line = json!({ "at": crate::state::now(), "request": request, "outcome": outcome });
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = writeln!(file, "{line}");
        }
    }

    fn expire_results(&self) {
        let Ok(entries) = fs::read_dir(self.dir.join("results")) else { return };
        for entry in entries.flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if old.is_some_and(|age| age > RESULT_TTL) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// The text of `path` when it is a regular file, never what a link points to:
/// the agent could swap the file for a link after it was listed.
fn read_plain_file(path: &Path) -> Option<String> {
    let before = fs::symlink_metadata(path).ok().filter(fs::Metadata::is_file)?;
    let mut file = fs::File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino()) != (opened.dev(), opened.ino()) {
            return None;
        }
    }
    #[cfg(not(unix))]
    let _ = (&before, &opened);
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).ok()?;
    Some(text)
}

/// Text from the agent made fit for one line of a dialog.
fn one_line(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    match words.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &words[..at]),
        None => words,
    }
}

fn unavailable(why: &str, host: &str) -> Outcome {
    Outcome::failed(format!(
        "sand could not ask on the host ({why}); allow {host} there with `sand allow` or `sbx policy`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_single_plain_hosts_are_valid() {
        for good in ["example.com", "example.com:443", "api.anthropic.com", "localhost:8080", "a-b.c1.io"] {
            assert!(valid_host(good), "{good}");
        }
        for bad in [
            "",
            "*",
            "**",
            "*.example.com",
            "a.com,b.com",
            "-a.com",
            "a-.com",
            "a..com",
            "a.com:",
            "a.com:0",
            "a.com:99999",
            "a com",
            "a.com/x",
            "--sandbox",
            "a.com:443:1",
        ] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn request_ids_are_plain_names() {
        assert!(valid_id("4f9c2a1e-0b7d-4c3e-9a51-2b6d8e0f1c3a"));
        for bad in ["", "../x", "a.b", "a/b", &"x".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn approvals_of_other_sandboxes_and_of_none_are_left_out() {
        let json = r#"{"approvals": [
            {"id": "a1", "host": "pypi.org:443", "sandbox": "claude-proj",
             "options": [{"id": "once", "label": "Allow once"}, {"id": "deny", "label": "Deny"}]},
            {"id": "a2", "domain": "evil.com", "sandbox_name": "codex-other", "options": ["allow"]},
            {"id": "a3", "host": "nobody.com"},
            {"approval_id": "a4", "resource": "zed.dev", "vm_name": "claude-proj", "options": ["allow", "deny"]}
        ]}"#;
        let approvals = parse_approvals(json, "claude-proj").unwrap();
        assert_eq!(
            approvals,
            vec![
                Approval {
                    id: "a1".into(),
                    host: "pypi.org:443".into(),
                    options: vec![
                        Choice { id: "once".into(), label: "Allow once".into() },
                        Choice { id: "deny".into(), label: "Deny".into() },
                    ],
                },
                Approval {
                    id: "a4".into(),
                    host: "zed.dev".into(),
                    options: vec![
                        Choice { id: "allow".into(), label: "allow".into() },
                        Choice { id: "deny".into(), label: "deny".into() },
                    ],
                },
            ]
        );
        assert_eq!(parse_approvals("[]", "x"), Some(vec![]));
        assert_eq!(parse_approvals("not json", "x"), None);
    }

    #[test]
    fn the_blocking_deny_rule_is_read_from_policy_check() {
        let denied = "Denied: example.com:443\nGovernance: Local policy only\nContext: sandbox:gov-lab-a\n\
                      Reason: denied by local rule rule \"local:66d96162-1162-4b19-b68d-226c4dc1fc29\"\n";
        assert_eq!(parse_deny_rule(denied).as_deref(), Some("66d96162-1162-4b19-b68d-226c4dc1fc29"));
        assert_eq!(parse_deny_rule("Allowed: example.com:443\n"), None);
        assert_eq!(
            parse_deny_rule("Denied: x\nReason: no matching allow rule - blocked by default deny policy\n"),
            None
        );
        assert_eq!(parse_deny_rule("Reason: denied by local rule rule \"local:$(id)\""), None);
    }

    #[test]
    fn deny_options_are_recognized() {
        let c = |id: &str, label: &str| Choice { id: id.into(), label: label.into() };
        assert!(is_deny(&c("deny", "Deny")) && is_deny(&c("x", "Reject request")) && is_deny(&c("block", "")));
        assert!(!is_deny(&c("once", "Allow once")) && !is_deny(&c("always", "Always allow")));
    }

    fn worker(dir: &Path, answer: Answer) -> (Worker, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let answer = Mutex::new(Some(answer));
        // Canonical, as `Dirs::discover` makes it, so that resolved mount paths compare equal.
        let root = dunce::canonicalize(dir).unwrap();
        let dirs = Dirs { home: root.join("home"), data: root.join("data"), config_file: root.join("config.toml") };
        let worker = Worker {
            sbx: Sbx::with_bin("/nonexistent/sbx"),
            dirs,
            mounter: Box::new(|mount| Ok(!mount.path.ends_with("already"))),
            sandbox: "claude-proj".into(),
            dir: dir.join("policy"),
            inbox: dir.join("requests"),
            log: dir.join("log"),
            pending: Vec::new(),
            pending_at: None,
            declined: HashMap::new(),
            summary: Arc::default(),
            confirm: Box::new(move |q| {
                seen.lock().unwrap().push(q.to_string());
                answer.lock().unwrap().take().unwrap_or(Answer::Deny)
            }),
            watch: None,
            answers: None,
            told: None,
        };
        worker.reset();
        (worker, asked)
    }

    #[test]
    fn approvals_answered_in_the_native_ui_are_not_asked_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Allow);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watched = seen.clone();
        worker.watch = Some(Box::new(move |pending: &[Approval]| {
            watched.lock().unwrap().push(pending.iter().map(|a| a.id.clone()).collect::<Vec<_>>());
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        worker.answers = Some(rx);
        let choice = |id: &str, label: &str| Choice { id: id.into(), label: label.into() };
        worker.pending = vec![
            Approval { id: "a1".into(), host: "api.github.com".into(), options: vec![choice("allow", "Allow")] },
            Approval { id: "a2".into(), host: "pypi.org".into(), options: vec![choice("allow", "Allow")] },
        ];
        worker.pending_at = Some(Instant::now());
        let told = Arc::new(Mutex::new(Vec::new()));
        let heard = told.clone();
        worker.told = Some(Box::new(move |asked: &Asked, outcome: &Outcome| {
            heard.lock().unwrap().push((asked.clone(), outcome.ok));
        }));
        tx.send(Answered::Approval("a1".into(), "allow".into())).unwrap();
        tx.send(Answered::Allow("example.com".into())).unwrap();
        worker.round(&AtomicBool::new(false));
        assert!(asked.lock().unwrap().is_empty(), "the person chose in the pane; no host dialog");
        assert_eq!(seen.lock().unwrap().last().unwrap(), &vec!["a2".to_string()], "a1 is no longer pending");
        let log = fs::read_to_string(tmp.path().join("log")).unwrap();
        assert!(log.contains("\"approval\":\"a1\""), "{log}");
        assert!(log.contains("\"host\":\"example.com\""), "{log}");
        // sbx is not there to answer: both fail, and the pane hears so for each host.
        assert_eq!(
            *told.lock().unwrap(),
            [(Asked::Host("api.github.com".into()), false), (Asked::Host("example.com".into()), false)]
        );
    }

    #[test]
    fn the_mcp_server_files_mount_requests_for_the_bridge() {
        use std::io::{BufRead, BufReader};
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("requests")).unwrap();
        let script = tmp.path().join("sand.mjs");
        fs::write(&script, mod_files().iter().find(|(rel, _)| rel == MCP_SERVER).unwrap().1).unwrap();
        let Ok(mut node) = Command::new("node")
            .arg(&script)
            .env("SAND_REQUESTS_DIR", tmp.path().join("requests"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("node is not installed; skipped");
            return;
        };
        let mut stdin = node.stdin.take().unwrap();
        for message in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "request_mount", "arguments": {"path": "~/notes:ro", "reason": "the docs"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "request_mount", "arguments": {}}}),
        ] {
            writeln!(stdin, "{message}").unwrap();
        }
        drop(stdin);
        let answers: Vec<Value> = BufReader::new(node.stdout.take().unwrap())
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect();
        node.wait().unwrap();
        assert_eq!(answers.len(), 4, "the notification gets no answer");
        assert_eq!(answers[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(answers[1]["result"]["tools"][0]["name"], "request_mount");
        assert!(
            answers[2]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Asked the user to mount ~/notes (ro)")
        );
        assert_eq!(answers[3]["result"]["isError"], true);
        let filed: Vec<_> = fs::read_dir(tmp.path().join("requests")).unwrap().flatten().collect();
        assert_eq!(filed.len(), 1);
        let request: Request = serde_json::from_str(&fs::read_to_string(filed[0].path()).unwrap()).unwrap();
        assert_eq!(
            (request.kind.as_deref(), request.path.as_deref(), request.mode.as_deref(), request.reason.as_deref()),
            (Some("mount"), Some("~/notes"), Some("ro"), Some("the docs"))
        );
        assert!(valid_id(filed[0].path().file_stem().unwrap().to_str().unwrap()), "the bridge takes its name");
    }

    #[test]
    fn mount_requests_are_told_to_a_session_that_draws_its_own_ui() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, _) = worker(tmp.path(), Answer::Deny);
        let told = Arc::new(Mutex::new(Vec::new()));
        let heard = told.clone();
        worker.told = Some(Box::new(move |asked: &Asked, outcome: &Outcome| {
            heard.lock().unwrap().push((asked.clone(), outcome.allowed));
        }));
        let request = json!({"kind": "mount", "path": tmp.path().to_string_lossy(), "mode": "ro"});
        fs::write(tmp.path().join("requests/r1.json"), request.to_string()).unwrap();
        worker.round(&AtomicBool::new(false));
        assert_eq!(*told.lock().unwrap(), [(Asked::Mount(tmp.path().to_string_lossy().into_owned()), false)]);
        assert!(tmp.path().join("policy/results/r1.json").exists(), "the result is written as for the mod");
    }

    #[test]
    fn a_mount_the_person_asked_for_needs_no_second_question() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Deny);
        let told = Arc::new(Mutex::new(Vec::new()));
        let heard = told.clone();
        worker.told = Some(Box::new(move |asked: &Asked, outcome: &Outcome| {
            heard.lock().unwrap().push((asked.clone(), outcome.allowed));
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        worker.answers = Some(rx);
        fs::create_dir_all(tmp.path().join("notes")).unwrap();
        let dir = format!("{}:ro", tmp.path().join("notes").display());
        tx.send(Answered::Mount(dir.clone())).unwrap();
        worker.round(&AtomicBool::new(false));
        assert!(asked.lock().unwrap().is_empty(), "/mount is the person's own ask");
        assert_eq!(*told.lock().unwrap(), [(Asked::Mount(dir), true)]);
    }

    #[test]
    fn blocks_are_read_from_the_proxys_403_bodies() {
        let text = "curl: (22) 403\nBlocked by network policy: domain PyPI.org\n  rule: x\n\
                    Approval required for api.github.com.\nBlocked by local rule for evil.test:8443\n\
                    Blocked by network policy: domain pypi.org\nBlocked by local rule for api.github.com";
        let blocks = blocks_in(text);
        assert_eq!(blocks.hosts, ["pypi.org"]);
        assert_eq!(blocks.ruled, ["evil.test:8443", "api.github.com"]);
        assert!(!blocks.org);
        assert!(blocks_in("Blocked by org policy").org);
        assert_eq!(blocks_in("Blocked by network policy: domain *.x"), Blocks::default());
    }

    #[test]
    fn mounts_are_confirmed_with_the_resolved_path_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Allow);
        let notes = worker.dirs.home.join("notes");
        fs::create_dir_all(&notes).unwrap();
        let notes = dunce::canonicalize(&notes).unwrap();

        let outcome = worker.answer_mount("~/notes", Some("ro"), Some("the user asked\nfor their notes"), false);
        let (shown, at) = (notes.display().to_string(), container_path(&notes.display().to_string()));
        assert_eq!(outcome, Outcome::allowed(format!("mounted {shown} (ro) at {at}")));
        let question = asked.lock().unwrap()[0].clone();
        assert!(question.contains(&format!("mount a directory of this computer:\n\n{}\n\nread-only", notes.display())));
        assert!(question.contains("The agent says: \u{201c}the user asked for their notes\u{201d}"), "{question}");
        assert_eq!(worker.summary.lock().unwrap().mounted, vec![format!("{} (ro)", notes.display())]);

        // Declined just now: not asked again. Refused outright, unasked:
        worker.confirm = Box::new(|_| Answer::Deny);
        assert!(!worker.answer_mount("~/notes", None, None, false).allowed);
        assert!(
            worker.answer_mount("~/notes", None, None, false).message.ends_with("declined on the host a moment ago")
        );
        assert!(!worker.answer_mount("~/missing", None, None, false).ok);
        // The command line's suffix names the mode, and wins over `mode`.
        worker.declined.clear();
        worker.confirm = Box::new(|q| if q.contains("read-only") { Answer::Allow } else { Answer::Deny });
        assert_eq!(
            worker.answer_mount("~/notes:ro", Some("rw"), None, false).message,
            format!("mounted {shown} (ro) at {at}")
        );
        assert!(!worker.answer_mount("~/notes:rw", None, None, false).allowed);
        worker.confirm = Box::new(|_| Answer::Deny);
        assert!(!worker.answer_mount("~/notes", Some("rwx"), None, false).ok);
        assert!(!worker.answer_mount("", None, None, false).ok);
        fs::create_dir_all(worker.dirs.sandboxes_dir()).unwrap();
        let data = worker.dirs.data.display().to_string();
        assert!(worker.answer_mount(&data, Some("ro"), None, false).message.contains("never mounted on request"));
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn extra_mounts_become_working_directories() {
        let mut set = MountSet { primary: Some(Mount::rw("/work/proj")), extras: vec![] };
        assert_eq!(working_directories(&set), None);
        set.extras = vec![Mount::rw("/work/lib"), Mount { path: "/docs".into(), mode: Mode::Ro }];
        assert_eq!(
            working_directories(&set),
            Some(json!({ "permissions": { "additionalDirectories": ["/work/lib", "/docs"] } }))
        );
    }

    #[test]
    fn agent_text_is_one_short_line() {
        assert_eq!(one_line("a\n\tb   c\u{1b}[31m"), "a b c [31m");
        assert_eq!(one_line(&"x".repeat(300)).chars().count(), 201);
    }

    #[test]
    fn a_declined_host_is_not_asked_about_again_right_away() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Deny);
        let declined = worker.answer_allow("example.com:443", false);
        assert_eq!(declined, Outcome::done("example.com:443: declined on the host".into()));
        let again = worker.answer_allow("example.com:443", false);
        assert_eq!(again.message, "example.com:443: declined on the host a moment ago");
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert!(asked.lock().unwrap()[0].contains("Sandbox 'claude-proj' asks for network access to example.com:443"));
    }

    #[test]
    fn requests_that_widen_or_name_unknown_approvals_are_refused_unasked() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Allow);
        assert!(!worker.answer_allow("*.example.com", false).ok);
        assert!(!worker.answer_allow("a.com,b.com", false).ok);
        assert!(!worker.answer_approval("made-up", Some("once"), false).ok);
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn requests_are_read_oldest_first_and_junk_is_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, _) = worker(tmp.path(), Answer::Deny);
        let requests = tmp.path().join("requests");
        fs::write(requests.join("one.json"), r#"{"host": "a.com"}"#).unwrap();
        fs::write(requests.join("not-json.txt"), "x").unwrap();
        fs::write(requests.join("half.json"), r#"{"host": "#).unwrap();
        let found: Vec<String> = worker.requests().into_iter().map(|(id, _)| id).collect();
        assert_eq!(found.len(), 2);
        assert!(!requests.join("not-json.txt").exists());
        // A request that does not parse yet is left for the next round.
        assert_eq!(worker.take(&requests.join("half.json")), None);
        assert!(requests.join("half.json").exists());
        let outcome = worker.take(&requests.join("one.json")).unwrap();
        assert_eq!(outcome.message, "a.com: declined on the host");
        assert!(!requests.join("one.json").exists());
        assert!(fs::read_to_string(tmp.path().join("log")).unwrap().contains(r#""host":"a.com""#));
    }

    #[test]
    fn mounts_below_a_writable_mount_are_refused_unless_the_person_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut worker, asked) = worker(tmp.path(), Answer::Allow);
        let proj = worker.dirs.home.join("proj");
        fs::create_dir_all(proj.join("docs")).unwrap();
        let proj = dunce::canonicalize(&proj).unwrap();
        let state = crate::state::SandboxState {
            name: "claude-proj".into(),
            harness: "claude".into(),
            agent: "claude".into(),
            kind: crate::state::Kind::Project,
            mounts: MountSet { primary: Some(Mount::rw(&proj)), extras: vec![] },
            mcp: vec![],
            template: None,
            image_digest: None,
            declined_digest: None,
            internal_mounts: vec![],
            vertex_secret: None,
            created_at: 0,
        };
        crate::state::save(&worker.dirs, &state).unwrap();

        let outcome = worker.answer_mount("docs", Some("rw"), None, false);
        assert!(!outcome.ok && outcome.message.contains("can already change"), "{outcome:?}");
        assert!(asked.lock().unwrap().is_empty(), "refused before the dialog");
        // The person's own `/mount` is not the agent's.
        assert!(worker.answer_mount("docs:ro", None, None, true).ok);
    }

    #[cfg(unix)]
    #[test]
    fn links_the_agent_files_are_removed_never_followed() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let precious = tmp.path().join("precious");
        fs::create_dir_all(precious.join("docs")).unwrap();
        fs::write(precious.join("notes.txt"), "keep").unwrap();
        fs::write(precious.join("secret.json"), r#"{"host": "leaked.example"}"#).unwrap();

        let (mut worker, _) = worker(tmp.path(), Answer::Deny);
        let requests = tmp.path().join("requests");
        symlink(&precious, requests.join("dir")).unwrap();
        symlink(precious.join("secret.json"), requests.join("swapped.json")).unwrap();
        fs::write(requests.join("r1.json"), r#"{"host": "bad host"}"#).unwrap();
        worker.round(&AtomicBool::new(false));

        assert!(precious.join("docs").is_dir() && precious.join("secret.json").exists());
        assert_eq!(fs::read_to_string(precious.join("notes.txt")).unwrap(), "keep");
        assert!(fs::read_dir(&requests).unwrap().next().is_none(), "links and answered requests are gone");
        assert_eq!(read_plain_file(&precious.join("notes.txt")).as_deref(), Some("keep"));
        symlink(precious.join("secret.json"), requests.join("late.json")).unwrap();
        assert_eq!(read_plain_file(&requests.join("late.json")), None);
    }
}
