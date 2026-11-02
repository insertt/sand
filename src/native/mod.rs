//! Native sessions in Tern: when the terminal speaks the Tern Surface
//! Protocol, sand hosts the session itself instead of handing the terminal to
//! the agent's own UI. It starts the harness's ACP server in the sandbox,
//! speaks ACP to it as the client, and draws the conversation as an inline
//! Tern surface: Tern lays it out, animates it and takes the pointer, and keys
//! still reach sand on the pty.

mod client;
mod composer;
mod model;
mod view;

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::Value;
use tern_sdk::{Font, Input, Key, Options, Session, SurfaceOptions, wire::feature};

use crate::acp::PathMap;
use crate::harness::Profile;
use crate::paths::{Dirs, container_path};
use crate::sbx::Sbx;
use crate::state::SandboxState;
use client::{Call, Client, Incoming};
use composer::Composer;
use model::{Approval, Mode, Transcript, Turn};
pub use view::{Context, Msg};
use view::{EDITOR, view};

/// Turns native sessions off, to keep the agent's own terminal UI.
const OPT_OUT: &str = "SAND_TERN_NATIVE";

/// How long the agent may take to exit once its input is closed.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Lines of the agent's log shown when it exits on its own.
const LOG_TAIL: usize = 12;

/// Whether this session may be drawn natively; the terminal decides with
/// [`connect`]. Agent arguments are for the terminal UI, which ACP has no
/// place for.
pub fn eligible(acp: bool, no_attach: bool, profile: &Profile, agent_args: &[String]) -> bool {
    !acp && !no_attach
        && !profile.acp_command.is_empty()
        && agent_args.is_empty()
        && env::var(OPT_OUT).map_or(true, |v| v != "0")
}

/// Asks the terminal whether it speaks TSP; `None` when it does not (or TSP
/// is off with `TERN_TSP=0`, or sand runs inside screen, zellij or a tmux
/// session Tern does not draw).
pub fn connect() -> Option<Session<Msg>> {
    let options = Options::new().app("sand").version(env!("CARGO_PKG_VERSION")).feature(feature::SEND);
    Session::connect(options).ok().flatten()
}

/// The agent and how to reach it, for [`drive`].
pub struct Agent {
    pub client: Client,
    pub rx: Receiver<Incoming>,
    /// The session's working directory, in the sandbox.
    pub cwd: String,
    /// Where the agent's stderr goes.
    pub log: Option<PathBuf>,
}

/// Runs the session: starts the agent with `launch` (the `sbx exec` of its
/// ACP command), draws it in `session` until the person quits or the agent
/// exits, then lets the agent go. Answers the exit code.
pub fn run(
    sbx: &Sbx,
    dirs: &Dirs,
    profile: &Profile,
    state: &SandboxState,
    mut session: Session<Msg>,
    launch: Vec<OsString>,
    sandbox_home: &str,
) -> Result<i32> {
    let log_path = dirs.runtime_dir(&state.name).join("acp-agent.log");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let log = File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let mut child = sbx.spawn_piped(launch, log)?;
    let paths = PathMap::new(&state.mounts);
    let (tx, rx) = mpsc::channel();
    let waker = session.waker();
    let stdout = child.stdout.take().expect("piped");
    client::read(BufReader::new(stdout), paths.clone(), tx, move || {
        if let Some(waker) = &waker {
            waker.wake();
        }
    });
    let client = Client::new(Box::new(child.stdin.take().expect("piped")), paths);
    let cwd = state
        .mounts
        .primary
        .as_ref()
        .map(|m| container_path(&m.path.to_string_lossy()))
        .unwrap_or_else(|| sandbox_home.to_string());
    let ctx = Context {
        product: crate::tern::product_name(&profile.name).to_string(),
        sandbox: state.name.clone(),
        workspace: state.mounts.primary.as_ref().map(|m| tilde(&m.path, &dirs.home)),
        extra_mounts: state.mounts.extras.len(),
        mcp: state.mcp.clone(),
    };
    let agent = Agent { client, rx, cwd, log: Some(log_path) };
    let outcome = drive(&mut session, agent, &ctx);
    let closed = session.close();
    // The agent ends once its input closes, which `drive` did by dropping the client.
    finish(&mut child);
    let code = outcome?;
    closed?;
    Ok(code)
}

fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Waits a moment for the agent to exit, then stops it.
fn finish(child: &mut Child) {
    let deadline = Instant::now() + EXIT_GRACE;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The last lines of the agent's log, for an agent that exited.
fn log_tail(path: Option<&Path>) -> Option<String> {
    let text = fs::read_to_string(path?).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n");
    (!tail.is_empty()).then_some(tail)
}

/// The session's state between keys, agent messages and Tern's events.
struct Driver<'a> {
    t: Transcript,
    composer: Composer,
    session: Option<String>,
    /// A mode asked for with `session/set_mode`, taken once the agent agrees.
    asked_mode: Option<String>,
    client: Client,
    ctx: &'a Context,
    cwd: String,
    log: Option<PathBuf>,
    quit: bool,
    /// The person quit while the agent still ran (exit code 0), rather than
    /// the agent exiting on its own.
    by_person: bool,
}

/// Opens the surface and runs the session in it. Answers the exit code.
pub fn drive(session: &mut Session<Msg>, agent: Agent, ctx: &Context) -> Result<i32> {
    let sf = session.open(
        SurfaceOptions::inline()
            .title(format!("{} · {}", ctx.product, ctx.sandbox))
            .role("sand.session")
            .font(Font::Sans),
    )?;
    let Agent { client, rx, cwd, log } = agent;
    let mut d = Driver {
        t: Transcript::default(),
        composer: Composer::default(),
        session: None,
        asked_mode: None,
        client,
        ctx,
        cwd,
        log,
        quit: false,
        by_person: false,
    };
    let sent = d.client.initialize();
    d.check(sent);
    session.render(sf, view(&d.t, &d.composer, ctx))?;
    session.focus(sf, Some(EDITOR))?;
    // Without a waker (a terminal that cannot be interrupted) the agent is polled.
    let poll = session.waker().is_none().then_some(Duration::from_millis(50));
    let mut agent_open = true;
    while !d.quit {
        let mut changed = false;
        while agent_open {
            match rx.try_recv() {
                Ok(message) => {
                    agent_open = message != Incoming::Closed;
                    d.incoming(message);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    d.incoming(Incoming::Closed);
                    agent_open = false;
                    changed = true;
                }
            }
        }
        if changed {
            session.render(sf, view(&d.t, &d.composer, ctx))?;
        }
        match session.next(poll)? {
            None => continue,
            Some(Input::Key(key)) => d.key(&key),
            Some(Input::Msg(Msg::Focus, _)) => {
                session.focus(sf, Some(EDITOR))?;
                continue;
            }
            Some(Input::Msg(msg, _)) => d.msg(msg),
            Some(Input::Event(_) | Input::Clipboard(_)) => continue,
        }
        session.render(sf, view(&d.t, &d.composer, ctx))?;
    }
    Ok(if d.by_person { 0 } else { 1 })
}

