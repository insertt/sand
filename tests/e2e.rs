//! End-to-end tests: the real `sand` binary driving a fake `sbx` that records
//! every command. They check the sequence of sbx calls sand makes.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    fake: PathBuf,
    proj: PathBuf,
    other: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let env = Self {
            home: root.join("home"),
            data: root.join("data"),
            fake: root.join("fake"),
            proj: root.join("work/proj"),
            other: root.join("work/other"),
            root,
            _tmp: tmp,
        };
        for dir in [&env.home, &env.fake, &env.proj, &env.other] {
            fs::create_dir_all(dir).unwrap();
        }
        let script = env.root.join("sbx");
        fs::write(&script, include_str!("fake_sbx.sh")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        env
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sand"));
        cmd.args(args)
            .current_dir(&self.proj)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("SAND_DATA_DIR", &self.data)
            .env("SAND_CONFIG", self.root.join("config.toml"))
            .env("SAND_SBX", self.root.join("sbx"))
            .env("FAKE_SBX_DIR", &self.fake)
            // Nothing listens here: tests never ask a real registry.
            .env("SAND_REGISTRY", "http://127.0.0.1:9")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("HTTP_PROXY")
            .env_remove("http_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .env_remove("CODEX_HOME")
            .env_remove("SAND_CONFIRM")
            .env("TERM", "xterm-256color")
            .env_remove("TERM_PROGRAM")
            .env_remove("TERM_PROGRAM_VERSION")
            .env_remove("LC_TERMINAL")
            .env_remove("LC_TERMINAL_VERSION")
            .env_remove("ITERM_SESSION_ID")
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("WT_SESSION")
            .env_remove("TERN_ACP")
            .env("TERN_CONFIG_DIR", self.home.join("tern"))
            .stdin(Stdio::null());
        cmd
    }

    fn sand(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs sand and asserts it succeeded.
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.sand(args);
        assert!(out.status.success(), "sand {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    /// Runs sand, asserts it failed, and returns stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.sand(args);
        assert!(!out.status.success(), "sand {args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// sbx invocations recorded since the last call, without the approval list
    /// the network-prompt bridge polls in the background while a session runs.
    fn take_log(&self) -> Vec<String> {
        let path = self.fake.join("log");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(&path);
        text.lines().filter(|line| *line != "policy approval ls --json").map(String::from).collect()
    }

    fn register_mcp(&self, names: &[&str]) {
        fs::write(self.fake.join("mcp"), names.join("\n") + "\n").unwrap();
    }

    fn config(&self, text: &str) {
        fs::write(self.root.join("config.toml"), text).unwrap();
    }

    /// What sand recorded about a sandbox.
    fn state(&self, name: &str) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.sandbox_dir(name).join("state.json")).unwrap()).unwrap()
    }

    fn p(&self, path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn sandbox_dir(&self, name: &str) -> PathBuf {
        self.data.join("sandboxes").join(name)
    }
}

fn has(log: &[String], needle: &str) -> bool {
    log.iter().any(|line| line.contains(needle))
}

fn starting(log: &[String], prefix: &str) -> Vec<String> {
    log.iter().filter(|line| line.starts_with(prefix)).cloned().collect()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn first_launch_creates_mounts_and_runs_with_every_step_logged() {
    let env = Env::new();
    let out = env.ok(&["codex", ".", "../other:ro"]);
    let log = env.take_log();

    let create = format!("create --name codex-proj codex {} {}:ro", env.p(&env.proj), env.p(&env.other));
    assert_eq!(starting(&log, "create"), vec![create]);
    // sand's own mounts: read-only runtime dir and host-backed history.
    let runtime = env.sandbox_dir("codex-proj").join("runtime");
    assert!(has(&log, &format!("mount codex-proj {}:/home/agent/.sand:ro", env.p(&runtime))), "{log:#?}");
    let history = env.sandbox_dir("codex-proj").join("history/.codex_sessions");
    assert!(has(&log, &format!("mount codex-proj {}:/home/agent/.codex/sessions:rw", env.p(&history))), "{log:#?}");
    assert!(history.is_dir());
    // Host skills are handed to sbx before the sandbox starts.
    let at = |needle: &str| log.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle}: {log:#?}"));
    assert!(at("skills import --force") < at("exec codex-proj"));
    // Configuration is applied inside the sandbox, then the agent starts.
    assert!(has(&log, "exec codex-proj sh /home/agent/.sand/apply.sh"));
    assert_eq!(log.last().unwrap(), "run --name codex-proj");

    let err = stderr(&out);
    for step in [
        "Looking for an existing sandbox",
        "Creating sandbox 'codex-proj'",
        "Starting the sandbox",
        "Mounting sand's own directories",
        "Checking agent API and MCP servers",
        "Starting codex (bypass permissions)",
        "Session ended",
    ] {
        assert!(err.contains(step), "missing step '{step}' in:\n{err}");
    }
    assert!(!err.contains("$ sbx") && !err.contains("created codex-proj"), "sbx is quiet by default:\n{err}");
    assert!(err.contains("api.openai.com reachable"), "{err}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("AGENT-STARTED"));
}

#[test]
fn harness_alone_means_the_current_directory() {
    let a = Env::new();
    a.ok(&["codex"]);
    let b = Env::new();
    b.ok(&["codex", "."]);
    let strip = |env: &Env| -> Vec<String> {
        let root = env.p(&env.root);
        // The keep-alive session starts in the background, so its position in the log varies.
        env.take_log().iter().filter(|l| !l.contains("read _")).map(|l| l.replace(&root, "<root>")).collect()
    };
    let (la, lb) = (strip(&a), strip(&b));
    assert_eq!(la, lb);
    assert!(has(&la, "create --name codex-proj codex <root>/work/proj"));
}

#[test]
fn relaunch_with_the_same_mounts_just_attaches() {
    let env = Env::new();
    env.ok(&["codex", ".", "../other:ro"]);
    env.take_log();
    let out = env.ok(&["codex", ".", "../other:ro"]);
    let log = env.take_log();
    assert!(
        starting(&log, "create").is_empty()
            && starting(&log, "mount").is_empty()
            && starting(&log, "umount").is_empty()
    );
    assert_eq!(log.last().unwrap(), "run --name codex-proj");
    assert!(stderr(&out).contains("existing sandbox matches the requested setup"));
}

#[test]
fn changed_mounts_need_a_decision() {
    let env = Env::new();
    let third = env.root.join("work/third");
    fs::create_dir_all(&third).unwrap();
    env.ok(&["codex", ".", "../other:ro"]);
    env.take_log();

    // No terminal and no --on-conflict: refuse rather than guess.
    let err = env.fails(&["codex", ".", "../third"]);
    assert!(err.contains("exists with a different setup") && err.contains("--on-conflict"), "{err}");
    assert!(err.contains("current mounts") && err.contains("requested mounts"), "{err}");
    assert!(starting(&env.take_log(), "mount").is_empty());

    // Abort changes nothing.
    assert!(!env.sand(&["codex", ".", "../third", "--on-conflict", "abort"]).status.success());
    let log = env.take_log();
    assert!(starting(&log, "mount").is_empty() && starting(&log, "run").is_empty());

    // Merge keeps ../other and adds ../third.
    env.ok(&["codex", ".", "../third", "--on-conflict", "merge"]);
    let log = env.take_log();
    assert_eq!(starting(&log, "mount"), vec![format!("mount codex-proj {}", env.p(&third))]);
    assert!(starting(&log, "umount").is_empty() && starting(&log, "create").is_empty());

    // Replace makes it exactly "." + "../other" (now read-write): unmount third and remount other.
    env.ok(&["codex", ".", "../other", "--on-conflict", "replace"]);
    let log = env.take_log();
    let mut umounts = starting(&log, "umount");
    umounts.sort();
    assert_eq!(
        umounts,
        vec![format!("umount codex-proj {}", env.p(&env.other)), format!("umount codex-proj {}", env.p(&third))]
    );
    assert_eq!(starting(&log, "mount"), vec![format!("mount codex-proj {}", env.p(&env.other))]);
    assert!(starting(&log, "create").is_empty() && starting(&log, "rm").is_empty());

    // The new state sticks: the same command again is a plain attach.
    env.ok(&["codex", ".", "../other"]);
    assert!(starting(&env.take_log(), "mount").is_empty());
}

#[test]
fn read_only_mounts_added_later_use_an_explicit_target() {
    let env = Env::new();
    env.ok(&["codex"]);
    env.take_log();
    env.ok(&["codex", ".", "../other:ro", "--on-conflict", "merge"]);
    let other = env.p(&env.other);
    assert_eq!(starting(&env.take_log(), "mount"), vec![format!("mount codex-proj {other}:{other}:ro")]);
}

#[test]
fn resume_attaches_without_needing_the_mounts() {
    let env = Env::new();
    let err = env.fails(&["codex", "--resume"]);
    assert!(err.contains("nothing to resume") && err.contains("codex-proj"), "{err}");
    assert!(starting(&env.take_log(), "create").is_empty());

    env.ok(&["codex", ".", "../other:ro"]);
    env.take_log();
    env.ok(&["codex", "--resume"]);
    let log = env.take_log();
    assert!(
        starting(&log, "create").is_empty()
            && starting(&log, "mount").is_empty()
            && starting(&log, "umount").is_empty()
    );
    assert_eq!(log.last().unwrap(), "run --name codex-proj");
}

#[test]
fn resume_adds_the_mounts_on_the_command_line() {
    let env = Env::new();
    env.ok(&["codex"]);
    env.take_log();
    env.ok(&["codex", "--resume", ".", "../other:ro"]);
    let log = env.take_log();
    let other = env.p(&env.other);
    assert!(starting(&log, "create").is_empty() && starting(&log, "umount").is_empty());
    assert_eq!(starting(&log, "mount"), vec![format!("mount codex-proj {other}:{other}:ro")]);
    assert_eq!(log.last().unwrap(), "run --name codex-proj");
    assert_eq!(env.state("codex-proj")["mounts"]["extras"][0]["path"], other.as_str());

    // Mounts it already has are left alone, and nothing is removed.
    env.ok(&["codex", "--resume", "."]);
    let log = env.take_log();
    assert!(starting(&log, "mount").is_empty() && starting(&log, "umount").is_empty());
    assert_eq!(env.state("codex-proj")["mounts"]["extras"].as_array().unwrap().len(), 1);

    // The first directory picks the sandbox, so naming only the new one misses it.
    let err = env.fails(&["codex", "--resume", "../other"]);
    assert!(err.contains("nothing to resume") && err.contains("sand mount"), "{err}");
}

#[test]
fn mount_and_umount_change_a_sandbox_during_a_session() {
    let env = Env::new();
    assert!(env.fails(&["mount", "codex", "../other"]).contains("no sandbox named 'codex-proj'"));

    let gate = env.root.join("gate");
    let mut session: Child = env
        .command(&["codex"])
        .env("FAKE_SBX_RUN_WAIT", &gate)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = PathBuf::from(format!("{}.started", gate.display()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(Instant::now() < deadline, "session never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    env.take_log();

    let other = env.p(&env.other);
    let out = env.ok(&["mount", "codex", "../other:ro"]);
    assert!(stderr(&out).contains("running session"), "{}", stderr(&out));
    assert_eq!(starting(&env.take_log(), "mount"), vec![format!("mount codex-proj {other}:{other}:ro")]);
    assert_eq!(env.state("codex-proj")["mounts"]["extras"][0]["mode"], "ro");

    // Mounting it again is a no-op; a different mode remounts it.
    env.ok(&["mount", "codex", "../other:ro"]);
    assert!(starting(&env.take_log(), "mount").is_empty());
    env.ok(&["mount", "codex", "../other"]);
    let log = env.take_log();
    assert_eq!(starting(&log, "umount"), vec![format!("umount codex-proj {other}")]);
    assert_eq!(starting(&log, "mount"), vec![format!("mount codex-proj {other}")]);

    assert!(env.fails(&["umount", "codex", "."]).contains("primary workspace"));
    env.ok(&["umount", "codex", "../other"]);
    assert_eq!(starting(&env.take_log(), "umount"), vec![format!("umount codex-proj {other}")]);
    assert!(env.state("codex-proj")["mounts"]["extras"].as_array().unwrap().is_empty());
    assert!(stderr(&env.ok(&["umount", "codex", "../other"])).contains("is not mounted"));

    fs::write(&gate, "").unwrap();
    assert!(session.wait().unwrap().success());
    // The next launch with the original command line sees a sandbox that matches it.
    env.take_log();
    env.ok(&["codex"]);
    assert!(starting(&env.take_log(), "mount").is_empty());
}

#[test]
fn network_prompts_are_confirmed_on_the_host_and_applied_for_that_sandbox() {
    let env = Env::new();
    let approvals = r#"{"approvals": [
        {"id": "ap-1", "host": "pypi.org:443", "sandbox": "claude-proj", "options": [{"id": "once", "label": "Allow once"}]},
        {"id": "ap-2", "host": "evil.com", "sandbox": "codex-other", "options": [{"id": "once", "label": "Allow once"}]}
    ]}"#;
    fs::write(env.fake.join("approvals.json"), approvals).unwrap();
    // Stands in for the native dialog: records the question and allows.
    let asked = env.root.join("asked");
    let confirm = env.root.join("confirm");
    fs::write(&confirm, format!("#!/bin/sh\nprintf '%s\\n---\\n' \"$1\" >> '{}'\n", asked.display())).unwrap();
    fs::set_permissions(&confirm, fs::Permissions::from_mode(0o755)).unwrap();

    let gate = env.root.join("gate");
    let session: Child = env
        .command(&["claude"])
        .env("FAKE_SBX_RUN_WAIT", &gate)
        .env("SAND_CONFIRM", &confirm)
        .env("TERM_PROGRAM", "iTerm.app")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_for = |path: &Path, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for(&PathBuf::from(format!("{}.started", gate.display())), "session never started");

    let dir = env.sandbox_dir("claude-proj").join("policy");
    let inbox = env.sandbox_dir("claude-proj").join("requests");
    // The agent writes only the requests; what sand writes back is read-only to it.
    let mounted = |target: &str, mode: &str, host: &Path| {
        env.state("claude-proj")["internal_mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["target"] == target && m["mode"] == mode && m["host"] == env.p(host).as_str())
    };
    assert!(mounted("/home/agent/.sand-policy", "ro", &dir), "{:#?}", env.state("claude-proj"));
    assert!(mounted("/home/agent/.sand-requests", "rw", &inbox), "{:#?}", env.state("claude-proj"));
    let staged = env.sandbox_dir("claude-proj").join("runtime/sync/home/.local/share/sand/mods/sand");
    assert!(staged.join("hooks/register.tsx").is_file() && staged.join(".claude-plugin/plugin.json").is_file());

    // Only this sandbox's approvals are listed for the mod.
    wait_for(&dir.join("pending.json"), "no pending list");
    let pending = fs::read_to_string(dir.join("pending.json")).unwrap();
    assert!(pending.contains("ap-1") && !pending.contains("ap-2"), "{pending}");

    fs::write(inbox.join("r1.json"), r#"{"host": "example.com:443"}"#).unwrap();
    fs::write(inbox.join("r2.json"), r#"{"host": "evil.com", "approval": "ap-2", "option": "once"}"#).unwrap();
    fs::write(inbox.join("r3.json"), r#"{"host": "*", "approval": null}"#).unwrap();
    // A host refused before is blocked by a deny rule: allowing lifts this sandbox's own,
    // but not one that applies to every sandbox.
    for (scope, host, id) in [("claude-proj", "refused.com:443", "rule-1"), ("global", "global.com:443", "rule-2")] {
        fs::create_dir_all(env.fake.join("deny").join(scope)).unwrap();
        fs::write(env.fake.join("deny").join(scope).join(host), id).unwrap();
    }
    fs::write(inbox.join("r5.json"), r#"{"host": "refused.com:443"}"#).unwrap();
    fs::write(inbox.join("r6.json"), r#"{"host": "global.com:443"}"#).unwrap();
    // A mount the agent asked for; relative paths are taken from the primary workspace.
    fs::write(inbox.join("m1.json"), r#"{"kind": "mount", "path": "../other:ro", "reason": "docs"}"#).unwrap();
    // The host and options of an approval come from sbx, whatever the request says.
    fs::write(inbox.join("r4.json"), r#"{"host": "other.com", "approval": "ap-1", "option": "once"}"#).unwrap();
    for id in ["r1", "r2", "r3", "r4", "r5", "r6", "m1"] {
        wait_for(&dir.join(format!("results/{id}.json")), "no result");
    }
    let result = |id: &str| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(dir.join(format!("results/{id}.json"))).unwrap()).unwrap()
    };
    assert_eq!(result("r1")["allowed"], true, "{}", result("r1"));
    // Another sandbox's approval is not this sandbox's to answer, and wildcards are refused.
    assert_eq!(result("r2")["ok"], false, "{}", result("r2"));
    assert_eq!(result("r3")["ok"], false, "{}", result("r3"));
    let questions = fs::read_to_string(&asked).unwrap();
    assert!(questions.contains("Sandbox 'claude-proj' asks for network access to example.com:443"), "{questions}");
    assert!(questions.contains("network access to pypi.org:443.\n\nAnswer: Allow once"), "{questions}");
    assert_eq!(questions.matches("---").count(), 5, "{questions}");
    let other = env.p(&env.other);
    assert!(
        questions.contains(&format!("asks to mount a directory of this computer:\n\n{other}\n\nread-only")),
        "{questions}"
    );
    assert_eq!(result("m1")["message"], format!("mounted {other} (ro) at {other}"));
    assert_eq!(env.state("claude-proj")["mounts"]["extras"][0]["path"], other.as_str());
    assert_eq!(result("r4")["message"], "pypi.org:443: Allow once");
    assert!(questions.contains("refused.com:443, which a deny rule blocks"), "{questions}");
    assert_eq!(result("r5")["allowed"], true, "{}", result("r5"));
    assert!(!env.fake.join("deny/claude-proj/refused.com:443").exists());
    assert_eq!(result("r6")["ok"], false, "{}", result("r6"));
    assert!(
        result("r6")["message"].as_str().unwrap().contains("sbx policy rm network --id rule-2"),
        "{}",
        result("r6")
    );
    assert!(env.fake.join("deny/global/global.com:443").exists());

    fs::write(&gate, "").unwrap();
    let out = session.wait_with_output().unwrap();
    assert!(out.status.success());
    let err = stderr(&out);
    let summary = err.lines().find(|l| l.contains("allowed during the session, for this sandbox:")).unwrap_or_default();
    for host in ["example.com:443", "refused.com:443", "pypi.org:443"] {
        assert!(summary.contains(host), "{err}");
    }
    assert!(!summary.contains("global.com"), "{err}");
    assert!(err.contains(&format!("mounted during the session: {other} (ro)")), "{err}");
    let log = env.take_log();
    assert!(has(&log, "policy allow network --sandbox claude-proj example.com:443"), "{log:#?}");
    assert_eq!(starting(&log, "policy approval respond"), vec!["policy approval respond ap-1 --option once"]);
    let lifted = "policy rm network --sandbox claude-proj --id rule-1 --force";
    let at = |line: &str| log.iter().position(|l| l == line).unwrap_or_else(|| panic!("{line}: {log:#?}"));
    assert!(at(lifted) < at("policy allow network --sandbox claude-proj refused.com:443"), "{log:#?}");
    assert!(!has(&log, "policy allow network --sandbox claude-proj global.com:443"), "{log:#?}");
    // The agent raises desktop notifications through the host's terminal, so it is told which one that is.
    assert!(has(&log, "run --name claude-proj -e TERM_PROGRAM=iTerm.app"), "{log:#?}");
    let record = fs::read_to_string(env.sandbox_dir("claude-proj").join("network-prompts.log")).unwrap();
    assert_eq!(record.lines().count(), 7, "{record}");
    assert!(log.contains(&format!("mount claude-proj {other}:{other}:ro")), "{log:#?}");

    // Turned off, the mod and the bridge are gone.
    env.config("[harness.claude]\nnetwork_prompts = false\n");
    // The mount the agent asked for is kept, as one `sand mount` adds: resuming keeps it without asking.
    env.ok(&["claude", "--resume"]);
    assert!(!staged.exists());
}

#[test]
fn extra_mounts_are_working_directories_of_claude_code() {
    let env = Env::new();
    env.ok(&["claude", ".", "../other:ro", "--no-attach"]);
    let staged = env.sandbox_dir("claude-proj").join("runtime/sync/home/.claude/settings.json");
    let settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&staged).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["additionalDirectories"], serde_json::json!([env.p(&env.other)]));
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
}

#[test]
fn mounts_that_reach_sands_own_data_are_warned_about() {
    let env = Env::new();
    fs::create_dir_all(&env.data).unwrap();
    let data = env.p(&env.data);
    let err = stderr(&env.ok(&["codex", ".", &format!("{data}:ro")]));
    assert!(
        err.contains("contains sand's per-sandbox data") && err.contains("read the history of every sandbox"),
        "{err}"
    );
    let err = stderr(&env.ok(&["mount", "codex", &data]));
    assert!(err.contains("file network prompts in their name"), "{err}");
    assert!(!stderr(&env.ok(&["codex", "--resume"])).contains("per-sandbox data"));
}

#[test]
fn no_attach_sets_the_sandbox_up_without_running_the_agent() {
    let env = Env::new();
    let out = env.ok(&["codex", ".", "../other:ro", "--no-attach"]);
    let log = env.take_log();
    let create = format!("create --name codex-proj codex {} {}:ro", env.p(&env.proj), env.p(&env.other));
    assert_eq!(starting(&log, "create"), vec![create]);
    assert!(has(&log, "exec codex-proj sh /home/agent/.sand/apply.sh"));
    assert!(starting(&log, "run").is_empty());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("AGENT-STARTED"));
    assert!(stderr(&out).contains("sandbox 'codex-proj' is ready"));

    // The sandbox it left behind is a normal one: the same command attaches.
    env.ok(&["codex", ".", "../other:ro"]);
    let log = env.take_log();
    assert!(starting(&log, "create").is_empty() && starting(&log, "mount").is_empty());
    assert_eq!(log.last().unwrap(), "run --name codex-proj");
}

#[test]
fn recreate_preserves_history_unless_told_otherwise() {
    let env = Env::new();
    env.ok(&["codex"]);
    let history = env.sandbox_dir("codex-proj").join("history/.codex_sessions");
    fs::write(history.join("rollout.jsonl"), "conversation").unwrap();
    env.take_log();

    let out = env.ok(&["codex", "--recreate"]);
    let log = env.take_log();
    assert_eq!(starting(&log, "rm"), vec!["rm --force codex-proj"]);
    assert_eq!(starting(&log, "create").len(), 1);
    assert_eq!(fs::read_to_string(history.join("rollout.jsonl")).unwrap(), "conversation");
    // The history mount is re-established on the new sandbox.
    assert!(has(&log, &format!("mount codex-proj {}:/home/agent/.codex/sessions:rw", env.p(&history))));
    assert!(stderr(&out).contains("history is preserved"));

    env.ok(&["codex", "--recreate", "--no-preserve-history"]);
    assert!(!history.join("rollout.jsonl").exists());
    assert!(history.is_dir(), "a fresh history directory is mounted");
}

#[test]
fn mcp_servers_are_static_at_creation_and_loaded_later() {
    let env = Env::new();
    env.register_mcp(&["atlassian", "gitlab", "notion"]);

    let err = env.fails(&["codex", "--mcp", "atlassian,missing"]);
    assert!(err.contains("'missing' is not registered") && err.contains("sbx mcp add missing"), "{err}");
    assert!(starting(&env.take_log(), "create").is_empty());

    env.ok(&["codex", "--mcp", "atlassian,gitlab"]);
    let log = env.take_log();
    assert!(has(&log, "create --name codex-proj --static-mcp atlassian,gitlab codex"), "{log:#?}");
    assert!(starting(&log, "mcp load").is_empty());

    // Adding a server works live.
    let out = env.ok(&["codex", "--mcp", "atlassian,gitlab,notion"]);
    let log = env.take_log();
    assert_eq!(starting(&log, "mcp load"), vec!["mcp load notion --sandbox codex-proj"]);
    assert!(starting(&log, "create").is_empty());
    assert!(stderr(&out).contains("existing sandbox matches"), "adding servers is not a conflict");

    // Dropping one is a conflict: merge keeps it, replace recreates.
    let err = env.fails(&["codex", "--mcp", "atlassian"]);
    assert!(err.contains("cannot detach MCP servers") && err.contains("gitlab, notion"), "{err}");
    env.ok(&["codex", "--mcp", "atlassian", "--on-conflict", "merge"]);
    let log = env.take_log();
    assert!(starting(&log, "create").is_empty() && starting(&log, "rm").is_empty());

    env.ok(&["codex", "--mcp", "atlassian", "--on-conflict", "replace"]);
    let log = env.take_log();
    assert_eq!(starting(&log, "rm"), vec!["rm --force codex-proj"]);
    assert!(has(&log, "create --name codex-proj --static-mcp atlassian codex"));
}

#[test]
fn mcp_servers_defined_in_config_are_registered_on_demand() {
    let env = Env::new();
    env.config("[mcp.linear]\nurl = \"https://mcp.linear.app/mcp\"\n\n[mcp.pw]\ncommand = \"npx\"\nargs = [\"@playwright/mcp@latest\", \"--headless\"]\n");
    env.ok(&["codex", "--mcp", "linear,pw"]);
    let log = env.take_log();
    assert!(has(&log, "mcp add linear --url https://mcp.linear.app/mcp"));
    assert!(has(&log, "mcp add pw --command npx --args @playwright/mcp@latest,--headless"));
    assert!(has(&log, "--static-mcp linear,pw"));
}

#[test]
fn mcp_status_reaches_the_banner_and_the_status_line_file() {
    let env = Env::new();
    env.register_mcp(&["atlassian", "gitlab"]);
    fs::write(
        env.fake.join("mcp.json"),
        r#"[{"name": "atlassian", "status": "ready"}, {"name": "gitlab", "auth_status": "needs authorization"}]"#,
    )
    .unwrap();
    let out = env.ok(&["claude", "--mcp", "atlassian,gitlab"]);
    let err = stderr(&out);
    assert!(err.contains("atlassian ✓") && err.contains("gitlab ⚠ needs auth"), "{err}");
    let runtime = env.sandbox_dir("claude-proj").join("runtime");
    assert_eq!(fs::read_to_string(runtime.join("mcp-status")).unwrap(), "atlassian\tok\ngitlab\tauth\n");
    assert!(fs::read_to_string(runtime.join("info")).unwrap().contains("sandbox=claude-proj"));
    assert!(fs::read_to_string(runtime.join("statusline.sh")).unwrap().starts_with("#!/bin/sh"));
}

#[test]
fn mcp_servers_needing_auth_are_not_authorized_without_a_terminal() {
    let env = Env::new();
    env.register_mcp(&["atlassian", "gitlab", "notion"]);
    fs::write(
        env.fake.join("mcp.json"),
        r#"[{"name": "atlassian", "status": "ready"}, {"name": "gitlab", "auth_status": "needs authorization"}, {"name": "notion", "auth_status": "expired"}]"#,
    )
    .unwrap();
    // notion needs it too, but this sandbox does not use it.
    let out = env.ok(&["claude", "--mcp", "atlassian,gitlab"]);
    let (log, err) = (env.take_log(), stderr(&out));
    assert!(starting(&log, "mcp auth").is_empty(), "{log:#?}");
    assert!(err.contains("Authorizing MCP servers: gitlab\n"), "{err}");
    assert!(err.contains("no terminal") && err.contains("run `sbx mcp auth gitlab` yourself"), "{err}");
    assert_eq!(log.last().unwrap(), "run --name claude-proj");

    // Nothing to do, nothing said.
    fs::write(
        env.fake.join("mcp.json"),
        r#"[{"name": "atlassian", "status": "ready"}, {"name": "gitlab", "status": "ready"}]"#,
    )
    .unwrap();
    let out = env.ok(&["claude", "--mcp", "atlassian,gitlab"]);
    assert!(!stderr(&out).contains("Authorizing MCP servers"));
}

#[test]
fn expired_sbx_login_is_reported_without_a_terminal() {
    let env = Env::new();
    fs::write(env.fake.join("logged-out"), "").unwrap();
    let out = env.sand(&["claude"]);
    assert!(!out.status.success());
    let (log, err) = (env.take_log(), stderr(&out));
    assert!(starting(&log, "login").is_empty(), "{log:#?}");
    assert!(
        err.contains("your Docker session has expired") && err.contains("no terminal to sign in on; run `sbx login`"),
        "{err}"
    );
}

#[test]
fn scratch_is_shared_and_dedicated_scratch_is_disposable() {
    let env = Env::new();
    env.ok(&["codex", "--scratch"]);
    let scratch = env.data.join("scratch");
    assert!(scratch.is_dir());
    let log = env.take_log();
    assert!(has(&log, &format!("create --name scratch-codex codex {}", env.p(&scratch))), "{log:#?}");
    assert_eq!(log.last().unwrap(), "run --name scratch-codex");

    // Another harness gets its own sandbox on the same scratch directory.
    env.ok(&["claude", "--scratch"]);
    assert!(has(&env.take_log(), &format!("create --name scratch-claude claude {}", env.p(&scratch))));

    let out = env.ok(&["codex", "--dedicated-scratch"]);
    let log = env.take_log();
    let create = starting(&log, "create");
    assert_eq!(create.len(), 1);
    let name = create[0].split_whitespace().nth(2).unwrap().to_string();
    assert!(name.starts_with("scratch-codex-") && create[0].ends_with(" codex"), "mountless: {create:?}");
    assert!(!log.iter().any(|l| l.contains("/.codex/sessions")), "no history for a disposable sandbox");
    assert_eq!(log.last().unwrap(), &format!("rm --force {name}"));
    assert!(!env.sandbox_dir(&name).exists());
    assert!(stderr(&out).contains("dedicated scratch sandbox; cleaning it up"));
}

#[test]
fn dedicated_scratch_is_removed_when_setup_fails_or_is_interrupted() {
    let env = Env::new();
    let created = |log: &[String]| starting(log, "create")[0].split_whitespace().nth(2).unwrap().to_string();
    let no_sandboxes = || fs::read_dir(env.fake.join("sandboxes")).unwrap().next().is_none();

    let out = env.command(&["codex", "--dedicated-scratch"]).env("FAKE_SBX_MOUNT_FAIL", "1").output().unwrap();
    assert!(!out.status.success());
    let log = env.take_log();
    let name = created(&log);
    assert_eq!(log.last().unwrap(), &format!("rm --force {name}"), "{log:#?}");
    assert!(no_sandboxes() && !env.sandbox_dir(&name).exists());

    // Ctrl-C while sand is still setting the sandbox up.
    let gate = env.root.join("gate");
    let child = env
        .command(&["codex", "--dedicated-scratch"])
        .env("FAKE_SBX_MOUNT_WAIT", &gate)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = PathBuf::from(format!("{}.started", gate.display()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(Instant::now() < deadline, "setup never reached the mount");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap().success());
    let out = child.wait_with_output().unwrap();
    fs::write(&gate, "").unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    let log = env.take_log();
    let name = created(&log);
    assert!(has(&log, &format!("rm --force {name}")), "{log:#?}");
    assert!(no_sandboxes() && !env.sandbox_dir(&name).exists());
}

#[test]
fn name_flag_picks_the_sandbox() {
    let env = Env::new();
    env.ok(&["codex", "--name", "feature-x"]);
    let log = env.take_log();
    assert!(has(&log, "create --name feature-x codex"));
    assert_eq!(log.last().unwrap(), "run --name feature-x");
    // From another directory, --resume --name finds it again.
    let out = env.command(&["codex", "--resume", "--name", "feature-x"]).current_dir(&env.other).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.take_log().last().unwrap(), "run --name feature-x");
    assert!(env.fails(&["codex", "--name", "bad name"]).contains("invalid sandbox name"));
}

#[test]
fn two_sessions_cannot_share_a_sandbox() {
    let env = Env::new();
    let gate = env.root.join("gate");
    let mut first: Child = env
        .command(&["codex"])
        .env("FAKE_SBX_RUN_WAIT", &gate)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = PathBuf::from(format!("{}.started", gate.display()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(Instant::now() < deadline, "first session never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    let err = env.fails(&["codex"]);
    assert!(err.contains("already has a running session") && err.contains("codex on"), "{err}");
    // A different sandbox on the same directory is fine.
    env.ok(&["claude"]);

    fs::write(&gate, "").unwrap();
    assert!(first.wait().unwrap().success());
    env.ok(&["codex"]);
}

#[test]
fn acp_mode_keeps_stdout_clean_and_merges_without_asking() {
    let env = Env::new();
    let out = env.ok(&["codex", "--acp"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ACP-READY\n", "stdout must carry only the ACP stream");
    let log = env.take_log();
    let acp = format!(
        "exec -i --workdir {} codex-proj bash -lc exec npx -y @agentclientprotocol/codex-acp",
        env.p(&env.proj)
    );
    assert_eq!(log.last().unwrap(), &acp);
    assert!(starting(&log, "run").is_empty());

    // An existing sandbox with other mounts is merged into, never prompted about.
    let out = env.ok(&["codex", ".", "../other", "--acp"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ACP-READY\n");
    assert_eq!(starting(&env.take_log(), "mount"), vec![format!("mount codex-proj {}", env.p(&env.other))]);

    // Without a harness the configured default is used.
    env.config("default_harness = \"opencode\"\n");
    env.ok(&["--acp"]);
    assert!(env.take_log().last().unwrap().ends_with("opencode-proj bash -lc exec opencode acp"));
}

#[test]
fn acp_mode_skips_what_it_cannot_ask_about() {
    let env = Env::new();
    env.register_mcp(&["gitlab"]);
    fs::write(env.fake.join("mcp.json"), r#"[{"name": "gitlab", "auth_status": "needs authorization"}]"#).unwrap();
    env.ok(&["claude", "--mcp", "gitlab"]);
    env.take_log();
    env.config("nightly = true\n");
    let out = env.ok(&["claude", "--acp"]);
    let (log, err) = (env.take_log(), stderr(&out));
    assert!(!err.contains("nightly") && !err.contains("Authorizing MCP servers"), "{err}");
    assert!(!has(&log, "version"), "{log:#?}");
    // The login probe's listing finds the sandbox; it is not listed again.
    assert_eq!(log.iter().filter(|l| l.as_str() == "ls -q").count(), 1, "{log:#?}");
}

#[test]
fn a_sandbox_handed_over_to_terns_chat_is_attached_without_setup() {
    let env = Env::new();
    env.ok(&["claude"]);
    env.take_log();
    // What `open_in_tern` holds while Tern's chat runs `sand claude --acp`.
    let locks = env.data.join("locks");
    fs::create_dir_all(&locks).unwrap();
    let handoff = fs::File::create(locks.join("claude-proj.handoff")).unwrap();
    handoff.lock().unwrap();

    let out = env.ok(&["claude", "--acp"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ACP-READY\n");
    let log = env.take_log();
    let acp = format!(
        "exec -i --workdir {} claude-proj bash -lc exec npx -y @agentclientprotocol/claude-agent-acp",
        env.p(&env.proj)
    );
    assert_eq!(log, vec![acp], "only the agent is started");
    assert!(stderr(&out).contains("has set the sandbox up"), "{}", stderr(&out));

    // Without the handoff the sandbox is set up as usual.
    drop(handoff);
    env.ok(&["claude", "--acp"]);
    assert!(has(&env.take_log(), "apply.sh"));
}

#[test]
fn host_configuration_is_copied_without_credentials() {
    let env = Env::new();
    let codex = env.home.join(".codex");
    fs::create_dir_all(codex.join("prompts")).unwrap();
    fs::write(
        codex.join("config.toml"),
        "model = \"o3\"\napproval_policy = \"on-request\"\n[mcp_servers.local]\ncommand = \"npx\"\n",
    )
    .unwrap();
    fs::write(codex.join("AGENTS.md"), "global instructions").unwrap();
    fs::write(codex.join("prompts/review.md"), "review prompt").unwrap();
    fs::write(codex.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
    fs::create_dir_all(env.home.join(".agents/skills/pdf")).unwrap();
    fs::write(env.home.join(".agents/skills/pdf/SKILL.md"), "pdf skill").unwrap();
    // What sbx put in the sandbox's config when it created it.
    fs::create_dir_all(env.fake.join("files/home/agent/.codex")).unwrap();
    fs::write(
        env.fake.join("files/home/agent/.codex/config.toml"),
        "[mcp_servers.gateway]\nurl = \"http://mcp-gateway.docker.internal/mcp\"\n",
    )
    .unwrap();

    let out = env.ok(&["codex"]);
    let staged = env.sandbox_dir("codex-proj").join("runtime/sync/home");
    assert_eq!(fs::read_to_string(staged.join(".codex/AGENTS.md")).unwrap(), "global instructions");
    assert_eq!(fs::read_to_string(staged.join(".codex/prompts/review.md")).unwrap(), "review prompt");
    // Skills go through sbx's shared store instead of sand's copy.
    assert!(!staged.join(".agents").exists());
    assert!(!staged.join(".codex/auth.json").exists(), "credentials must stay on the host");

    let merged: toml::Table = toml::from_str(&fs::read_to_string(staged.join(".codex/config.toml")).unwrap()).unwrap();
    assert_eq!(merged["model"].as_str(), Some("o3"));
    assert_eq!(merged["approval_policy"].as_str(), Some("never"), "bypass wins over the host setting");
    assert_eq!(merged["sandbox_mode"].as_str(), Some("danger-full-access"));
    let status_line = merged["tui"]["status_line"].as_array().unwrap();
    assert!(status_line.iter().any(|item| item.as_str() == Some("five-hour-limit")), "{status_line:?}");
    let servers = merged["mcp_servers"].as_table().unwrap();
    assert!(servers.contains_key("gateway") && !servers.contains_key("local"));

    let log = env.take_log();
    assert!(has(&log, "skills import --force"));
    assert!(has(&log, "exec codex-proj cat /home/agent/.codex/config.toml"));
    assert!(has(&log, "exec codex-proj sh /home/agent/.sand/apply.sh"));
    assert!(stderr(&out).contains("credentials are never copied"));
    let apply = fs::read_to_string(env.sandbox_dir("codex-proj").join("runtime/apply.sh")).unwrap();
    assert!(apply.contains("cp -RL \"$SRC/home/.\" \"$DEST/\""));
}

#[test]
fn skills_are_imported_by_sbx_unless_it_cannot_or_the_harness_opts_out() {
    let env = Env::new();
    // An sbx without the skills store must not stop the launch.
    let out = env.command(&["claude"]).env("FAKE_SBX_SKILLS_FAIL", "1").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("`sbx skills import` failed"), "{}", stderr(&out));
    assert_eq!(env.take_log().last().unwrap(), "run --name claude-proj");

    env.ok(&["opencode"]);
    assert!(starting(&env.take_log(), "skills").is_empty(), "sbx has no skills mount for opencode");
    env.config("[harness.claude]\nimport_skills = false\n");
    env.ok(&["claude"]);
    assert!(starting(&env.take_log(), "skills").is_empty());
}

#[test]
fn claude_gets_a_status_line_and_bypass_flags() {
    let env = Env::new();
    fs::create_dir_all(env.home.join(".claude/commands")).unwrap();
    fs::write(env.home.join(".claude/settings.json"), r#"{"model": "sonnet", "permissions": {"defaultMode": "plan"}}"#)
        .unwrap();
    fs::write(env.home.join(".claude/commands/ship.md"), "ship it").unwrap();
    fs::write(env.home.join(".claude/.credentials.json"), "secret").unwrap();

    env.ok(&["claude", "--", "fix the build"]);
    let log = env.take_log();
    assert_eq!(log.last().unwrap(), "run --name claude-proj -- --dangerously-skip-permissions fix the build");

    let staged = env.sandbox_dir("claude-proj").join("runtime/sync/home/.claude");
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(staged.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["model"], "sonnet");
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(settings["statusLine"]["command"], "sh /home/agent/.sand/statusline.sh");
    assert_eq!(fs::read_to_string(staged.join("commands/ship.md")).unwrap(), "ship it");
    assert!(!staged.join(".credentials.json").exists());

    // Flag-style arguments are appended to sbx's defaults untouched.
    env.ok(&["claude", "--", "--continue"]);
    assert_eq!(env.take_log().last().unwrap(), "run --name claude-proj -- --continue");
}

#[test]
fn vertex_auth_registers_a_refreshing_token_secret_and_points_claude_at_vertex() {
    let env = Env::new();
    fs::create_dir_all(env.home.join(".claude")).unwrap();
    fs::write(env.home.join(".claude/settings.json"), r#"{"env": {"ANTHROPIC_AUTH_TOKEN": "real", "DEBUG": "1"}}"#)
        .unwrap();
    env.config("[vertex]\nproject = \"acme\"\nregion = \"us-east5\"\n\n[harness.claude]\nauth = \"vertex\"\n");

    let out = env.ok(&["claude"]);
    let log = env.take_log();
    let secret = "secret set-custom --sandbox claude-proj --host us-east5-aiplatform.googleapis.com \
                  --host aiplatform.googleapis.com --env ANTHROPIC_AUTH_TOKEN --placeholder sand-vertex-claude-proj \
                  --command gcloud auth application-default print-access-token --refresh 45m";
    assert_eq!(starting(&log, "secret"), vec![secret]);
    let at = |needle: &str| log.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle}: {log:#?}"));
    assert!(at("secret set-custom") < at("create --name claude-proj"), "{log:#?}");
    assert_eq!(log.last().unwrap(), "run --name claude-proj -e ANTHROPIC_AUTH_TOKEN=sand-vertex-claude-proj");
    assert!(has(&log, "curl -sS -m 6 -w '\\n%{http_code}' https://us-east5-aiplatform.googleapis.com/"), "{log:#?}");

    let staged = env.sandbox_dir("claude-proj").join("runtime/sync/home/.claude/settings.json");
    let settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&staged).unwrap()).unwrap();
    assert_eq!(
        settings["env"],
        serde_json::json!({
            "DEBUG": "1",
            "CLAUDE_CODE_PLUGIN_DIRS": "/home/agent/.local/share/sand/mods/sand",
            "SAND_POLICY_DIR": "/home/agent/.sand-policy",
            "SAND_REQUESTS_DIR": "/home/agent/.sand-requests",
            "CLAUDE_CODE_USE_VERTEX": "1",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH": "1",
            "ANTHROPIC_VERTEX_PROJECT_ID": "acme",
            "CLOUD_ML_REGION": "us-east5",
            "ANTHROPIC_AUTH_TOKEN": "sand-vertex-claude-proj",
        })
    );
    let err = stderr(&out);
    assert!(err.contains("Registering the Vertex AI access token") && err.contains("refresh 45m"), "{err}");

    // An existing sandbox whose secret is unchanged is not registered again, ACP sessions neither.
    let out = env.ok(&["claude", "--acp"]);
    let log = env.take_log();
    assert!(starting(&log, "secret").is_empty(), "{log:#?}");
    assert!(stderr(&out).contains("already registered"), "{}", stderr(&out));
    assert!(has(&log, "bash -lc ANTHROPIC_AUTH_TOKEN=sand-vertex-claude-proj exec "), "{log:#?}");
    // The ACP banner is not read before the agent starts, so the API is not probed.
    assert!(!has(&log, "curl -sS"), "{log:#?}");

    // A changed command or host list is registered again.
    env.config(
        "[vertex]\nproject = \"acme\"\nregion = \"us-east5\"\nrefresh = \"30m\"\n\n[harness.claude]\nauth = \"vertex\"\n",
    );
    env.ok(&["claude"]);
    assert_eq!(starting(&env.take_log(), "secret"), vec![secret.replace("45m", "30m")]);

    // Without a working token source there is no point in starting the agent.
    env.config("[vertex]\nproject = \"acme\"\nregion = \"us-east5\"\n\n[harness.claude]\nauth = \"vertex\"\n");
    let out = env.command(&["claude"]).env("FAKE_SBX_SECRET_FAIL", "1").output().unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("gcloud auth application-default print-access-token")
            && !has(&env.take_log(), "run --name")
    );

    // Back to sbx's own credentials: no secret, and the Vertex settings are gone.
    env.config("");
    env.ok(&["claude"]);
    let log = env.take_log();
    assert!(starting(&log, "secret").is_empty());
    assert_eq!(log.last().unwrap(), "run --name claude-proj");
    let settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&staged).unwrap()).unwrap();
    assert_eq!(
        settings["env"],
        serde_json::json!({
            "DEBUG": "1",
            "CLAUDE_CODE_PLUGIN_DIRS": "/home/agent/.local/share/sand/mods/sand",
            "SAND_POLICY_DIR": "/home/agent/.sand-policy",
            "SAND_REQUESTS_DIR": "/home/agent/.sand-requests",
        })
    );
}

#[test]
fn templates_from_config_and_snapshots() {
    let env = Env::new();
    env.config("[templates.corp]\nimage = \"ghcr.io/acme/agent:1\"\n\n[harness.claude]\ntemplate = \"corp\"\n");
    env.ok(&["claude"]);
    assert!(has(&env.take_log(), "create --name claude-proj --template ghcr.io/acme/agent:1 claude"));

    // Snapshot a sandbox, then use the snapshot by name (local image: never pulled).
    env.ok(&["template", "save", "claude-proj", "tuned"]);
    assert!(has(&env.take_log(), "template save claude-proj sand-template-tuned:latest"));
    let listing = env.ok(&["template", "ls"]);
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(listing.contains("corp") && listing.contains("tuned") && listing.contains("snapshot"), "{listing}");

    env.ok(&["codex", "--template", "tuned"]);
    assert!(has(&env.take_log(), "create --name codex-proj --template sand-template-tuned:latest --pull never codex"));

    // Asking for a different template on an existing sandbox is a conflict that needs a recreate.
    let err = env.fails(&["codex", "--template", "corp"]);
    assert!(err.contains("template is fixed when a sandbox is created"), "{err}");

    env.ok(&["template", "rm", "tuned"]);
    assert!(has(&env.take_log(), "template rm sand-template-tuned:latest"));
}

#[test]
fn nightly_images_are_pulled_on_create() {
    let env = Env::new();
    env.config(
        "nightly = true\n\n[harness.codex]\nnightly = false\n\n[harness.pi]\nagent = \"docker.io/sbx/pi-kit\"\n",
    );
    env.ok(&["claude"]);
    assert!(has(
        &env.take_log(),
        "create --name claude-proj --template docker.io/sbx/claude-image:latest --pull always claude"
    ));

    // Starting it again reuses the sandbox: the image is only chosen at creation.
    env.ok(&["claude"]);
    assert!(!env.take_log().iter().any(|line| line.starts_with("create ")));

    // A harness can opt out, and an explicit template wins over the nightly image.
    env.ok(&["codex"]);
    assert!(has(&env.take_log(), "create --name codex-proj codex"));
    env.ok(&["claude", "--name", "pinned", "--template", "ghcr.io/acme/agent:1"]);
    assert!(has(&env.take_log(), "create --name pinned --template ghcr.io/acme/agent:1 claude"));

    // Agents without a known nightly image fall back to the sbx default.
    let out = env.ok(&["pi"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no nightly image is known"));
    assert!(has(&env.take_log(), "create --name pi-proj docker.io/sbx/pi-kit"));
}

/// A registry that answers every manifest request with the digest it was given.
struct Registry {
    url: String,
    digest: Arc<Mutex<String>>,
}

impl Registry {
    fn start(digest: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let digest = Arc::new(Mutex::new(digest.to_string()));
        let served = Arc::clone(&digest);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let digest = served.lock().unwrap().clone();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nDocker-Content-Digest: {digest}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        Self { url, digest }
    }

    fn publish(&self, digest: &str) {
        *self.digest.lock().unwrap() = digest.to_string();
    }
}

#[test]
fn a_newer_nightly_image_offers_a_recreate() {
    let env = Env::new();
    env.config("nightly = true\n");
    let registry = Registry::start("sha256:aaaa");
    let run = |args: &[&str]| {
        let out = env.command(args).env("SAND_REGISTRY", &registry.url).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "sand {args:?} failed:\n{err}");
        err
    };
    let digest_of = |name: &str| env.state(name)["image_digest"].as_str().map(str::to_string);
    let recreated = |log: &[String]| log.iter().any(|line| line.starts_with("rm ") || line.starts_with("create "));

    run(&["claude"]);
    assert!(has(&env.take_log(), "--template docker.io/sbx/claude-image:latest --pull always claude"));
    assert_eq!(digest_of("claude-proj").as_deref(), Some("sha256:aaaa"));

    let err = run(&["claude"]);
    assert!(err.contains("nightly image is up to date"), "{err}");
    assert!(!recreated(&env.take_log()));

    // A newer build: without a terminal the sandbox is kept and sand says so.
    registry.publish("sha256:bbbb");
    let err = run(&["claude"]);
    assert!(err.contains("a newer nightly image is available") && err.contains("--recreate"), "{err}");
    assert!(!recreated(&env.take_log()));

    // --resume attaches to the sandbox as it is and does not check.
    let err = run(&["claude", "--resume"]);
    assert!(!err.contains("nightly"), "{err}");

    // Keeping it is remembered for that build; a later build is offered again.
    run(&["claude", "--on-conflict", "merge"]);
    assert_eq!(env.state("claude-proj")["declined_digest"], "sha256:bbbb");
    let err = run(&["claude"]);
    assert!(err.contains("you chose to keep the sandbox"), "{err}");
    assert!(!recreated(&env.take_log()));

    run(&["claude", "--on-conflict", "replace"]);
    let log = env.take_log();
    assert!(has(&log, "rm --force claude-proj") && has(&log, "create --name claude-proj --template"), "{log:?}");
    assert_eq!(digest_of("claude-proj").as_deref(), Some("sha256:bbbb"));
    assert!(env.state("claude-proj")["declined_digest"].is_null());

    // Recreating for a newer image keeps the mounts and MCP servers the sandbox had,
    // even when this command does not name them.
    env.register_mcp(&["gitlab"]);
    run(&["claude", ".", "../other:ro", "--mcp", "gitlab", "--on-conflict", "merge"]);
    env.take_log();
    registry.publish("sha256:cccc");
    let err = run(&["claude", "--on-conflict", "replace"]);
    let log = env.take_log();
    let expected = format!("--static-mcp gitlab claude {} {}:ro", env.p(&env.proj), env.p(&env.other));
    assert!(has(&log, &expected), "{log:#?}");
    assert!(!err.contains("different setup") && starting(&log, "umount").is_empty(), "{err}\n{log:#?}");
    assert_eq!(env.state("claude-proj")["mcp"], serde_json::json!(["gitlab"]));

    // A sandbox that predates `nightly = true` is offered the switch too.
    env.config("");
    run(&["codex"]);
    env.take_log();
    env.config("nightly = true\n");
    let err = run(&["codex", "--on-conflict", "replace"]);
    assert!(err.contains("was not created from the nightly image"), "{err}");
    assert!(has(&env.take_log(), "create --name codex-proj --template docker.io/sbx/codex-image:latest --pull always"));

    // An unreachable registry never blocks a launch.
    let err = String::from_utf8_lossy(&env.ok(&["claude", "--on-conflict", "merge"]).stderr).into_owned();
    assert!(err.contains("could not check for a newer nightly image"), "{err}");
}

#[test]
fn open_config_creates_edits_and_validates() {
    let env = Env::new();
    let editor = env.root.join("editor.sh");
    let config = env.root.join("config.toml");
    let edit_with = |script: &str| {
        fs::write(&editor, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        env.command(&["--open-config"]).env("EDITOR", &editor).output().unwrap()
    };

    // No config yet: a commented starter is created, and it is valid as it is.
    let out = edit_with("echo 'default_harness = \"codex\"' >> \"$1\"");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("# nightly = true") && text.ends_with("default_harness = \"codex\"\n"), "{text}");
    assert!(env.take_log().is_empty(), "sbx is not involved");

    // A mistake is reported, and the config can still be opened to fix it.
    let out = edit_with("echo 'defualt_harness = 1' >> \"$1\"");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success() && err.contains("defualt_harness"), "{err}");
    let out = edit_with("grep -v defualt \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn install_acp_writes_the_client_config() {
    let env = Env::new();
    let zed = env.home.join(".config/zed/settings.json");
    fs::create_dir_all(zed.parent().unwrap()).unwrap();
    fs::write(&zed, "// zed settings\n{\n  \"theme\": \"One Dark\"\n}\n").unwrap();

    env.ok(&["codex", "--install-acp", "zed"]);
    let text = fs::read_to_string(&zed).unwrap();
    assert!(text.contains("// zed settings") && text.contains("\"theme\": \"One Dark\""), "{text}");
    assert!(text.contains("\"sand (codex)\"") && text.contains("\"--acp\""), "{text}");
    assert!(zed.with_file_name("settings.json.sand-backup").exists());
    assert!(env.take_log().is_empty(), "installing an editor never touches sbx");

    env.ok(&["--install-acp", "jetbrains"]);
    let acp: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.home.join(".jetbrains/acp.json")).unwrap()).unwrap();
    assert_eq!(acp["agent_servers"]["sand"]["args"], serde_json::json!(["--acp"]));
    assert!(acp["agent_servers"]["sand"]["command"].as_str().unwrap().ends_with("sand"));

    env.ok(&["claude", "--install-acp", "tern"]);
    let tern: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.home.join("tern/settings.json")).unwrap()).unwrap();
    assert_eq!(tern["agents"]["custom"][0]["id"], "sand-claude");
    assert_eq!(tern["agents"]["custom"][0]["args"], serde_json::json!(["claude", "--acp"]));
    assert_eq!(tern["agents"]["own_tools"], serde_json::json!(["sand-claude"]));
    assert!(env.take_log().is_empty(), "installing never touches sbx");
}

#[test]
fn sandboxes_removed_or_created_outside_sand_are_handled() {
    let env = Env::new();
    env.ok(&["codex"]);
    env.take_log();
    // Removed behind sand's back: recreated, with history kept.
    fs::remove_file(env.fake.join("sandboxes/codex-proj")).unwrap();
    let out = env.ok(&["codex"]);
    assert!(stderr(&out).contains("removed outside sand"));
    assert_eq!(starting(&env.take_log(), "create").len(), 1);

    // Created by plain sbx: sand asks before touching it.
    fs::write(env.fake.join("sandboxes/claude-proj"), "").unwrap();
    let err = env.fails(&["claude"]);
    assert!(err.contains("was not created by sand") && err.contains("--on-conflict"), "{err}");
    env.ok(&["claude", ".", "../other", "--on-conflict", "merge"]);
    let log = env.take_log();
    assert!(starting(&log, "create").is_empty());
    assert!(has(&log, &format!("mount claude-proj {}", env.p(&env.other))));
}

#[test]
fn agent_exit_code_and_unknown_harnesses_pass_through() {
    let env = Env::new();
    let out = env.command(&["codex"]).env("FAKE_SBX_RUN_EXIT", "7").output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    env.take_log();

    let out = env.ok(&["gemini"]);
    let log = env.take_log();
    assert!(has(&log, "create --name gemini-proj gemini"));
    assert!(!log.iter().any(|l| l.contains("apply.sh")), "no config profile, nothing copied");
    assert!(stderr(&out).contains("not a harness sand has a profile for"));
}

#[test]
fn status_shows_the_banner_for_an_existing_sandbox() {
    let env = Env::new();
    assert!(env.fails(&["status", "codex"]).contains("no sandbox named 'codex-proj'"));
    env.ok(&["codex", ".", "../other:ro"]);
    let out = env.command(&["status", "codex"]).env("FAKE_SBX_API_CODE", "502").output().unwrap();
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("codex-proj") && err.contains("[primary workspace]") && err.contains("(ro)"), "{err}");
    assert!(err.contains("api.openai.com unreachable"), "{err}");
}

#[test]
fn describe_prints_the_record_and_the_sbx_state() {
    let env = Env::new();
    assert!(env.fails(&["describe", "codex"]).contains("no sandbox named 'codex-proj'"));
    env.ok(&["codex", ".", "../other:ro", "--no-attach"]);
    fs::write(env.fake.join("ls.json"), r#"[{"name": "codex-proj", "status": "running"}]"#).unwrap();
    env.take_log();

    let out = env.ok(&["describe", "codex"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("codex-proj") && text.contains("running") && text.contains("none attached"), "{text}");
    assert!(text.contains("[primary workspace]") && text.contains(&format!("{} (ro)", env.p(&env.other))), "{text}");
    assert!(text.contains("history/.codex_sessions → /home/agent/.codex/sessions (rw)"), "{text}");
    // Nothing runs inside the sandbox.
    assert!(!has(&env.take_log(), "exec"));

    let out = env.ok(&["describe", "--name", "codex-proj", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["sbx_state"], "running");
    assert_eq!(json["record"]["harness"], "codex");
    assert_eq!(json["record"]["mounts"]["extras"][0]["mode"], "ro");

    // A sandbox sand did not create is described from what sbx knows.
    fs::write(env.fake.join("sandboxes/hand-made"), "").unwrap();
    fs::remove_file(env.fake.join("ls.json")).unwrap();
    let out = env.ok(&["describe", "--name", "hand-made"]);
    assert!(stderr(&out).contains("was not created by sand"), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("unknown"));
    let out = env.ok(&["describe", "--name", "hand-made", "--json"]);
    assert!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["record"].is_null());
}

#[test]
fn allow_lists_blocked_hosts_and_adds_sandbox_scoped_rules() {
    let env = Env::new();
    assert!(env.fails(&["allow", "codex"]).contains("no sandbox named 'codex-proj'"));
    env.ok(&["codex"]);
    env.take_log();
    assert!(stderr(&env.ok(&["allow", "codex"])).contains("nothing is blocked"));
    env.take_log();

    fs::write(
        env.fake.join("policy.json"),
        r#"{"blocked": [
              {"host": "zed.dev:443", "sandbox_id": "codex-proj", "rule": "no applicable policies for op(x)",
               "last_seen": "2026-10-12T12:51:22.4219199+02:00", "count": 1, "reason": "No matching allow rule (default deny)"},
              {"host": "gw.internal", "sandbox_id": "codex-proj", "rule": "<dns proxy policy>",
               "last_seen": "2026-10-12T12:51:16.790945+02:00", "count": 2, "reason": "DNS lookup blocked by proxy policy"},
              {"host": "other.dev:443", "sandbox_id": "claude-proj", "rule": "no applicable policies for op(x)",
               "last_seen": "2026-10-12T12:51:22+02:00", "count": 1, "reason": "No matching allow rule (default deny)"}],
            "allowed": []}"#,
    )
    .unwrap();
    let out = env.ok(&["allow", "codex", "--list"]);
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(listed.contains("zed.dev:443  (1 request, last 2026-10-12 12:51:22)"), "{listed}");
    assert!(listed.contains("gw.internal") && listed.contains("[DNS lookup blocked by proxy policy]"), "{listed}");
    assert!(!listed.contains("other.dev"), "{listed}");
    assert_eq!(starting(&env.take_log(), "policy"), ["policy log codex-proj --json"]);

    // Choosing needs a terminal; the tests have none.
    let err = env.fails(&["allow", "codex"]);
    assert!(err.contains("has 2 blocked host(s)") && err.contains("pass --host"), "{err}");
    assert!(!has(&env.take_log(), "policy allow"));

    let err = stderr(&env.ok(&["allow", "codex", "--host", "zed.dev:443,*.example.com", "-v"]));
    assert!(err.contains("$ sbx policy allow network --sandbox codex-proj zed.dev:443,*.example.com"), "{err}");
    assert_eq!(
        starting(&env.take_log(), "policy"),
        ["policy allow network --sandbox codex-proj zed.dev:443,*.example.com"]
    );

    env.ok(&["allow", "--name", "codex-proj", "--host", "zed.dev", "--global"]);
    assert_eq!(starting(&env.take_log(), "policy"), ["policy allow network zed.dev"]);

    assert!(env.fails(&["allow", "codex", "--host", "--sandbox"]).contains("error"));
    let out = env.command(&["allow", "codex", "--host", "zed.dev"]).env("FAKE_SBX_ALLOW_FAIL", "1").output().unwrap();
    let err = stderr(&out);
    assert!(
        !out.status.success() && err.contains("`sbx policy allow network --sandbox codex-proj zed.dev` failed"),
        "{err}"
    );
    assert!(err.contains("fake sbx: allow refused"), "a failing command's output is shown:\n{err}");
}

#[test]
fn verbose_echoes_sbx_commands_and_their_output() {
    let env = Env::new();
    for flag in ["--verbose", "-v"] {
        let err = stderr(&env.ok(&["codex", flag, "--recreate"]));
        assert!(err.contains("$ sbx create --name codex-proj"), "commands are echoed:\n{err}");
        assert!(err.contains("created codex-proj"), "sbx output is shown:\n{err}");
    }
}

#[test]
fn apply_script_installs_the_staged_config_and_prunes_stale_files() {
    let env = Env::new();
    // Point the "sandbox home" at a real directory so the generated script can run here.
    let sandbox_home = env.root.join("sandbox-home");
    fs::create_dir_all(&sandbox_home).unwrap();
    env.config(&format!("sandbox_home = \"{}\"\n", env.p(&sandbox_home)));
    let claude = env.home.join(".claude");
    fs::create_dir_all(claude.join("commands/review")).unwrap();
    fs::write(claude.join("CLAUDE.md"), "v1").unwrap();
    fs::write(claude.join("commands/review/go.md"), "review").unwrap();

    let runtime = env.sandbox_dir("claude-proj").join("runtime");
    let apply = |env: &Env| {
        env.ok(&["claude"]);
        // Stand in for sbx: expose the runtime dir at ~/.sand and run the script.
        let link = sandbox_home.join(".sand");
        if !link.exists() {
            std::os::unix::fs::symlink(&runtime, &link).unwrap();
        }
        let out = Command::new("sh").arg(link.join("apply.sh")).output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
    };

    apply(&env);
    let installed = sandbox_home.join(".claude");
    assert_eq!(fs::read_to_string(installed.join("CLAUDE.md")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(installed.join("commands/review/go.md")).unwrap(), "review");
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(installed.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(settings["statusLine"]["command"], format!("sh {}/.sand/statusline.sh", env.p(&sandbox_home)));
    // Something the agent created itself inside the sandbox must survive later syncs.
    fs::write(installed.join("commands/own.md"), "made in the sandbox").unwrap();

    // The host changes: one file updated, one command deleted.
    fs::write(claude.join("CLAUDE.md"), "v2 with more text").unwrap();
    fs::remove_dir_all(claude.join("commands/review")).unwrap();
    apply(&env);
    assert_eq!(fs::read_to_string(installed.join("CLAUDE.md")).unwrap(), "v2 with more text");
    assert!(!installed.join("commands/review/go.md").exists(), "deleted on the host, so removed in the sandbox");
    assert_eq!(fs::read_to_string(installed.join("commands/own.md")).unwrap(), "made in the sandbox");
}
