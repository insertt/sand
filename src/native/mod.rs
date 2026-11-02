//! Native sessions in Tern: when the terminal speaks the Tern Surface
//! Protocol, sand draws the whole session itself, from the first step of
//! setup to the end of the chat, instead of printing its log and handing the
//! terminal to the agent's own UI. Setup's progress and questions show on an
//! inline Tern surface; then sand starts the harness's ACP server in the
//! sandbox, speaks ACP to it as the client and draws the conversation on the
//! same surface. Tern lays it out, animates it and takes the pointer; keys
//! still reach sand on the pty.
//!
//! The surface lives on a thread of its own, since setup runs blocking `sbx`
//! commands on the main thread: setup talks to it through [`start`],
//! [`ask`], [`update`], [`chat`] and [`shutdown`], and sand's log reaches it
//! through a log sink.

mod client;
mod commands;
mod composer;
mod driver;
mod model;
mod view;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use anyhow::{Result, anyhow};
use tern_sdk::{Options, Session, Waker, wire::feature};

use crate::acp::PathMap;
use crate::harness::Profile;
use crate::log::{self, Line};
use crate::mcp::McpStatus;
use crate::runtime::ApiState;
use crate::sbx::Sbx;
use driver::Driver;
pub use view::Info;
use view::Msg;

/// Turns native sessions off, to keep the agent's own terminal UI.
const OPT_OUT: &str = "SAND_TERN_NATIVE";

/// What setup tells the surface thread.
pub enum Command {
    Log(Line),
    /// A question; the answer (`None`: abort) goes to `reply`.
    Ask {
        question: String,
        options: Vec<String>,
        reply: Sender<Option<usize>>,
    },
    Update(Update),
    /// Setup is done: start the agent.
    Chat(Box<ChatSpec>),
    /// Setup ended without a chat: close the surface.
    Close,
    /// Something only the person may allow (a mount, a network host): the
    /// answer goes to `reply`.
    Confirm {
        question: String,
        reply: Sender<bool>,
    },
    /// Where answers to sbx's pending network approvals go.
    Approvals(Sender<(String, String)>),
    /// Talk to this agent (tests stand one in for `sbx exec`).
    #[cfg(test)]
    Agent(client::Client, Receiver<client::Incoming>),
}

/// What the status shows, as it changes.
pub enum Update {
    Api(ApiState),
    Mcp(Vec<McpStatus>),
    Branch(Option<String>),
    Mounts(Vec<String>),
    /// The network approvals sbx has pending for the sandbox.
    Pending(Vec<crate::network::Approval>),
}

/// How to start the agent, and what to keep looking at while it runs.
pub struct ChatSpec {
    pub sbx: Sbx,
    /// The `sbx exec` of the harness's ACP command.
    pub launch: Vec<OsString>,
    /// Where the agent's stderr goes.
    pub log: PathBuf,
    pub paths: Option<PathMap>,
    /// The session's working directory, in the sandbox.
    pub cwd: String,
    pub profile: Profile,
    pub sandbox: String,
    pub mcp: Vec<String>,
    /// The primary workspace on the host, for the git branch.
    pub workspace: Option<PathBuf>,
}

struct Host {
    tx: Sender<Command>,
    waker: Waker,
    thread: JoinHandle<Result<i32>>,
    /// Everything setup logged, to print it again once the surface is gone.
    lines: Arc<Mutex<Vec<Line>>>,
}

impl Host {
    fn send(&self, command: Command) -> bool {
        let sent = self.tx.send(command).is_ok();
        self.waker.wake();
        sent
    }
}

static HOST: Mutex<Option<Host>> = Mutex::new(None);

fn host() -> std::sync::MutexGuard<'static, Option<Host>> {
    HOST.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether this session may be drawn natively; the terminal decides in
/// [`start`]. Agent arguments are for the terminal UI, which ACP has no
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
fn connect() -> Option<Session<Msg>> {
    let options = Options::new()
        .app("sand")
        .version(env!("CARGO_PKG_VERSION"))
        .feature(feature::SEND)
        .feature(feature::EDIT)
        .feature(feature::UNDO);
    Session::connect(options).ok().flatten()
}

