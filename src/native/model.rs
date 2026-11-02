//! What the native session shows, built from the agent's ACP messages: the
//! transcript, the plan, a permission waiting for an answer and where the turn
//! is. Pure state; `view` draws it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User(String),
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

#[derive(Debug)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// Tool call id to its index in `entries`.
    tools: HashMap<String, usize>,
    pub plan: Vec<PlanEntry>,
    pub permission: Option<Permission>,
    pub turn: Turn,
    pub modes: Vec<Mode>,
    pub mode: Option<String>,
    pub commands: Vec<String>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            tools: HashMap::new(),
            plan: Vec::new(),
            permission: None,
            turn: Turn::Starting,
            modes: Vec::new(),
            mode: None,
            commands: Vec::new(),
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
                    Some(Entry::User(message)) => message.push_str(&text),
                    _ => self.entries.push(Entry::User(text)),
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
            "available_commands_update" => {
                self.commands = update["availableCommands"]
                    .as_array()
                    .map(|c| c.iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
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
    pub fn prompt(&mut self, text: &str, now: Instant) {
        self.settle_thought(now);
        self.entries.push(Entry::User(text.to_string()));
        self.turn = Turn::Working;
    }

    /// The prompt's turn ended with ACP's `stopReason`.
    pub fn turn_ended(&mut self, stop_reason: Option<&str>, now: Instant) {
        self.settle_thought(now);
        self.settle_tools(ToolStatus::Cancelled, now);
        let notice = match stop_reason {
            Some("cancelled") => Some("Cancelled"),
            Some("max_tokens") => Some("The agent reached its token limit for this turn"),
            Some("max_turn_requests") => Some("The agent reached its request limit for this turn"),
            Some("refusal") => Some("The agent refused to continue"),
            _ => None,
        };
        if let Some(text) = notice {
            self.entries.push(Entry::Notice(text.to_string()));
        }
        if self.turn != Turn::Exited {
            self.turn = Turn::Idle;
        }
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

    /// The name of the current mode, as the agent calls it.
    pub fn mode_name(&self) -> Option<&str> {
        let id = self.mode.as_deref()?;
        Some(self.modes.iter().find(|m| m.id == id).map_or(id, |m| m.name.as_str()))
    }

    /// The mode after the current one, wrapping around.
    pub fn next_mode(&self) -> Option<&Mode> {
        let at = self.modes.iter().position(|m| Some(&m.id) == self.mode.as_ref());
        self.modes.get(at.map_or(0, |i| (i + 1) % self.modes.len()))
    }

    /// The tail of `entries` is a turn still streaming text, which shows itself.
    pub fn streaming(&self) -> bool {
        matches!(self.entries.last(), Some(Entry::Agent(_)))
            || matches!(self.entries.last(), Some(Entry::Thought(t)) if t.took.is_none())
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
        t.prompt("hi", now);
        t.apply(&chunk("agent_thought_chunk", "Let me "), now);
        t.apply(&chunk("agent_thought_chunk", "think"), now);
        t.apply(&chunk("agent_message_chunk", "Hello"), now + Duration::from_secs(2));
        t.apply(&chunk("agent_message_chunk", ", world"), now);
        assert_eq!(t.entries.len(), 3);
        let Entry::Thought(thought) = &t.entries[1] else { panic!("{:?}", t.entries[1]) };
        assert_eq!(thought.text, "Let me think");
        assert_eq!(thought.took, Some(Duration::from_secs(2)));
        assert_eq!(t.entries[2], Entry::Agent("Hello, world".into()));
        assert!(t.streaming());
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
        t.prompt("go", now);
        t.apply(&json!({"sessionUpdate": "tool_call", "toolCallId": "t", "status": "in_progress"}), now);
        t.turn_ended(Some("cancelled"), now);
        assert_eq!(t.tool("t").unwrap().status, ToolStatus::Cancelled);
        assert_eq!(t.entries.last(), Some(&Entry::Notice("Cancelled".into())));
        assert_eq!(t.turn, Turn::Idle);
        t.turn_ended(Some("end_turn"), now);
        assert_eq!(t.entries.len(), 3);
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
        assert_eq!(t.next_mode().map(|m| m.id.as_str()), Some("default"));
        t.apply(&json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "init"}]}), now);
        assert_eq!(t.commands, vec!["init"]);
    }
}
