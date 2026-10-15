//! Thin wrapper around the `sbx` CLI. With `--verbose` every invocation is
//! echoed and its output shown; otherwise sbx is only heard when it fails.

use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::log;

#[derive(Clone, Debug)]
pub struct Sbx {
    bin: OsString,
    /// Whether setup commands may read the terminal (false in ACP mode, where
    /// stdin carries the protocol stream).
    interactive: bool,
}

/// Builds an argument vector from anything that converts to `OsString`.
#[macro_export]
macro_rules! args {
    ($($arg:expr),* $(,)?) => {
        vec![$(::std::ffi::OsString::from($arg)),*]
    };
}

fn display(args: &[OsString]) -> String {
    args.iter()
        .map(|a| {
            let s = a.to_string_lossy();
            if s.is_empty() || s.contains(char::is_whitespace) { format!("'{s}'") } else { s.into_owned() }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quotes `word` for a POSIX shell, leaving plain words as they are.
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=+".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Copies lines from one stream to another through `filter`, until either end closes.
fn pump(mut from: impl BufRead, mut to: impl Write, filter: impl Fn(&str) -> String) {
    let mut line = String::new();
    while matches!(from.read_line(&mut line), Ok(n) if n > 0) {
        let message = filter(line.trim_end_matches(['\r', '\n']));
        if to.write_all(message.as_bytes()).and_then(|_| to.write_all(b"\n")).and_then(|_| to.flush()).is_err() {
            break;
        }
        line.clear();
    }
}

impl Sbx {
    fn new(interactive: bool) -> Self {
        let bin = env::var_os("SAND_SBX").unwrap_or_else(|| OsString::from("sbx"));
        Self { bin, interactive }
    }

    /// The wrapper every command starts from: it makes sure sbx is signed in
    /// first, so that a missing or expired login is dealt with up front rather
    /// than by whichever sbx call happens to run into it.
    pub fn connect(interactive: bool) -> Result<Self> {
        let sbx = Self::new(interactive);
        sbx.ensure_signed_in()?;
        Ok(sbx)
    }

    /// Runs `sbx login` when sbx reports that the user is not signed in or that
    /// the login expired. `sbx ls` is the probe: cheap, and it needs a login.
    /// Any other failure is left to the command that runs into it.
    fn ensure_signed_in(&self) -> Result<()> {
        let output = Command::new(&self.bin)
            .args(["ls", "-q"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(e))?;
        if output.status.success() || !needs_login(&output) {
            return Ok(());
        }
        if !self.can_prompt() {
            let mut err = io::stderr().lock();
            let _ = err.write_all(&output.stderr);
            drop(err);
            bail!(
                "sbx is not signed in (or the login expired) and there is no terminal to sign in on; run `sbx login`"
            );
        }
        log::step("sbx is not signed in (or the login expired); running `sbx login`");
        if !self.interact(args!["login"])?.success() {
            bail!("`sbx login` failed; sign in with `sbx login` and try again");
        }
        Ok(())
    }

    /// Whether a command may prompt the user: sbx login opens a browser and waits on the terminal.
    fn can_prompt(&self) -> bool {
        self.interactive && io::stdin().is_terminal() && io::stderr().is_terminal()
    }

    fn command(&self, args: &[OsString]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        cmd
    }

    fn spawn_error(&self, err: io::Error) -> anyhow::Error {
        if err.kind() == io::ErrorKind::NotFound {
            anyhow!(
                "`{}` was not found on PATH. Install Docker Sandboxes (https://docs.docker.com/ai/sandboxes/install/) \
                 or point SAND_SBX at the binary.",
                self.bin.to_string_lossy()
            )
        } else {
            anyhow!(err).context(format!("running `{}`", self.bin.to_string_lossy()))
        }
    }

    /// Runs a setup command. Its output goes to stderr: as it comes with
    /// `--verbose`, otherwise only when the command fails.
    pub fn run(&self, args: Vec<OsString>) -> Result<()> {
        let status = self.status(args.clone())?;
        if !status.success() {
            bail!("`sbx {}` failed ({status})", display(&args));
        }
        Ok(())
    }

    /// Like [`Sbx::run`], for a command that can take minutes: a spinner with
    /// `waiting` and the elapsed time shows that sand is not stuck.
    pub fn run_slow(&self, args: Vec<OsString>, waiting: &str) -> Result<()> {
        let status = self.status_while(args.clone(), Some(waiting))?;
        if !status.success() {
            bail!("`sbx {}` failed ({status})", display(&args));
        }
        Ok(())
    }

    /// Like [`Sbx::run`] but hands back the exit status instead of failing.
    pub fn status(&self, args: Vec<OsString>) -> Result<ExitStatus> {
        self.status_while(args, None)
    }

    fn status_while(&self, args: Vec<OsString>, waiting: Option<&str>) -> Result<ExitStatus> {
        log::command(format!("sbx {}", display(&args)));
        let mut cmd = self.command(&args);
        if log::verbose() {
            cmd.stdin(if self.interactive { Stdio::inherit() } else { Stdio::null() });
            cmd.stdout(io::stderr()).stderr(Stdio::inherit());
            return cmd.status().map_err(|e| self.spawn_error(e));
        }
        // No terminal on stdin: a prompt nobody can see would hang, this way it fails and is shown.
        let spinner = waiting.map(log::spinner);
        let output = cmd.stdin(Stdio::null()).output();
        // Gone before anything else is printed, so that it cannot garble the output of a failure.
        drop(spinner);
        let output = output.map_err(|e| self.spawn_error(e))?;
        if !output.status.success() {
            let mut err = io::stderr().lock();
            let _ = err.write_all(&output.stdout).and_then(|_| err.write_all(&output.stderr));
        }
        Ok(output.status)
    }

    /// Runs a command that may talk to the user: it gets the terminal, with its
    /// stdout on stderr like every other setup command.
    pub fn interact(&self, args: Vec<OsString>) -> Result<ExitStatus> {
        log::command(format!("sbx {}", display(&args)));
        self.command(&args).stdout(io::stderr()).status().map_err(|e| self.spawn_error(e))
    }

    /// Runs a command silently and returns its stdout when it succeeds.
    pub fn capture(&self, args: Vec<OsString>) -> Result<Option<String>> {
        let output =
            self.command(&args).stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(|e| self.spawn_error(e))?;
        Ok(output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }

    /// Runs a command with the terminal (or ACP stream) fully attached.
    pub fn attach(&self, args: Vec<OsString>) -> Result<ExitStatus> {
        log::command(format!("sbx {}", display(&args)));
        self.command(&args).status().map_err(|e| self.spawn_error(e))
    }

    /// Like [`Sbx::attach`], but every line on stdin and stdout goes through a filter.
    pub fn attach_filtered<I, O>(&self, args: Vec<OsString>, input: I, output: O) -> Result<ExitStatus>
    where
        I: Fn(&str) -> String + Send + 'static,
        O: Fn(&str) -> String + Send + 'static,
    {
        log::command(format!("sbx {}", display(&args)));
        let mut child = self
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(e))?;
        let (child_in, child_out) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"));
        // Not joined: it blocks on our stdin, which the editor may keep open after the agent exits.
        thread::spawn(move || pump(io::stdin().lock(), child_in, input));
        let out = thread::spawn(move || pump(BufReader::new(child_out), io::stdout().lock(), output));
        let status = child.wait().context("waiting for sbx")?;
        let _ = out.join();
        Ok(status)
    }

    pub fn version(&self) -> Result<String> {
        self.capture(args!["version"])?
            .map(|v| v.trim().to_string())
            .ok_or_else(|| anyhow!("`sbx version` failed; is Docker Sandboxes installed and working?"))
    }

    pub fn list(&self) -> Result<Vec<String>> {
        let out = self.capture(args!["ls", "-q"])?.ok_or_else(|| {
            anyhow!("`sbx ls` failed; is the sandbox daemon running and are you signed in (`sbx login`)?")
        })?;
        Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    }

    pub fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.list()?.iter().any(|n| n == name))
    }

    /// Runs `sh -c script` inside the sandbox, streaming output to stderr.
    pub fn exec_sh(&self, sandbox: &str, script: &str) -> Result<()> {
        self.run(args!["exec", sandbox, "sh", "-c", script])
    }

    /// Reads a file from inside the sandbox; `None` when it does not exist.
    pub fn read_file(&self, sandbox: &str, path: &str) -> Result<Option<String>> {
        self.capture(args!["exec", sandbox, "cat", path])
    }

    /// Holds a session open so the sandbox keeps running while sand mounts
    /// directories and copies configuration into it.
    pub fn keep_alive(&self, sandbox: &str) -> Result<KeepAlive> {
        let args = args!["exec", "-i", sandbox, "sh", "-c", "read _ || true"];
        log::command(format!("sbx {}   # keeps the sandbox running during setup", display(&args)));
        let child = self
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| self.spawn_error(e))?;
        Ok(KeepAlive { child })
    }

    /// Waits until commands can be executed inside the sandbox.
    pub fn wait_ready(&self, sandbox: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.capture(args!["exec", sandbox, "true"])?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "sandbox '{sandbox}' did not become ready within {}s (`sbx exec {sandbox} true` keeps failing)",
                    timeout.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(750));
        }
    }
}