/// Opens the native surface if the terminal speaks TSP, and from then on
/// shows sand's log there. False when it does not; nothing changed then.
pub fn start(info: Info) -> bool {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let thread = {
        let (tx, lines) = (tx.clone(), Arc::clone(&lines));
        thread::spawn(move || surface(info, rx, tx, ready_tx, lines))
    };
    let Ok(Some(waker)) = ready_rx.recv() else {
        let _ = thread.join();
        return false;
    };
    {
        let (tx, waker, lines) = (tx.clone(), waker.clone(), Arc::clone(&lines));
        log::set_sink(Some(Box::new(move |line| {
            lines.lock().unwrap_or_else(PoisonError::into_inner).push(line.clone());
            let _ = tx.send(Command::Log(line));
            waker.wake();
        })));
    }
    *host() = Some(Host { tx, waker, thread, lines });
    true
}

/// The surface thread: connects, then draws until the chat ends or setup closes it.
fn surface(
    info: Info,
    rx: Receiver<Command>,
    tx: Sender<Command>,
    ready: SyncSender<Option<Waker>>,
    lines: Arc<Mutex<Vec<Line>>>,
) -> Result<i32> {
    let Some(mut session) = connect() else {
        let _ = ready.send(None);
        return Ok(0);
    };
    let Some(waker) = session.waker() else {
        let _ = session.close();
        let _ = ready.send(None);
        return Ok(0);
    };
    let sf = driver::open(&mut session, &info)?;
    let _ = ready.send(Some(waker.clone()));
    let mut driver = Driver::new(info, tx, Some(waker));
    let code = driver.run(&mut session, sf, &rx);
    let closed = session.close();
    if driver.interrupted {
        log::set_sink(None);
        for line in lines.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            log::replay(line);
        }
        eprintln!();
        log::error("interrupted");
        crate::session::dispose();
        std::process::exit(130);
    }
    let code = code?;
    closed?;
    Ok(code)
}

/// How long a question to the person waits before it counts as declined.
const CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The network bridge's hooks into the native surface: its questions become
/// cards there instead of host dialogs, and sbx's pending network approvals
/// show there with their options. `None` without a native surface.
pub fn bridge_hooks() -> Option<crate::network::Hooks> {
    use crate::network::Answer;

    let (tx, waker) = {
        let host = host();
        let host = host.as_ref()?;
        (host.tx.clone(), host.waker.clone())
    };
    let send = move |command: Command| {
        let sent = tx.send(command).is_ok();
        waker.wake();
        sent
    };
    let (answers_tx, answers) = mpsc::channel();
    if !send(Command::Approvals(answers_tx)) {
        return None;
    }
    let ask = send.clone();
    let confirm = Box::new(move |question: &str| {
        let (reply, answer) = mpsc::channel();
        if !ask(Command::Confirm { question: question.to_string(), reply }) {
            return Answer::Unavailable("the native session has ended".to_string());
        }
        match answer.recv_timeout(CONFIRM_TIMEOUT) {
            Ok(true) => Answer::Allow,
            Ok(false) | Err(mpsc::RecvTimeoutError::Timeout) => Answer::Deny,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Answer::Unavailable("the native session has ended".to_string())
            }
        }
    });
    let pending = Box::new(move |approvals: &[crate::network::Approval]| {
        send(Command::Update(Update::Pending(approvals.to_vec())));
    });
    Some(crate::network::Hooks { confirm, pending, answers })
}

/// Whether a native surface shows the session (setup has not handed it over yet).
pub fn active() -> bool {
    host().is_some()
}

/// Asks `question` on the native surface: `Some(answer)` when it is active,
/// the answer being the option picked (`None`: abort).
pub fn ask(question: &str, options: &[&str]) -> Option<Option<usize>> {
    let (reply, answer) = mpsc::channel();
    let asked = {
        let host = host();
        let host = host.as_ref()?;
        let options = options.iter().map(|o| o.to_string()).collect();
        host.send(Command::Ask { question: question.to_string(), options, reply })
    };
    // Asked outside the lock: logging while waiting still works.
    Some(if asked { answer.recv().ok().flatten() } else { None })
}

/// Shows `update` on the native surface; false when there is none.
pub fn update(update: Update) -> bool {
    host().as_ref().is_some_and(|h| h.send(Command::Update(update)))
}

/// Starts the chat on the native surface and waits for it to end; `None`
/// when there is no native surface. Answers the exit code.
pub fn chat(spec: ChatSpec) -> Option<Result<i32>> {
    let host = host().take()?;
    host.send(Command::Chat(Box::new(spec)));
    let result = host.thread.join().map_err(|_| anyhow!("the native session failed")).and_then(|r| r);
    log::set_sink(None);
    Some(result)
}

