//! What the native session shows: the sandbox's setup as sand reports it,
//! then the transcript built from the agent's ACP messages, the plan, a
//! permission waiting for an answer, usage and where the turn is. Pure state;
//! `view` draws it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::commands::SlashCommand;
use super::files::Link;
use crate::log::Line;

/// An image pasted into the composer, sent with a prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub data: std::sync::Arc<Vec<u8>>,
    pub mime: String,
    pub name: String,
    /// Width and height, when sand can tell.
    pub dims: Option<(u32, u32)>,
    /// Tern's blob for its thumbnail, once sent.
    pub blob: Option<String>,
}

/// What the person sends: text, the files it mentions, and images; and
/// what sand tells the agent with it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<Image>,
    pub links: Vec<Link>,
    /// sand's notes for the agent (a host allowed since), before the text.
    pub context: Vec<String>,
}

impl Prompt {
    #[cfg(test)]
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// A message of the person's, with the images sent with it.
    User(String, Vec<Image>),
    Agent(String),
    Thought(Thought),
    Tool(ToolCall),
    /// Something sand says, such as a cancelled turn.
    Notice(String),
    /// Something that went wrong, with the agent's last words when there are any.
    Error {
        text: String,
        detail: Option<String>,
    },
    /// The end of a turn: how long it took and what it cost.
    TurnEnd(TurnUsage),
    /// Where a resumed session's earlier conversation ends.
    Resumed(String),
}

/// A note above the composer about something that happened while the pane was hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    /// `success`, `warning` or `error`.
    pub tone: &'static str,
}

/// A session the agent can resume (ACP's `session/list`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// When it last changed, as the agent says (ISO 8601).
    pub updated: Option<String>,
}

impl SessionEntry {
    pub fn parse(session: &Value) -> Option<Self> {
        let text = |key: &str| session[key].as_str().filter(|v| !v.is_empty()).map(str::to_string);
        Some(Self { id: text("sessionId")?, title: text("title"), cwd: text("cwd"), updated: text("updatedAt") })
    }

    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| format!("Session {}", self.id.chars().take(8).collect::<String>()))
    }
}

/// The session picker `/sessions` and `/resume` open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionPicker {
    pub sessions: Vec<SessionEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

/// How much of a Claude subscription's windows is spent, in percent. Known once
/// the agent reports a rate limit, which it does only for Pro and Max plans.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