impl Driver<'_> {
    /// A failed write to the agent means it is gone.
    fn check(&mut self, sent: io::Result<()>) {
        if let Err(err) = sent
            && self.t.turn != Turn::Exited
        {
            let detail = log_tail(self.log.as_deref());
            self.t.exited(format!("Lost the connection to {}: {err}", self.ctx.product), detail, Instant::now());
        }
    }

    fn incoming(&mut self, message: Incoming) {
        let now = Instant::now();
        match message {
            Incoming::Update(update) => self.t.apply(&update, now),
            Incoming::Permission { id, params } => {
                // A request for another session than ours cannot be answered by the person.
                if self.session.as_deref().is_some_and(|s| params["sessionId"].as_str().is_some_and(|p| p != s)) {
                    let sent = self.client.permit(id, None);
                    self.check(sent);
                } else {
                    self.t.ask(id, &params, now);
                }
            }
            Incoming::Unsupported { id, method } => {
                let sent = self.client.respond_unsupported(id, &method);
                self.check(sent);
            }
            Incoming::Response { id, result } => {
                if let Some(call) = self.client.call(id) {
                    self.response(call, result, now)
                }
            }
            Incoming::Closed => {
                if self.t.turn != Turn::Exited {
                    let detail = log_tail(self.log.as_deref());
                    self.t.exited(format!("{} exited", self.ctx.product), detail, now);
                }
            }
        }
    }

    fn response(&mut self, call: Call, result: Result<Value, String>, now: Instant) {
        match (call, result) {
            (Call::Initialize, Ok(_)) => {
                let sent = self.client.new_session(&self.cwd.clone());
                self.check(sent);
            }
            (Call::NewSession, Ok(result)) => {
                self.session = result["sessionId"].as_str().map(str::to_string);
                let modes = &result["modes"];
                self.t.modes = modes["availableModes"]
                    .as_array()
                    .map(|modes| {
                        modes
                            .iter()
                            .filter_map(|m| {
                                let id = m["id"].as_str()?.to_string();
                                let name = m["name"].as_str().map_or_else(|| id.clone(), str::to_string);
                                Some(Mode { id, name })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.t.mode = modes["currentModeId"].as_str().map(str::to_string);
                self.t.turn = Turn::Idle;
            }
            (Call::Initialize | Call::NewSession, Err(err)) => {
                self.t.exited(format!("{} could not start a session: {err}", self.ctx.product), None, now);
            }
            (Call::Prompt, Ok(result)) => self.t.turn_ended(result["stopReason"].as_str(), now),
            (Call::Prompt, Err(err)) => {
                self.t.error(err, None);
                self.t.turn_ended(None, now);
            }
            (Call::SetMode, Ok(_)) => self.t.mode = self.asked_mode.take().or(self.t.mode.take()),
            (Call::SetMode, Err(err)) => {
                self.asked_mode = None;
                self.t.error(format!("The mode could not be changed: {err}"), None);
            }
        }
    }

    fn send(&mut self, text: String) {
        let (Turn::Idle, Some(session)) = (self.t.turn, self.session.clone()) else { return };
        if text.trim().is_empty() {
            return;
        }
        self.t.prompt(&text, Instant::now());
        let sent = self.client.prompt(&session, &text);
        self.check(sent);
    }

    fn answer(&mut self, approval: Approval) {
        if let Some((id, option)) = self.t.answer(approval) {
            let sent = self.client.permit(id, option.as_deref());
            self.check(sent);
        }
    }

    /// Stops the turn; a permission still waiting is answered as cancelled.
    fn cancel(&mut self) {
        let Some(session) = self.session.clone() else { return };
        if let Some(permission) = self.t.permission.take() {
            let sent = self.client.permit(permission.request, None);
            self.check(sent);
        }
        let sent = self.client.cancel(&session);
        self.check(sent);
    }

    fn quit(&mut self) {
        self.by_person = self.t.turn != Turn::Exited;
        self.quit = true;
    }

    fn msg(&mut self, msg: Msg) {
        match msg {
            Msg::Send(text) => self.send(text),
            Msg::Answer(approval) => self.answer(approval),
            Msg::Focus => {}
        }
    }

    fn key(&mut self, key: &Key) {
        let working = self.t.turn == Turn::Working;
        if self.t.turn == Turn::Exited {
            if ["enter", "escape", "ctrl+c", "ctrl+d"].iter().any(|k| key.is(k)) {
                self.quit();
            }
            return;
        }
        if self.t.permission.is_some() && self.composer.is_empty() {
            if key.is("enter") {
                return self.answer(Approval::Once);
            }
            if key.is("escape") {
                return self.answer(Approval::Denied);
            }
        }
        if key.is("escape") {
            if working {
                self.cancel();
            }
        } else if key.is("ctrl+c") {
            if working {
                self.cancel();
            } else if !self.composer.is_empty() {
                self.composer.take();
            } else {
                self.quit();
            }
        } else if key.is("ctrl+d") {
            if self.composer.is_empty() && !working {
                self.quit();
            }
        } else if key.is("shift+tab") {
            self.next_mode();
        } else if key.is("enter") {
            if self.t.turn == Turn::Idle && !self.composer.is_empty() {
                let text = self.composer.take();
                self.send(text);
            }
        } else {
            self.composer.key(key);
        }
    }

    fn next_mode(&mut self) {
        let (Some(session), Some(mode)) = (self.session.clone(), self.t.next_mode().map(|m| m.id.clone())) else {
            return;
        };
        if self.t.modes.len() < 2 {
            return;
        }
        let sent = self.client.set_mode(&session, &mode);
        self.asked_mode = Some(mode);
        self.check(sent);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{BufRead, Write};
    use std::rc::Rc;

    use serde_json::json;
    use tern_sdk::Waker;
    use tern_sdk::term::Terminal;

    use super::*;

    /// A Tern pane: answers the hello, acks every frame, and plays the person
    /// through `script` once what it waits for has been drawn.
    struct Pane {
        input: Rc<RefCell<VecDeque<Vec<u8>>>>,
        output: Rc<RefCell<String>>,
        frames: u64,
        step: usize,
        mark: usize,
        deadline: Instant,
        waker: Waker,
    }

    fn event(body: Value) -> Vec<u8> {
        format!("\x1b_tsp;e;{body}\x1b\\").into_bytes()
    }

    impl Pane {
        fn play(&mut self) {
            let out = self.output.borrow().clone();
            let since = &out[self.mark.min(out.len())..];
            let mut input = self.input.borrow_mut();
            match self.step {
                0 if since.contains(r#""sendable":true"#) => input.push_back(b"hi\r".to_vec()),
                1 if since.contains(r#""status":"ask""#) => {
                    let at = since.find(r#""k":"tool""#).expect("a tool node");
                    let start = since[..at].rfind(r#""id":""#).expect("its id") + 6;
                    let id = &since[start..start + since[start..].find('"').unwrap()];
                    input.push_back(event(json!({"ev": "action", "sf": "s1", "id": id, "act": "allow"})));
                }
                2 if since.contains("Ready") => input.push_back(vec![0x04]),
                _ => return,
            }
            self.step += 1;
            self.mark = out.len();
        }
    }

    impl Terminal for Pane {
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            if text.contains("tsp;q;{") {
                let hello = json!({"r": "hello", "v": 1, "term": "tern", "ver": "0.4.3", "kinds": [],
                    "features": ["flow", "font"], "apc": 65536, "credits": 2, "cols": 100});
                self.input.borrow_mut().push_back(format!("\x1b_tsp;r;{hello}\x1b\\").into_bytes());
            }
            for _ in 0..text.matches("tsp;f;").count() {
                self.frames += 1;
                let ack = event(json!({"ev": "ack", "sf": "s1", "s": self.frames}));
                self.input.borrow_mut().push_back(ack);
            }
            self.output.borrow_mut().push_str(&text);
            self.play();
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<Option<usize>> {
            if Instant::now() > self.deadline {
                // A stuck session still ends.
                self.input.borrow_mut().push_back(vec![0x04]);
            }
            let Some(chunk) = self.input.borrow_mut().pop_front() else {
                thread::sleep(timeout.unwrap_or(Duration::ZERO).min(Duration::from_millis(1)));
                return Ok(None);
            };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(Some(chunk.len()))
        }

        fn waker(&self) -> Option<Waker> {
            Some(self.waker.clone())
        }
    }

    /// An ACP agent that answers a prompt with two chunks and a tool that
    /// needs permission; returns every message sand sent it.
    fn agent(from_sand: io::PipeReader, mut to_sand: io::PipeWriter) -> thread::JoinHandle<Vec<Value>> {
        thread::spawn(move || {
            let mut lines = io::BufReader::new(from_sand).lines();
            let mut seen = Vec::new();
            let mut next = |seen: &mut Vec<Value>| -> Option<Value> {
                let message: Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
                seen.push(message.clone());
                Some(message)
            };
            let mut say = |message: Value| {
                writeln!(to_sand, "{message}").unwrap();
            };
            let update = |update: Value| {
                json!({"jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": "S", "update": update}})
            };
            let chunk = |text: &str| {
                update(json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}}))
            };

            let init = next(&mut seen).unwrap();
            say(json!({"jsonrpc": "2.0", "id": init["id"], "result": {"protocolVersion": 1}}));
            let new = next(&mut seen).unwrap();
            say(json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "S", "modes": {
                "currentModeId": "default", "availableModes": [{"id": "default", "name": "Default"}]}}}));
            let prompt = next(&mut seen).unwrap();
            say(chunk("Hel"));
            thread::sleep(Duration::from_millis(300));
            say(chunk("lo"));
            thread::sleep(Duration::from_millis(100));
            say(update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "rm", "kind": "execute",
                "status": "pending", "rawInput": {"command": "rm -rf build"}})));
            say(json!({"jsonrpc": "2.0", "id": "perm", "method": "session/request_permission", "params": {
                "sessionId": "S", "toolCall": {"toolCallId": "t1"},
                "options": [{"optionId": "allow-1", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "reject-1", "name": "Reject", "kind": "reject_once"}]}}));
            next(&mut seen).unwrap();
            say(update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"})));
            say(json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}));
            while next(&mut seen).is_some() {}
            seen
        })
    }

    #[test]
    fn a_native_session_prompts_streams_and_asks_for_permission() {
        let output = Rc::new(RefCell::new(String::new()));
        let pane = Pane {
            input: Rc::default(),
            output: Rc::clone(&output),
            frames: 0,
            step: 0,
            mark: 0,
            deadline: Instant::now() + Duration::from_secs(20),
            waker: Waker::new().unwrap(),
        };
        let options = Options::new().app("sand").record(None).timeout(Duration::from_secs(5));
        let mut session = Session::<Msg>::with_terminal(pane, options).unwrap().expect("TSP answers");

        let (from_sand, client_out) = io::pipe().unwrap();
        let (agent_in, to_sand) = io::pipe().unwrap();
        let fake = agent(from_sand, to_sand);
        let (tx, rx) = mpsc::channel();
        let waker = session.waker().unwrap();
        client::read(io::BufReader::new(agent_in), None, tx, move || waker.wake());
        let agent = Agent { client: Client::new(Box::new(client_out), None), rx, cwd: "/w".into(), log: None };
        let ctx = Context { product: "Claude Code".into(), sandbox: "claude-w".into(), ..Context::default() };

        let code = drive(&mut session, agent, &ctx).unwrap();
        session.close().unwrap();
        let seen = fake.join().unwrap();

        assert_eq!(code, 0, "the person quit with Ctrl+D");
        let methods: Vec<&str> = seen.iter().map(|m| m["method"].as_str().unwrap_or("(response)")).collect();
        assert_eq!(methods, ["initialize", "session/new", "session/prompt", "(response)"]);
        assert_eq!(seen[1]["params"]["cwd"], "/w");
        assert_eq!(seen[2]["params"]["prompt"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(
            seen[3],
            json!({"jsonrpc": "2.0", "id": "perm",
            "result": {"outcome": {"outcome": "selected", "optionId": "allow-1"}}})
        );

        let output = output.borrow();
        assert!(output.contains(r#""role":"sand.session""#) && output.contains(r#""font":"sans""#), "{output}");
        assert!(output.contains(r#""append","lo"]"#), "the reply streams as an append: {output}");
        assert!(output.contains(r#""approved":"once""#), "{output}");
        assert!(output.contains(r#""target":"rm -rf build""#), "{output}");
    }
}