/// Background `sbx exec` session; closing its stdin lets it exit cleanly.
pub struct KeepAlive {
    child: Child,
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a failed sbx command failed because the user is not signed in. sbx
/// has no error code for it, so this matches its messages: "not signed in to
/// Docker" (with "try: sbx login") and "your Docker session has expired; run
/// `sbx login` and try again". A session that could not be refreshed because
/// of the network is not one: signing in again would not help.
fn needs_login(output: &Output) -> bool {
    mentions_login(&format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

fn mentions_login(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    if text.contains("could not be refreshed due to a network issue") {
        return false;
    }
    ["not signed in to docker", "docker session has expired", "docker session expired", "sbx login"]
        .iter()
        .any(|signal| text.contains(signal))
}

/// Runs an arbitrary host command (docker, …), streaming its output to stderr.
pub fn run_host(program: &str, args: Vec<OsString>) -> Result<()> {
    log::command(format!("{program} {}", display(&args)));
    let status = Command::new(program)
        .args(&args)
        .stdout(io::stderr())
        .status()
        .with_context(|| format!("running `{program}` (is it installed?)"))?;
    if !status.success() {
        bail!("`{program} {}` failed ({status})", display(&args));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pump_filters_line_by_line() {
        let mut out = Vec::new();
        pump(io::Cursor::new("one\r\ntwo\n\nlast"), &mut out, |line| line.to_uppercase());
        assert_eq!(String::from_utf8(out).unwrap(), "ONE\nTWO\n\nLAST\n");
    }

    #[test]
    fn login_failures_are_recognized() {
        // What sbx prints for each case.
        assert!(mentions_login("error: not signed in to Docker\n  try: sbx login"));
        assert!(mentions_login("your Docker session has expired; run `sbx login` and try again (401 Unauthorized)"));
        assert!(!mentions_login(
            "your Docker session could not be refreshed due to a network issue; this should resolve automatically, try again shortly"
        ));
        assert!(!mentions_login("pull failed due to an unauthenticated request"));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("npx"), "npx");
        assert_eq!(shell_quote("@agentclientprotocol/claude-agent-acp"), "@agentclientprotocol/claude-agent-acp");
        assert_eq!(shell_quote("https://api.anthropic.com/v1/models"), "https://api.anthropic.com/v1/models");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("https://x/?a=1&b=$(id)"), "'https://x/?a=1&b=$(id)'");
        assert_eq!(shell_quote(""), "''");
    }
}
