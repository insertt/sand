//! The thread that owns the Tern session from the first step of setup to the
//! end of the chat: it draws what setup reports and asks, then starts the
//! agent and speaks ACP to it, while keys and Tern's events come in on the pty.

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tern_sdk::{Input, Key, Session, Surface, SurfaceOptions, Waker};

use super::client::{self, Call, Client, Incoming};
use super::commands::{self, Builtin};
use super::composer::Composer;
use super::model::{Approval, Entry, Mode, Question, SessionEntry, SessionPicker, Setting, Transcript, Turn};
use super::view::{EDITOR, Info, Msg, STYLE, view};
use super::{ChatSpec, Command, Update};
use crate::log::Line;

/// How long the agent may take to exit once its input is closed.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Lines of the agent's log shown when it exits on its own.
const LOG_TAIL: usize = 12;

/// How often the git branch, and the API and MCP servers, are looked at again.
const BRANCH_EVERY: Duration = Duration::from_secs(5);
const PROBE_EVERY: Duration = Duration::from_secs(30);

/// The agent, once setup is done.
struct Agent {
    client: Client,
    rx: Receiver<Incoming>,
    child: Option<Child>,
    log: Option<PathBuf>,
    cwd: String,
    open: bool,
}

pub struct Driver {
    pub t: Transcript,
    pub composer: Composer,
    pub info: Info,
    agent: Option<Agent>,
    session: Option<String>,
    asked_mode: Option<String>,
    asked_model: Option<String>,
    /// What the agent said it can do with sessions (`initialize`).
    can_load: bool,
    can_resume: bool,
    /// The session to go back to if resuming another fails.
    previous: Option<String>,
    /// Where the answer to the question on screen goes.
    reply: Option<Sender<Option<usize>>>,
    /// Commands, for the probes started with the chat.
    commands: Sender<Command>,
    waker: Option<Waker>,
    stop_probes: Arc<AtomicBool>,
    quit: bool,
    /// The person pressed Ctrl+C during setup.
    pub interrupted: bool,
    /// The chat ended because the person quit (exit code 0).
    by_person: bool,
    closed: bool,
}

/// Opens the surface, with sand's stylesheet for the composer.
pub fn open(session: &mut Session<Msg>, info: &Info) -> Result<Surface> {
    let title = format!("{} · {}", info.product, info.sandbox);
    let sf = session.open(SurfaceOptions::inline().title(title).role("sand.session"))?;
    session.stylesheet(sf, "sand", Some(STYLE))?;
    Ok(sf)
}

impl Driver {
    pub fn new(info: Info, commands: Sender<Command>, waker: Option<Waker>) -> Self {
        Self {
            t: Transcript::default(),
            composer: Composer::default(),
            info,
            agent: None,
            session: None,
            asked_mode: None,
            asked_model: None,
            can_load: false,
            can_resume: false,
            previous: None,
            reply: None,
            commands,
            waker,
            stop_probes: Arc::new(AtomicBool::new(false)),
            quit: false,
            interrupted: false,
            by_person: false,
            closed: false,
        }
    }

    /// Runs the session in `sf` until the chat ends or setup closes it.
    /// Answers the exit code.
    pub fn run(&mut self, session: &mut Session<Msg>, sf: Surface, rx: &Receiver<Command>) -> Result<i32> {
        session.render(sf, view(&self.t, &self.composer, &self.info))?;
        session.focus(sf, Some(EDITOR))?;
        // Without a waker (a terminal that cannot be interrupted) the channels are polled.
        let poll = self.waker.is_none().then_some(Duration::from_millis(50));
        let result = self.event_loop(session, sf, rx, poll);
        self.stop_probes.store(true, Ordering::Relaxed);
        if let Some(agent) = self.agent.take() {
            let Agent { client, mut child, .. } = agent;
            // The agent ends once its input closes.
            drop(client);
            if let Some(child) = child.as_mut() {
                finish(child);
            }
        }
        result?;
        Ok(if self.by_person || self.closed { 0 } else { 1 })
    }

