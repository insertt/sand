//! End-to-end tests: the real `sand` binary driving a fake `sbx` that records
//! every command. They check the sequence of sbx calls sand makes.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
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

    /// sbx invocations recorded since the last call.
    fn take_log(&self) -> Vec<String> {
        let path = self.fake.join("log");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(&path);
        text.lines().map(String::from).collect()
    }

    fn register_mcp(&self, names: &[&str]) {
        fs::write(self.fake.join("mcp"), names.join("\n") + "\n").unwrap();
    }

    fn config(&self, text: &str) {
        fs::write(self.root.join("config.toml"), text).unwrap();
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
    // Configuration is applied inside the sandbox, then the agent starts.
    assert!(has(&log, "exec codex-proj sh /home/agent/.sand/apply.sh"));
    assert_eq!(log.last().unwrap(), "run --name codex-proj");

    let err = stderr(&out);
    for step in [
        "Looking for an existing sandbox",
        "Creating sandbox 'codex-proj'",
        "Starting the sandbox",
        "Mounting sand's status and history directories",
        "Checking agent API and MCP servers",
        "Starting codex (bypass permissions)",
        "Session ended",
    ] {
        assert!(err.contains(step), "missing step '{step}' in:\n{err}");
    }
    assert!(err.contains("$ sbx create --name codex-proj"), "commands are echoed:\n{err}");
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
    assert_eq!(fs::read_to_string(staged.join(".agents/skills/pdf/SKILL.md")).unwrap(), "pdf skill");
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
    assert!(has(&log, "exec codex-proj cat /home/agent/.codex/config.toml"));
    assert!(has(&log, "exec codex-proj sh /home/agent/.sand/apply.sh"));
    assert!(stderr(&out).contains("credentials are never copied"));
    let apply = fs::read_to_string(env.sandbox_dir("codex-proj").join("runtime/apply.sh")).unwrap();
    assert!(apply.contains("cp -RL \"$SRC/home/.\" \"$DEST/\""));
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
fn install_editor_writes_the_editor_config() {
    let env = Env::new();
    let zed = env.home.join(".config/zed/settings.json");
    fs::create_dir_all(zed.parent().unwrap()).unwrap();
    fs::write(&zed, "// zed settings\n{\n  \"theme\": \"One Dark\"\n}\n").unwrap();

    env.ok(&["codex", "--install-editor", "zed"]);
    let text = fs::read_to_string(&zed).unwrap();
    assert!(text.contains("// zed settings") && text.contains("\"theme\": \"One Dark\""), "{text}");
    assert!(text.contains("\"sand (codex)\"") && text.contains("\"--acp\""), "{text}");
    assert!(zed.with_file_name("settings.json.sand-backup").exists());
    assert!(env.take_log().is_empty(), "installing an editor never touches sbx");

    env.ok(&["--install-editor", "jetbrains"]);
    let acp: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.home.join(".jetbrains/acp.json")).unwrap()).unwrap();
    assert_eq!(acp["agent_servers"]["sand"]["args"], serde_json::json!(["--acp"]));
    assert!(acp["agent_servers"]["sand"]["command"].as_str().unwrap().ends_with("sand"));
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
fn allow_lists_blocked_hosts_and_adds_sandbox_scoped_rules() {
    let env = Env::new();
    assert!(env.fails(&["allow", "codex"]).contains("no sandbox named 'codex-proj'"));
    env.ok(&["codex"]);
    env.take_log();
    assert!(stderr(&env.ok(&["allow", "codex"])).contains("nothing is blocked"));
    env.take_log();

    fs::write(
        env.fake.join("policy.json"),
        r#"{"blocked_hosts": [
              {"host": "zed.dev:443", "vm_name": "codex-proj", "rule": "no applicable policies for op(x)",
               "last_seen": "2026-10-12T12:51:22.4219199+02:00", "count_since": 1, "reason": "No matching allow rule (default deny)"},
              {"host": "gw.internal", "vm_name": "codex-proj", "rule": "<dns proxy policy>",
               "last_seen": "2026-10-12T12:51:16.790945+02:00", "count_since": 2, "reason": "DNS lookup blocked by proxy policy"},
              {"host": "other.dev:443", "vm_name": "claude-proj", "rule": "no applicable policies for op(x)",
               "last_seen": "2026-10-12T12:51:22+02:00", "count_since": 1, "reason": "No matching allow rule (default deny)"}],
            "allowed_hosts": []}"#,
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
    assert!(err.contains("not offered: gw.internal") && err.contains("pass --host"), "{err}");
    assert!(!has(&env.take_log(), "policy allow"));

    let err = stderr(&env.ok(&["allow", "codex", "--host", "zed.dev:443,*.example.com"]));
    assert!(err.contains("$ sbx policy allow network --sandbox codex-proj zed.dev:443,*.example.com"), "{err}");
    assert_eq!(
        starting(&env.take_log(), "policy"),
        ["policy allow network --sandbox codex-proj zed.dev:443,*.example.com"]
    );

    env.ok(&["allow", "--name", "codex-proj", "--host", "zed.dev", "--global"]);
    assert_eq!(starting(&env.take_log(), "policy"), ["policy allow network zed.dev"]);

    assert!(env.fails(&["allow", "codex", "--host", "--sandbox"]).contains("error"));
    let out = env.command(&["allow", "codex", "--host", "zed.dev"]).env("FAKE_SBX_ALLOW_FAIL", "1").output().unwrap();
    assert!(
        !out.status.success()
            && stderr(&out).contains("`sbx policy allow network --sandbox codex-proj zed.dev` failed")
    );
}

#[test]
fn apply_script_installs_the_staged_config_and_prunes_stale_files() {
    let env = Env::new();
    // Point the "sandbox home" at a real directory so the generated script can run here.
    let sandbox_home = env.root.join("sandbox-home");
    fs::create_dir_all(&sandbox_home).unwrap();
    env.config(&format!("sandbox_home = \"{}\"\n", env.p(&sandbox_home)));
    let claude = env.home.join(".claude");
    fs::create_dir_all(claude.join("skills/review")).unwrap();
    fs::write(claude.join("CLAUDE.md"), "v1").unwrap();
    fs::write(claude.join("skills/review/SKILL.md"), "review").unwrap();

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
    assert_eq!(fs::read_to_string(installed.join("skills/review/SKILL.md")).unwrap(), "review");
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(installed.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(settings["statusLine"]["command"], format!("sh {}/.sand/statusline.sh", env.p(&sandbox_home)));
    // Something the agent created itself inside the sandbox must survive later syncs.
    fs::write(installed.join("skills/own.md"), "made in the sandbox").unwrap();

    // The host changes: one file updated, one skill deleted.
    fs::write(claude.join("CLAUDE.md"), "v2 with more text").unwrap();
    fs::remove_dir_all(claude.join("skills/review")).unwrap();
    apply(&env);
    assert_eq!(fs::read_to_string(installed.join("CLAUDE.md")).unwrap(), "v2 with more text");
    assert!(!installed.join("skills/review/SKILL.md").exists(), "deleted on the host, so removed in the sandbox");
    assert_eq!(fs::read_to_string(installed.join("skills/own.md")).unwrap(), "made in the sandbox");
}
