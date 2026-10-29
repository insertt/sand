//! End-to-end tests: the real `sand` binary against a real, signed-in `sbx`.
//! They create real sandboxes, so they are ignored by default; CI runs them with
//!
//!     cargo test --test e2e -- --ignored --test-threads=1
//!
//! Every sbx call goes through `--app-name` (`$SAND_E2E_APP_NAME`, default
//! `sand-e2e`), so the tests get their own daemon, sandboxes, policy and login
//! and never touch your everyday ones. Sign that instance in once with
//! `sbx --app-name sand-e2e login`. `$SAND_E2E_SBX` names the sbx binary.
//!
//! What a real sbx cannot be made to do on demand (fail, ask for a login, run
//! an agent session without a terminal) is tested against a fake in fake_sbx.rs.
#![cfg(unix)]

use std::cell::RefCell;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn sbx_bin() -> String {
    env::var("SAND_E2E_SBX").unwrap_or_else(|_| "sbx".into())
}

fn app_name() -> String {
    env::var("SAND_E2E_APP_NAME").unwrap_or_else(|_| "sand-e2e".into())
}

/// Runs the real sbx, scoped to the test instance.
fn sbx(args: &[&str]) -> Output {
    Command::new(sbx_bin()).arg("--app-name").arg(app_name()).args(args).stdin(Stdio::null()).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn stderr(out: &Output) -> String {
    text(&out.stderr)
}

/// A project directory of its own, with sand data and a home of its own. The
/// project directory is named after `token`, which is unique to the test, and
/// so are the default sandbox names (`claude-<token>`). Everything the test
/// creates in sbx is removed when it is dropped.
struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    proj: PathBuf,
    other: PathBuf,
    third: PathBuf,
    token: String,
    /// sbx commands that undo what the test created outside its sandboxes.
    cleanup: RefCell<Vec<Vec<String>>>,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let token = format!("e2e-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
        let env = Self {
            home: root.join("home"),
            data: root.join("data"),
            proj: root.join("work").join(&token),
            other: root.join("work/other"),
            third: root.join("work/third"),
            root,
            token,
            cleanup: RefCell::new(Vec::new()),
            _tmp: tmp,
        };
        for dir in [&env.home, &env.proj, &env.other, &env.third] {
            fs::create_dir_all(dir).unwrap();
        }
        // sand runs with a home of its own, so that it copies no harness
        // configuration from yours; sbx keeps the real one, where its login is.
        let real_home = env::var("HOME").unwrap();
        let wrapper = env.root.join("sbx");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nHOME='{real_home}' exec '{}' --app-name '{}' \"$@\"\n",
                sbx_bin().replace('\'', "'\\''"),
                app_name()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        env
    }

    /// The default name of `harness`'s sandbox for the project directory.
    fn default_name(&self, harness: &str) -> String {
        format!("{harness}-{}", self.token)
    }

    /// A sandbox name for `--name`, removed with the others.
    fn named(&self, suffix: &str) -> String {
        format!("{}-{suffix}", self.token)
    }

    /// Something sbx keeps outside the test's sandboxes, removed when the test ends.
    fn on_drop(&self, args: &[&str]) {
        self.cleanup.borrow_mut().push(args.iter().map(|a| a.to_string()).collect());
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
            .env("TERN_CONFIG_DIR", self.home.join("tern"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("SAND_CONFIRM")
            .stdin(Stdio::null());
        cmd
    }

    fn sand(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs sand with `-v`, so that a failure shows every sbx command and its output.
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.sand(&[args, &["-v"]].concat());
        assert!(out.status.success(), "sand {args:?} failed:\n{}", stderr(&out));
        out
    }

    /// Runs sand, asserts it failed, and returns stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.sand(args);
        assert!(!out.status.success(), "sand {args:?} unexpectedly succeeded:\n{}", stderr(&out));
        stderr(&out)
    }

    fn config(&self, text: &str) {
        fs::write(self.root.join("config.toml"), text).unwrap();
    }

    /// Runs a shell script inside a sandbox.
    fn exec(&self, sandbox: &str, script: &str) -> Output {
        sbx(&["exec", sandbox, "sh", "-c", script])
    }

    fn exec_ok(&self, sandbox: &str, script: &str) -> String {
        let out = self.exec(sandbox, script);
        assert!(out.status.success(), "`{script}` failed in {sandbox}:\n{}{}", text(&out.stdout), stderr(&out));
        text(&out.stdout)
    }

    fn exists_in(&self, sandbox: &str, path: &str) -> bool {
        self.exec(sandbox, &format!("test -e '{path}'")).status.success()
    }

    fn writable_in(&self, sandbox: &str, dir: &Path) -> bool {
        let probe = dir.join(format!("probe-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        self.exec(sandbox, &format!("touch '{}'", probe.display())).status.success()
    }

    /// What sand recorded about a sandbox.
    fn state(&self, name: &str) -> serde_json::Value {
        let path = self.data.join("sandboxes").join(name).join("state.json");
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn listed(&self, name: &str) -> bool {
        let out = sbx(&["ls", "-q"]);
        assert!(out.status.success(), "sbx ls failed:\n{}", stderr(&out));
        text(&out.stdout).lines().any(|line| line.trim() == name)
    }

    fn p(&self, path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Sandboxes first: a template or MCP server in use may not be removable.
        let out = sbx(&["ls", "-q"]);
        for name in text(&out.stdout).lines().map(str::trim).filter(|n| n.contains(&self.token)) {
            let _ = sbx(&["rm", "--force", name]);
        }
        for args in self.cleanup.borrow().iter() {
            let _ = sbx(&args.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn a_sandbox_is_created_set_up_and_reused() {
    let env = Env::new();
    let name = env.default_name("claude");
    assert!(env.fails(&["status", "claude"]).contains(&format!("no sandbox named '{name}'")));
    fs::write(env.proj.join("marker"), "from the host").unwrap();
    fs::write(env.other.join("shared"), "shared").unwrap();
    let claude = env.home.join(".claude");
    fs::create_dir_all(claude.join("commands/review")).unwrap();
    fs::write(claude.join("settings.json"), r#"{"model": "sonnet", "permissions": {"defaultMode": "plan"}}"#).unwrap();
    fs::write(claude.join("CLAUDE.md"), "v1").unwrap();
    fs::write(claude.join("commands/review/go.md"), "review").unwrap();
    fs::write(claude.join(".credentials.json"), "secret").unwrap();

    let out = env.ok(&["claude", ".", "../other:ro", "--no-attach"]);
    let err = stderr(&out);
    // -v echoes the sbx commands.
    assert!(err.contains(&format!("$ sbx create --name {name}")), "{err}");
    assert!(err.contains(&format!("sandbox '{name}' is ready")), "{err}");
    assert!(env.listed(&name));

    // Mounts are at the host paths, the extra one read-only.
    let (proj, other) = (env.p(&env.proj), env.p(&env.other));
    assert_eq!(env.exec_ok(&name, &format!("cat '{proj}/marker'")), "from the host");
    env.exec_ok(&name, &format!("echo back > '{proj}/from-sandbox'"));
    assert_eq!(fs::read_to_string(env.proj.join("from-sandbox")).unwrap(), "back\n");
    assert_eq!(env.exec_ok(&name, &format!("cat '{other}/shared'")), "shared");
    assert!(!env.writable_in(&name, &env.other), "the :ro mount is writable");

    // The host configuration is installed by the apply script, without credentials.
    assert!(env.exists_in(&name, "/home/agent/.sand/apply.sh"));
    let settings: serde_json::Value =
        serde_json::from_str(&env.exec_ok(&name, "cat /home/agent/.claude/settings.json")).unwrap();
    assert_eq!(settings["model"], "sonnet");
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(settings["permissions"]["additionalDirectories"], serde_json::json!([other]));
    assert_eq!(settings["statusLine"]["command"], "sh /home/agent/.sand/statusline.sh");
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.claude/CLAUDE.md"), "v1");
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.claude/commands/review/go.md"), "review");
    assert!(!env.exists_in(&name, "/home/agent/.claude/.credentials.json"), "credentials must stay on the host");
    // Something the agent creates itself must survive later syncs.
    env.exec_ok(&name, "echo mine > /home/agent/.claude/commands/own.md");

    // status and describe read the sandbox back.
    let err = stderr(&env.ok(&["status", "claude"]));
    assert!(err.contains(&name) && err.contains("[primary workspace]") && err.contains("(ro)"), "{err}");
    let out = env.ok(&["describe", "claude"]);
    let described = text(&out.stdout);
    assert!(described.contains(&name) && described.contains("none attached"), "{described}");
    assert!(described.contains(&format!("{other} (ro)")), "{described}");
    let out = env.ok(&["describe", "--name", &name, "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["sbx_state"].is_string(), "{json:#}");
    assert_eq!(json["record"]["harness"], "claude");
    assert_eq!(json["record"]["mounts"]["extras"][0]["mode"], "ro");

    // The host configuration changes: the same command reuses the sandbox and syncs it.
    fs::write(claude.join("CLAUDE.md"), "v2 with more text").unwrap();
    fs::remove_dir_all(claude.join("commands/review")).unwrap();
    let created = env.state(&name)["created_at"].clone();
    let err = stderr(&env.ok(&["claude", ".", "../other:ro", "--no-attach"]));
    assert!(err.contains("existing sandbox matches the requested setup") && !err.contains("$ sbx create"), "{err}");
    assert_eq!(env.state(&name)["created_at"], created);
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.claude/CLAUDE.md"), "v2 with more text");
    assert!(!env.exists_in(&name, "/home/agent/.claude/commands/review/go.md"), "deleted on the host");
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.claude/commands/own.md"), "mine\n");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn changed_mounts_need_a_decision() {
    let env = Env::new();
    let name = env.default_name("claude");
    env.ok(&["claude", "--no-attach"]);

    // A read-only mount added later.
    env.ok(&["claude", ".", "../other:ro", "--on-conflict", "merge", "--no-attach"]);
    assert!(env.exists_in(&name, &env.p(&env.other)));
    assert!(!env.writable_in(&name, &env.other), "the :ro mount is writable");

    // No terminal and no --on-conflict: refuse rather than guess.
    let err = env.fails(&["claude", ".", "../third", "--no-attach"]);
    assert!(err.contains("exists with a different setup") && err.contains("--on-conflict"), "{err}");
    assert!(err.contains("current mounts") && err.contains("requested mounts"), "{err}");
    // Abort changes nothing.
    env.fails(&["claude", ".", "../third", "--on-conflict", "abort", "--no-attach"]);
    assert!(!env.exists_in(&name, &env.p(&env.third)));

    // Merge keeps ../other and adds ../third.
    env.ok(&["claude", ".", "../third", "--on-conflict", "merge", "--no-attach"]);
    assert!(env.writable_in(&name, &env.third) && env.exists_in(&name, &env.p(&env.other)));

    // Replace makes it exactly "." + "../other", now read-write, in place.
    env.exec_ok(&name, "touch /home/agent/same-sandbox");
    env.ok(&["claude", ".", "../other", "--on-conflict", "replace", "--no-attach"]);
    assert!(!env.exists_in(&name, &env.p(&env.third)), "../third is still mounted");
    assert!(env.writable_in(&name, &env.other), "../other was not remounted read-write");
    assert!(env.exists_in(&name, "/home/agent/same-sandbox"), "the sandbox was recreated");

    // The new state sticks.
    let err = stderr(&env.ok(&["claude", ".", "../other", "--no-attach"]));
    assert!(err.contains("existing sandbox matches the requested setup"), "{err}");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn resume_adds_the_mounts_on_the_command_line() {
    let env = Env::new();
    let name = env.default_name("claude");
    env.ok(&["claude", "--no-attach"]);

    env.ok(&["claude", "--resume", ".", "../other:ro", "--no-attach"]);
    assert!(env.exists_in(&name, &env.p(&env.other)));
    assert!(!env.writable_in(&name, &env.other), "the :ro mount is writable");
    assert_eq!(env.state(&name)["mounts"]["extras"][0]["path"], env.p(&env.other).as_str());

    // Mounts it already has are left alone, and nothing is removed.
    env.ok(&["claude", "--resume", ".", "--no-attach"]);
    assert!(env.exists_in(&name, &env.p(&env.other)));

    // The first directory picks the sandbox, so naming only the new one misses it.
    let err = env.fails(&["claude", "--resume", "../other", "--no-attach"]);
    assert!(err.contains("nothing to resume") && err.contains("sand mount"), "{err}");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn mounts_are_added_and_removed_in_place() {
    let env = Env::new();
    let name = env.default_name("claude");
    assert!(env.fails(&["mount", "claude", "../other"]).contains(&format!("no sandbox named '{name}'")));
    fs::write(env.other.join("shared"), "shared").unwrap();
    env.ok(&["claude", "--no-attach"]);
    let other = env.p(&env.other);
    assert!(!env.exists_in(&name, &format!("{other}/shared")));

    env.ok(&["mount", "claude", "../other:ro"]);
    assert_eq!(env.exec_ok(&name, &format!("cat '{other}/shared'")), "shared");
    assert!(!env.writable_in(&name, &env.other), "the :ro mount is writable");
    assert_eq!(env.state(&name)["mounts"]["extras"][0]["mode"], "ro");

    // A different mode remounts it.
    env.ok(&["mount", "claude", "../other"]);
    assert!(env.writable_in(&name, &env.other));

    assert!(env.fails(&["umount", "claude", "."]).contains("primary workspace"));
    env.ok(&["umount", "claude", "../other"]);
    assert!(!env.exists_in(&name, &format!("{other}/shared")), "still mounted");
    assert!(env.state(&name)["mounts"]["extras"].as_array().unwrap().is_empty());
    assert!(stderr(&env.ok(&["umount", "claude", "../other"])).contains("is not mounted"));

    // The next launch with the original command line sees a sandbox that matches it.
    let err = stderr(&env.ok(&["claude", "--no-attach"]));
    assert!(err.contains("existing sandbox matches the requested setup"), "{err}");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn mounts_that_reach_sands_own_data_are_warned_about() {
    let env = Env::new();
    env.ok(&["claude", "--no-attach"]);
    let data = env.p(&env.data);
    let err = stderr(&env.ok(&["claude", ".", &format!("{data}:ro"), "--on-conflict", "merge", "--no-attach"]));
    assert!(
        err.contains("contains sand's per-sandbox data") && err.contains("read the history of every sandbox"),
        "{err}"
    );
    let err = stderr(&env.ok(&["mount", "claude", &data]));
    assert!(err.contains("file network prompts in their name"), "{err}");
    assert!(!stderr(&env.ok(&["claude", "--resume", "--no-attach"])).contains("per-sandbox data"));
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn recreate_replaces_the_sandbox_and_keeps_history() {
    let env = Env::new();
    let name = env.named("named");
    assert!(env.fails(&["claude", "--name", "bad name", "--no-attach"]).contains("invalid sandbox name"));
    env.ok(&["claude", "--name", &name, "--no-attach"]);
    assert!(env.listed(&name));
    env.exec_ok(
        &name,
        "mkdir -p /home/agent/.claude/projects && echo conversation > /home/agent/.claude/projects/e2e.jsonl",
    );
    // A file outside sand's mounts tells the old sandbox from the new one.
    env.exec_ok(&name, "touch /home/agent/old-sandbox");

    // From another directory, --name finds it again.
    let out =
        env.command(&["claude", "--resume", "--name", &name, "--no-attach"]).current_dir(&env.other).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.exists_in(&name, "/home/agent/old-sandbox"));

    let err = stderr(&env.ok(&["claude", "--name", &name, "--recreate", "--no-attach"]));
    assert!(err.contains("history is preserved"), "{err}");
    assert!(!env.exists_in(&name, "/home/agent/old-sandbox"), "the sandbox was not recreated");
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.claude/projects/e2e.jsonl"), "conversation\n");

    env.ok(&["claude", "--name", &name, "--recreate", "--no-preserve-history", "--no-attach"]);
    assert!(!env.exists_in(&name, "/home/agent/.claude/projects/e2e.jsonl"), "history survived");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn sandboxes_removed_or_created_outside_sand_are_handled() {
    let env = Env::new();
    let name = env.default_name("claude");
    env.ok(&["claude", "--no-attach"]);

    // Removed behind sand's back: recreated.
    assert!(sbx(&["rm", "--force", &name]).status.success());
    let err = stderr(&env.ok(&["claude", "--no-attach"]));
    assert!(err.contains("removed outside sand") && err.contains("$ sbx create"), "{err}");
    assert!(env.listed(&name));

    // Created by plain sbx: sand asks before touching it.
    let hand_made = env.named("hand-made");
    let out = sbx(&["create", "--name", &hand_made, "claude", &env.p(&env.proj)]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = env.ok(&["describe", "--name", &hand_made]);
    assert!(stderr(&out).contains("was not created by sand"), "{}", stderr(&out));
    let out = env.ok(&["describe", "--name", &hand_made, "--json"]);
    assert!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["record"].is_null());

    let err = env.fails(&["claude", "--name", &hand_made, "--no-attach"]);
    assert!(err.contains("was not created by sand") && err.contains("--on-conflict"), "{err}");
    env.exec_ok(&hand_made, "touch /home/agent/same-sandbox");
    env.ok(&["claude", ".", "../other", "--name", &hand_made, "--on-conflict", "merge", "--no-attach"]);
    assert!(env.exists_in(&hand_made, "/home/agent/same-sandbox"), "the sandbox was recreated");
    assert!(env.writable_in(&hand_made, &env.other));
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn scratch_is_shared_between_harnesses() {
    let env = Env::new();
    // Scratch sandboxes have fixed names; leftovers from an earlier run would be reused.
    for name in ["scratch-claude", "scratch-codex"] {
        let _ = sbx(&["rm", "--force", name]);
        env.on_drop(&["rm", "--force", name]);
    }
    env.ok(&["claude", "--scratch", "--no-attach"]);
    let scratch = env.data.join("scratch");
    assert!(scratch.is_dir());
    env.exec_ok("scratch-claude", &format!("echo shared > '{}/note'", scratch.display()));

    // Another harness gets its own sandbox on the same scratch directory.
    env.ok(&["codex", "--scratch", "--no-attach"]);
    assert_eq!(env.exec_ok("scratch-codex", &format!("cat '{}/note'", scratch.display())), "shared\n");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn host_codex_configuration_is_merged_without_credentials() {
    let env = Env::new();
    let name = env.default_name("codex");
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

    let out = env.ok(&["codex", "--no-attach"]);
    assert!(stderr(&out).contains("credentials are never copied"), "{}", stderr(&out));
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.codex/AGENTS.md"), "global instructions");
    assert_eq!(env.exec_ok(&name, "cat /home/agent/.codex/prompts/review.md"), "review prompt");
    assert!(!env.exists_in(&name, "/home/agent/.codex/auth.json"), "credentials must stay on the host");

    let merged: toml::Table = toml::from_str(&env.exec_ok(&name, "cat /home/agent/.codex/config.toml")).unwrap();
    assert_eq!(merged["model"].as_str(), Some("o3"));
    assert_eq!(merged["approval_policy"].as_str(), Some("never"), "bypass wins over the host setting");
    assert_eq!(merged["sandbox_mode"].as_str(), Some("danger-full-access"));
    let servers = merged.get("mcp_servers").and_then(|s| s.as_table());
    assert!(!servers.is_some_and(|s| s.contains_key("local")), "host MCP servers are not copied: {merged:#?}");
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn mcp_servers_are_static_at_creation_and_loaded_later() {
    let env = Env::new();
    let name = env.default_name("claude");
    let [a, b, c] = ["a", "b", "c"].map(|s| env.named(s));
    for server in [&a, &b, &c] {
        env.on_drop(&["mcp", "rm", server]);
    }
    // Servers defined in config.toml are registered with sbx when first used.
    env.config(&format!(
        "[mcp.{a}]\nurl = \"https://{a}.example.com/mcp\"\n\n[mcp.{b}]\nurl = \"https://{b}.example.com/mcp\"\n\n\
         [mcp.{c}]\nurl = \"https://{c}.example.com/mcp\"\n"
    ));
    let missing = env.named("missing");
    let err = env.fails(&["claude", "--mcp", &format!("{a},{missing}"), "--no-attach"]);
    assert!(err.contains(&format!("'{missing}' is not registered")), "{err}");
    assert!(!env.listed(&name));

    env.ok(&["claude", "--mcp", &format!("{a},{b}"), "--no-attach"]);
    let registered = text(&sbx(&["mcp", "ls", "-q"]).stdout);
    assert!(registered.contains(&a) && registered.contains(&b), "{registered}");
    assert_eq!(env.state(&name)["mcp"], serde_json::json!([a, b]));

    // Adding a server works live.
    env.exec_ok(&name, "touch /home/agent/same-sandbox");
    let err = stderr(&env.ok(&["claude", "--mcp", &format!("{a},{b},{c}"), "--no-attach"]));
    assert!(err.contains("existing sandbox matches"), "adding servers is not a conflict:\n{err}");
    assert!(err.contains(&format!("$ sbx mcp load {c} --sandbox {name}")), "{err}");
    assert_eq!(env.state(&name)["mcp"], serde_json::json!([a, b, c]));

    // Dropping one is a conflict: merge keeps it, replace recreates.
    let err = env.fails(&["claude", "--mcp", &a, "--no-attach"]);
    assert!(err.contains("cannot detach MCP servers"), "{err}");
    env.ok(&["claude", "--mcp", &a, "--on-conflict", "merge", "--no-attach"]);
    assert!(env.exists_in(&name, "/home/agent/same-sandbox"));
    env.ok(&["claude", "--mcp", &a, "--on-conflict", "replace", "--no-attach"]);
    assert!(!env.exists_in(&name, "/home/agent/same-sandbox"), "the sandbox was not recreated");
    assert_eq!(env.state(&name)["mcp"], serde_json::json!([a]));
}

#[test]
#[ignore = "needs a real, signed-in sbx; see the module docs"]
fn templates_snapshot_a_sandbox() {
    let env = Env::new();
    let (source, copy) = (env.named("source"), env.named("copy"));
    let template = env.named("tuned");
    env.on_drop(&["template", "rm", &format!("sand-template-{template}:latest")]);
    env.config("[templates.corp]\nimage = \"docker/sandbox-templates:claude-code\"\n");

    env.ok(&["claude", "--name", &source, "--no-attach"]);
    env.exec_ok(&source, "echo tuned > /home/agent/from-template");
    env.ok(&["template", "save", &source, &template]);
    let listing = text(&env.ok(&["template", "ls"]).stdout);
    assert!(listing.contains("corp") && listing.contains(&template) && listing.contains("snapshot"), "{listing}");

    // A sandbox created from the snapshot has what the source had.
    let err = stderr(&env.ok(&["claude", "--name", &copy, "--template", &template, "--no-attach"]));
    assert!(err.contains("--pull never"), "a local snapshot is never pulled:\n{err}");
    assert_eq!(env.exec_ok(&copy, "cat /home/agent/from-template"), "tuned\n");

    // Asking for a different template on an existing sandbox is a conflict that needs a recreate.
    let err = env.fails(&["claude", "--name", &copy, "--template", "corp", "--no-attach"]);
    assert!(err.contains("template is fixed when a sandbox is created"), "{err}");

    let _ = sbx(&["rm", "--force", &copy]);
    env.ok(&["template", "rm", &template]);
    assert!(!text(&env.ok(&["template", "ls"]).stdout).contains(&template));
}