    fn event_loop(
        &mut self,
        session: &mut Session<Msg>,
        sf: Surface,
        rx: &Receiver<Command>,
        poll: Option<Duration>,
    ) -> Result<()> {
        while !self.quit {
            let mut changed = false;
            loop {
                match rx.try_recv() {
                    Ok(command) => {
                        self.command(command);
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    // Setup went away without closing: nothing more will come.
                    Err(TryRecvError::Disconnected) if self.agent.is_none() => {
                        self.closed = true;
                        return Ok(());
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            if self.closed {
                return Ok(());
            }
            changed |= self.drain_agent();
            if changed {
                session.render(sf, view(&self.t, &self.composer, &self.info))?;
            }
            if self.quit {
                break;
            }
            match session.next(poll)? {
                None => continue,
                Some(Input::Key(key)) => {
                    // Ctrl+C stops setup, as it would on a terminal.
                    if key.is("ctrl+c") && self.t.turn == Turn::Setup {
                        self.interrupted = true;
                        return Ok(());
                    }
                    self.key(&key);
                }
                Some(Input::Msg(Msg::Focus, _)) => {
                    session.focus(sf, Some(EDITOR))?;
                    continue;
                }
                Some(Input::Msg(msg, _)) => {
                    self.msg(msg);
                    // A click on a button never leaves the composer without the keys.
                    session.focus(sf, Some(EDITOR))?;
                }
                Some(Input::Event(_) | Input::Clipboard(_)) => continue,
            }
            session.render(sf, view(&self.t, &self.composer, &self.info))?;
        }
        Ok(())
    }

    fn drain_agent(&mut self) -> bool {
        let mut changed = false;
        while let Some(agent) = self.agent.as_mut().filter(|a| a.open) {
            let message = match agent.rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => Incoming::Closed,
            };
            if message == Incoming::Closed {
                agent.open = false;
            }
            self.incoming(message);
            changed = true;
        }
        changed
    }

    pub fn command(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::Log(line) if self.t.turn == Turn::Setup => self.t.setup.log(line, now),
            // Once the chat runs, only what needs attention is shown.
            Command::Log(Line::Warn(text)) => self.t.entries.push(Entry::Notice(text)),
            Command::Log(Line::Error(text)) => self.t.error(text, None),
            Command::Log(_) => {}
            Command::Ask { question, options, reply } => {
                self.t.question = Some(Question { text: question, options });
                self.reply = Some(reply);
            }
            Command::Update(update) => self.update(update),
            Command::Chat(spec) => self.start(*spec, now),
            Command::Close => self.closed = true,
            #[cfg(test)]
            Command::Agent(client, rx) => self.attach(client, rx, None, None, "/w".to_string()),
        }
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Api(api) => self.info.api = Some(api),
            Update::Mcp(mcp) => self.info.mcp = mcp,
            Update::Branch(branch) => self.info.branch = branch,
            Update::Mounts(mounts) => self.info.mounts = mounts,
        }
    }

    /// Setup is done: starts the agent and the probes that keep the status fresh.
    fn start(&mut self, spec: ChatSpec, now: Instant) {
        self.t.setup.finish(now);
        self.t.turn = Turn::Starting;
        let ChatSpec { sbx, launch, log, paths, cwd, profile, sandbox, mcp, workspace } = spec;
        self.probe(sbx.clone(), profile, sandbox, mcp, workspace);
        let spawned = (|| -> Result<Child> {
            if let Some(parent) = log.parent() {
                fs::create_dir_all(parent)?;
            }
            sbx.spawn_piped(launch, File::create(&log)?)
        })();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                self.t.exited(format!("{} could not be started: {err:#}", self.info.product), None, now);
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        let stdout = child.stdout.take().expect("piped");
        client::read(BufReader::new(stdout), paths.clone(), tx, move || {
            if let Some(waker) = &waker {
                waker.wake();
            }
        });
        let client = Client::new(Box::new(child.stdin.take().expect("piped")), paths);
        self.attach(client, rx, Some(child), Some(log), cwd);
    }

    /// Talks to an agent from now on, starting with `initialize`.
    pub fn attach(
        &mut self,
        client: Client,
        rx: Receiver<Incoming>,
        child: Option<Child>,
        log: Option<PathBuf>,
        cwd: String,
    ) {
        self.t.setup.finish(Instant::now());
        self.t.turn = Turn::Starting;
        self.agent = Some(Agent { client, rx, child, log, cwd, open: true });
        let sent = self.client().map(Client::initialize);
        self.check(sent);
    }

    /// Looks at the branch, the API and the MCP servers again now and then.
    fn probe(
        &self,
        sbx: crate::sbx::Sbx,
        profile: crate::harness::Profile,
        sandbox: String,
        mcp: Vec<String>,
        workspace: Option<PathBuf>,
    ) {
        let (tx, waker, stop) = (self.commands.clone(), self.waker.clone(), Arc::clone(&self.stop_probes));
        thread::spawn(move || {
            let send = |update: Update| {
                let sent = tx.send(Command::Update(update)).is_ok();
                if let Some(waker) = &waker {
                    waker.wake();
                }
                sent
            };
            let mut probed = Instant::now();
            loop {
                if let Some(dir) = &workspace
                    && !send(Update::Branch(branch(dir)))
                {
                    return;
                }
                if probed.elapsed() >= PROBE_EVERY {
                    probed = Instant::now();
                    if !send(Update::Mcp(crate::mcp::status(&sbx, &mcp))) {
                        return;
                    }
                    if profile.api_check_url.is_some()
                        && !send(Update::Api(crate::runtime::check_api(&sbx, &sandbox, &profile)))
                    {
                        return;
                    }
                }
                let slept = Instant::now();
                while slept.elapsed() < BRANCH_EVERY {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
        });
    }

    fn client(&mut self) -> Option<&mut Client> {
        self.agent.as_mut().map(|a| &mut a.client)
    }

    /// A failed write to the agent means it is gone.
    fn check(&mut self, sent: Option<io::Result<()>>) {
        if let Some(Err(err)) = sent
            && self.t.turn != Turn::Exited
        {
            let detail = log_tail(self.agent.as_ref().and_then(|a| a.log.as_deref()));
            self.t.exited(format!("Lost the connection to {}: {err}", self.info.product), detail, Instant::now());
        }
    }

    pub fn incoming(&mut self, message: Incoming) {
        let now = Instant::now();
        match message {
            Incoming::Update(update) => self.t.apply(&update, now),
            Incoming::Permission { id, params } => {
                // A request for another session than ours cannot be answered by the person.
                if self.session.as_deref().is_some_and(|s| params["sessionId"].as_str().is_some_and(|p| p != s)) {
                    let sent = self.client().map(|c| c.permit(id, None));
                    self.check(sent);
                } else {
                    self.t.ask(id, &params, now);
                }
            }
            Incoming::Unsupported { id, method } => {
                let sent = self.client().map(|c| c.respond_unsupported(id, &method));
                self.check(sent);
            }
            Incoming::Response { id, result } => {
                if let Some(call) = self.client().and_then(|c| c.call(id)) {
                    self.response(call, result, now);
                }
            }
            Incoming::Closed => {
                if self.t.turn != Turn::Exited {
                    let detail = log_tail(self.agent.as_ref().and_then(|a| a.log.as_deref()));
                    self.t.exited(format!("{} exited", self.info.product), detail, now);
                }
            }
        }
    }

    fn response(&mut self, call: Call, result: Result<Value, String>, now: Instant) {
        match (call, result) {
            (Call::Initialize, Ok(result)) => {
                let caps = &result["agentCapabilities"];
                self.can_load = caps["loadSession"].as_bool().unwrap_or(false);
                self.can_resume = !caps["sessionCapabilities"]["resume"].is_null();
                let cwd = self.agent.as_ref().map(|a| a.cwd.clone()).unwrap_or_default();
                let sent = self.client().map(|c| c.new_session(&cwd));
                self.check(sent);
            }
            (Call::NewSession, Ok(result)) => {
                self.session = result["sessionId"].as_str().map(str::to_string);
                self.ready(&result);
            }
            (Call::LoadSession | Call::ResumeSession, Ok(result)) => {
                let title = self.t.resuming.take().unwrap_or_default();
                self.t.entries.push(Entry::Resumed(title));
                self.ready(&result);
            }
            (Call::LoadSession | Call::ResumeSession, Err(err)) => {
                self.t.resuming = None;
                self.session = self.previous.take();
                self.t.error(format!("The session could not be resumed: {err}"), None);
                self.t.turn = Turn::Idle;
            }
            (Call::ListSessions, result) => {
                let Some(picker) = self.t.picker.as_mut() else { return };
                picker.loading = false;
                match result {
                    Ok(result) => {
                        picker.sessions = result["sessions"]
                            .as_array()
                            .map(|all| all.iter().filter_map(SessionEntry::parse).collect())
                            .unwrap_or_default();
                        if picker.sessions.is_empty() {
                            picker.error = Some("No sessions yet in this project".to_string());
                        }
                    }
                    Err(err) => {
                        let product = &self.info.product;
                        picker.error = Some(format!("{product} does not list its sessions over ACP ({err})"));
                    }
                }
            }
            (Call::Initialize | Call::NewSession, Err(err)) => {
                self.t.exited(format!("{} could not start a session: {err}", self.info.product), None, now);
            }
            (Call::Prompt, Ok(result)) => self.t.turn_ended(result["stopReason"].as_str(), &result["usage"], now),
            (Call::Prompt, Err(err)) => {
                self.t.error(err, None);
                self.t.turn_ended(None, &Value::Null, now);
            }
            (Call::SetMode, Ok(_)) => self.t.mode = self.asked_mode.take().or(self.t.mode.take()),
            (Call::SetModel, Ok(_)) => self.t.model = self.asked_model.take().or(self.t.model.take()),
            (Call::SetConfig, Ok(result)) => self.t.set_options(&result["configOptions"]),
            (Call::SetModel | Call::SetConfig, Err(err)) => {
                self.asked_model = None;
                self.t.error(format!("The setting could not be changed: {err}"), None);
            }
            (Call::SetMode, Err(err)) => {
                self.asked_mode = None;
                self.t.error(format!("The mode could not be changed: {err}"), None);
            }
        }
    }

    /// A session is ready (new or resumed): takes its modes, models and
    /// settings, and sends a prompt that waited for it.
    fn ready(&mut self, result: &Value) {
        let modes = &result["modes"];
        if let Some(available) = modes["availableModes"].as_array() {
            self.t.modes = available
                .iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    let name = m["name"].as_str().map_or_else(|| id.clone(), str::to_string);
                    Some(Mode { id, name })
                })
                .collect();
        }
        if let Some(mode) = modes["currentModeId"].as_str() {
            self.t.mode = Some(mode.to_string());
        }
        self.t.set_models(&result["models"]);
        self.t.set_options(&result["configOptions"]);
        self.t.turn = Turn::Idle;
        if let Some(text) = self.t.queued.take() {
            self.send(text);
        }
    }

    fn cwd(&self) -> String {
        self.agent.as_ref().map(|a| a.cwd.clone()).unwrap_or_default()
    }

    /// Runs one of sand's commands.
    fn builtin(&mut self, command: Builtin) {
        match command {
            Builtin::Sessions | Builtin::Resume(None) => {
                self.t.picker = Some(SessionPicker { loading: true, ..SessionPicker::default() });
                let cwd = self.cwd();
                let sent = self.client().map(|c| c.list_sessions(&cwd));
                self.check(sent);
            }
            Builtin::Resume(Some(id)) => self.resume(SessionEntry { id, title: None, cwd: None, updated: None }),
            Builtin::New => {
                self.t.reset_conversation();
                self.session = None;
                self.t.turn = Turn::Starting;
                let cwd = self.cwd();
                let sent = self.client().map(|c| c.new_session(&cwd));
                self.check(sent);
            }
            Builtin::Help => {
                let agent = self.t.commands.iter().map(|c| format!("/{}", c.name)).collect::<Vec<_>>().join(" ");
                let sand: Vec<String> =
                    commands::builtins().map(|c| format!("/{} — {}", c.name, c.description)).collect();
                let mut text = sand.join("\n");
                if !agent.is_empty() {
                    text.push_str(&format!("\n{}: {agent}", self.info.product));
                }
                self.t.entries.push(Entry::Notice(text));
            }
        }
    }

    /// Leaves this session for `session`, which the agent loads (replaying
    /// its conversation) or resumes.
    fn resume(&mut self, session: SessionEntry) {
        self.t.picker = None;
        if !self.can_load && !self.can_resume {
            let product = &self.info.product;
            self.t.error(format!("{product} cannot resume sessions over ACP"), None);
            return;
        }
        self.previous = self.session.replace(session.id.clone());
        self.t.reset_conversation();
        self.t.resuming = Some(session.label());
        self.t.turn = Turn::Starting;
        let (cwd, load) = (session.cwd.unwrap_or_else(|| self.cwd()), self.can_load);
        let sent = self.client().map(|c| c.open_session(&session.id, &cwd, load));
        self.check(sent);
    }

    fn send(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let (Turn::Idle, Some(session)) = (self.t.turn, self.session.clone()) else {
            // Before the agent is ready, one prompt waits for it.
            if matches!(self.t.turn, Turn::Setup | Turn::Starting) && self.t.queued.is_none() {
                self.t.queued = Some(text);
            }
            return;
        };
        self.t.prompt(&text, Instant::now());
        let sent = self.client().map(|c| c.prompt(&session, &text));
        self.check(sent);
    }

    fn submit(&mut self) {
        if self.composer.is_empty() || matches!(self.t.turn, Turn::Working | Turn::Exited) {
            return;
        }
        if self.t.turn != Turn::Idle && self.t.queued.is_some() {
            return;
        }
        let text = self.composer.take();
        match commands::parse(&text) {
            Some(command) if self.t.turn == Turn::Idle => self.builtin(command),
            // sand's commands wait for the session; the agent's go as a prompt.
            Some(_) => self.composer.set(&text),
            None => self.send(text),
        }
    }

    /// The commands completing the draft, unless dismissed.
    fn completions(&self) -> Vec<commands::SlashCommand> {
        if self.composer.dismissed {
            return Vec::new();
        }
        commands::complete(&self.composer.draft, &self.t.commands)
    }

    /// Takes the completion `index`: a command that takes nothing runs, the
    /// rest wait for what follows its name.
    fn accept(&mut self, index: usize, run: bool) {
        let Some(command) = self.completions().into_iter().nth(index) else { return };
        if run && command.hint.is_none() {
            self.composer.set(&format!("/{}", command.name));
            self.submit();
        } else {
            self.composer.set(&format!("/{} ", command.name));
        }
    }

    fn picker_key(&mut self, key: &Key) {
        let Some(picker) = self.t.picker.as_mut() else { return };
        let count = picker.sessions.len();
        if key.is("escape") || key.is("ctrl+c") {
            self.t.picker = None;
        } else if key.is("up") && count > 0 {
            picker.selected = (picker.selected + count - 1) % count;
        } else if key.is("down") && count > 0 {
            picker.selected = (picker.selected + 1) % count;
        } else if key.is("enter") && count > 0 {
            let session = picker.sessions[picker.selected.min(count - 1)].clone();
            self.resume(session);
        }
    }

    fn answer(&mut self, approval: Approval) {
        if let Some((id, option)) = self.t.answer(approval) {
            let sent = self.client().map(|c| c.permit(id, option.as_deref()));
            self.check(sent);
        }
    }

    /// Answers setup's question (`None`: abort).
    fn choose(&mut self, choice: Option<usize>) {
        let Some(question) = self.t.question.take() else { return };
        let choice = choice.filter(|i| *i < question.options.len());
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(choice);
        }
        let answer = choice.map_or_else(|| "Aborted".to_string(), |i| question.options[i].clone());
        self.t.setup.log(Line::Info(format!("{} {answer}", question.text)), Instant::now());
    }