impl Limits {
    /// Takes in the adapter's `_claude/rateLimit`, whose `utilization` is a fraction.
    fn apply(&mut self, info: &Value) {
        let Some(used) = info["utilization"].as_f64() else { return };
        let percent = (used * 100.0).clamp(0.0, 100.0);
        match info["rateLimitType"].as_str() {
            Some("five_hour") => self.five_hour = Some(percent),
            Some("seven_day") => self.seven_day = Some(percent),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnUsage {
    pub took: Duration,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
    /// Why the turn ended early (`Interrupted`).
    pub stopped: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Running,
    Done,
    Warned,
    Failed,
}

/// One step of the setup, as sand's log reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub title: String,
    /// What sand said about it, in order.
    pub notes: Vec<String>,
    /// What a failed command printed.
    pub output: Option<String>,
    /// Something slow under way.
    pub waiting: Option<String>,
    pub status: StepStatus,
    pub started: Instant,
    pub took: Option<Duration>,
}

#[derive(Debug, PartialEq)]
pub struct Setup {
    pub steps: Vec<Step>,
    pub started: Instant,
    /// Set once the agent starts (or setup ends otherwise).
    pub took: Option<Duration>,
}

impl Setup {
    pub fn new(now: Instant) -> Self {
        Self { steps: Vec::new(), started: now, took: None }
    }

    /// The step notes go to: the running one, else the last.
    fn current(&mut self, now: Instant) -> &mut Step {
        if self.steps.is_empty() {
            self.steps.push(Step::new("Preparing", StepStatus::Running, now));
        }
        let at = self.steps.iter().rposition(|s| s.status == StepStatus::Running).unwrap_or(self.steps.len() - 1);
        &mut self.steps[at]
    }

    fn settle(&mut self, now: Instant) {
        for step in &mut self.steps {
            if step.status == StepStatus::Running {
                step.status = StepStatus::Done;
                step.took = Some(now.saturating_duration_since(step.started));
                step.waiting = None;
            }
        }
    }

    pub fn log(&mut self, line: Line, now: Instant) {
        match line {
            Line::Step(title) => {
                self.settle(now);
                self.steps.push(Step::new(title, StepStatus::Running, now));
            }
            Line::Info(note) | Line::Ok(note) | Line::Command(note) => {
                let step = self.current(now);
                step.notes.push(note);
                step.waiting = None;
            }
            Line::Wait(what) => self.current(now).waiting = Some(what),
            Line::Warn(note) => {
                self.steps.push(Step { took: Some(Duration::ZERO), ..Step::new(note, StepStatus::Warned, now) })
            }
            Line::Error(note) => {
                let step = self.current(now);
                step.status = StepStatus::Failed;
                step.waiting = None;
                step.notes.push(note);
            }
            Line::Output(text) => self.current(now).output.get_or_insert_default().push_str(&text),
        }
    }

    pub fn finish(&mut self, now: Instant) {
        self.settle(now);
        self.took.get_or_insert(now.saturating_duration_since(self.started));
    }

    pub fn failed(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }
}

impl Step {
    fn new(title: impl Into<String>, status: StepStatus, now: Instant) -> Self {
        Self { title: title.into(), notes: Vec::new(), output: None, waiting: None, status, started: now, took: None }
    }
}

/// A question setup asks, answered with one of `options`.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thought {
    pub text: String,
    pub started: Instant,
    /// Set once anything else follows the thought.
    pub took: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ToolStatus {
    fn parse(status: &str) -> Option<Self> {
        Some(match status {
            "pending" => Self::Pending,
            "in_progress" => Self::Running,
            "completed" => Self::Done,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub fn settled(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Command,
    Path,
    Pattern,
    Query,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolContent {
    Text(String),
    Diff { path: String, old: Option<String>, new: String },
}

/// How the person answered a tool's permission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Once,
    Always,
    Denied,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    /// ACP's tool kind: read, edit, delete, move, search, execute, think, fetch, other.
    pub kind: String,
    pub status: ToolStatus,
    pub target: Option<(String, TargetKind)>,
    pub content: Vec<ToolContent>,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub approval: Option<Approval>,
}

/// A host the network policy blocked, as a tool's output said.
#[derive(Clone, Debug, PartialEq)]
pub struct Blocked {
    pub host: String,
    /// A local deny rule blocks it (it was refused before).
    pub ruled: bool,
    /// Allowed in the pane: sand is changing the policy.
    pub sent: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    /// pending, in_progress or completed.
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// allow_once, allow_always, reject_once or reject_always.
    pub kind: String,
}

/// A `session/request_permission` the agent waits on.
#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    /// The JSON-RPC id to answer.
    pub request: Value,
    pub tool: String,
    pub options: Vec<PermissionOption>,
}

impl Permission {
    /// The option of the first kind in `kinds` the agent offers.
    pub fn option(&self, kinds: &[&str]) -> Option<&PermissionOption> {
        kinds.iter().find_map(|kind| self.options.iter().find(|o| o.kind == *kind))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// sand is setting the sandbox up.
    Setup,
    /// The agent is starting: `initialize` and `session/new` are under way.
    Starting,
    Idle,
    Working,
    /// The agent is gone; any key closes the surface.
    Exited,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub id: String,
    pub name: String,
}

/// A session setting the agent offers (ACP's `configOptions`): its model,
/// its reasoning effort, and others.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOption {
    pub id: String,
    /// `model`, `thought_level`, `mode`, …
    pub category: String,
    pub current: String,
    /// (value, name) of each choice.
    pub choices: Vec<(String, String)>,
}

impl ConfigOption {
    fn parse(option: &Value) -> Option<Self> {
        let mut choices = Vec::new();
        fn collect(options: &Value, out: &mut Vec<(String, String)>) {
            for option in options.as_array().into_iter().flatten() {
                match option["value"].as_str() {
                    Some(value) => {
                        let name = option["name"].as_str().unwrap_or(value);
                        out.push((value.to_string(), name.to_string()));
                    }
                    // A group of choices.
                    None => collect(&option["options"], out),
                }
            }
        }
        collect(&option["options"], &mut choices);
        Some(Self {
            id: option["id"].as_str()?.to_string(),
            category: option["category"].as_str().unwrap_or_default().to_string(),
            current: option["currentValue"].as_str().unwrap_or_default().to_string(),
            choices,
        })
    }

    pub fn current_name(&self) -> &str {
        self.choices.iter().find(|(v, _)| *v == self.current).map_or(self.current.as_str(), |(_, n)| n.as_str())
    }

    fn is_effort(&self) -> bool {
        self.category == "thought_level" || ["effort", "reasoning", "thinking"].iter().any(|w| self.id.contains(w))
    }
}

/// Which setting a chip changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Model,
    /// The permission mode (ask, accept edits, bypass permissions, …).
    Mode,
    Effort,
}

#[derive(Debug)]
pub struct Transcript {
    pub setup: Setup,
    pub question: Option<Question>,
    /// Follow-ups written while the agent works (or starts), sent in order
    /// as each turn ends; paused after a stop or a failure.
    pub queue: Vec<Prompt>,
    pub queue_paused: bool,
    /// The pane is hidden (another tab): what happens meanwhile is noted.
    pub hidden: bool,
    /// Notes for the person, about what happened while the pane was hidden.
    pub notes: Vec<Note>,
    /// The session before this one, for the welcome card's Continue.
    pub last_session: Option<SessionEntry>,
    /// Network access the sandbox was blocked from, waiting for the person.
    pub network: Vec<crate::network::Approval>,
    /// Hosts a tool's output says the policy blocked with no approval to
    /// answer, waiting for the person.
    pub blocked: Vec<Blocked>,
    /// (tool call, host) pairs already offered, so that a tool's output
    /// sent again, or a dismissed host, is not offered twice.
    blocked_seen: HashSet<(String, String)>,
    /// Questions only the person may answer (a mount, a host), oldest first.
    pub confirms: Vec<String>,
    pub entries: Vec<Entry>,
    /// Tool call id to its index in `entries`.
    tools: HashMap<String, usize>,
    pub plan: Vec<PlanEntry>,
    pub permission: Option<Permission>,
    pub turn: Turn,
    pub modes: Vec<Mode>,
    pub mode: Option<String>,
    pub commands: Vec<SlashCommand>,
    /// The workspace's files and directories, for `@` mentions.
    pub files: Vec<String>,
    /// A directory above the workspace an `@../` mention reaches, and what is in it.
    pub above: Option<(String, Vec<String>)>,
    pub picker: Option<SessionPicker>,
    /// A session being resumed: the working line says so until the agent answers.
    pub resuming: Option<String>,
    /// A first press of an exit key (when, and which), waiting for the second.
    pub exit_armed: Option<(Instant, String)>,
    /// The model, by id, and the models the agent offers (ACP's `models`).
    pub model: Option<String>,
    pub models: Vec<Mode>,
    pub options: Vec<ConfigOption>,
    /// Context used and the window's size, in tokens.
    pub context: Option<(u64, u64)>,
    /// What the session has cost so far, in USD.
    pub cost: Option<f64>,
    /// The subscription limits; `None` for API keys and Vertex AI, which have none.
    pub limits: Option<Limits>,
    /// When the turn started, and the cost then.
    turn_start: Option<(Instant, Option<f64>)>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            setup: Setup::new(Instant::now()),
            question: None,
            queue: Vec::new(),
            queue_paused: false,
            hidden: false,
            notes: Vec::new(),
            last_session: None,
            network: Vec::new(),
            confirms: Vec::new(),
            entries: Vec::new(),
            tools: HashMap::new(),
            plan: Vec::new(),
            permission: None,
            turn: Turn::Setup,
            modes: Vec::new(),
            mode: None,
            commands: Vec::new(),
            files: Vec::new(),
            above: None,
            blocked: Vec::new(),
            blocked_seen: HashSet::new(),
            picker: None,
            resuming: None,
            exit_armed: None,
            model: None,
            models: Vec::new(),
            options: Vec::new(),
            context: None,
            cost: None,
            limits: None,
            turn_start: None,
        }
    }
}

/// The text of an ACP content block, or a stand-in for content without text.
fn block_text(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(str::to_string),
        "resource_link" => block["name"].as_str().or(block["uri"].as_str()).map(|n| format!("`{n}`")),
        "resource" => block["resource"]["uri"].as_str().map(|u| format!("`{u}`")),
        "image" => Some("*[image]*".to_string()),
        "audio" => Some("*[audio]*".to_string()),
        _ => None,
    }
}

/// What a tool acts on, from its input or the locations it reports.
fn target(raw_input: &Value, locations: &Value) -> Option<(String, TargetKind)> {
    let command = match &raw_input["command"] {
        Value::String(c) => Some(c.clone()),
        Value::Array(words) => Some(words.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    };
    if let Some(command) = command.filter(|c| !c.is_empty()) {
        return Some((command, TargetKind::Command));
    }
    if let Some(location) = locations.as_array().and_then(|l| l.first())
        && let Some(path) = location["path"].as_str()
    {
        let target = match location["line"].as_u64() {
            Some(line) => format!("{path}:{line}"),
            None => path.to_string(),
        };
        return Some((target, TargetKind::Path));
    }
    let field = |keys: &[&str]| keys.iter().find_map(|k| raw_input[*k].as_str().filter(|v| !v.is_empty()));
    if let Some(path) = field(&["file_path", "path", "notebook_path"]) {
        return Some((path.to_string(), TargetKind::Path));
    }
    if let Some(pattern) = field(&["pattern"]) {
        return Some((pattern.to_string(), TargetKind::Pattern));
    }
    field(&["url", "query"]).map(|q| (q.to_string(), TargetKind::Query))
}

fn tool_content(content: &Value) -> Vec<ToolContent> {
    let Some(items) = content.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| match item["type"].as_str()? {
            "content" => block_text(&item["content"]).map(ToolContent::Text),
            "diff" => Some(ToolContent::Diff {
                path: item["path"].as_str().unwrap_or_default().to_string(),
                old: item["oldText"].as_str().map(str::to_string),
                new: item["newText"].as_str().unwrap_or_default().to_string(),
            }),
            // Terminals are the client's; sand offers none.
            _ => None,
        })
        .collect()
}

impl ToolCall {
    fn new(id: &str, now: Instant) -> Self {
        Self {
            id: id.to_string(),
            title: String::new(),
            kind: "other".to_string(),
            status: ToolStatus::Pending,
            target: None,
            content: Vec::new(),
            started: now,
            finished: None,
            approval: None,
        }
    }