/// Closes the native surface of a session that ends without a chat (it was
/// aborted, or failed), and prints setup's log on the terminal, where it
/// stays once the surface is gone.
pub fn shutdown() {
    let Some(host) = host().take() else { return };
    log::set_sink(None);
    host.send(Command::Close);
    let _ = host.thread.join();
    for line in host.lines.lock().unwrap_or_else(PoisonError::into_inner).iter() {
        log::replay(line);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{self, BufRead, Write};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tern_sdk::term::Terminal;

    use super::*;

    /// A Tern pane: answers the hello, acks every frame, and plays the person
    /// once what each step waits for has been drawn.
    struct Pane {
        input: Rc<RefCell<VecDeque<Vec<u8>>>>,
        output: Rc<RefCell<String>>,
        frames: u64,
        /// The person's steps: each looks at what was drawn since the last
        /// step and answers with input once what it waits for is there.
        script: VecDeque<Step>,
        mark: usize,
        deadline: Instant,
        waker: Waker,
    }

    type Step = Box<dyn Fn(&str) -> Option<Vec<u8>>>;

    /// A step that types `input` once `seen` was drawn.
    fn after(seen: &'static str, input: impl Into<Vec<u8>>) -> Step {
        let input = input.into();
        Box::new(move |since: &str| since.contains(seen).then(|| input.clone()))
    }

    impl Pane {
        fn new(output: &Rc<RefCell<String>>, script: Vec<Step>) -> Self {
            Self {
                input: Rc::default(),
                output: Rc::clone(output),
                frames: 0,
                script: script.into(),
                mark: 0,
                deadline: Instant::now() + Duration::from_secs(20),
                waker: Waker::new().unwrap(),
            }
        }
    }

    fn session(pane: Pane) -> Session<Msg> {
        // `TERN_TSP_RECORD=<file>` keeps the frames, for Tern's `surface-play`.
        let record = env::var_os("TERN_TSP_RECORD").map(PathBuf::from);
        let options = Options::new().app("sand").record(record).timeout(Duration::from_secs(5));
        Session::<Msg>::with_terminal(pane, options).unwrap().expect("TSP answers")
    }

    fn event(body: Value) -> Vec<u8> {
        format!("\x1b_tsp;e;{body}\x1b\\").into_bytes()
    }

    impl Pane {
        fn play(&mut self) {
            let out = self.output.borrow().clone();
            let since = &out[self.mark.min(out.len())..];
            let Some(input) = self.script.front().and_then(|step| step(since)) else { return };
            self.script.pop_front();
            self.input.borrow_mut().push_back(input);
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
                self.input.borrow_mut().push_back(vec![0x04, 0x04]);
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
    fn agent(from_sand: io::PipeReader, mut to_sand: io::PipeWriter) -> JoinHandle<Vec<Value>> {
        thread::spawn(move || {
            let mut lines = io::BufReader::new(from_sand).lines();
            let mut seen = Vec::new();
            let mut answer = to_sand.try_clone().unwrap();
            let mut next = |seen: &mut Vec<Value>| -> Option<Value> {
                loop {
                    let message: Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
                    // The welcome card's quiet lookup: no earlier sessions here.
                    if message["method"] == "session/list" {
                        writeln!(
                            answer,
                            "{}",
                            json!({"jsonrpc": "2.0", "id": message["id"], "result": {"sessions": []}})
                        )
                        .unwrap();
                        continue;
                    }
                    seen.push(message.clone());
                    return Some(message);
                }
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
            let options = |current: &str| {
                json!([
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": current,
                 "options": [{"value": "opus", "name": "Opus 5.5"}, {"value": "haiku", "name": "Haiku 5.5"}]},
                {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "low"}, {"value": "high", "name": "high"}]}])
            };
            say(
                json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "S", "configOptions": options("opus"),
                "modes": {"currentModeId": "default", "availableModes": [{"id": "default", "name": "Default"}]}}}),
            );
            let set = next(&mut seen).unwrap();
            say(
                json!({"jsonrpc": "2.0", "id": set["id"], "result": {"configOptions": options(set["params"]["value"].as_str().unwrap())}}),
            );
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
            say(update(json!({"sessionUpdate": "usage_update", "used": 50000, "size": 200000,
                "cost": {"amount": 0.12, "currency": "USD"}})));
            say(json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}));
            while next(&mut seen).is_some() {}
            seen
        })
    }

    #[test]
    fn a_native_session_shows_setup_asks_then_chats() {
        let output = Rc::new(RefCell::new(String::new()));
        let pane = Pane::new(
            &output,
            vec![
                // Setup asks; the second answer is picked with its key.
                after("How do you want to continue?", "2"),
                // The model chip is a picker of the agent's models.
                after(
                    r#""sendable":true"#,
                    event(json!({"ev": "change", "sf": "s1", "id": "dock.composer.bar.model", "value": "haiku"})),
                ),
                after(r#""value":"haiku""#, "hi\r"),
                Box::new(|since: &str| {
                    let at = since.find(r#""k":"tool""#).filter(|_| since.contains(r#""status":"ask""#))?;
                    let start = since[..at].rfind(r#""id":""#).expect("its id") + 6;
                    let id = &since[start..start + since[start..].find('"').unwrap()];
                    Some(event(json!({"ev": "action", "sf": "s1", "id": id, "act": "allow"})))
                }),
                // Ctrl+D twice in a row quits.
                after("this turn", [0x04, 0x04]),
            ],
        );
        let mut session = session(pane);
        let info = Info {
            harness: "claude".into(),
            product: "Claude Code".into(),
            sandbox: "claude-w".into(),
            ..Info::default()
        };
        let sf = driver::open(&mut session, &info).unwrap();
        let waker = session.waker().unwrap();
        let (tx, rx) = mpsc::channel();

        // Setup, on its own thread as in sand: two steps, a question, then the agent.
        let (from_sand, client_out) = io::pipe().unwrap();
        let (agent_in, to_sand) = io::pipe().unwrap();
        let fake = agent(from_sand, to_sand);
        let setup = {
            let (tx, waker) = (tx.clone(), waker.clone());
            thread::spawn(move || {
                let send = |command| {
                    tx.send(command).unwrap();
                    waker.wake();
                };
                send(Command::Log(Line::Step("Looking for an existing sandbox".into())));
                send(Command::Log(Line::Info("found 'claude-w'".into())));
                send(Command::Log(Line::Step("Sandbox 'claude-w' exists with a different setup".into())));
                let (reply, answer) = mpsc::channel();
                let options = vec!["Replace — make it match".to_string(), "Merge — add what is missing".to_string()];
                send(Command::Ask { question: "How do you want to continue?".into(), options, reply });
                let answer = answer.recv().unwrap();
                send(Command::Log(Line::Step("Starting the sandbox".into())));
                let (agent_tx, agent_rx) = mpsc::channel();
                let wake = waker.clone();
                client::read(io::BufReader::new(agent_in), None, agent_tx, move || wake.wake());
                send(Command::Agent(client::Client::new(Box::new(client_out), None), agent_rx));
                answer
            })
        };

        let mut driver = Driver::new(info, tx, Some(waker));
        let code = driver.run(&mut session, sf, &rx).unwrap();
        session.close().unwrap();
        let answer = setup.join().unwrap();
        let seen = fake.join().unwrap();

        assert_eq!(answer, Some(1), "the second answer was picked with 2");
        assert_eq!(code, 0, "the person quit with Ctrl+D");
        let methods: Vec<&str> = seen.iter().map(|m| m["method"].as_str().unwrap_or("(response)")).collect();
        assert_eq!(methods, ["initialize", "session/new", "session/set_config_option", "session/prompt", "(response)"]);
        assert_eq!(seen[1]["params"]["cwd"], "/w");
        assert_eq!(seen[2]["params"], json!({"sessionId": "S", "configId": "model", "value": "haiku"}));
        assert_eq!(seen[3]["params"]["prompt"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(
            seen[4],
            json!({"jsonrpc": "2.0", "id": "perm",
            "result": {"outcome": {"outcome": "selected", "optionId": "allow-1"}}})
        );

        let output = output.borrow();
        let has = |text: &str| assert!(output.contains(text), "{text} in {output}");
        has(r#""role":"sand.session""#);
        has("tsp;s;");
        has(r#""role":"sand.composer""#);
        assert!(!output.contains("omp.editor"), "Tern folds omp's composer when the pane is not typed in");
        has("Looking for an existing sandbox");
        has(r#""variant":"summary""#);
        has(r#""append","lo"]"#);
        has(r#""approved":"once""#);
        has(r#""target":"rm -rf build""#);
        has("Haiku 5.5");
        has(r#""label":"25%""#);
        has("$0.12");
    }

    #[test]
    fn commands_complete_and_sessions_resume() {
        let output = Rc::new(RefCell::new(String::new()));
        let pane = Pane::new(
            &output,
            vec![
                // `/he` completes to `/help` with Tab; Enter runs it.
                after(r#""sendable":true"#, "/he"),
                after(r#""role":"omp.autocomplete""#, "\t\r"),
                after("/sessions — Browse", "/sessions\r"),
                // The second session, with the arrow keys.
                after("Fix the build", "\x1b[B\r"),
                after("Resumed · Add a parser", [0x03, 0x03]),
            ],
        );
        let mut session = session(pane);
        let info = Info { product: "Claude Code".into(), sandbox: "claude-w".into(), ..Info::default() };
        let sf = driver::open(&mut session, &info).unwrap();
        let waker = session.waker().unwrap();
        let (tx, rx) = mpsc::channel();

        let (from_sand, client_out) = io::pipe().unwrap();
        let (agent_in, mut to_sand) = io::pipe().unwrap();
        let fake = thread::spawn(move || {
            let mut lines = io::BufReader::new(from_sand).lines();
            let mut seen: Vec<Value> = Vec::new();
            let mut next = || -> Option<Value> { serde_json::from_str(&lines.next()?.ok()?).ok() };
            let mut say = |message: Value| writeln!(to_sand, "{message}").unwrap();
            let answer = |id: &Value, result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            let update = |update: Value| {
                json!({"jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": "S2", "update": update}})
            };
            while let Some(message) = next() {
                let id = &message["id"];
                match message["method"].as_str() {
                    Some("initialize") => {
                        say(answer(id, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}})))
                    }
                    Some("session/new") => {
                        say(answer(id, json!({"sessionId": "S1"})));
                        say(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "S1",
                            "update": {"sessionUpdate": "available_commands_update",
                                "availableCommands": [{"name": "init", "description": "Write CLAUDE.md"}]}}}));
                    }
                    Some("session/list") => say(answer(
                        id,
                        json!({"sessions": [
                        {"sessionId": "S9", "cwd": "/w", "title": "Fix the build", "updatedAt": "2026-11-02T10:00:00Z"},
                        {"sessionId": "S2", "cwd": "/w", "title": "Add a parser", "updatedAt": "2026-11-01T09:00:00Z"}]}),
                    )),
                    Some("session/load") => {
                        say(update(
                            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "parse it"}}),
                        ));
                        say(update(
                            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Parsed."}}),
                        ));
                        say(answer(id, json!({})));
                    }
                    _ => {}
                }
                seen.push(message);
            }
            seen
        });
        let (agent_tx, agent_rx) = mpsc::channel();
        let wake = waker.clone();
        client::read(io::BufReader::new(agent_in), None, agent_tx, move || wake.wake());
        tx.send(Command::Agent(client::Client::new(Box::new(client_out), None), agent_rx)).unwrap();

        let mut driver = Driver::new(info, tx, Some(waker));
        let code = driver.run(&mut session, sf, &rx).unwrap();
        session.close().unwrap();
        let seen = fake.join().unwrap();

        assert_eq!(code, 0);
        let methods: Vec<&str> = seen.iter().filter_map(|m| m["method"].as_str()).collect();
        // The first list is the welcome card's quiet lookup, the second the picker's.
        assert_eq!(methods, ["initialize", "session/new", "session/list", "session/list", "session/load"]);
        assert_eq!(seen[3]["params"], json!({"cwd": "/w"}));
        assert_eq!(seen[4]["params"], json!({"sessionId": "S2", "cwd": "/w", "mcpServers": []}));
        assert!(!seen.iter().any(|m| m["method"] == "session/prompt"), "sand's commands never reach the agent");

        let output = output.borrow();
        let has = |text: &str| assert!(output.contains(text), "{text} in {output}");
        has("/sessions — Browse this project");
        has("Write CLAUDE.md");
        has(r#""role":"sand.sessions""#);
        has("Continue “Fix the build”");
        has("parse it");
        has("Parsed.");
    }
}