    /// Stops the turn; a permission still waiting is answered as cancelled.
    fn cancel(&mut self) {
        let Some(session) = self.session.clone() else { return };
        if let Some(permission) = self.t.permission.take() {
            let sent = self.client().map(|c| c.permit(permission.request, None));
            self.check(sent);
        }
        let sent = self.client().map(|c| c.cancel(&session));
        self.check(sent);
    }

    fn quit(&mut self) {
        self.by_person = self.t.turn != Turn::Exited;
        self.quit = true;
    }

    pub fn msg(&mut self, msg: Msg) {
        match msg {
            Msg::Send(text) => self.send(text),
            Msg::Submit => self.submit(),
            Msg::Stop => self.cancel(),
            Msg::Answer(approval) => self.answer(approval),
            Msg::Choose(choice) => self.choose(Some(choice)),
            Msg::Pick(setting, value) => self.pick(setting, &value),
            Msg::Complete(index) => self.accept(index, true),
            Msg::Session(index) => {
                if let Some(session) = self.t.picker.as_ref().and_then(|p| p.sessions.get(index)).cloned() {
                    self.resume(session);
                }
            }
            Msg::HoverSession(index) => {
                if let Some(picker) = self.t.picker.as_mut() {
                    picker.selected = index;
                }
            }
            Msg::ClosePicker => self.t.picker = None,
            Msg::Focus => {}
        }
    }