    /// Takes every field `fields` (a `tool_call` or `tool_call_update`) sets.
    fn merge(&mut self, fields: &Value, now: Instant) {
        if let Some(title) = fields["title"].as_str() {
            self.title = title.to_string();
        }
        if let Some(kind) = fields["kind"].as_str() {
            self.kind = kind.to_string();
        }
        if let Some(status) = fields["status"].as_str().and_then(ToolStatus::parse) {
            self.set_status(status, now);
        }
        if (fields.get("rawInput").is_some() || fields.get("locations").is_some())
            && let Some(target) = target(&fields["rawInput"], &fields["locations"])
        {
            self.target = Some(target);
        }
        if fields.get("content").is_some_and(|c| !c.is_null()) {
            self.content = tool_content(&fields["content"]);
        }
    }

    fn set_status(&mut self, status: ToolStatus, now: Instant) {
        if status.settled() && self.finished.is_none() {
            self.finished = Some(now);
        }
        self.status = status;
    }
}

impl Transcript {
    /// Applies one `session/update`.
    pub fn apply(&mut self, update: &Value, now: Instant) {
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => {
                let Some(text) = block_text(&update["content"]) else { return };
                self.settle_thought(now);
                match self.entries.last_mut() {
                    Some(Entry::Agent(message)) => message.push_str(&text),
                    _ => self.entries.push(Entry::Agent(text)),
                }
            }
            "user_message_chunk" => {
                let Some(text) = block_text(&update["content"]) else { return };
                self.settle_thought(now);
                match self.entries.last_mut() {
                    Some(Entry::User(message, _)) => message.push_str(&text),
                    _ => self.entries.push(Entry::User(text, Vec::new())),
                }
            }
            "agent_thought_chunk" => {
                let Some(text) = block_text(&update["content"]) else { return };
                match self.entries.last_mut() {
                    Some(Entry::Thought(thought)) if thought.took.is_none() => thought.text.push_str(&text),
                    _ => self.entries.push(Entry::Thought(Thought { text, started: now, took: None })),
                }
            }
            "tool_call" | "tool_call_update" => {
                let Some(id) = update["toolCallId"].as_str() else { return };
                self.tool_mut(id, now).merge(update, now);
                // A resumed session's replay tells of blocks long answered.
                if self.resuming.is_none() && update.get("content").is_some_and(|c| !c.is_null()) {
                    self.scan_blocks(id);
                }
            }
            "plan" => {
                self.plan = update["entries"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|e| PlanEntry {
                                content: e["content"].as_str().unwrap_or_default().to_string(),
                                status: e["status"].as_str().unwrap_or("pending").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "current_mode_update" => {
                if let Some(mode) = update["currentModeId"].as_str() {
                    self.mode = Some(mode.to_string());
                }
            }
            "usage_update" => {
                if let (Some(used), Some(size)) = (update["used"].as_u64(), update["size"].as_u64()) {
                    self.context = Some((used, size));
                }
                if let Some(cost) = update["cost"]["amount"].as_f64() {
                    self.cost = Some(cost);
                }
                let rate_limit = &update["_meta"]["_claude/rateLimit"];
                if rate_limit.is_object() {
                    self.limits.get_or_insert_with(Limits::default).apply(rate_limit);
                }
            }
            "config_option_update" => self.set_options(&update["configOptions"]),
            "current_model_update" => {
                if let Some(model) = update["currentModelId"].as_str().or(update["modelId"].as_str()) {
                    self.model = Some(model.to_string());
                }
            }
            "available_commands_update" => {
                self.commands = update["availableCommands"]
                    .as_array()
                    .map(|commands| {
                        commands
                            .iter()
                            .filter_map(|c| {
                                Some(SlashCommand {
                                    name: c["name"].as_str()?.trim_start_matches('/').to_string(),
                                    description: c["description"].as_str().unwrap_or_default().to_string(),
                                    hint: c["input"]["hint"].as_str().map(str::to_string),
                                    builtin: false,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            _ => {}
        }
    }

    /// The tool call `id`, added (after settling a thought) when it is new.
    fn tool_mut(&mut self, id: &str, now: Instant) -> &mut ToolCall {
        let index = match self.tools.get(id) {
            Some(&index) => index,
            None => {
                self.settle_thought(now);
                self.entries.push(Entry::Tool(ToolCall::new(id, now)));
                self.tools.insert(id.to_string(), self.entries.len() - 1);
                self.entries.len() - 1
            }
        };
        match &mut self.entries[index] {
            Entry::Tool(tool) => tool,
            _ => unreachable!("tool index points at a tool"),
        }
    }

    #[cfg(test)]
    pub fn tool(&self, id: &str) -> Option<&ToolCall> {
        match self.entries.get(*self.tools.get(id)?) {
            Some(Entry::Tool(tool)) => Some(tool),
            _ => None,
        }
    }

    /// A thought still streaming ends when anything else arrives.
    fn settle_thought(&mut self, now: Instant) {
        if let Some(Entry::Thought(thought)) = self.entries.last_mut()
            && thought.took.is_none()
        {
            thought.took = Some(now.saturating_duration_since(thought.started));
        }
    }

    /// The person sent a prompt; the turn starts.
    pub fn prompt(&mut self, prompt: &Prompt, now: Instant) {
        self.settle_thought(now);
        self.entries.push(Entry::User(prompt.text.clone(), prompt.images.clone()));
        self.turn = Turn::Working;
        self.turn_start = Some((now, self.cost));
    }

    /// The prompt's turn ended with ACP's `stopReason`, and the usage the
    /// response reports (if any).
    pub fn turn_ended(&mut self, stop_reason: Option<&str>, usage: &Value, now: Instant) {
        self.settle_thought(now);
        self.settle_tools(ToolStatus::Cancelled, now);
        let stopped = match stop_reason {
            Some("cancelled") => Some("Interrupted"),
            Some("max_tokens") => Some("token limit reached"),
            Some("max_turn_requests") => Some("request limit reached"),
            Some("refusal") => Some("refused"),
            _ => None,
        };
        let tokens = usage["totalTokens"].as_u64().or_else(|| {
            let (input, output) = (usage["inputTokens"].as_u64(), usage["outputTokens"].as_u64());
            (input.is_some() || output.is_some()).then(|| input.unwrap_or(0) + output.unwrap_or(0))
        });
        if let Some((started, cost_before)) = self.turn_start.take() {
            let cost = match (self.cost, cost_before) {
                (Some(now), Some(before)) => Some(now - before),
                (cost, None) => cost,
                (None, Some(_)) => None,
            };
            self.entries.push(Entry::TurnEnd(TurnUsage {
                took: now.saturating_duration_since(started),
                tokens,
                cost: cost.filter(|c| *c > 0.0),
                stopped: stopped.map(str::to_string),
            }));
        }
        if self.turn != Turn::Exited {
            self.turn = Turn::Idle;
        }
    }

    /// Notes `text` for later, if the pane is hidden now.
    pub fn note(&mut self, text: impl Into<String>, tone: &'static str) {
        if self.hidden {
            self.notes.push(Note { text: text.into(), tone });
        }
    }

    /// Offers the hosts tool `id`'s output says the network policy blocked.
    fn scan_blocks(&mut self, id: &str) {
        let Some(Entry::Tool(tool)) = self.tools.get(id).and_then(|&i| self.entries.get(i)) else { return };
        let text: Vec<&str> = tool
            .content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text(text) => Some(text.as_str()),
                ToolContent::Diff { .. } => None,
            })
            .collect();
        let blocks = crate::network::blocks_in(&text.join("\n"));
        if blocks.org && self.blocked_seen.insert((id.to_string(), String::new())) {
            self.entries.push(Entry::Notice(
                "A request was blocked by your organization's policy; ask your IT team if you need access".into(),
            ));
        }
        let found = blocks.ruled.into_iter().map(|h| (h, true)).chain(blocks.hosts.into_iter().map(|h| (h, false)));
        for (host, ruled) in found {
            if !self.blocked_seen.insert((id.to_string(), host.clone())) {
                continue;
            }
            match self.blocked.iter_mut().find(|b| b.host == host) {
                Some(known) => known.ruled |= ruled,
                None => {
                    self.note(format!("The sandbox was blocked from reaching {host}"), "warning");
                    self.blocked.push(Blocked { host, ruled, sent: false });
                }
            }
        }
    }

    /// Forgets the conversation (setup stays), for another session.
    pub fn reset_conversation(&mut self) {
        self.entries.clear();
        self.tools.clear();
        self.plan.clear();
        self.permission = None;
        self.context = None;
        self.cost = None;
        self.turn_start = None;
    }

    /// The tool running now, newest first, for the working line.
    pub fn running_tool(&self) -> Option<&ToolCall> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Tool(tool) if tool.status == ToolStatus::Running => Some(tool),
            _ => None,
        })
    }

    /// Tools still open when their turn ends never finish.
    fn settle_tools(&mut self, status: ToolStatus, now: Instant) {
        for entry in &mut self.entries {
            if let Entry::Tool(tool) = entry
                && !tool.status.settled()
            {
                tool.set_status(status, now);
            }
        }
    }

    pub fn error(&mut self, text: impl Into<String>, detail: Option<String>) {
        self.entries.push(Entry::Error { text: text.into(), detail });
    }

    /// The agent asks before running a tool; `params` are the request's.
    pub fn ask(&mut self, request: Value, params: &Value, now: Instant) {
        let fields = &params["toolCall"];
        let Some(id) = fields["toolCallId"].as_str() else { return };
        self.tool_mut(id, now).merge(fields, now);
        let options = params["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            id: o["optionId"].as_str()?.to_string(),
                            name: o["name"].as_str().unwrap_or_default().to_string(),
                            kind: o["kind"].as_str().unwrap_or_default().to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.permission = Some(Permission { request, tool: id.to_string(), options });
    }

    /// Records the answer to the waiting permission on its tool and hands the
    /// request back, with the option picked (`None`: the request is cancelled).
    pub fn answer(&mut self, approval: Approval) -> Option<(Value, Option<String>)> {
        let permission = self.permission.take()?;
        let kinds: &[&str] = match approval {
            Approval::Once => &["allow_once", "allow_always"],
            Approval::Always => &["allow_always", "allow_once"],
            Approval::Denied => &["reject_once", "reject_always"],
        };
        let option = permission.option(kinds).map(|o| o.id.clone());
        let recorded = match (approval, permission.option(kinds).map(|o| o.kind.as_str())) {
            (Approval::Denied, _) => Approval::Denied,
            (_, Some("allow_always")) => Approval::Always,
            _ => Approval::Once,
        };
        if let Some(&index) = self.tools.get(&permission.tool)
            && let Entry::Tool(tool) = &mut self.entries[index]
        {
            tool.approval = Some(recorded);
        }
        Some((permission.request, option))
    }

    /// The agent is gone: the turn cannot end, and nothing can be sent.
    pub fn exited(&mut self, message: impl Into<String>, detail: Option<String>, now: Instant) {
        self.settle_thought(now);
        self.settle_tools(ToolStatus::Cancelled, now);
        self.permission = None;
        self.error(message, detail);
        self.turn = Turn::Exited;
    }

    /// Takes the agent's `configOptions` (all of them, as it sends them).
    pub fn set_options(&mut self, options: &Value) {
        if let Some(options) = options.as_array() {
            self.options = options.iter().filter_map(ConfigOption::parse).collect();
        }
    }

    /// Takes the agent's `models` (`currentModelId` and `availableModels`).
    pub fn set_models(&mut self, models: &Value) {
        if let Some(available) = models["availableModels"].as_array() {
            self.models = available
                .iter()
                .filter_map(|m| {
                    let id = m["modelId"].as_str()?.to_string();
                    Some(Mode { name: m["name"].as_str().map_or_else(|| id.clone(), str::to_string), id })
                })
                .collect();
        }
        if let Some(current) = models["currentModelId"].as_str() {
            self.model = Some(current.to_string());
        }
    }

    pub fn option(&self, setting: Setting) -> Option<&ConfigOption> {
        self.options.iter().find(|o| match setting {
            Setting::Model => o.category == "model" || o.id == "model",
            Setting::Mode => o.category == "mode" || o.id == "mode",
            Setting::Effort => o.is_effort(),
        })
    }

    /// The model's name, as the agent reports it.
    pub fn model_name(&self) -> Option<&str> {
        if let Some(option) = self.option(Setting::Model) {
            return Some(option.current_name());
        }
        let id = self.model.as_deref()?;
        Some(self.models.iter().find(|m| m.id == id).map_or(id, |m| m.name.as_str()))
    }

    pub fn effort_name(&self) -> Option<&str> {
        self.option(Setting::Effort).map(ConfigOption::current_name)
    }

    /// The agent's choices for `setting` as (value, name), and the current value.
    pub fn choices(&self, setting: Setting) -> (Vec<(String, String)>, Option<String>) {
        let listed = |list: &[Mode]| list.iter().map(|m| (m.id.clone(), m.name.clone())).collect();
        match (self.option(setting), setting) {
            (Some(option), _) => (option.choices.clone(), Some(option.current.clone())),
            (None, Setting::Model) => (listed(&self.models), self.model.clone()),
            (None, Setting::Mode) => (listed(&self.modes), self.mode.clone()),
            (None, Setting::Effort) => (Vec::new(), None),
        }
    }

    /// The name of the current mode, as the agent calls it.
    pub fn mode_name(&self) -> Option<&str> {
        if let Some(option) = self.option(Setting::Mode) {
            return Some(option.current_name());
        }
        let id = self.mode.as_deref()?;
        Some(self.modes.iter().find(|m| m.id == id).map_or(id, |m| m.name.as_str()))
    }

    /// The value of the mode after the current one, wrapping around.
    pub fn next_mode(&self) -> Option<String> {
        let (choices, current) = self.choices(Setting::Mode);
        let at = choices.iter().position(|(v, _)| Some(v) == current.as_ref());
        choices.get(at.map_or(0, |i| (i + 1) % choices.len())).map(|(v, _)| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn chunk(kind: &str, text: &str) -> Value {
        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
    }

    #[test]
    fn chunks_are_joined_into_messages_and_thoughts() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.prompt(&Prompt::text("hi"), now);
        t.apply(&chunk("agent_thought_chunk", "Let me "), now);
        t.apply(&chunk("agent_thought_chunk", "think"), now);
        t.apply(&chunk("agent_message_chunk", "Hello"), now + Duration::from_secs(2));
        t.apply(&chunk("agent_message_chunk", ", world"), now);
        assert_eq!(t.entries.len(), 3);
        let Entry::Thought(thought) = &t.entries[1] else { panic!("{:?}", t.entries[1]) };
        assert_eq!(thought.text, "Let me think");
        assert_eq!(thought.took, Some(Duration::from_secs(2)));
        assert_eq!(t.entries[2], Entry::Agent("Hello, world".into()));
    }

    #[test]
    fn tool_calls_merge_their_updates() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.apply(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run tests", "kind": "execute",
                "status": "pending", "rawInput": {"command": "cargo test"}}),
            now,
        );
        t.apply(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}), now);
        let later = now + Duration::from_millis(1500);
        t.apply(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "ok"}},
                            {"type": "diff", "path": "/w/a.rs", "oldText": "a", "newText": "b"},
                            {"type": "terminal", "terminalId": "x"}]}),
            later,
        );
        assert_eq!(t.entries.len(), 1);
        let tool = t.tool("t1").unwrap();
        assert_eq!(tool.title, "Run tests");
        assert_eq!(tool.status, ToolStatus::Done);
        assert_eq!(tool.target, Some(("cargo test".into(), TargetKind::Command)));
        assert_eq!(tool.finished, Some(later));
        assert_eq!(
            tool.content,
            vec![
                ToolContent::Text("ok".into()),
                ToolContent::Diff { path: "/w/a.rs".into(), old: Some("a".into()), new: "b".into() }
            ]
        );
    }

    #[test]
    fn targets_come_from_input_or_locations() {
        let loc = json!([{"path": "/w/src/main.rs", "line": 12}]);
        assert_eq!(target(&json!({}), &loc), Some(("/w/src/main.rs:12".into(), TargetKind::Path)));
        assert_eq!(target(&json!({"pattern": "fn main"}), &Value::Null), Some(("fn main".into(), TargetKind::Pattern)));
        assert_eq!(target(&json!({"command": ["ls", "-la"]}), &loc), Some(("ls -la".into(), TargetKind::Command)));
        assert_eq!(
            target(&json!({"url": "https://x.dev"}), &Value::Null),
            Some(("https://x.dev".into(), TargetKind::Query))
        );
        assert_eq!(target(&json!({"other": 1}), &Value::Null), None);
    }

    #[test]
    fn permissions_pick_the_matching_option_and_mark_the_tool() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        let params = json!({"sessionId": "s", "toolCall": {"toolCallId": "t1", "title": "rm -rf build", "kind": "execute"},
            "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                        {"optionId": "no", "name": "Reject", "kind": "reject_once"}]});
        t.ask(json!(7), &params, now);
        assert_eq!(t.permission.as_ref().unwrap().tool, "t1");
        assert_eq!(t.answer(Approval::Always), Some((json!(7), Some("always".into()))));
        assert_eq!(t.tool("t1").unwrap().approval, Some(Approval::Always));
        assert!(t.permission.is_none());

        t.ask(json!(8), &params, now);
        assert_eq!(t.answer(Approval::Denied), Some((json!(8), Some("no".into()))));
        assert_eq!(t.tool("t1").unwrap().approval, Some(Approval::Denied));
        assert_eq!(t.answer(Approval::Once), None);
    }

    #[test]
    fn an_offered_only_always_option_is_recorded_as_always() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        let params = json!({"toolCall": {"toolCallId": "t"},
            "options": [{"optionId": "a", "name": "Always", "kind": "allow_always"}]});
        t.ask(json!(1), &params, now);
        assert_eq!(t.answer(Approval::Once), Some((json!(1), Some("a".into()))));
        assert_eq!(t.tool("t").unwrap().approval, Some(Approval::Always));
    }

    #[test]
    fn a_turn_end_settles_open_tools_and_says_why() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.prompt(&Prompt::text("go"), now);
        t.apply(&json!({"sessionUpdate": "tool_call", "toolCallId": "t", "status": "in_progress"}), now);
        t.apply(&json!({"sessionUpdate": "usage_update", "used": 64000, "size": 200000, "cost": {"amount": 0.25, "currency": "USD"}}), now);
        let later = now + Duration::from_secs(3);
        t.turn_ended(Some("cancelled"), &json!({"inputTokens": 1000, "outputTokens": 200}), later);
        assert_eq!(t.tool("t").unwrap().status, ToolStatus::Cancelled);
        assert_eq!(
            t.entries.last(),
            Some(&Entry::TurnEnd(TurnUsage {
                took: Duration::from_secs(3),
                tokens: Some(1200),
                cost: Some(0.25),
                stopped: Some("Interrupted".into())
            }))
        );
        assert_eq!((t.context, t.turn), (Some((64000, 200000)), Turn::Idle));
        // A response without a turn in flight adds nothing.
        t.turn_ended(Some("end_turn"), &Value::Null, now);
        assert_eq!(t.entries.len(), 3);
    }

    #[test]
    fn subscription_limits_come_with_the_usage_updates() {
        let now = Instant::now();
        let mut t = Transcript::default();
        t.apply(&json!({"sessionUpdate": "usage_update", "used": 1, "size": 2}), now);
        assert_eq!(t.limits, None);
        let event = |kind: &str, used: f64| {
            json!({"sessionUpdate": "usage_update", "used": 1, "size": 2,
                "_meta": {"_claude/rateLimit": {"status": "allowed", "rateLimitType": kind, "utilization": used}}})
        };
        t.apply(&event("five_hour", 0.235), now);
        assert_eq!(t.limits.map(|l| (l.five_hour.map(|p| p.round()), l.seven_day)), Some((Some(24.0), None)));
        t.apply(&event("seven_day", 0.9), now);
        t.apply(&event("seven_day_opus", 0.5), now);
        assert_eq!(t.limits.unwrap().seven_day.map(|p| p.round()), Some(90.0));
    }

    #[test]
    fn setup_steps_follow_the_log() {
        let now = Instant::now();
        let mut setup = Setup::new(now);
        setup.log(Line::Step("Looking for an existing sandbox".into()), now);
        setup.log(Line::Info("found 'claude-app'".into()), now);
        setup.log(Line::Step("Starting the sandbox".into()), now + Duration::from_secs(1));
        setup.log(Line::Wait("waiting for the sandbox".into()), now);
        setup.log(Line::Warn("newer image".into()), now);
        setup.log(Line::Ok("sandbox is running".into()), now + Duration::from_secs(4));
        let titles: Vec<(&str, StepStatus)> = setup.steps.iter().map(|s| (s.title.as_str(), s.status)).collect();
        assert_eq!(
            titles,
            [
                ("Looking for an existing sandbox", StepStatus::Done),
                ("Starting the sandbox", StepStatus::Running),
                ("newer image", StepStatus::Warned)
            ]
        );
        assert_eq!(setup.steps[0].took, Some(Duration::from_secs(1)));
        assert_eq!(setup.steps[1].notes, ["sandbox is running"]);
        assert_eq!(setup.steps[1].waiting, None);
        setup.log(Line::Output("boom\n".into()), now);
        setup.log(Line::Error("`sbx run` failed".into()), now);
        assert!(setup.failed());
        assert_eq!(setup.steps[1].output.as_deref(), Some("boom\n"));
        setup.finish(now + Duration::from_secs(5));
        assert_eq!(setup.took, Some(Duration::from_secs(5)));
    }

    #[test]
    fn plan_modes_and_commands_are_kept() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.apply(
            &json!({"sessionUpdate": "plan", "entries": [{"content": "Read", "status": "completed", "priority": "high"},
                                                          {"content": "Fix", "status": "in_progress"}]}),
            now,
        );
        assert_eq!(t.plan.len(), 2);
        assert_eq!(t.plan[1], PlanEntry { content: "Fix".into(), status: "in_progress".into() });
        t.modes = vec![
            Mode { id: "default".into(), name: "Default".into() },
            Mode { id: "plan".into(), name: "Plan".into() },
        ];
        t.apply(&json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}), now);
        assert_eq!(t.mode_name(), Some("Plan"));
        assert_eq!(t.next_mode().as_deref(), Some("default"));
        t.apply(
            &json!({"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "init", "description": "Write CLAUDE.md"},
                {"name": "review", "description": "Review a PR", "input": {"hint": "PR number"}}]}),
            now,
        );
        assert_eq!(t.commands.len(), 2);
        assert_eq!((t.commands[0].name.as_str(), t.commands[0].description.as_str()), ("init", "Write CLAUDE.md"));
        assert_eq!(t.commands[1].hint.as_deref(), Some("PR number"));
    }

    #[test]
    fn model_and_effort_come_from_config_options_or_models() {
        let (mut t, now) = (Transcript::default(), Instant::now());
        t.set_models(&json!({"currentModelId": "sonnet", "availableModels": [
            {"modelId": "sonnet", "name": "Sonnet 5.5"}, {"modelId": "opus", "name": "Opus 5.5"}]}));
        assert_eq!((t.model_name(), t.effort_name()), (Some("Sonnet 5.5"), None));
        t.apply(
            &json!({"sessionUpdate": "config_option_update", "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "opus",
                 "options": [{"group": "Claude", "name": "Claude", "options": [
                    {"value": "opus", "name": "Opus 5.5"}, {"value": "haiku", "name": "Haiku 5.5"}]}]},
                {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "low"}, {"value": "high", "name": "high"}]}]}),
            now,
        );
        assert_eq!((t.model_name(), t.effort_name()), (Some("Opus 5.5"), Some("high")));
        assert_eq!(t.option(Setting::Model).map(|o| o.choices.len()), Some(2));
    }
}