    pub fn key(&mut self, key: &Key) {
        if self.t.picker.is_some() {
            return self.picker_key(key);
        }
        let completions = self.completions();
        if !completions.is_empty() {
            let count = completions.len();
            if key.is("escape") {
                self.composer.dismissed = true;
                return;
            } else if key.is("up") {
                self.composer.pick = (self.composer.pick + count - 1) % count;
                return;
            } else if key.is("down") {
                self.composer.pick = (self.composer.pick + 1) % count;
                return;
            } else if key.is("tab") || key.is("enter") {
                return self.accept(self.composer.pick.min(count - 1), key.is("enter"));
            }
        }
        if let Some(question) = &self.t.question {
            let count = question.options.len();
            if key.is("escape") {
                return self.choose(None);
            }
            if key.is("enter") && self.composer.is_empty() {
                return self.choose(Some(0));
            }
            if let Some(n) = key.typed().and_then(|t| t.parse::<usize>().ok()).filter(|n| (1..=count).contains(n)) {
                return self.choose(Some(n - 1));
            }
        }
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
            if self.composer.is_empty() && !working && self.t.turn != Turn::Setup {
                self.quit();
            }
        } else if key.is("shift+tab") {
            self.next_mode();
        } else if key.is("enter") {
            self.submit();
        } else {
            self.composer.key(key);
        }
    }

    /// Switches `setting` to `value`, one of the agent's choices.
    fn pick(&mut self, setting: Setting, value: &str) {
        let Some(session) = self.session.clone() else { return };
        let sent = match self.t.option(setting) {
            Some(option) if option.current != value => {
                let id = option.id.clone();
                self.client().map(|c| c.set_config(&session, &id, value))
            }
            None if setting == Setting::Model && self.t.model.as_deref() != Some(value) => {
                self.asked_model = Some(value.to_string());
                self.client().map(|c| c.set_model(&session, value))
            }
            None if setting == Setting::Mode && self.t.mode.as_deref() != Some(value) => {
                self.asked_mode = Some(value.to_string());
                self.client().map(|c| c.set_mode(&session, value))
            }
            _ => return,
        };
        self.check(sent);
    }

    /// Shift+Tab: the next mode, as the mode chip would pick it.
    fn next_mode(&mut self) {
        if self.t.choices(Setting::Mode).0.len() < 2 {
            return;
        }
        if let Some(mode) = self.t.next_mode() {
            self.pick(Setting::Mode, &mode);
        }
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

/// The branch checked out in `dir` (or the commit, detached), as the status line shows it.
fn branch(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !text.is_empty()).then_some(text)
    };
    git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).or_else(|| git(&["rev-parse", "--short", "HEAD"]))
}
